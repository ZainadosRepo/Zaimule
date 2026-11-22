//! Phase-2 software rasterizer for the 3D geometry latched by SWAP_BUFFERS.
//!
//! Runs once per displayed frame (from ppu.rs, when engine A's BG0 is in 3D
//! mode) over Gpu3d::disp_polys/disp_verts. Uses Sutherland-Hodgman clipping in
//! clip space, perspective divide, viewport transform, exact scanline-span
//! incremental triangle rasterization with perspective-correct (1/w)
//! interpolation of color and texture coordinates, depth test per
//! POLYGON_ATTR, and W- or Z-buffering per the latched SWAP_BUFFERS parameter.

use super::bus::Machine;
use super::ppu::{HEIGHT, WIDTH};

static POLYSTATS: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var("NDS_POLYSTATS").is_ok());

/// NDS_PIXDBG="x,y": log every rasterizer event affecting that pixel.
static PIXDBG: std::sync::LazyLock<Option<(usize, usize)>> = std::sync::LazyLock::new(|| {
    let v = std::env::var("NDS_PIXDBG").ok()?;
    let (x, y) = v.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
});

static DUMP3D: std::sync::LazyLock<Option<String>> =
    std::sync::LazyLock::new(|| std::env::var("NDS_DUMP3D").ok());

/// Sentinel for "nothing drawn here": the 2D compositor treats it as
/// transparent so lower layers show through.
pub const TRANSPARENT: u32 = u32::MAX;

/// Sub-texel bias applied before picking a texel (NDS_TEXBIAS overrides it).
static TEXBIAS: std::sync::LazyLock<f64> = std::sync::LazyLock::new(|| {
    std::env::var("NDS_TEXBIAS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0 / 1024.0)
});

static TEXDBG: std::sync::LazyLock<Option<u32>> = std::sync::LazyLock::new(|| {
    let want = std::env::var("NDS_TEXDBG").ok()?;
    u32::from_str_radix(want.trim().trim_start_matches("0x"), 16).ok()
});

#[derive(Clone, Copy, Default)]
struct V {
    // Clip space (raw 20.12 units as floats; the scale cancels out).
    x: f64,
    y: f64,
    z: f64,
    w: f64,
    r: f64,
    g: f64,
    b: f64,
    s: f64, // texcoords in texels (input is 12.4 fixed)
    t: f64,
    // Screen space, filled after projection.
    sx: f64,
    sy: f64,
    d: f64,  // depth value
    rw: f64, // 1/w
}

#[derive(Clone, Copy)]
struct PreparedPoly {
    pi: usize,
    vs: usize,
    ve: usize,
    avg_d: f64,
    py_min: usize,
    py_max: usize,
    depth_equal: bool,
}

/// Reusable scratch buffers owned by [`super::ppu::Ppu`] so 3D rasterization
/// performs zero heap allocations per frame.
pub struct Render3dScratch {
    pub fb: Vec<u32>,
    depth: Vec<f64>,
    poly_id: Vec<u8>,
    stencil: Vec<bool>,
    prep_verts: Vec<V>,
    prepared: Vec<PreparedPoly>,
    clip_a: Vec<V>,
    clip_b: Vec<V>,
    order_pass0: Vec<usize>,
    order_pass1: Vec<usize>,
}

impl Default for Render3dScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl Render3dScratch {
    pub fn new() -> Self {
        Self {
            fb: vec![TRANSPARENT; WIDTH * HEIGHT],
            depth: vec![1.0; WIDTH * HEIGHT],
            poly_id: vec![0; WIDTH * HEIGHT],
            stencil: vec![false; WIDTH * HEIGHT],
            prep_verts: Vec::with_capacity(4096),
            prepared: Vec::with_capacity(1024),
            clip_a: Vec::with_capacity(12),
            clip_b: Vec::with_capacity(12),
            order_pass0: Vec::with_capacity(1024),
            order_pass1: Vec::with_capacity(256),
        }
    }
}

// ---- Persistent zero-dependency scanline-band worker pool ------------------

struct BandJob {
    ctx_ptr: usize,
    run: fn(usize, usize),
    band_count: usize,
}

struct PoolShared {
    generation: u64,
    job: Option<BandJob>,
}

struct BandWorkerPool {
    dispatch_lock: std::sync::Mutex<()>,
    shared: std::sync::Arc<(
        std::sync::Mutex<PoolShared>,
        std::sync::Condvar,
        std::sync::atomic::AtomicUsize,
        std::sync::Mutex<()>,
        std::sync::Condvar,
    )>,
    max_bands: usize,
}

impl BandWorkerPool {
    fn global() -> &'static Self {
        static POOL: std::sync::OnceLock<BandWorkerPool> = std::sync::OnceLock::new();
        POOL.get_or_init(Self::new)
    }

    fn new() -> Self {
        let hw = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let max_bands = hw.clamp(1, 6);
        let shared = std::sync::Arc::new((
            std::sync::Mutex::new(PoolShared {
                generation: 0,
                job: None,
            }),
            std::sync::Condvar::new(),
            std::sync::atomic::AtomicUsize::new(0),
            std::sync::Mutex::new(()),
            std::sync::Condvar::new(),
        ));
        let mut actual_bands = 1usize;
        for _ in 1..max_bands {
            let worker_band = actual_bands;
            let shared_clone = std::sync::Arc::clone(&shared);
            let spawned = std::thread::Builder::new()
                .name(format!("zaimule-3d-{worker_band}"))
                .spawn(move || {
                    let mut seen_gen = 0u64;
                    loop {
                        let (job_ctx, job_fn, band_count) = {
                            let mut guard = shared_clone.0.lock().unwrap();
                            while guard.generation == seen_gen {
                                guard = shared_clone.1.wait(guard).unwrap();
                            }
                            seen_gen = guard.generation;
                            match guard.job {
                                Some(ref j) => (j.ctx_ptr, j.run, j.band_count),
                                None => continue,
                            }
                        };
                        if worker_band < band_count {
                            let _ = std::panic::catch_unwind(|| {
                                job_fn(job_ctx, worker_band);
                            });
                            if shared_clone
                                .2
                                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
                                == 1
                            {
                                let _done_guard = shared_clone.3.lock().unwrap();
                                shared_clone.4.notify_one();
                            }
                        }
                    }
                });
            if spawned.is_ok() {
                actual_bands += 1;
            }
        }
        Self {
            dispatch_lock: std::sync::Mutex::new(()),
            shared,
            max_bands: actual_bands,
        }
    }

    /// Executes `run(ctx_ptr, band_idx)` for `band_idx in 0..bands`, running band 0 on the
    /// caller thread and bands `1..bands` on persistent workers, blocking until all bands finish.
    fn execute_bands(&self, bands: usize, ctx_ptr: usize, run: fn(usize, usize)) {
        let bands = bands.min(self.max_bands).max(1);
        if bands == 1 {
            run(ctx_ptr, 0);
            return;
        }
        let _dispatch = self.dispatch_lock.lock().unwrap();
        let worker_count = bands - 1;
        self.shared
            .2
            .store(worker_count, std::sync::atomic::Ordering::Release);
        {
            let mut guard = self.shared.0.lock().unwrap();
            guard.job = Some(BandJob {
                ctx_ptr,
                run,
                band_count: bands,
            });
            guard.generation = guard.generation.wrapping_add(1);
            self.shared.1.notify_all();
        }

        // Guard guarantees all worker threads finish reading `ctx_ptr` even if band 0 panics.
        struct WaitGuard<'a>(&'a BandWorkerPool);
        impl Drop for WaitGuard<'_> {
            fn drop(&mut self) {
                if self.0.shared.2.load(std::sync::atomic::Ordering::Acquire) != 0 {
                    let mut done_guard = self.0.shared.3.lock().unwrap();
                    while self.0.shared.2.load(std::sync::atomic::Ordering::Acquire) != 0 {
                        done_guard = self.0.shared.4.wait(done_guard).unwrap();
                    }
                }
                self.0.shared.0.lock().unwrap().job = None;
            }
        }
        let _wait = WaitGuard(self);
        run(ctx_ptr, 0);
    }
}

#[inline(always)]
fn lerp(a: &V, b: &V, f: f64) -> V {
    let l = |p: f64, q: f64| p + (q - p) * f;
    V {
        x: l(a.x, b.x),
        y: l(a.y, b.y),
        z: l(a.z, b.z),
        w: l(a.w, b.w),
        r: l(a.r, b.r),
        g: l(a.g, b.g),
        b: l(a.b, b.b),
        s: l(a.s, b.s),
        t: l(a.t, b.t),
        ..Default::default()
    }
}

/// Clip `src` against one half-space `dist(v) >= 0`.
/// Returns `true` when `dst` now holds the active polygon (either clipped or empty),
/// or `false` when every vertex is already inside so `src` remains valid in-place.
#[inline(always)]
fn clip_plane_into(src: &[V], dst: &mut Vec<V>, dist: impl Fn(&V) -> f64) -> bool {
    let mut all_in = true;
    let mut any_in = false;
    for v in src {
        if dist(v) >= 0.0 {
            any_in = true;
        } else {
            all_in = false;
        }
    }
    if all_in {
        return false;
    }
    dst.clear();
    if !any_in {
        return true;
    }
    let n = src.len();
    for i in 0..n {
        let a = &src[i];
        let b = &src[if i + 1 < n { i + 1 } else { 0 }];
        let da = dist(a);
        let db = dist(b);
        if da >= 0.0 {
            dst.push(*a);
        }
        if (da >= 0.0) != (db >= 0.0) {
            dst.push(lerp(a, b, da / (da - db)));
        }
    }
    true
}

#[inline(always)]
fn fast_floor(x: f64) -> i32 {
    let i = x as i32;
    i - i32::from(x < i as f64)
}

#[derive(Clone, Copy)]
struct TexInfo {
    format: u32,
    tw: i32,
    th: i32,
    tw_mask: i32,
    th_mask: i32,
    repeat_s: bool,
    repeat_t: bool,
    flip_s: bool,
    flip_t: bool,
    color0_clear: bool,
    base: u32,
    pltt: u32,
    bias: f64,
}

impl TexInfo {
    #[inline(always)]
    fn from_poly(tp: u32, pltt: u32, bias: f64) -> Self {
        let tw = 8i32 << ((tp >> 20) & 7);
        let th = 8i32 << ((tp >> 23) & 7);
        Self {
            format: (tp >> 26) & 7,
            tw,
            th,
            tw_mask: tw - 1,
            th_mask: th - 1,
            repeat_s: tp & (1 << 16) != 0,
            repeat_t: tp & (1 << 17) != 0,
            flip_s: tp & (1 << 18) != 0,
            flip_t: tp & (1 << 19) != 0,
            color0_clear: tp & (1 << 29) != 0,
            base: (tp & 0xFFFF) << 3,
            pltt,
            bias,
        }
    }
}

/// Fast power-of-two texel coordinate wrap (`size` is always `8..=1024`).
#[inline(always)]
fn wrap_texcoord(c: i32, size: i32, mask: i32, repeat: bool, flip: bool) -> i32 {
    if repeat {
        if flip {
            let two = size << 1;
            let m = c & (two - 1);
            if m >= size {
                two - 1 - m
            } else {
                m
            }
        } else {
            c & mask
        }
    } else {
        c.clamp(0, mask)
    }
}

/// Pre-resolved VRAM slices for 3D textures (4 × 128 KiB slots) and palettes (8 × 16 KiB chunks),
/// built once per 3D frame to eliminate per-texel VRAM bank loops.
struct TexVramCache<'a> {
    tex_slots: [Option<&'a [u8]>; 4],
    pal_chunks: [Option<&'a [u8]>; 8],
}

impl<'a> TexVramCache<'a> {
    fn new(m: &'a Machine) -> Self {
        let mut tex_slots = [None; 4];
        for slot in 0..4u8 {
            for i in 0..4 {
                let cnt = m.vramcnt[i];
                if cnt & 0x87 == 0x83 && (cnt >> 3 & 3) == slot {
                    tex_slots[slot as usize] = Some(m.vram[i].as_slice());
                    break;
                }
            }
            if tex_slots[slot as usize].is_none() {
                if let Some(b) = m.last_tex_bank[slot as usize] {
                    let bi = b as usize;
                    if bi < 4 && (m.vramcnt[bi] & 0x80 == 0 || m.vramcnt[bi] & 0x87 == 0x80) {
                        tex_slots[slot as usize] = Some(m.vram[bi].as_slice());
                    }
                }
            }
        }
        let mut pal_chunks = [None; 8];
        if m.vramcnt[4] & 0x87 == 0x83 {
            for chunk in 0..4usize {
                let start = chunk * 0x4000;
                pal_chunks[chunk] = Some(&m.vram[4][start..start + 0x4000]);
            }
        }
        for i in [5usize, 6] {
            let cnt = m.vramcnt[i];
            if cnt & 0x87 == 0x83 {
                let ofs = (cnt >> 3 & 3) as usize;
                let chunk = (ofs & 1) + ((ofs >> 1) & 1) * 4;
                if chunk < 8 && pal_chunks[chunk].is_none() {
                    pal_chunks[chunk] = Some(m.vram[i].as_slice());
                }
            }
        }
        for (chunk, slot) in pal_chunks.iter_mut().enumerate() {
            if slot.is_none() {
                if let Some((b, sub)) = m.last_pal_chunk[chunk] {
                    let bi = b as usize;
                    if bi < 7 && (m.vramcnt[bi] & 0x80 == 0 || m.vramcnt[bi] & 0x87 == 0x80) {
                        let start = (sub as usize) * 0x4000;
                        if start + 0x4000 <= m.vram[bi].len() {
                            *slot = Some(&m.vram[bi][start..start + 0x4000]);
                        }
                    }
                }
            }
        }
        Self {
            tex_slots,
            pal_chunks,
        }
    }

    #[inline(always)]
    fn tex_read8(&self, off: u32) -> u8 {
        let slot = ((off >> 17) & 3) as usize;
        if let Some(buf) = self.tex_slots[slot] {
            buf[(off & 0x1_FFFF) as usize]
        } else {
            0
        }
    }

    #[inline(always)]
    fn texpal_read16(&self, off: u32) -> u16 {
        let off = off & 0x1_FFFE;
        let chunk = (off >> 14) as usize;
        if let Some(buf) = self.pal_chunks[chunk] {
            let o = (off & 0x3FFF) as usize;
            u16::from_le_bytes([buf[o], buf[o + 1]])
        } else {
            0
        }
    }
}

/// Tiny per-polygon cache for Format 5 (4×4 compressed) texture blocks. Adjacent
/// horizontal pixels stay inside the same 4×4 block 75%+ of the time, so caching
/// the 4 decoded palette entries avoids redundant palette/blend work per texel.
#[derive(Clone, Copy)]
struct Format5BlockCache {
    last_block_addr: u32,
    entries: [u32; 4], // RGB555 or u32::MAX for transparent
}

impl Default for Format5BlockCache {
    fn default() -> Self {
        Self {
            last_block_addr: u32::MAX,
            entries: [u32::MAX; 4],
        }
    }
}

impl Format5BlockCache {
    #[inline(always)]
    fn sample(
        &mut self,
        cache: &TexVramCache<'_>,
        tex: &TexInfo,
        x: i32,
        y: i32,
    ) -> Option<(u16, u32)> {
        let bw = tex.tw >> 2;
        let blk = ((y >> 2) * bw + (x >> 2)) as u32;
        let block_addr = (tex.base + blk * 4) & 0x7_FFFF;
        let row = cache.tex_read8(block_addr + (y & 3) as u32);
        let v = ((row >> ((x & 3) * 2)) as usize) & 3;
        if block_addr != self.last_block_addr {
            self.last_block_addr = block_addr;
            let idx_addr =
                0x2_0000 + ((block_addr & 0x1_FFFF) >> 1) + ((block_addr & 0x4_0000) >> 2);
            let info =
                cache.tex_read8(idx_addr) as u32 | ((cache.tex_read8(idx_addr + 1) as u32) << 8);
            let pal = (tex.pltt << 4) + (info & 0x3FFF) * 4;
            let c0 = cache.texpal_read16(pal);
            let c1 = cache.texpal_read16(pal + 2);
            let mix = |a: u16, b: u16, wa: u32, wb: u32, sh_div: u32| -> u32 {
                let r = (((a as u32 & 0x1F) * wa + (b as u32 & 0x1F) * wb) >> sh_div) & 0x1F;
                let g = ((((a as u32 >> 5) & 0x1F) * wa + ((b as u32 >> 5) & 0x1F) * wb) >> sh_div)
                    & 0x1F;
                let bl = ((((a as u32 >> 10) & 0x1F) * wa + ((b as u32 >> 10) & 0x1F) * wb)
                    >> sh_div)
                    & 0x1F;
                r | (g << 5) | (bl << 10)
            };
            self.entries[0] = c0 as u32;
            self.entries[1] = c1 as u32;
            match info >> 14 {
                0 => {
                    self.entries[2] = cache.texpal_read16(pal + 4) as u32;
                    self.entries[3] = u32::MAX;
                }
                1 => {
                    self.entries[2] = mix(c0, c1, 1, 1, 1);
                    self.entries[3] = u32::MAX;
                }
                2 => {
                    self.entries[2] = cache.texpal_read16(pal + 4) as u32;
                    self.entries[3] = cache.texpal_read16(pal + 6) as u32;
                }
                _ => {
                    self.entries[2] = mix(c0, c1, 5, 3, 3);
                    self.entries[3] = mix(c0, c1, 3, 5, 3);
                }
            }
        }
        let col = self.entries[v];
        if col == u32::MAX {
            None
        } else {
            Some((col as u16, 31))
        }
    }
}

/// Sample a texture using pre-resolved VRAM slices; `None` = transparent texel, `Some((rgb15, alpha 0..=31))`.
#[inline(always)]
fn sample_tex_cached(
    cache: &TexVramCache<'_>,
    f5_cache: &mut Format5BlockCache,
    tex: &TexInfo,
    s: f64,
    t: f64,
) -> Option<(u16, u32)> {
    let x = wrap_texcoord(
        fast_floor(s + tex.bias),
        tex.tw,
        tex.tw_mask,
        tex.repeat_s,
        tex.flip_s,
    );
    let y = wrap_texcoord(
        fast_floor(t + tex.bias),
        tex.th,
        tex.th_mask,
        tex.repeat_t,
        tex.flip_t,
    );
    if tex.format == 5 {
        return f5_cache.sample(cache, tex, x, y);
    }
    let base = tex.base;
    let i = (y * tex.tw + x) as u32;
    match tex.format {
        2 => {
            let b = cache.tex_read8(base + (i >> 2));
            let idx = ((b >> ((i & 3) * 2)) & 3) as u32;
            if idx == 0 && tex.color0_clear {
                return None;
            }
            Some((cache.texpal_read16((tex.pltt << 3) + idx * 2), 31))
        }
        3 => {
            let b = cache.tex_read8(base + (i >> 1));
            let idx = (if i & 1 == 0 { b & 0xF } else { b >> 4 }) as u32;
            if idx == 0 && tex.color0_clear {
                return None;
            }
            Some((cache.texpal_read16((tex.pltt << 4) + idx * 2), 31))
        }
        4 => {
            let idx = cache.tex_read8(base + i) as u32;
            if idx == 0 && tex.color0_clear {
                return None;
            }
            Some((cache.texpal_read16((tex.pltt << 4) + idx * 2), 31))
        }
        1 => {
            let b = cache.tex_read8(base + i);
            let a3 = (b >> 5) as u32;
            if a3 == 0 {
                return None;
            }
            Some((
                cache.texpal_read16((tex.pltt << 4) + (b as u32 & 0x1F) * 2),
                (a3 << 2) | (a3 >> 1),
            ))
        }
        6 => {
            let b = cache.tex_read8(base + i);
            if b >> 3 == 0 {
                return None;
            }
            Some((
                cache.texpal_read16((tex.pltt << 4) + (b as u32 & 7) * 2),
                (b >> 3) as u32,
            ))
        }
        7 => {
            let lo = cache.tex_read8(base + i * 2) as u16;
            let hi = cache.tex_read8(base + i * 2 + 1) as u16;
            let c = lo | (hi << 8);
            if c & 0x8000 == 0 {
                return None;
            }
            Some((c & 0x7FFF, 31))
        }
        _ => None,
    }
}

/// Format-specialized opaque texel sampler (eliminates `match tex.format` inside hot loops).
#[inline(always)]
fn sample_opaque_fmt<const FMT: u32>(
    cache: &TexVramCache<'_>,
    f5_cache: &mut Format5BlockCache,
    tex: &TexInfo,
    s: f64,
    t: f64,
) -> Option<u16> {
    let x = wrap_texcoord(
        fast_floor(s + tex.bias),
        tex.tw,
        tex.tw_mask,
        tex.repeat_s,
        tex.flip_s,
    );
    let y = wrap_texcoord(
        fast_floor(t + tex.bias),
        tex.th,
        tex.th_mask,
        tex.repeat_t,
        tex.flip_t,
    );
    if FMT == 5 {
        return f5_cache.sample(cache, tex, x, y).map(|(c, _)| c);
    }
    let base = tex.base;
    let i = (y * tex.tw + x) as u32;
    match FMT {
        2 => {
            let b = cache.tex_read8(base + (i >> 2));
            let idx = ((b >> ((i & 3) * 2)) & 3) as u32;
            if idx == 0 && tex.color0_clear {
                return None;
            }
            Some(cache.texpal_read16((tex.pltt << 3) + idx * 2))
        }
        3 => {
            let b = cache.tex_read8(base + (i >> 1));
            let idx = (if i & 1 == 0 { b & 0xF } else { b >> 4 }) as u32;
            if idx == 0 && tex.color0_clear {
                return None;
            }
            Some(cache.texpal_read16((tex.pltt << 4) + idx * 2))
        }
        4 => {
            let idx = cache.tex_read8(base + i) as u32;
            if idx == 0 && tex.color0_clear {
                return None;
            }
            Some(cache.texpal_read16((tex.pltt << 4) + idx * 2))
        }
        7 => {
            let lo = cache.tex_read8(base + i * 2) as u16;
            let hi = cache.tex_read8(base + i * 2 + 1) as u16;
            let c = lo | (hi << 8);
            if c & 0x8000 == 0 {
                return None;
            }
            Some(c & 0x7FFF)
        }
        _ => None,
    }
}

/// Sample a texture; `None` = transparent texel, `Some((rgb15, alpha 0..=31))`.
#[cfg(test)]
#[inline]
fn sample_tex(m: &Machine, tex: &TexInfo, s: f64, t: f64) -> Option<(u16, u32)> {
    let cache = TexVramCache::new(m);
    let mut f5 = Format5BlockCache::default();
    sample_tex_cached(&cache, &mut f5, tex, s, t)
}

#[inline(always)]
fn rgb555_to_rgb(c: u16) -> u32 {
    let r = (c & 0x1F) as u32;
    let g = ((c >> 5) & 0x1F) as u32;
    let b = ((c >> 10) & 0x1F) as u32;
    ((r << 19) | ((r >> 2) << 16)) | ((g << 11) | ((g >> 2) << 8)) | ((b << 3) | (b >> 2))
}

#[inline(always)]
fn blend_modulate_u32(t: u32, v6: u32) -> u32 {
    let t6 = if t > 0 { (t << 1) + 1 } else { 0 };
    ((((t6 + 1) * (v6 + 1)).saturating_sub(1)) >> 7).min(31)
}

#[inline(always)]
fn f64_to_v6(v: f64) -> u32 {
    let vc = v.clamp(0.0, 31.0);
    if vc > 0.0 {
        (vc * 2.0 + 1.0) as u32
    } else {
        0
    }
}

/// Render the latched 3D frame into a newly allocated WIDTH*HEIGHT buffer of 0xRRGGBB pixels.
pub fn render(m: &Machine) -> Vec<u32> {
    let mut scratch = Render3dScratch::new();
    render_into(m, &mut scratch);
    scratch.fb
}

/// Render the latched 3D frame into reusable `scratch.fb` (`WIDTH * HEIGHT` `0xRRGGBB` pixels,
/// `TRANSPARENT` where nothing was drawn and clear alpha is 0).
pub fn render_into(m: &Machine, scratch: &mut Render3dScratch) {
    render_into_scaled(m, scratch, 1);
}

/// Render the latched 3D frame at `scale`× internal resolution (`1` = `256×192`, `2` = `512×384` HD)
/// into reusable `scratch.fb` without per-frame heap allocations.
pub fn render_into_scaled(m: &Machine, scratch: &mut Render3dScratch, scale: usize) {
    render_into_scaled_impl(m, scratch, scale, false);
}

fn render_into_scaled_impl(
    m: &Machine,
    scratch: &mut Render3dScratch,
    scale: usize,
    force_single_band: bool,
) {
    let scale = scale.clamp(1, 2);
    let out_w = WIDTH * scale;
    let out_h = HEIGHT * scale;
    let sf = scale as f64;
    let gx = &m.gx;
    let tex_cache = TexVramCache::new(m);
    let wbuffer = gx.swap_param & 2 != 0;
    let clear_rgb = (gx.clear_color & 0x7FFF) as u16;
    let clear_alpha = gx.clear_color >> 16 & 0x1F;
    let clear_px = if clear_alpha == 0 {
        TRANSPARENT
    } else {
        rgb555_to_rgb(clear_rgb)
    };
    let len = out_w * out_h;
    if scratch.fb.len() != len {
        scratch.fb.resize(len, clear_px);
        scratch.depth.resize(len, 1.0);
        scratch.poly_id.resize(len, 0);
        scratch.stencil.resize(len, false);
    }
    scratch.fb.fill(clear_px);

    if gx.disp_polys.is_empty() {
        return;
    }

    let clear_d = if wbuffer {
        f64::MAX
    } else {
        gx.clear_depth as f64 / 0x7FFF as f64
    };
    scratch.depth.fill(clear_d);
    scratch.poly_id.fill(0);

    // Viewport (y flipped: clip-space +Y is up, screen +Y is down).
    let vx0 = gx.viewport[0] as f64;
    let vy0 = gx.viewport[1] as f64;
    let vw = (gx.viewport[2] as f64 - vx0 + 1.0).max(1.0);
    let vh = (gx.viewport[3] as f64 - vy0 + 1.0).max(1.0);

    scratch.prep_verts.clear();
    scratch.prepared.clear();
    scratch.order_pass0.clear();
    scratch.order_pass1.clear();
    let mut has_pass1 = false;
    let mut has_shadows = false;

    for (pi, poly) in gx.disp_polys.iter().enumerate() {
        scratch.clip_a.clear();
        for &i in &poly.verts[..poly.nverts as usize] {
            let v = &gx.disp_verts[i as usize];
            scratch.clip_a.push(V {
                x: v.clip[0] as f64,
                y: v.clip[1] as f64,
                z: v.clip[2] as f64,
                w: v.clip[3] as f64,
                r: (v.color & 0x1F) as f64,
                g: (v.color >> 5 & 0x1F) as f64,
                b: (v.color >> 10 & 0x1F) as f64,
                s: v.tex[0] as f64 * 0.0625,
                t: v.tex[1] as f64 * 0.0625,
                ..Default::default()
            });
        }

        // Frustum clipping (w first so the divides below are safe).
        let mut in_b = clip_plane_into(&scratch.clip_a, &mut scratch.clip_b, |v| v.w - 1e-4);
        for f in [
            |v: &V| v.w - v.x,
            |v: &V| v.w + v.x,
            |v: &V| v.w - v.y,
            |v: &V| v.w + v.y,
            |v: &V| v.w - v.z,
            |v: &V| v.w + v.z,
        ] {
            let plen = if in_b {
                scratch.clip_b.len()
            } else {
                scratch.clip_a.len()
            };
            if plen < 3 {
                break;
            }
            if in_b {
                if clip_plane_into(&scratch.clip_b, &mut scratch.clip_a, f) {
                    in_b = false;
                }
            } else if clip_plane_into(&scratch.clip_a, &mut scratch.clip_b, f) {
                in_b = true;
            }
        }
        let pv = if in_b {
            &mut scratch.clip_b
        } else {
            &mut scratch.clip_a
        };
        if pv.len() < 3 {
            continue;
        }

        // Hardware computes screen coordinates by integer division, so every vertex
        // lands on a whole 1x pixel and a polygon's texture is re-fitted to that
        // integer outline. Locking to the 1x integer grid before multiplying by `sf`
        // prevents outdoor buildings and ground tiles from wobbling while walking.
        let mut ymin_f = f64::MAX;
        let mut ymax_f = f64::MIN;
        for v in pv.iter_mut() {
            v.rw = 1.0 / v.w;
            v.sx = (vx0 + (v.x * v.rw * 0.5 + 0.5) * vw).floor() * sf;
            v.sy = ((HEIGHT as f64) - (vy0 + (v.y * v.rw * 0.5 + 0.5) * vh)).floor() * sf;
            v.d = if wbuffer { v.w } else { v.z * v.rw * 0.5 + 0.5 };
            if v.sy < ymin_f {
                ymin_f = v.sy;
            }
            if v.sy > ymax_f {
                ymax_f = v.sy;
            }
        }

        let py_min = ((ymin_f.floor() as i64).clamp(0, out_h as i64)) as usize;
        let py_max = (((ymax_f.ceil() as i64) + 1).clamp(0, out_h as i64)) as usize;
        if py_min >= py_max {
            continue;
        }

        // Face culling: signed area in screen space (y down).
        let mut area2 = 0.0f64;
        for i in 0..pv.len() {
            let a = &pv[i];
            let b = &pv[if i + 1 < pv.len() { i + 1 } else { 0 }];
            area2 += a.sx * b.sy - b.sx * a.sy;
        }
        let front = area2 < 0.0;
        let show_back = poly.attr & (1 << 6) != 0;
        let show_front = poly.attr & (1 << 7) != 0;
        if (front && !show_front) || (!front && !show_back) {
            continue;
        }

        let poly_mode = (poly.attr >> 4) & 3;
        let raw_alpha = (poly.attr >> 16) & 0x1F;
        let format = (poly.texparam >> 26) & 7;
        let shadow = poly_mode == 3;
        let poly_alpha = if raw_alpha == 0 { 31 } else { raw_alpha };
        let can_translucent = poly_alpha < 31 || (poly_mode != 1 && (format == 1 || format == 6));
        if shadow {
            has_shadows = true;
            has_pass1 = true;
        } else if can_translucent {
            has_pass1 = true;
        }

        let avg_d = pv.iter().map(|v| v.d).sum::<f64>() / pv.len() as f64;
        let v_start = scratch.prep_verts.len();
        scratch.prep_verts.extend_from_slice(pv);
        let v_end = scratch.prep_verts.len();
        let pidx = scratch.prepared.len();
        scratch.prepared.push(PreparedPoly {
            pi,
            vs: v_start,
            ve: v_end,
            avg_d,
            py_min,
            py_max,
            depth_equal: poly.attr & (1 << 14) != 0,
        });
        if !shadow {
            scratch.order_pass0.push(pidx);
        }
        if shadow || can_translucent {
            scratch.order_pass1.push(pidx);
        }
    }

    if scratch.prepared.is_empty() {
        return;
    }

    if has_shadows {
        scratch.stencil.fill(false);
    }

    if *POLYSTATS {
        let mut modes = [0usize; 4];
        let mut fmts = [0usize; 8];
        let mut translucent = 0usize;
        for pp in scratch.prepared.iter() {
            let p = &gx.disp_polys[pp.pi];
            modes[(p.attr >> 4 & 3) as usize] += 1;
            fmts[(p.texparam >> 26 & 7) as usize] += 1;
            let a = p.attr >> 16 & 0x1F;
            if a != 0 && a < 31 {
                translucent += 1;
            }
        }
        eprintln!(
            "[poly] drawn={} mode(mod/decal/toon/shadow)={:?} fmt0-7={:?} translucent={}",
            scratch.prepared.len(),
            modes,
            fmts,
            translucent
        );
    }

    if let Some(want) = *TEXDBG {
        for pp in scratch.prepared.iter() {
            let p = &gx.disp_polys[pp.pi];
            if p.texparam != want {
                continue;
            }
            let (fmt, tw, th) = (
                p.texparam >> 26 & 7,
                8usize << (p.texparam >> 20 & 7),
                8usize << (p.texparam >> 23 & 7),
            );
            let shift = if fmt == 2 { 3 } else { 4 };
            eprintln!(
                "[tex] param={:#010X} pltt={:#X} fmt={fmt} {tw}x{th} base={:#X} pal@{:#X} color0clear={}",
                p.texparam,
                p.pltt,
                (p.texparam & 0xFFFF) << 3,
                p.pltt << shift,
                p.texparam & 1 << 29 != 0
            );
            let vc: Vec<String> = scratch.prep_verts[pp.vs..pp.ve]
                .iter()
                .map(|v| format!("({:.0},{:.0},{:.0})", v.r, v.g, v.b))
                .collect();
            eprintln!("[tex] vertex colours (0-31): {}", vc.join(" "));
            let pal: Vec<String> = (0..16)
                .map(|c| format!("{:04X}", m.texpal_read16((p.pltt << shift) + c * 2)))
                .collect();
            eprintln!("[tex] palette: {}", pal.join(" "));
            let base = (p.texparam & 0xFFFF) << 3;
            for y in 0..th.min(48) {
                let row: String = (0..tw.min(48))
                    .map(|x| {
                        let i = (y * tw + x) as u32;
                        let idx = match fmt {
                            3 => {
                                let b = m.tex_read8(base + i / 2);
                                if i & 1 == 0 {
                                    b & 0xF
                                } else {
                                    b >> 4
                                }
                            }
                            4 => m.tex_read8(base + i),
                            _ => 0,
                        };
                        std::char::from_digit((idx & 0xF) as u32, 16).unwrap()
                    })
                    .collect();
                eprintln!("[tex] {row}");
            }
            break;
        }
    }

    // Sort Pass 0 opaque polygons front-to-back (keeping `depth_equal` overlays after standard
    // `<` polygons in original submission order) to maximize early-Z rejection before `1/w` & texel decode.
    {
        let prep = &scratch.prepared;
        scratch.order_pass0.sort_by(|&a, &b| {
            let pa = &prep[a];
            let pb = &prep[b];
            pa.depth_equal.cmp(&pb.depth_equal).then_with(|| {
                if pa.depth_equal {
                    std::cmp::Ordering::Equal
                } else {
                    pa.avg_d
                        .partial_cmp(&pb.avg_d)
                        .unwrap_or(std::cmp::Ordering::Equal)
                }
            })
        });
    }
    if has_pass1 {
        let prep = &scratch.prepared;
        scratch.order_pass1.sort_by(|&a, &b| {
            prep[b]
                .avg_d
                .partial_cmp(&prep[a].avg_d)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let tex_bias = *TEXBIAS;
    let highlight_mode = m.io2d[0][0x60] & 2 != 0;
    let pixdbg = *PIXDBG;

    let want_bands = if force_single_band || pixdbg.is_some() || scratch.prepared.len() < 24 {
        1
    } else if scale >= 2 {
        6
    } else {
        4
    };
    let pool = BandWorkerPool::global();
    let bands = want_bands.min(pool.max_bands).max(1);

    if bands <= 1 {
        raster_band(
            m,
            &tex_cache,
            &scratch.prepared,
            &scratch.prep_verts,
            &scratch.order_pass0,
            if has_pass1 { &scratch.order_pass1 } else { &[] },
            wbuffer,
            highlight_mode,
            tex_bias,
            pixdbg,
            out_w,
            out_h,
            0,
            out_h,
            &mut scratch.fb,
            &mut scratch.depth,
            &mut scratch.poly_id,
            &mut scratch.stencil,
        );
    } else {
        struct BandCtx<'a> {
            m: &'a Machine,
            tex_cache: &'a TexVramCache<'a>,
            prepared: &'a [PreparedPoly],
            prep_verts: &'a [V],
            order_pass0: &'a [usize],
            order_pass1: &'a [usize],
            wbuffer: bool,
            highlight_mode: bool,
            tex_bias: f64,
            out_w: usize,
            out_h: usize,
            bands: usize,
            fb_ptr: *mut u32,
            depth_ptr: *mut f64,
            poly_id_ptr: *mut u8,
            stencil_ptr: *mut bool,
        }
        // SAFETY: Every band `b` exclusively accesses disjoint scanlines `[y0..y1) * out_w`.
        let ctx = BandCtx {
            m,
            tex_cache: &tex_cache,
            prepared: &scratch.prepared,
            prep_verts: &scratch.prep_verts,
            order_pass0: &scratch.order_pass0,
            order_pass1: if has_pass1 { &scratch.order_pass1 } else { &[] },
            wbuffer,
            highlight_mode,
            tex_bias,
            out_w,
            out_h,
            bands,
            fb_ptr: scratch.fb.as_mut_ptr(),
            depth_ptr: scratch.depth.as_mut_ptr(),
            poly_id_ptr: scratch.poly_id.as_mut_ptr(),
            stencil_ptr: scratch.stencil.as_mut_ptr(),
        };
        fn run_band_ctx(ctx_addr: usize, b: usize) {
            // SAFETY: `ctx_addr` points to live stack `BandCtx` guarded by `WaitGuard` in `execute_bands`.
            let c = unsafe { &*(ctx_addr as *const BandCtx<'_>) };
            let y0 = (b * c.out_h) / c.bands;
            let y1 = ((b + 1) * c.out_h) / c.bands;
            if y0 >= y1 {
                return;
            }
            let off = y0 * c.out_w;
            let len = (y1 - y0) * c.out_w;
            // SAFETY: Bands `[y0, y1)` are strictly disjoint across `b in 0..c.bands`.
            let (fb_band, depth_band, pid_band, stencil_band) = unsafe {
                (
                    std::slice::from_raw_parts_mut(c.fb_ptr.add(off), len),
                    std::slice::from_raw_parts_mut(c.depth_ptr.add(off), len),
                    std::slice::from_raw_parts_mut(c.poly_id_ptr.add(off), len),
                    std::slice::from_raw_parts_mut(c.stencil_ptr.add(off), len),
                )
            };
            raster_band(
                c.m,
                c.tex_cache,
                c.prepared,
                c.prep_verts,
                c.order_pass0,
                c.order_pass1,
                c.wbuffer,
                c.highlight_mode,
                c.tex_bias,
                None,
                c.out_w,
                c.out_h,
                y0,
                y1,
                fb_band,
                depth_band,
                pid_band,
                stencil_band,
            );
        }
        pool.execute_bands(bands, (&ctx as *const BandCtx<'_>) as usize, run_band_ctx);
    }

    // Close single-pixel pinholes between adjacent fixed-point tile edges when clear is transparent.
    if clear_px == TRANSPARENT {
        let fb = &mut scratch.fb;
        for y in 1..out_h - 1 {
            let row = y * out_w;
            for x in 1..out_w - 1 {
                let i = row + x;
                if fb[i] != TRANSPARENT {
                    continue;
                }
                let (a, b) = if fb[i - 1] != TRANSPARENT && fb[i + 1] != TRANSPARENT {
                    (fb[i - 1], fb[i + 1])
                } else if fb[i - out_w] != TRANSPARENT && fb[i + out_w] != TRANSPARENT {
                    (fb[i - out_w], fb[i + out_w])
                } else {
                    continue;
                };
                let r = (((a >> 16) & 0xFF) + ((b >> 16) & 0xFF)) >> 1;
                let g = (((a >> 8) & 0xFF) + ((b >> 8) & 0xFF)) >> 1;
                let bl = ((a & 0xFF) + (b & 0xFF)) >> 1;
                fb[i] = (r << 16) | (g << 8) | bl;
            }
        }
    }

    if let Some(ref p) = *DUMP3D {
        let mut out = format!("P6\n{out_w} {out_h}\n255\n").into_bytes();
        for px in scratch.fb.iter() {
            let (r, g, b) = if *px == TRANSPARENT {
                (255, 0, 255)
            } else {
                ((px >> 16) as u8, (px >> 8) as u8, *px as u8)
            };
            out.extend_from_slice(&[r, g, b]);
        }
        let _ = std::fs::write(p, out);
    }
}

#[allow(clippy::too_many_arguments)]
fn raster_band(
    m: &Machine,
    tex_cache: &TexVramCache<'_>,
    prepared: &[PreparedPoly],
    prep_verts: &[V],
    order_pass0: &[usize],
    order_pass1: &[usize],
    wbuffer: bool,
    highlight_mode: bool,
    tex_bias: f64,
    pixdbg: Option<(usize, usize)>,
    out_w: usize,
    out_h: usize,
    band_y0: usize,
    band_y1: usize,
    fb_band: &mut [u32],
    depth_band: &mut [f64],
    poly_id_band: &mut [u8],
    stencil_band: &mut [bool],
) {
    let gx = &m.gx;
    for &pidx in order_pass0 {
        let pp = &prepared[pidx];
        if pp.py_max <= band_y0 || pp.py_min >= band_y1 {
            continue;
        }
        let poly = &gx.disp_polys[pp.pi];
        let raw_alpha = (poly.attr >> 16) & 0x1F;
        let poly_alpha = if raw_alpha == 0 { 31 } else { raw_alpha };
        raster_poly(
            m,
            tex_cache,
            poly,
            &prep_verts[pp.vs..pp.ve],
            poly_alpha,
            0,
            wbuffer,
            highlight_mode,
            tex_bias,
            pixdbg,
            out_w,
            out_h,
            band_y0,
            band_y1,
            fb_band,
            depth_band,
            poly_id_band,
            stencil_band,
        );
    }

    for &pidx in order_pass1 {
        let pp = &prepared[pidx];
        let poly = &gx.disp_polys[pp.pi];
        let poly_mode = (poly.attr >> 4) & 3;
        let shadow = poly_mode == 3;
        if shadow && (poly.attr >> 24) & 0x3F == 0 {
            stencil_band.fill(false);
        }
        if pp.py_max <= band_y0 || pp.py_min >= band_y1 {
            continue;
        }
        let raw_alpha = (poly.attr >> 16) & 0x1F;
        let poly_alpha = if raw_alpha == 0 { 31 } else { raw_alpha };
        raster_poly(
            m,
            tex_cache,
            poly,
            &prep_verts[pp.vs..pp.ve],
            poly_alpha,
            1,
            wbuffer,
            highlight_mode,
            tex_bias,
            pixdbg,
            out_w,
            out_h,
            band_y0,
            band_y1,
            fb_band,
            depth_band,
            poly_id_band,
            stencil_band,
        );
    }
}

/// Computes the exact inclusive horizontal offset range `[k_lo, k_hi]` within `0..width`
/// where all three half-space edge equations `e_i + k * dx_i >= 0` hold.
#[inline(always)]
fn row_span_k(
    e0: i64,
    dx0: i64,
    e1: i64,
    dx1: i64,
    e2: i64,
    dx2: i64,
    max_k: i64,
) -> Option<(i64, i64)> {
    let mut k_lo = 0i64;
    let mut k_hi = max_k;

    #[inline(always)]
    fn constrain(e: i64, dx: i64, k_lo: &mut i64, k_hi: &mut i64) -> bool {
        if dx > 0 {
            if e < 0 {
                let need = (-e + dx - 1) / dx;
                if need > *k_lo {
                    *k_lo = need;
                }
            }
        } else if dx < 0 {
            if e < 0 {
                return false;
            }
            let allow = e / (-dx);
            if allow < *k_hi {
                *k_hi = allow;
            }
        } else if e < 0 {
            return false;
        }
        *k_lo <= *k_hi
    }

    if !constrain(e0, dx0, &mut k_lo, &mut k_hi) {
        return None;
    }
    if !constrain(e1, dx1, &mut k_lo, &mut k_hi) {
        return None;
    }
    if !constrain(e2, dx2, &mut k_lo, &mut k_hi) {
        return None;
    }
    Some((k_lo, k_hi))
}

/// Format-specialized opaque white-vertex scanline span rasterizer with adaptive 8-pixel
/// affine subdivision: eliminates ~87% of per-pixel `f64` divisions while automatically
/// falling back to exact per-pixel `1/w` under steep near-plane perspective foreshortening.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn raster_opaque_white_span<const FMT: u32>(
    tex_cache: &TexVramCache<'_>,
    f5_cache: &mut Format5BlockCache,
    tex: &TexInfo,
    pid: u8,
    row_base: usize,
    x_start: usize,
    x_end: usize,
    mut d: f64,
    dd_dx: f64,
    mut rw: f64,
    drw_dx: f64,
    mut s_acc: f64,
    ds_dx: f64,
    mut t_acc: f64,
    dt_dx: f64,
    fb: &mut [u32],
    depth: &mut [f64],
    poly_id: &mut [u8],
) {
    const SUBSPAN: usize = 8;
    let mut k_px = x_start;
    while k_px <= x_end {
        let seg_end = (k_px + SUBSPAN - 1).min(x_end);
        let seg_steps = seg_end - k_px;
        let rw_end = rw + drw_dx * (seg_steps as f64);
        // Use linear (s, t) across up to 8 pixels only when 1/w varies by <= 2% across the segment.
        if seg_steps >= 2 && (rw_end - rw).abs() <= rw.abs() * 0.02 {
            let inv0 = 1.0 / rw;
            let inv1 = 1.0 / rw_end;
            let mut s = s_acc * inv0;
            let mut t = t_acc * inv0;
            let s_end = (s_acc + ds_dx * (seg_steps as f64)) * inv1;
            let t_end = (t_acc + dt_dx * (seg_steps as f64)) * inv1;
            let inv_steps = 1.0 / (seg_steps as f64);
            let ds = (s_end - s) * inv_steps;
            let dt = (t_end - t) * inv_steps;
            for cur in k_px..=seg_end {
                let i = row_base + cur;
                if d < depth[i] {
                    if let Some(tc) = sample_opaque_fmt::<FMT>(tex_cache, f5_cache, tex, s, t) {
                        depth[i] = d;
                        fb[i] = rgb555_to_rgb(tc);
                        poly_id[i] = pid;
                    }
                }
                d += dd_dx;
                s += ds;
                t += dt;
            }
            let adv = (seg_steps + 1) as f64;
            rw += drw_dx * adv;
            s_acc += ds_dx * adv;
            t_acc += dt_dx * adv;
        } else {
            for cur in k_px..=seg_end {
                let i = row_base + cur;
                if d < depth[i] {
                    let inv_rw = 1.0 / rw;
                    if let Some(tc) = sample_opaque_fmt::<FMT>(
                        tex_cache,
                        f5_cache,
                        tex,
                        s_acc * inv_rw,
                        t_acc * inv_rw,
                    ) {
                        depth[i] = d;
                        fb[i] = rgb555_to_rgb(tc);
                        poly_id[i] = pid;
                    }
                }
                d += dd_dx;
                rw += drw_dx;
                s_acc += ds_dx;
                t_acc += dt_dx;
            }
        }
        k_px = seg_end + 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn raster_poly(
    m: &Machine,
    tex_cache: &TexVramCache<'_>,
    poly: &super::gpu3d::Polygon,
    pv: &[V],
    poly_alpha: u32,
    pass: usize,
    wbuffer: bool,
    highlight_mode: bool,
    tex_bias: f64,
    pixdbg: Option<(usize, usize)>,
    out_w: usize,
    out_h: usize,
    band_y0: usize,
    band_y1: usize,
    fb: &mut [u32],
    depth: &mut [f64],
    poly_id: &mut [u8],
    stencil: &mut [bool],
) {
    let depth_equal = poly.attr & (1 << 14) != 0;
    let depth_write_translucent = poly.attr & (1 << 11) != 0;
    let pid = ((poly.attr >> 24) & 0x3F) as u8;
    let poly_mode = (poly.attr >> 4) & 3;
    let shadow_mask = poly_mode == 3 && pid == 0;
    let shadow_draw = poly_mode == 3 && pid != 0;
    let eps = if wbuffer { 0x200 as f64 } else { 1.0 / 4096.0 };
    let tex = TexInfo::from_poly(poly.texparam, poly.pltt, tex_bias);
    let textured = tex.format != 0;
    let fast_opaque_mod = pass == 0 && poly_mode == 0 && poly_alpha >= 31 && pixdbg.is_none();
    let mut f5_cache = Format5BlockCache::default();

    for k in 1..pv.len() - 1 {
        let mut tri = [&pv[0], &pv[k], &pv[k + 1]];
        let snap = |v: &V| ((v.sx * 16.0).round() as i64, (v.sy * 16.0).round() as i64);
        let orient = |a: (i64, i64), b: (i64, i64), c: (i64, i64)| {
            (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
        };
        let p0 = snap(tri[0]);
        let mut p1 = snap(tri[1]);
        let mut p2 = snap(tri[2]);
        let mut area = orient(p0, p1, p2);
        if area == 0 {
            continue;
        }
        if area < 0 {
            tri.swap(1, 2);
            std::mem::swap(&mut p1, &mut p2);
            area = -area;
        }
        let top_left = |a: (i64, i64), b: (i64, i64)| b.1 < a.1 || (b.1 == a.1 && b.0 < a.0);
        let bias = |tl: bool| if tl { 0i64 } else { -1 };
        let b0 = bias(top_left(p1, p2));
        let b1 = bias(top_left(p2, p0));
        let b2 = bias(top_left(p0, p1));
        let xmin = p0.0.min(p1.0).min(p2.0);
        let xmax = p0.0.max(p1.0).max(p2.0);
        let ymin = p0.1.min(p1.1).min(p2.1);
        let ymax = p0.1.max(p1.1).max(p2.1);
        let px0 = (((xmin - 8) + 15).div_euclid(16)).max(0) as usize;
        let px1 = (((xmax - 8).div_euclid(16)) + 1).clamp(0, out_w as i64) as usize;
        let py0_full = (((ymin - 8) + 15).div_euclid(16)).max(0) as usize;
        let py1_full = (((ymax - 8).div_euclid(16)) + 1).clamp(0, out_h as i64) as usize;
        let py0 = py0_full.max(band_y0);
        let py1 = py1_full.min(band_y1);
        if px0 >= px1 || py0 >= py1 {
            continue;
        }

        let inv = 1.0f64 / area as f64;
        let (d0, d1, d2) = (tri[0].d, tri[1].d, tri[2].d);
        let (rw0, rw1, rw2) = (tri[0].rw, tri[1].rw, tri[2].rw);
        let (r0, r1, r2) = (tri[0].r * rw0, tri[1].r * rw1, tri[2].r * rw2);
        let (g0, g1, g2) = (tri[0].g * rw0, tri[1].g * rw1, tri[2].g * rw2);
        let (b0c, b1c, b2c) = (tri[0].b * rw0, tri[1].b * rw1, tri[2].b * rw2);
        let (s0, s1, s2) = (tri[0].s * rw0, tri[1].s * rw1, tri[2].s * rw2);
        let (t0, t1, t2) = (tri[0].t * rw0, tri[1].t * rw1, tri[2].t * rw2);

        let flat_color = (tri[0].r - tri[1].r).abs() < 1e-4
            && (tri[0].r - tri[2].r).abs() < 1e-4
            && (tri[0].g - tri[1].g).abs() < 1e-4
            && (tri[0].g - tri[2].g).abs() < 1e-4
            && (tri[0].b - tri[1].b).abs() < 1e-4
            && (tri[0].b - tri[2].b).abs() < 1e-4;
        let flat_vr6 = f64_to_v6(tri[0].r);
        let flat_vg6 = f64_to_v6(tri[0].g);
        let flat_vb6 = f64_to_v6(tri[0].b);
        let flat_white = flat_color && flat_vr6 == 63 && flat_vg6 == 63 && flat_vb6 == 63;

        let dx0 = (p1.1 - p2.1) * 16;
        let dy0 = (p2.0 - p1.0) * 16;
        let dx1 = (p2.1 - p0.1) * 16;
        let dy1 = (p0.0 - p2.0) * 16;
        let dx2 = (p0.1 - p1.1) * 16;
        let dy2 = (p1.0 - p0.0) * 16;

        let dl0 = dx0 as f64 * inv;
        let dl1 = dx1 as f64 * inv;
        let dl2 = dx2 as f64 * inv;
        let dd_dx = dl0 * d0 + dl1 * d1 + dl2 * d2;
        let drw_dx = dl0 * rw0 + dl1 * rw1 + dl2 * rw2;
        let dr_dx = dl0 * r0 + dl1 * r1 + dl2 * r2;
        let dg_dx = dl0 * g0 + dl1 * g1 + dl2 * g2;
        let db_dx = dl0 * b0c + dl1 * b1c + dl2 * b2c;
        let ds_dx = dl0 * s0 + dl1 * s1 + dl2 * s2;
        let dt_dx = dl0 * t0 + dl1 * t1 + dl2 * t2;

        let start_c = (px0 as i64 * 16 + 8, py0 as i64 * 16 + 8);
        let mut w0_row = orient(p1, p2, start_c);
        let mut w1_row = orient(p2, p0, start_c);
        let mut w2_row = orient(p0, p1, start_c);
        let max_k = (px1 - px0 - 1) as i64;

        // Fast path 1: opaque untextured flat-colored modulation triangle.
        if fast_opaque_mod && !textured && flat_color {
            let flat_rgb15 = (tri[0].r.clamp(0.0, 31.0) as u16)
                | ((tri[0].g.clamp(0.0, 31.0) as u16) << 5)
                | ((tri[0].b.clamp(0.0, 31.0) as u16) << 10);
            let flat_rgb = rgb555_to_rgb(flat_rgb15);
            for py in py0..py1 {
                if let Some((k_lo, k_hi)) =
                    row_span_k(w0_row + b0, dx0, w1_row + b1, dx1, w2_row + b2, dx2, max_k)
                {
                    let l0 = (w0_row + k_lo * dx0) as f64 * inv;
                    let l1 = (w1_row + k_lo * dx1) as f64 * inv;
                    let l2 = (w2_row + k_lo * dx2) as f64 * inv;
                    let mut d = l0 * d0 + l1 * d1 + l2 * d2;
                    let row_base = (py - band_y0) * out_w + px0;
                    let x_start = k_lo as usize;
                    let x_end = k_hi as usize;
                    if depth_equal {
                        for k_px in x_start..=x_end {
                            let i = row_base + k_px;
                            if d <= depth[i] + eps {
                                depth[i] = d;
                                fb[i] = flat_rgb;
                                poly_id[i] = pid;
                            }
                            d += dd_dx;
                        }
                    } else {
                        for k_px in x_start..=x_end {
                            let i = row_base + k_px;
                            if d < depth[i] {
                                depth[i] = d;
                                fb[i] = flat_rgb;
                                poly_id[i] = pid;
                            }
                            d += dd_dx;
                        }
                    }
                }
                w0_row += dy0;
                w1_row += dy1;
                w2_row += dy2;
            }
            continue;
        }

        // Fast path 2: opaque textured modulation triangle (covers the vast majority of 3D DS geometry).
        if fast_opaque_mod && textured {
            let has_tex_alpha = tex.format == 1 || tex.format == 6;
            if flat_white && !has_tex_alpha && !depth_equal {
                // Hottest DS 3D path: format-specialized + adaptive 8px spans + Format 5 block cache.
                macro_rules! run_fmt_rows {
                    ($fmt:expr) => {
                        for py in py0..py1 {
                            if let Some((k_lo, k_hi)) = row_span_k(
                                w0_row + b0,
                                dx0,
                                w1_row + b1,
                                dx1,
                                w2_row + b2,
                                dx2,
                                max_k,
                            ) {
                                let l0 = (w0_row + k_lo * dx0) as f64 * inv;
                                let l1 = (w1_row + k_lo * dx1) as f64 * inv;
                                let l2 = (w2_row + k_lo * dx2) as f64 * inv;
                                raster_opaque_white_span::<$fmt>(
                                    tex_cache,
                                    &mut f5_cache,
                                    &tex,
                                    pid,
                                    (py - band_y0) * out_w + px0,
                                    k_lo as usize,
                                    k_hi as usize,
                                    l0 * d0 + l1 * d1 + l2 * d2,
                                    dd_dx,
                                    l0 * rw0 + l1 * rw1 + l2 * rw2,
                                    drw_dx,
                                    l0 * s0 + l1 * s1 + l2 * s2,
                                    ds_dx,
                                    l0 * t0 + l1 * t1 + l2 * t2,
                                    dt_dx,
                                    fb,
                                    depth,
                                    poly_id,
                                );
                            }
                            w0_row += dy0;
                            w1_row += dy1;
                            w2_row += dy2;
                        }
                    };
                }
                match tex.format {
                    2 => run_fmt_rows!(2),
                    3 => run_fmt_rows!(3),
                    4 => run_fmt_rows!(4),
                    5 => run_fmt_rows!(5),
                    7 => run_fmt_rows!(7),
                    _ => {}
                }
                continue;
            }
            for py in py0..py1 {
                if let Some((k_lo, k_hi)) =
                    row_span_k(w0_row + b0, dx0, w1_row + b1, dx1, w2_row + b2, dx2, max_k)
                {
                    let l0 = (w0_row + k_lo * dx0) as f64 * inv;
                    let l1 = (w1_row + k_lo * dx1) as f64 * inv;
                    let l2 = (w2_row + k_lo * dx2) as f64 * inv;
                    let mut d = l0 * d0 + l1 * d1 + l2 * d2;
                    let mut rw = l0 * rw0 + l1 * rw1 + l2 * rw2;
                    let mut s_acc = l0 * s0 + l1 * s1 + l2 * s2;
                    let mut t_acc = l0 * t0 + l1 * t1 + l2 * t2;
                    let mut r_acc = l0 * r0 + l1 * r1 + l2 * r2;
                    let mut g_acc = l0 * g0 + l1 * g1 + l2 * g2;
                    let mut b_acc = l0 * b0c + l1 * b1c + l2 * b2c;

                    let row_base = (py - band_y0) * out_w + px0;
                    let x_start = k_lo as usize;
                    let x_end = k_hi as usize;
                    for k_px in x_start..=x_end {
                        let i = row_base + k_px;
                        let dpass = if depth_equal {
                            d <= depth[i] + eps
                        } else {
                            d < depth[i]
                        };
                        if dpass {
                            let inv_rw = 1.0 / rw;
                            let s = s_acc * inv_rw;
                            let t = t_acc * inv_rw;
                            if let Some((tc, ta)) =
                                sample_tex_cached(tex_cache, &mut f5_cache, &tex, s, t)
                            {
                                if !has_tex_alpha || ta >= 31 {
                                    let rgb15 = if flat_white {
                                        tc
                                    } else if flat_color {
                                        (blend_modulate_u32((tc & 0x1F) as u32, flat_vr6) as u16)
                                            | ((blend_modulate_u32(
                                                ((tc >> 5) & 0x1F) as u32,
                                                flat_vg6,
                                            )
                                                as u16)
                                                << 5)
                                            | ((blend_modulate_u32(
                                                ((tc >> 10) & 0x1F) as u32,
                                                flat_vb6,
                                            )
                                                as u16)
                                                << 10)
                                    } else {
                                        let vr6 = f64_to_v6(r_acc * inv_rw);
                                        let vg6 = f64_to_v6(g_acc * inv_rw);
                                        let vb6 = f64_to_v6(b_acc * inv_rw);
                                        (blend_modulate_u32((tc & 0x1F) as u32, vr6) as u16)
                                            | ((blend_modulate_u32(((tc >> 5) & 0x1F) as u32, vg6)
                                                as u16)
                                                << 5)
                                            | ((blend_modulate_u32(((tc >> 10) & 0x1F) as u32, vb6)
                                                as u16)
                                                << 10)
                                    };
                                    depth[i] = d;
                                    fb[i] = rgb555_to_rgb(rgb15);
                                    poly_id[i] = pid;
                                }
                            }
                        }
                        d += dd_dx;
                        rw += drw_dx;
                        s_acc += ds_dx;
                        t_acc += dt_dx;
                        if !flat_color {
                            r_acc += dr_dx;
                            g_acc += dg_dx;
                            b_acc += db_dx;
                        }
                    }
                }
                w0_row += dy0;
                w1_row += dy1;
                w2_row += dy2;
            }
            continue;
        }

        // General path: Decal, Toon/Highlight, Translucent Pass 1, Shadow Volumes, Gouraud untextured, PIXDBG.
        for py in py0..py1 {
            if let Some((k_lo, k_hi)) =
                row_span_k(w0_row + b0, dx0, w1_row + b1, dx1, w2_row + b2, dx2, max_k)
            {
                let l0 = (w0_row + k_lo * dx0) as f64 * inv;
                let l1 = (w1_row + k_lo * dx1) as f64 * inv;
                let l2 = (w2_row + k_lo * dx2) as f64 * inv;
                let mut d = l0 * d0 + l1 * d1 + l2 * d2;
                let mut rw = l0 * rw0 + l1 * rw1 + l2 * rw2;
                let mut r_acc = l0 * r0 + l1 * r1 + l2 * r2;
                let mut g_acc = l0 * g0 + l1 * g1 + l2 * g2;
                let mut b_acc = l0 * b0c + l1 * b1c + l2 * b2c;
                let mut s_acc = l0 * s0 + l1 * s1 + l2 * s2;
                let mut t_acc = l0 * t0 + l1 * t1 + l2 * t2;

                let row_base = (py - band_y0) * out_w;
                let x_start = px0 + k_lo as usize;
                let x_end = px0 + k_hi as usize;

                for px in x_start..=x_end {
                    let cur_d = d;
                    let cur_rw = rw;
                    let cur_r = r_acc;
                    let cur_g = g_acc;
                    let cur_b = b_acc;
                    let cur_s = s_acc;
                    let cur_t = t_acc;
                    d += dd_dx;
                    rw += drw_dx;
                    r_acc += dr_dx;
                    g_acc += dg_dx;
                    b_acc += db_dx;
                    s_acc += ds_dx;
                    t_acc += dt_dx;

                    let i = row_base + px;
                    let dbg = pixdbg == Some((px, py));
                    let dpass = if depth_equal {
                        cur_d <= depth[i] + eps
                    } else {
                        cur_d < depth[i]
                    };
                    if dbg {
                        eprintln!(
                            "[pix] pass={pass} attr={:#010X} tex={:#010X} pltt={:#X} d={cur_d:.5} zbuf={:.5} dpass={dpass}",
                            poly.attr, poly.texparam, poly.pltt, depth[i]
                        );
                    }
                    if shadow_mask {
                        if !dpass {
                            stencil[i] = true;
                        }
                        continue;
                    }
                    if !dpass {
                        continue;
                    }
                    if shadow_draw && (!stencil[i] || poly_id[i] == pid) {
                        continue;
                    }
                    let inv_rw = 1.0 / cur_rw;
                    let vr = cur_r * inv_rw;
                    let vg = cur_g * inv_rw;
                    let vb = cur_b * inv_rw;

                    let (rgb, alpha) = match poly_mode {
                        1 => {
                            let vr_c = vr.clamp(0.0, 31.0);
                            let vg_c = vg.clamp(0.0, 31.0);
                            let vb_c = vb.clamp(0.0, 31.0);
                            let v_rgb =
                                (vr_c as u16) | ((vg_c as u16) << 5) | ((vb_c as u16) << 10);
                            if textured {
                                let s = cur_s * inv_rw;
                                let t = cur_t * inv_rw;
                                match sample_tex_cached(tex_cache, &mut f5_cache, &tex, s, t) {
                                    None => (v_rgb, poly_alpha),
                                    Some((tc, ta)) if ta >= 31 => (tc, poly_alpha),
                                    Some((tc, ta)) => {
                                        let tr = (tc & 0x1F) as u32;
                                        let tg = ((tc >> 5) & 0x1F) as u32;
                                        let tb = ((tc >> 10) & 0x1F) as u32;
                                        let dec = |tc_ch: u32, vc_ch: f64| -> u16 {
                                            ((tc_ch * (ta + 1) + (vc_ch as u32) * (31 - ta)) >> 5)
                                                .min(31)
                                                as u16
                                        };
                                        (
                                            dec(tr, vr_c)
                                                | (dec(tg, vg_c) << 5)
                                                | (dec(tb, vb_c) << 10),
                                            poly_alpha,
                                        )
                                    }
                                }
                            } else {
                                (v_rgb, poly_alpha)
                            }
                        }
                        2 => {
                            let toon_idx = (vr.clamp(0.0, 31.0).round() as usize).min(31);
                            let toon_c = m.toon_table[toon_idx];
                            let tr_n = (toon_c & 0x1F) as u32;
                            let tg_n = ((toon_c >> 5) & 0x1F) as u32;
                            let tb_n = ((toon_c >> 10) & 0x1F) as u32;
                            if !highlight_mode {
                                if textured {
                                    let s = cur_s * inv_rw;
                                    let t = cur_t * inv_rw;
                                    let Some((tc, ta)) =
                                        sample_tex_cached(tex_cache, &mut f5_cache, &tex, s, t)
                                    else {
                                        continue;
                                    };
                                    let vr6 = if tr_n > 0 { (tr_n << 1) + 1 } else { 0 };
                                    let vg6 = if tg_n > 0 { (tg_n << 1) + 1 } else { 0 };
                                    let vb6 = if tb_n > 0 { (tb_n << 1) + 1 } else { 0 };
                                    let rgb = (blend_modulate_u32((tc & 0x1F) as u32, vr6) as u16)
                                        | ((blend_modulate_u32(((tc >> 5) & 0x1F) as u32, vg6)
                                            as u16)
                                            << 5)
                                        | ((blend_modulate_u32(((tc >> 10) & 0x1F) as u32, vb6)
                                            as u16)
                                            << 10);
                                    (rgb, poly_alpha * ta / 31)
                                } else {
                                    (toon_c & 0x7FFF, poly_alpha)
                                }
                            } else {
                                let (base_r, base_g, base_b, ta) = if textured {
                                    let s = cur_s * inv_rw;
                                    let t = cur_t * inv_rw;
                                    let Some((tc, ta)) =
                                        sample_tex_cached(tex_cache, &mut f5_cache, &tex, s, t)
                                    else {
                                        continue;
                                    };
                                    let vr6 = f64_to_v6(vr);
                                    (
                                        blend_modulate_u32((tc & 0x1F) as u32, vr6),
                                        blend_modulate_u32(((tc >> 5) & 0x1F) as u32, vr6),
                                        blend_modulate_u32(((tc >> 10) & 0x1F) as u32, vr6),
                                        ta,
                                    )
                                } else {
                                    let v_i = vr.clamp(0.0, 31.0) as u32;
                                    (v_i, v_i, v_i, 31)
                                };
                                let hr = (base_r + tr_n).min(31) as u16;
                                let hg = (base_g + tg_n).min(31) as u16;
                                let hb = (base_b + tb_n).min(31) as u16;
                                (hr | (hg << 5) | (hb << 10), poly_alpha * ta / 31)
                            }
                        }
                        _ => {
                            if textured {
                                let s = cur_s * inv_rw;
                                let t = cur_t * inv_rw;
                                match sample_tex_cached(tex_cache, &mut f5_cache, &tex, s, t) {
                                    None => {
                                        if dbg {
                                            eprintln!(
                                                "[pix]   texel transparent at s={s:.2} t={t:.2}"
                                            );
                                        }
                                        continue;
                                    }
                                    Some((tc, ta)) => {
                                        if dbg {
                                            eprintln!(
                                                "[pix]   texel s={s:.3} t={t:.3} -> rgb15={tc:#06X} a={ta}"
                                            );
                                        }
                                        let vr6 = f64_to_v6(vr);
                                        let vg6 = f64_to_v6(vg);
                                        let vb6 = f64_to_v6(vb);
                                        let rgb = (blend_modulate_u32((tc & 0x1F) as u32, vr6)
                                            as u16)
                                            | ((blend_modulate_u32(((tc >> 5) & 0x1F) as u32, vg6)
                                                as u16)
                                                << 5)
                                            | ((blend_modulate_u32(((tc >> 10) & 0x1F) as u32, vb6)
                                                as u16)
                                                << 10);
                                        (rgb, poly_alpha * ta / 31)
                                    }
                                }
                            } else {
                                (
                                    (vr.clamp(0.0, 31.0) as u16)
                                        | ((vg.clamp(0.0, 31.0) as u16) << 5)
                                        | ((vb.clamp(0.0, 31.0) as u16) << 10),
                                    poly_alpha,
                                )
                            }
                        }
                    };
                    if dbg {
                        eprintln!("[pix]   rgb={rgb:#06X} alpha={alpha}");
                    }
                    if alpha == 0 {
                        continue;
                    }
                    if pass == 0 {
                        if alpha < 31 {
                            continue;
                        }
                        depth[i] = cur_d;
                        fb[i] = rgb555_to_rgb(rgb);
                        poly_id[i] = pid;
                    } else {
                        if alpha >= 31 && !shadow_draw {
                            continue;
                        }
                        let src = rgb555_to_rgb(rgb);
                        let dst = fb[i];
                        fb[i] = if dst == TRANSPARENT {
                            src
                        } else {
                            let bl = |sh: u32| {
                                let s = (src >> sh) & 0xFF;
                                let dv = (dst >> sh) & 0xFF;
                                (s * alpha + dv * (31 - alpha)) / 31
                            };
                            (bl(16) << 16) | (bl(8) << 8) | bl(0)
                        };
                        if depth_write_translucent {
                            depth[i] = cur_d;
                        }
                        if shadow_draw {
                            stencil[i] = false;
                        } else {
                            poly_id[i] = pid;
                        }
                    }
                }
            }
            w0_row += dy0;
            w1_row += dy1;
            w2_row += dy2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::gpu3d::{Polygon, Vertex};
    use super::*;

    fn setup_fullscreen_tri(m: &mut Machine, attr: u32, texparam: u32, pltt: u32, color0: u16) {
        m.gx.disp_verts = vec![
            Vertex {
                clip: [-0x1000, 0x1000, 0, 0x1000],
                color: color0,
                tex: [0, 0],
            },
            Vertex {
                clip: [0x1000, 0x1000, 0, 0x1000],
                color: color0,
                tex: [4 << 4, 0],
            },
            Vertex {
                clip: [-0x1000, -0x1000, 0, 0x1000],
                color: color0,
                tex: [0, 4 << 4],
            },
            Vertex {
                clip: [0x1000, -0x1000, 0, 0x1000],
                color: color0,
                tex: [4 << 4, 4 << 4],
            },
        ];
        m.gx.disp_polys = vec![Polygon {
            attr,
            texparam,
            pltt,
            verts: [0, 1, 3, 2],
            nverts: 4,
        }];
    }

    #[test]
    fn format5_4x4_compressed_textures_decode_slot0_and_slot2() {
        let mut m = Machine::new();
        m.vramcnt[0] = 0x83 | (0 << 3);
        m.vramcnt[1] = 0x83 | (1 << 3);
        m.vramcnt[2] = 0x83 | (2 << 3);
        m.vramcnt[4] = 0x83;

        let c0: u16 = 31;
        let c1: u16 = 31 << 10;
        let c2: u16 = 31 << 5;
        let c3: u16 = 0x7FFF;
        m.vram[4][0..2].copy_from_slice(&c0.to_le_bytes());
        m.vram[4][2..4].copy_from_slice(&c1.to_le_bytes());
        m.vram[4][4..6].copy_from_slice(&c2.to_le_bytes());
        m.vram[4][6..8].copy_from_slice(&c3.to_le_bytes());

        m.vram[0][0] = 0xE4;
        m.vram[1][0..2].copy_from_slice(&0x8000u16.to_le_bytes());

        let tp_slot0 = 5 << 26;
        let tex0 = TexInfo::from_poly(tp_slot0, 0, 1.0 / 1024.0);
        assert_eq!(sample_tex(&m, &tex0, 0.0, 0.0), Some((c0, 31)));
        assert_eq!(sample_tex(&m, &tex0, 1.0, 0.0), Some((c1, 31)));
        assert_eq!(sample_tex(&m, &tex0, 2.0, 0.0), Some((c2, 31)));
        assert_eq!(sample_tex(&m, &tex0, 3.0, 0.0), Some((c3, 31)));

        m.vram[2][0] = 0xE4;
        m.vram[1][0x1_0000..0x1_0002].copy_from_slice(&0x4000u16.to_le_bytes());

        let tp_slot2 = (5 << 26) | 0x8000;
        let tex2 = TexInfo::from_poly(tp_slot2, 0, 1.0 / 1024.0);
        assert_eq!(sample_tex(&m, &tex2, 0.0, 0.0), Some((c0, 31)));
        assert_eq!(sample_tex(&m, &tex2, 1.0, 0.0), Some((c1, 31)));
        let mid = sample_tex(&m, &tex2, 2.0, 0.0).unwrap().0;
        assert_eq!(mid & 0x1F, 15);
        assert_eq!((mid >> 5) & 0x1F, 0);
        assert_eq!((mid >> 10) & 0x1F, 15);
        assert_eq!(sample_tex(&m, &tex2, 3.0, 0.0), None);
    }

    #[test]
    fn renders_decal_toon_and_highlight_polygon_modes() {
        let mut m = Machine::new();
        m.vramcnt[0] = 0x83;
        m.vramcnt[4] = 0x83;

        for y in 0..8 {
            for x in 0..8 {
                m.vram[0][y * 8 + x] = if x < 2 { 0 } else { 1 };
            }
        }
        m.vram[4][2..4].copy_from_slice(&(31u16 << 5).to_le_bytes());

        let decal_attr = (1 << 4) | (3 << 6) | (31 << 16);
        let decal_tp = (4 << 26) | (1 << 29);
        setup_fullscreen_tri(&mut m, decal_attr, decal_tp, 0, 31 << 10);
        let fb_decal = render(&m);
        let left_px = fb_decal[96 * WIDTH + 20];
        let right_px = fb_decal[96 * WIDTH + 220];
        assert_eq!(left_px & 0xFF, 0xFF);
        assert_eq!((right_px >> 8) & 0xFF, 0xFF);

        m.io2d[0][0x60] = 0;
        m.toon_table[10] = 31;
        let toon_attr = (2 << 4) | (3 << 6) | (31 << 16);
        setup_fullscreen_tri(&mut m, toon_attr, 0, 0, 10);
        let fb_toon = render(&m);
        let center = fb_toon[96 * WIDTH + 128];
        assert_eq!(center >> 16 & 0xFF, 0xFF);
        assert_eq!(center & 0xFFFF, 0);

        m.io2d[0][0x60] = 2;
        m.toon_table[10] = 31 << 10;
        let fb_hi = render(&m);
        let hi_center = fb_hi[96 * WIDTH + 128];
        assert!(hi_center >> 16 & 0xFF > 60);
        assert_eq!(hi_center & 0xFF, 0xFF);
    }

    #[test]
    fn renders_2x_hd_512x384_3d_and_composites_with_2d_ppu() {
        let mut m = Machine::new();
        m.rebuild_vram_lut();
        let mod_attr = (3 << 6) | (31 << 16); // opaque modulation
        setup_fullscreen_tri(&mut m, mod_attr, 0, 0, 31); // pure red

        let mut scratch = Render3dScratch::new();
        render_into_scaled(&m, &mut scratch, 2);
        assert_eq!(scratch.fb.len(), (WIDTH * 2) * (HEIGHT * 2));
        let hd_center = scratch.fb[192 * (WIDTH * 2) + 256];
        assert_eq!(hd_center >> 16 & 0xFF, 0xFF);
        assert_eq!(hd_center & 0xFFFF, 0);

        // Enable Engine A Graphics + BG0 3D (bit 3 + bit 8) and run PPU in 2x HD mode
        let dispcnt: u32 = 0x0001_0108;
        m.io2d[0][0..4].copy_from_slice(&dispcnt.to_le_bytes());
        let mut ppu = super::super::ppu::Ppu::new();
        ppu.render_frame_scaled(&mut m, 2);
        assert!(ppu.hd_active);
        assert_eq!(ppu.fb_a_hd.len(), (WIDTH * 2) * (HEIGHT * 2));
        assert_eq!(ppu.fb_a_hd[192 * (WIDTH * 2) + 256], hd_center);
        assert_eq!(ppu.fb_a[96 * WIDTH + 128], hd_center);
    }

    #[test]
    fn preserves_3d_texture_and_palette_while_vram_banks_are_in_lcdc_upload_mode() {
        let mut m = Machine::new();
        m.vramcnt[2] = 0x83 | (2 << 3); // Bank C -> 3D Texture Slot 2
        m.vramcnt[5] = 0x83; // Bank F -> 3D Palette Slot 0
        m.rebuild_vram_lut();

        // Populate a 16-color texture in Slot 2 (`base = 0x4_0000`, `tp & 0xFFFF = 0x8000`)
        m.vram[2][0] = 0x11; // color index 1
        m.vram[5][2..4].copy_from_slice(&(31u16 << 5).to_le_bytes()); // color 1 = pure green

        let tp = (3 << 26) | (1 << 29) | 0x8000; // Format 3 (16-color), color0_clear, base=0x4_0000
        let tex = TexInfo::from_poly(tp, 0, 1.0 / 1024.0);
        assert_eq!(sample_tex(&m, &tex, 0.0, 0.0), Some((31 << 5, 31)));

        // Game temporarily switches Bank C and Bank F to LCDC mode (`0x80`) to stream character/outdoor frames
        m.vramcnt[2] = 0x80;
        m.vramcnt[5] = 0x80;
        m.rebuild_vram_lut();
        assert_eq!(
            sample_tex(&m, &tex, 0.0, 0.0),
            Some((31 << 5, 31)),
            "3D texture & palette must remain readable while bank is in LCDC upload mode"
        );
        assert_eq!(m.tex_read8(0x4_0000), 0x11);
        assert_eq!(m.texpal_read16(2), 31 << 5);

        // Once repurposed for 2D BG (`0x81`), the 3D texture fallback is evicted
        m.vramcnt[2] = 0x81;
        m.vramcnt[5] = 0x81;
        m.rebuild_vram_lut();
        assert_eq!(sample_tex(&m, &tex, 0.0, 0.0), None);
    }

    #[test]
    fn multiband_rasterization_matches_single_band_for_overlapping_format5_and_translucent_polys() {
        let mut m = Machine::new();
        m.vramcnt[0] = 0x83 | (0 << 3); // Slot 0 texture
        m.vramcnt[1] = 0x83 | (1 << 3); // Slot 1 Format-5 index
        m.vramcnt[4] = 0x83; // Palette
        m.rebuild_vram_lut();

        // Populate Format-5 4x4 block texture + palette
        let pal = [31u16, 31 << 5, 31 << 10, 0x7FFF];
        for (idx, &c) in pal.iter().enumerate() {
            m.vram[4][idx * 2..idx * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        for b in 0..16usize {
            m.vram[0][b] = 0xE4;
        }
        for i in 0..4usize {
            m.vram[1][i * 2..i * 2 + 2].copy_from_slice(&0x8000u16.to_le_bytes());
        }

        // Build 40 overlapping quads (>24 threshold) spanning all vertical bands:
        // mix of opaque white-vertex Format-5, depth_equal overlay, and Pass-1 translucent quads.
        m.gx.disp_verts.clear();
        m.gx.disp_polys.clear();
        for i in 0..40usize {
            let base_v = m.gx.disp_verts.len() as u16;
            let row = (i / 5) as i32;
            let col = (i % 5) as i32;
            let x0 = -0xE00 + col * 0x480;
            let x1 = x0 + 0x700;
            let y1 = 0xE00 - row * 0x360;
            let y0 = y1 - 0x600;
            let z = -0x800 + (i as i32) * 0x40;
            let w0 = 0x1000 + ((i % 4) as i32) * 0x180;
            let w1 = 0x1200 + ((i % 3) as i32) * 0x200;
            let vcolor = if i % 4 == 0 {
                0x7FFF // triggers white-vertex fast span path
            } else {
                ((i as u16 * 7) & 0x1F)
                    | (((i as u16 * 11) & 0x1F) << 5)
                    | (((i as u16 * 13) & 0x1F) << 10)
            };
            m.gx.disp_verts.push(Vertex {
                clip: [x0, y1, z, w0],
                color: vcolor,
                tex: [0, 0],
            });
            m.gx.disp_verts.push(Vertex {
                clip: [x1, y1, z, w1],
                color: vcolor,
                tex: [8 << 4, 0],
            });
            m.gx.disp_verts.push(Vertex {
                clip: [x1, y0, z, w1],
                color: vcolor,
                tex: [8 << 4, 8 << 4],
            });
            m.gx.disp_verts.push(Vertex {
                clip: [x0, y0, z, w0],
                color: vcolor,
                tex: [0, 8 << 4],
            });

            let (attr, texparam) = if i % 7 == 0 {
                // Translucent Pass 1 quad (alpha = 16)
                ((3 << 6) | (16 << 16) | (((i as u32) & 0x3F) << 24), 5 << 26)
            } else if i % 5 == 0 {
                // depth_equal opaque overlay
                ((3 << 6) | (1 << 14) | (31 << 16), 5 << 26)
            } else {
                // Standard opaque Format-5 textured quad
                ((3 << 6) | (31 << 16), 5 << 26)
            };
            m.gx.disp_polys.push(Polygon {
                attr,
                texparam,
                pltt: 0,
                verts: [base_v, base_v + 1, base_v + 2, base_v + 3],
                nverts: 4,
            });
        }

        for scale in [1usize, 2usize] {
            let mut single = Render3dScratch::new();
            let mut multi = Render3dScratch::new();
            render_into_scaled_impl(&m, &mut single, scale, true);
            render_into_scaled_impl(&m, &mut multi, scale, false);
            assert_eq!(
                single.fb, multi.fb,
                "multi-band 3D output at {scale}x must match single-band bit-for-bit"
            );
            assert!(
                multi.fb.iter().any(|&px| px != TRANSPARENT),
                "scene at {scale}x must render visible pixels"
            );
        }
    }

    #[test]
    fn box_test_culling_handles_visible_straddling_and_offscreen_boxes_conservatively() {
        let mut gx = super::super::gpu3d::Gpu3d::new();
        let run_box = |gx: &mut super::super::gpu3d::Gpu3d,
                       x: i16,
                       y: i16,
                       z: i16,
                       w: i16,
                       h: i16,
                       d: i16|
         -> bool {
            let p0 = (x as u16 as u32) | ((y as u16 as u32) << 16);
            let p1 = (z as u16 as u32) | ((w as u16 as u32) << 16);
            let p2 = (h as u16 as u32) | ((d as u16 as u32) << 16);
            gx.write_port(0x70, p0);
            gx.write_port(0x70, p1);
            gx.write_port(0x70, p2);
            (gx.gxstat_lo() & 2) != 0
        };

        // Identity clip matrix: clip = (x, y, z, 0x1000).
        assert!(run_box(
            &mut gx, -0x400, -0x400, -0x400, 0x800, 0x800, 0x800
        ));
        // Just outside frustum edge (x in [0x1020, 0x1080]) within 12.5% guard band -> kept visible.
        assert!(run_box(&mut gx, 0x1020, 0, 0, 0x60, 0x100, 0x100));
        // Clearly far off-screen to the right (x >= 0x2000 > 0x1200) -> culled.
        assert!(!run_box(&mut gx, 0x2000, 0, 0, 0x400, 0x400, 0x400));

        // Perspective-like matrix where clip.w = -z (row 2 col 3 = -1.0, row 3 col 3 = 0).
        // Column-major 4x4 fixed 20.12: m[11] = -0x1000, m[15] = 0.
        gx.write_port(0x10, 0); // MTX_MODE = PROJ
        let mut persp = [0i32; 16];
        persp[0] = 0x1000;
        persp[5] = 0x1000;
        persp[10] = -0x1000;
        persp[11] = -0x1000; // w = -z
        for word in persp {
            gx.write_port(0x16, word as u32);
        }
        // Strictly behind camera (z in [+0x400, +0x800] -> w <= -0x400) -> culled.
        assert!(!run_box(
            &mut gx, -0x200, -0x200, 0x400, 0x400, 0x400, 0x400
        ));
        // Straddling camera near plane (z in [-0x400, +0x400] -> mixed w) -> conservatively visible.
        assert!(run_box(
            &mut gx, -0x200, -0x200, -0x400, 0x400, 0x400, 0x800
        ));
    }
}
