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
static TEXBIAS: std::sync::LazyLock<f32> = std::sync::LazyLock::new(|| {
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
    x: f32,
    y: f32,
    z: f32,
    w: f32,
    r: f32,
    g: f32,
    b: f32,
    s: f32, // texcoords in texels (input is 12.4 fixed)
    t: f32,
    // Screen space, filled after projection.
    sx: f32,
    sy: f32,
    d: f32,  // depth value
    rw: f32, // 1/w
}

/// Reusable scratch buffers owned by [`super::ppu::Ppu`] so 3D rasterization
/// performs zero heap allocations per frame.
pub struct Render3dScratch {
    pub fb: Vec<u32>,
    depth: Vec<f32>,
    poly_id: Vec<u8>,
    stencil: Vec<bool>,
    prep_verts: Vec<V>,
    prepared: Vec<(usize, usize, usize, f32)>,
    clip_a: Vec<V>,
    clip_b: Vec<V>,
    order: Vec<usize>,
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
            order: Vec::with_capacity(1024),
        }
    }
}

#[inline(always)]
fn lerp(a: &V, b: &V, f: f32) -> V {
    let l = |p: f32, q: f32| p + (q - p) * f;
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
fn clip_plane_into(src: &[V], dst: &mut Vec<V>, dist: impl Fn(&V) -> f32) -> bool {
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

#[derive(Clone, Copy)]
struct TexInfo {
    format: u32,
    tw: i32,
    th: i32,
    repeat_s: bool,
    repeat_t: bool,
    flip_s: bool,
    flip_t: bool,
    color0_clear: bool,
    base: u32,
    pltt: u32,
    bias: f32,
}

impl TexInfo {
    #[inline(always)]
    fn from_poly(tp: u32, pltt: u32, bias: f32) -> Self {
        Self {
            format: (tp >> 26) & 7,
            tw: 8i32 << ((tp >> 20) & 7),
            th: 8i32 << ((tp >> 23) & 7),
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
fn wrap_texcoord(c: i32, size: i32, repeat: bool, flip: bool) -> i32 {
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
            c & (size - 1)
        }
    } else {
        c.clamp(0, size - 1)
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

/// Sample a texture using pre-resolved VRAM slices; `None` = transparent texel, `Some((rgb15, alpha 0..=31))`.
#[inline(always)]
fn sample_tex_cached(
    cache: &TexVramCache<'_>,
    tex: &TexInfo,
    s: f32,
    t: f32,
) -> Option<(u16, u32)> {
    let x = wrap_texcoord(
        (s + tex.bias).floor() as i32,
        tex.tw,
        tex.repeat_s,
        tex.flip_s,
    );
    let y = wrap_texcoord(
        (t + tex.bias).floor() as i32,
        tex.th,
        tex.repeat_t,
        tex.flip_t,
    );
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
        5 => {
            // 4x4-texel compressed:
            // - Slot 0 (`0x0_0000..0x1_FFFF`) keeps palette-index pairs in `0x2_0000..0x2_FFFF`
            // - Slot 2 (`0x4_0000..0x5_FFFF`) keeps palette-index pairs in `0x3_0000..0x3_FFFF`
            let bw = tex.tw >> 2;
            let blk = ((y >> 2) * bw + (x >> 2)) as u32;
            let block_addr = (base + blk * 4) & 0x7_FFFF;
            let row = cache.tex_read8(block_addr + (y & 3) as u32);
            let v = ((row >> ((x & 3) * 2)) as u32) & 3;
            let idx_addr =
                0x2_0000 + ((block_addr & 0x1_FFFF) >> 1) + ((block_addr & 0x4_0000) >> 2);
            let info =
                cache.tex_read8(idx_addr) as u32 | ((cache.tex_read8(idx_addr + 1) as u32) << 8);
            let pal = (tex.pltt << 4) + (info & 0x3FFF) * 4;
            let c = |n: u32| cache.texpal_read16(pal + n * 2);
            let mix = |a: u16, b: u16, wa: u32, wb: u32, sh_div: u32| -> u16 {
                let ch = |sh: u32| {
                    ((((a as u32 >> sh) & 0x1F) * wa + ((b as u32 >> sh) & 0x1F) * wb) >> sh_div)
                        & 0x1F
                };
                (ch(0) | (ch(5) << 5) | (ch(10) << 10)) as u16
            };
            match (info >> 14, v) {
                (_, 0) => Some((c(0), 31)),
                (_, 1) => Some((c(1), 31)),
                (0 | 2, 2) => Some((c(2), 31)),
                (1, 2) => Some((mix(c(0), c(1), 1, 1, 1), 31)),
                (3, 2) => Some((mix(c(0), c(1), 5, 3, 3), 31)),
                (2, 3) => Some((c(3), 31)),
                (3, 3) => Some((mix(c(0), c(1), 3, 5, 3), 31)),
                _ => None, // modes 0/1 value 3 = transparent
            }
        }
        _ => None,
    }
}

/// Sample a texture; `None` = transparent texel, `Some((rgb15, alpha 0..=31))`.
#[cfg(test)]
#[inline]
fn sample_tex(m: &Machine, tex: &TexInfo, s: f32, t: f32) -> Option<(u16, u32)> {
    let cache = TexVramCache::new(m);
    sample_tex_cached(&cache, tex, s, t)
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
fn f32_to_v6(v: f32) -> u32 {
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
    let len = WIDTH * HEIGHT;
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
        f32::MAX
    } else {
        gx.clear_depth as f32 / 0x7FFF as f32
    };
    scratch.depth.fill(clear_d);
    scratch.poly_id.fill(0);

    // Viewport (y flipped: clip-space +Y is up, screen +Y is down).
    let vx0 = gx.viewport[0] as f32;
    let vy0 = gx.viewport[1] as f32;
    let vw = (gx.viewport[2] as f32 - vx0 + 1.0).max(1.0);
    let vh = (gx.viewport[3] as f32 - vy0 + 1.0).max(1.0);

    scratch.prep_verts.clear();
    scratch.prepared.clear();
    let mut has_pass1 = false;
    let mut has_shadows = false;

    for (pi, poly) in gx.disp_polys.iter().enumerate() {
        scratch.clip_a.clear();
        for &i in &poly.verts[..poly.nverts as usize] {
            let v = &gx.disp_verts[i as usize];
            scratch.clip_a.push(V {
                x: v.clip[0] as f32,
                y: v.clip[1] as f32,
                z: v.clip[2] as f32,
                w: v.clip[3] as f32,
                r: (v.color & 0x1F) as f32,
                g: (v.color >> 5 & 0x1F) as f32,
                b: (v.color >> 10 & 0x1F) as f32,
                s: v.tex[0] as f32 * 0.0625,
                t: v.tex[1] as f32 * 0.0625,
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

        // Project.
        for v in pv.iter_mut() {
            v.rw = 1.0 / v.w;
            v.sx = (vx0 + (v.x * v.rw * 0.5 + 0.5) * vw).floor();
            v.sy = ((HEIGHT as f32) - (vy0 + (v.y * v.rw * 0.5 + 0.5) * vh)).floor();
            v.d = if wbuffer { v.w } else { v.z * v.rw * 0.5 + 0.5 };
        }

        // Face culling: signed area in screen space (y down).
        let mut area2 = 0.0f32;
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
        if poly_mode == 3 {
            has_shadows = true;
            has_pass1 = true;
        } else if (raw_alpha != 0 && raw_alpha < 31)
            || (poly_mode != 1 && (format == 1 || format == 6))
        {
            has_pass1 = true;
        }

        let avg_d = pv.iter().map(|v| v.d).sum::<f32>() / pv.len() as f32;
        let v_start = scratch.prep_verts.len();
        scratch.prep_verts.extend_from_slice(pv);
        let v_end = scratch.prep_verts.len();
        scratch.prepared.push((pi, v_start, v_end, avg_d));
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
        for &(pi, _, _, _) in scratch.prepared.iter() {
            let p = &gx.disp_polys[pi];
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
        for &(pi, vs, ve, _) in scratch.prepared.iter() {
            let p = &gx.disp_polys[pi];
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
            let vc: Vec<String> = scratch.prep_verts[vs..ve]
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

    let tex_bias = *TEXBIAS;
    let highlight_mode = m.io2d[0][0x60] & 2 != 0;
    let pixdbg = *PIXDBG;

    let passes = if has_pass1 { 2 } else { 1 };
    scratch.order.clear();
    scratch.order.extend(0..scratch.prepared.len());

    for pass in 0..passes {
        if pass == 1 {
            let prep = &scratch.prepared;
            scratch.order.sort_by(|&a, &b| {
                prep[b]
                    .3
                    .partial_cmp(&prep[a].3)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        for oi in 0..scratch.order.len() {
            let pidx = scratch.order[oi];
            let (pi, vs, ve, _) = scratch.prepared[pidx];
            let poly = &gx.disp_polys[pi];
            let poly_alpha = {
                let a = (poly.attr >> 16) & 0x1F;
                if a == 0 {
                    31
                } else {
                    a
                }
            };
            let format = (poly.texparam >> 26) & 7;
            let poly_mode = (poly.attr >> 4) & 3;
            let shadow = poly_mode == 3;
            let can_translucent =
                poly_alpha < 31 || (poly_mode != 1 && (format == 1 || format == 6));
            if pass == 0 && shadow {
                continue;
            }
            if pass == 1 && !can_translucent && !shadow {
                continue;
            }
            if shadow && (poly.attr >> 24) & 0x3F == 0 {
                scratch.stencil.fill(false);
            }
            raster_poly(
                m,
                &tex_cache,
                poly,
                &scratch.prep_verts[vs..ve],
                poly_alpha,
                pass,
                wbuffer,
                highlight_mode,
                tex_bias,
                pixdbg,
                &mut scratch.fb,
                &mut scratch.depth,
                &mut scratch.poly_id,
                &mut scratch.stencil,
            );
        }
    }

    // Close single-pixel pinholes between adjacent fixed-point tile edges.
    if clear_px == TRANSPARENT {
        let fb = &mut scratch.fb;
        for y in 1..HEIGHT - 1 {
            let row = y * WIDTH;
            for x in 1..WIDTH - 1 {
                let i = row + x;
                if fb[i] != TRANSPARENT {
                    continue;
                }
                let (a, b) = if fb[i - 1] != TRANSPARENT && fb[i + 1] != TRANSPARENT {
                    (fb[i - 1], fb[i + 1])
                } else if fb[i - WIDTH] != TRANSPARENT && fb[i + WIDTH] != TRANSPARENT {
                    (fb[i - WIDTH], fb[i + WIDTH])
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
        let mut out = format!("P6\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
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
    tex_bias: f32,
    pixdbg: Option<(usize, usize)>,
    fb: &mut [u32],
    depth: &mut [f32],
    poly_id: &mut [u8],
    stencil: &mut [bool],
) {
    let depth_equal = poly.attr & (1 << 14) != 0;
    let depth_write_translucent = poly.attr & (1 << 11) != 0;
    let pid = ((poly.attr >> 24) & 0x3F) as u8;
    let poly_mode = (poly.attr >> 4) & 3;
    let shadow_mask = poly_mode == 3 && pid == 0;
    let shadow_draw = poly_mode == 3 && pid != 0;
    let eps = if wbuffer { 0x200 as f32 } else { 1.0 / 4096.0 };
    let tex = TexInfo::from_poly(poly.texparam, poly.pltt, tex_bias);
    let textured = tex.format != 0;
    let fast_opaque_mod = pass == 0 && poly_mode == 0 && poly_alpha >= 31 && pixdbg.is_none();

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
        let px1 = (((xmax - 8).div_euclid(16)) + 1).clamp(0, WIDTH as i64) as usize;
        let py0 = (((ymin - 8) + 15).div_euclid(16)).max(0) as usize;
        let py1 = (((ymax - 8).div_euclid(16)) + 1).clamp(0, HEIGHT as i64) as usize;
        if px0 >= px1 || py0 >= py1 {
            continue;
        }

        let inv = 1.0f32 / area as f32;
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
        let flat_vr6 = f32_to_v6(tri[0].r);
        let flat_vg6 = f32_to_v6(tri[0].g);
        let flat_vb6 = f32_to_v6(tri[0].b);
        let flat_white = flat_color && flat_vr6 == 63 && flat_vg6 == 63 && flat_vb6 == 63;

        let dx0 = (p1.1 - p2.1) * 16;
        let dy0 = (p2.0 - p1.0) * 16;
        let dx1 = (p2.1 - p0.1) * 16;
        let dy1 = (p0.0 - p2.0) * 16;
        let dx2 = (p0.1 - p1.1) * 16;
        let dy2 = (p1.0 - p0.0) * 16;

        let dl0 = dx0 as f32 * inv;
        let dl1 = dx1 as f32 * inv;
        let dl2 = dx2 as f32 * inv;
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
                    let l0 = (w0_row + k_lo * dx0) as f32 * inv;
                    let l1 = (w1_row + k_lo * dx1) as f32 * inv;
                    let l2 = (w2_row + k_lo * dx2) as f32 * inv;
                    let mut d = l0 * d0 + l1 * d1 + l2 * d2;
                    let row_base = py * WIDTH + px0;
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
            for py in py0..py1 {
                if let Some((k_lo, k_hi)) =
                    row_span_k(w0_row + b0, dx0, w1_row + b1, dx1, w2_row + b2, dx2, max_k)
                {
                    let l0 = (w0_row + k_lo * dx0) as f32 * inv;
                    let l1 = (w1_row + k_lo * dx1) as f32 * inv;
                    let l2 = (w2_row + k_lo * dx2) as f32 * inv;
                    let mut d = l0 * d0 + l1 * d1 + l2 * d2;
                    let mut rw = l0 * rw0 + l1 * rw1 + l2 * rw2;
                    let mut s_acc = l0 * s0 + l1 * s1 + l2 * s2;
                    let mut t_acc = l0 * t0 + l1 * t1 + l2 * t2;
                    let mut r_acc = l0 * r0 + l1 * r1 + l2 * r2;
                    let mut g_acc = l0 * g0 + l1 * g1 + l2 * g2;
                    let mut b_acc = l0 * b0c + l1 * b1c + l2 * b2c;

                    let row_base = py * WIDTH + px0;
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
                            if let Some((tc, ta)) = sample_tex_cached(tex_cache, &tex, s, t) {
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
                                        let vr6 = f32_to_v6(r_acc * inv_rw);
                                        let vg6 = f32_to_v6(g_acc * inv_rw);
                                        let vb6 = f32_to_v6(b_acc * inv_rw);
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
                let l0 = (w0_row + k_lo * dx0) as f32 * inv;
                let l1 = (w1_row + k_lo * dx1) as f32 * inv;
                let l2 = (w2_row + k_lo * dx2) as f32 * inv;
                let mut d = l0 * d0 + l1 * d1 + l2 * d2;
                let mut rw = l0 * rw0 + l1 * rw1 + l2 * rw2;
                let mut r_acc = l0 * r0 + l1 * r1 + l2 * r2;
                let mut g_acc = l0 * g0 + l1 * g1 + l2 * g2;
                let mut b_acc = l0 * b0c + l1 * b1c + l2 * b2c;
                let mut s_acc = l0 * s0 + l1 * s1 + l2 * s2;
                let mut t_acc = l0 * t0 + l1 * t1 + l2 * t2;

                let row_base = py * WIDTH;
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
                                match sample_tex_cached(tex_cache, &tex, s, t) {
                                    None => (v_rgb, poly_alpha),
                                    Some((tc, ta)) if ta >= 31 => (tc, poly_alpha),
                                    Some((tc, ta)) => {
                                        let tr = (tc & 0x1F) as u32;
                                        let tg = ((tc >> 5) & 0x1F) as u32;
                                        let tb = ((tc >> 10) & 0x1F) as u32;
                                        let dec = |tc_ch: u32, vc_ch: f32| -> u16 {
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
                                    let Some((tc, ta)) = sample_tex_cached(tex_cache, &tex, s, t)
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
                                    let Some((tc, ta)) = sample_tex_cached(tex_cache, &tex, s, t)
                                    else {
                                        continue;
                                    };
                                    let vr6 = f32_to_v6(vr);
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
                                match sample_tex_cached(tex_cache, &tex, s, t) {
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
                                        let vr6 = f32_to_v6(vr);
                                        let vg6 = f32_to_v6(vg);
                                        let vb6 = f32_to_v6(vb);
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
}
