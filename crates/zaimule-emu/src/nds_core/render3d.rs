//! Phase-2 software rasterizer for the 3D geometry latched by SWAP_BUFFERS.
//!
//! Runs once per displayed frame (from ppu.rs, when engine A's BG0 is in 3D
//! mode) over Gpu3d::disp_polys/disp_verts. Floating point throughout:
//! Sutherland-Hodgman clipping in clip space, perspective divide, viewport
//! transform, half-space triangle fill with perspective-correct (1/w)
//! interpolation of color and texture coordinates, depth test per
//! POLYGON_ATTR, W- or Z-buffering per the latched SWAP_BUFFERS parameter.

use super::bus::Machine;

static POLYSTATS: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var("NDS_POLYSTATS").is_ok());
use super::ppu::{HEIGHT, WIDTH};

/// NDS_PIXDBG="x,y": log every rasterizer event affecting that pixel.
static PIXDBG: std::sync::LazyLock<Option<(usize, usize)>> = std::sync::LazyLock::new(|| {
    let v = std::env::var("NDS_PIXDBG").ok()?;
    let (x, y) = v.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
});

/// Sentinel for "nothing drawn here": the 2D compositor treats it as
/// transparent so lower layers show through.
pub const TRANSPARENT: u32 = u32::MAX;

/// Sub-texel bias applied before picking a texel (NDS_TEXBIAS overrides it).
/// Too small and coordinates that land exactly on a texel edge flip between
/// neighbours as the camera moves; too large and a sample at the edge of a
/// sub-image inside a texture atlas spills into the transparent gutter beside
/// it, punching a one-pixel hole in the ground.
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

#[inline]
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
#[inline]
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
    bias: f64,
}

impl TexInfo {
    #[inline]
    fn from_poly(tp: u32, pltt: u32, bias: f64) -> Self {
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

#[inline]
fn wrap_texcoord(mut c: i32, size: i32, repeat: bool, flip: bool) -> i32 {
    if repeat {
        if flip {
            let two = size * 2;
            c = c.rem_euclid(two);
            if c >= size {
                c = two - 1 - c;
            }
            c
        } else {
            c.rem_euclid(size)
        }
    } else {
        c.clamp(0, size - 1)
    }
}

/// Sample a texture; `None` = transparent texel, `Some((rgb15, alpha 0..=31))`.
#[inline]
fn sample_tex(m: &Machine, tex: &TexInfo, s: f64, t: f64) -> Option<(u16, u32)> {
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
            // 4-color paletted, 4 texels per byte; palette base in 8-byte steps.
            let b = m.tex_read8(base + (i >> 2));
            let idx = ((b >> ((i & 3) * 2)) & 3) as u32;
            if idx == 0 && tex.color0_clear {
                return None;
            }
            Some((m.texpal_read16((tex.pltt << 3) + idx * 2), 31))
        }
        3 => {
            let b = m.tex_read8(base + (i >> 1));
            let idx = (if i & 1 == 0 { b & 0xF } else { b >> 4 }) as u32;
            if idx == 0 && tex.color0_clear {
                return None;
            }
            Some((m.texpal_read16((tex.pltt << 4) + idx * 2), 31))
        }
        4 => {
            let idx = m.tex_read8(base + i) as u32;
            if idx == 0 && tex.color0_clear {
                return None;
            }
            Some((m.texpal_read16((tex.pltt << 4) + idx * 2), 31))
        }
        1 => {
            // A3I5: 3-bit alpha expanded to 5 bits.
            let b = m.tex_read8(base + i);
            let a3 = (b >> 5) as u32;
            if a3 == 0 {
                return None;
            }
            Some((
                m.texpal_read16((tex.pltt << 4) + (b as u32 & 0x1F) * 2),
                (a3 << 2) | (a3 >> 1),
            ))
        }
        6 => {
            let b = m.tex_read8(base + i);
            if b >> 3 == 0 {
                return None;
            }
            Some((
                m.texpal_read16((tex.pltt << 4) + (b as u32 & 7) * 2),
                (b >> 3) as u32,
            ))
        }
        7 => {
            let lo = m.tex_read8(base + i * 2) as u16;
            let hi = m.tex_read8(base + i * 2 + 1) as u16;
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
            let row = m.tex_read8(block_addr + (y & 3) as u32);
            let v = ((row >> ((x & 3) * 2)) as u32) & 3;
            let idx_addr =
                0x2_0000 + ((block_addr & 0x1_FFFF) >> 1) + ((block_addr & 0x4_0000) >> 2);
            let info = m.tex_read8(idx_addr) as u32 | ((m.tex_read8(idx_addr + 1) as u32) << 8);
            let pal = (tex.pltt << 4) + (info & 0x3FFF) * 4;
            let c = |n: u32| m.texpal_read16(pal + n * 2);
            let mix = |a: u16, b: u16, wa: u32, wb: u32, d: u32| -> u16 {
                let ch = |sh: u32| {
                    (((a as u32 >> sh & 0x1F) * wa + (b as u32 >> sh & 0x1F) * wb) / d) & 0x1F
                };
                (ch(0) | (ch(5) << 5) | (ch(10) << 10)) as u16
            };
            match (info >> 14, v) {
                (_, 0) => Some((c(0), 31)),
                (_, 1) => Some((c(1), 31)),
                (0 | 2, 2) => Some((c(2), 31)),
                (1, 2) => Some((mix(c(0), c(1), 1, 1, 2), 31)),
                (3, 2) => Some((mix(c(0), c(1), 5, 3, 8), 31)),
                (2, 3) => Some((c(3), 31)),
                (3, 3) => Some((mix(c(0), c(1), 3, 5, 8), 31)),
                _ => None, // modes 0/1 value 3 = transparent
            }
        }
        _ => None,
    }
}

#[inline]
fn rgb555_to_rgb(c: u16) -> u32 {
    let r = (c & 0x1F) as u32;
    let g = ((c >> 5) & 0x1F) as u32;
    let b = ((c >> 10) & 0x1F) as u32;
    ((r << 19) | ((r >> 2) << 16)) | ((g << 11) | ((g >> 2) << 8)) | ((b << 3) | (b >> 2))
}

#[inline]
fn blend_modulate(t: u32, v: f64) -> u32 {
    let t6 = if t > 0 { t * 2 + 1 } else { 0 };
    let vc = v.clamp(0.0, 31.0);
    let v6 = if vc > 0.0 { vc * 2.0 + 1.0 } else { 0.0 };
    let c6 = (((t6 as f64 + 1.0) * (v6 + 1.0) - 1.0) / 64.0) as u32;
    (c6 >> 1).min(31)
}

/// Render the latched 3D frame into a WIDTH*HEIGHT buffer of 0xRRGGBB
/// pixels, TRANSPARENT where nothing was drawn (clear alpha 0).
pub fn render(m: &Machine) -> Vec<u32> {
    let gx = &m.gx;
    let wbuffer = gx.swap_param & 2 != 0;
    let clear_rgb = (gx.clear_color & 0x7FFF) as u16;
    let clear_alpha = gx.clear_color >> 16 & 0x1F;
    let clear_px = if clear_alpha == 0 {
        TRANSPARENT
    } else {
        rgb555_to_rgb(clear_rgb)
    };
    let mut fb = vec![clear_px; WIDTH * HEIGHT];
    let clear_d = if wbuffer {
        f64::MAX
    } else {
        gx.clear_depth as f64 / 0x7FFF as f64
    };
    let mut depth = vec![clear_d; WIDTH * HEIGHT];
    // Shadow polygons (POLYGON_ATTR mode 3) need two more per-pixel buffers:
    // the id of the polygon that owns each pixel, and the stencil marking
    // where the current shadow volume falls. See raster_poly.
    let mut poly_id = vec![0u8; WIDTH * HEIGHT];
    let mut stencil = vec![false; WIDTH * HEIGHT];

    // Viewport (y flipped: clip-space +Y is up, screen +Y is down).
    let vx0 = gx.viewport[0] as f64;
    let vy0 = gx.viewport[1] as f64;
    let vw = (gx.viewport[2] as f64 - vx0 + 1.0).max(1.0);
    let vh = (gx.viewport[3] as f64 - vy0 + 1.0).max(1.0);

    // Prepare (clip, project, cull) every polygon once using reusable scratch buffers;
    // rasterize in two passes: opaque pixels first, then translucent pixels back-to-front.
    let mut prep_verts: Vec<V> = Vec::with_capacity(gx.disp_verts.len().saturating_mul(2));
    let mut prepared: Vec<(usize, usize, usize, f64)> = Vec::with_capacity(gx.disp_polys.len());
    let mut clip_a: Vec<V> = Vec::with_capacity(12);
    let mut clip_b: Vec<V> = Vec::with_capacity(12);

    for (pi, poly) in gx.disp_polys.iter().enumerate() {
        clip_a.clear();
        for &i in &poly.verts[..poly.nverts as usize] {
            let v = &gx.disp_verts[i as usize];
            clip_a.push(V {
                x: v.clip[0] as f64,
                y: v.clip[1] as f64,
                z: v.clip[2] as f64,
                w: v.clip[3] as f64,
                r: (v.color & 0x1F) as f64,
                g: (v.color >> 5 & 0x1F) as f64,
                b: (v.color >> 10 & 0x1F) as f64,
                s: v.tex[0] as f64 / 16.0,
                t: v.tex[1] as f64 / 16.0,
                ..Default::default()
            });
        }

        // Frustum clipping (w first so the divides below are safe).
        let mut in_b = clip_plane_into(&clip_a, &mut clip_b, |v| v.w - 1e-4);
        for f in [
            |v: &V| v.w - v.x,
            |v: &V| v.w + v.x,
            |v: &V| v.w - v.y,
            |v: &V| v.w + v.y,
            |v: &V| v.w - v.z,
            |v: &V| v.w + v.z,
        ] {
            let len = if in_b { clip_b.len() } else { clip_a.len() };
            if len < 3 {
                break;
            }
            if in_b {
                if clip_plane_into(&clip_b, &mut clip_a, f) {
                    in_b = false;
                }
            } else if clip_plane_into(&clip_a, &mut clip_b, f) {
                in_b = true;
            }
        }
        let pv = if in_b { &mut clip_b } else { &mut clip_a };
        if pv.len() < 3 {
            continue;
        }

        // Project.
        for v in pv.iter_mut() {
            v.rw = 1.0 / v.w;
            // Hardware computes screen coordinates by integer division, so
            // every vertex lands on a whole pixel and a polygon's texture is
            // re-fitted to that integer outline. Keeping sub-pixel positions
            // here lets the interior texture phase drift against the snapped
            // silhouette, which shows up as buildings wobbling while walking.
            v.sx = (vx0 + (v.x * v.rw * 0.5 + 0.5) * vw).floor();
            v.sy = ((HEIGHT as f64) - (vy0 + (v.y * v.rw * 0.5 + 0.5) * vh)).floor();
            v.d = if wbuffer { v.w } else { v.z * v.rw * 0.5 + 0.5 };
        }

        // Face culling: signed area in screen space (y down).
        let mut area2 = 0.0f64;
        for i in 0..pv.len() {
            let a = &pv[i];
            let b = &pv[if i + 1 < pv.len() { i + 1 } else { 0 }];
            area2 += a.sx * b.sy - b.sx * a.sy;
        }
        let front = area2 < 0.0;
        let show_back = poly.attr & 1 << 6 != 0;
        let show_front = poly.attr & 1 << 7 != 0;
        if (front && !show_front) || (!front && !show_back) {
            continue;
        }
        let avg_d = pv.iter().map(|v| v.d).sum::<f64>() / pv.len() as f64;
        let v_start = prep_verts.len();
        prep_verts.extend_from_slice(pv);
        let v_end = prep_verts.len();
        prepared.push((pi, v_start, v_end, avg_d));
    }

    // NDS_POLYSTATS=1: one line per frame summarising the polygon list by
    // mode, texture format and alpha.
    if *POLYSTATS {
        let mut modes = [0usize; 4];
        let mut fmts = [0usize; 8];
        let mut translucent = 0usize;
        for &(pi, _, _, _) in prepared.iter() {
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
            prepared.len(),
            modes,
            fmts,
            translucent
        );
    }

    // NDS_TEXDBG=<hex TEXIMAGE_PARAM>: dump that texture's palette and its
    // index grid once.
    if let Some(want) = *TEXDBG {
        for &(pi, vs, ve, _) in prepared.iter() {
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
            let vc: Vec<String> = prep_verts[vs..ve]
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

    // Pass 0: opaque pixels in submission order. Pass 1: translucent pixels,
    // farthest polygons first.
    let mut order: Vec<usize> = (0..prepared.len()).collect();
    for pass in 0..2 {
        if pass == 1 {
            order.sort_by(|&a, &b| {
                prepared[b]
                    .3
                    .partial_cmp(&prepared[a].3)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        for &pidx in &order {
            let (pi, vs, ve, _) = prepared[pidx];
            let poly = &gx.disp_polys[pi];
            let poly_alpha = {
                let a = poly.attr >> 16 & 0x1F;
                if a == 0 {
                    31
                } else {
                    a
                } // wireframe: drawn opaque
            };
            let format = poly.texparam >> 26 & 7;
            let poly_mode = poly.attr >> 4 & 3;
            let shadow = poly_mode == 3;
            let can_translucent =
                poly_alpha < 31 || (poly_mode != 1 && (format == 1 || format == 6));
            // Shadow volumes run entirely in the second pass so a mask and
            // the shadow it enables stay adjacent.
            if pass == 0 && shadow {
                continue;
            }
            if pass == 1 && !can_translucent && !shadow {
                continue;
            }
            // A new mask (id 0) begins a new volume; the previous volume's
            // stencil must not leak into it.
            if shadow && poly.attr >> 24 & 0x3F == 0 {
                stencil.fill(false);
            }
            raster_poly(
                m,
                poly,
                &prep_verts[vs..ve],
                poly_alpha,
                pass,
                wbuffer,
                highlight_mode,
                tex_bias,
                &mut fb,
                &mut depth,
                &mut poly_id,
                &mut stencil,
            );
        }
    }
    // Close single-pixel pinholes between adjacent fixed-point tile edges.
    for y in 1..HEIGHT - 1 {
        for x in 1..WIDTH - 1 {
            let i = y * WIDTH + x;
            if fb[i] != TRANSPARENT {
                continue;
            }
            let pair = if fb[i - 1] != TRANSPARENT && fb[i + 1] != TRANSPARENT {
                [i - 1, i + 1]
            } else if fb[i - WIDTH] != TRANSPARENT && fb[i + WIDTH] != TRANSPARENT {
                [i - WIDTH, i + WIDTH]
            } else {
                continue;
            };
            let avg = |sh: u32| pair.iter().map(|&j| fb[j] >> sh & 0xFF).sum::<u32>() / 2;
            fb[i] = avg(16) << 16 | avg(8) << 8 | avg(0);
        }
    }

    if let Ok(p) = std::env::var("NDS_DUMP3D") {
        let mut out = format!("P6\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
        for px in fb.iter() {
            let (r, g, b) = if *px == TRANSPARENT {
                (255, 0, 255)
            } else {
                ((px >> 16) as u8, (px >> 8) as u8, *px as u8)
            };
            out.extend_from_slice(&[r, g, b]);
        }
        let _ = std::fs::write(p, out);
    }
    fb
}

#[allow(clippy::too_many_arguments)]
fn raster_poly(
    m: &Machine,
    poly: &super::gpu3d::Polygon,
    pv: &[V],
    poly_alpha: u32,
    pass: usize,
    wbuffer: bool,
    highlight_mode: bool,
    tex_bias: f64,
    fb: &mut [u32],
    depth: &mut [f64],
    poly_id: &mut [u8],
    stencil: &mut [bool],
) {
    let depth_equal = poly.attr & 1 << 14 != 0;
    let depth_write_translucent = poly.attr & 1 << 11 != 0;
    let pid = (poly.attr >> 24 & 0x3F) as u8;
    let poly_mode = (poly.attr >> 4) & 3;
    let shadow_mask = poly_mode == 3 && pid == 0;
    let shadow_draw = poly_mode == 3 && pid != 0;
    let eps = if wbuffer { 0x200 as f64 } else { 1.0 / 4096.0 };
    let tex = TexInfo::from_poly(poly.texparam, poly.pltt, tex_bias);
    let textured = tex.format != 0;
    let pixdbg = *PIXDBG;

    // Triangle fan over the clipped polygon using incremental 28.4 fixed-point
    // edge functions and the top-left fill rule.
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

        let inv = 1.0 / area as f64;
        let (d0, d1, d2) = (tri[0].d, tri[1].d, tri[2].d);
        let (rw0, rw1, rw2) = (tri[0].rw, tri[1].rw, tri[2].rw);
        let (r0, r1, r2) = (tri[0].r * rw0, tri[1].r * rw1, tri[2].r * rw2);
        let (g0, g1, g2) = (tri[0].g * rw0, tri[1].g * rw1, tri[2].g * rw2);
        let (b0c, b1c, b2c) = (tri[0].b * rw0, tri[1].b * rw1, tri[2].b * rw2);
        let (s0, s1, s2) = (tri[0].s * rw0, tri[1].s * rw1, tri[2].s * rw2);
        let (t0, t1, t2) = (tri[0].t * rw0, tri[1].t * rw1, tri[2].t * rw2);

        let dx0 = (p1.1 - p2.1) * 16;
        let dy0 = (p2.0 - p1.0) * 16;
        let dx1 = (p2.1 - p0.1) * 16;
        let dy1 = (p0.0 - p2.0) * 16;
        let dx2 = (p0.1 - p1.1) * 16;
        let dy2 = (p1.0 - p0.0) * 16;
        let start_c = (px0 as i64 * 16 + 8, py0 as i64 * 16 + 8);
        let mut w0_row = orient(p1, p2, start_c);
        let mut w1_row = orient(p2, p0, start_c);
        let mut w2_row = orient(p0, p1, start_c);

        for py in py0..py1 {
            let mut w0 = w0_row;
            let mut w1 = w1_row;
            let mut w2 = w2_row;
            let row_base = py * WIDTH;
            for px in px0..px1 {
                let cw0 = w0;
                let cw1 = w1;
                let cw2 = w2;
                w0 += dx0;
                w1 += dx1;
                w2 += dx2;
                if cw0 + b0 < 0 || cw1 + b1 < 0 || cw2 + b2 < 0 {
                    continue;
                }
                let l0 = cw0 as f64 * inv;
                let l1 = cw1 as f64 * inv;
                let l2 = cw2 as f64 * inv;
                let i = row_base + px;
                let dbg = pixdbg == Some((px, py));
                let d = l0 * d0 + l1 * d1 + l2 * d2;
                let dpass = if depth_equal {
                    d <= depth[i] + eps
                } else {
                    d < depth[i]
                };
                if dbg {
                    eprintln!(
                        "[pix] pass={pass} attr={:#010X} tex={:#010X} pltt={:#X} d={d:.5} zbuf={:.5} dpass={dpass}",
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
                let rw = l0 * rw0 + l1 * rw1 + l2 * rw2;
                let inv_rw = 1.0 / rw;
                let vr = (l0 * r0 + l1 * r1 + l2 * r2) * inv_rw;
                let vg = (l0 * g0 + l1 * g1 + l2 * g2) * inv_rw;
                let vb = (l0 * b0c + l1 * b1c + l2 * b2c) * inv_rw;

                let (rgb, alpha) = match poly_mode {
                    1 => {
                        // Decal: blend texture over vertex color by texel alpha; final alpha = poly_alpha.
                        let vr_c = vr.clamp(0.0, 31.0);
                        let vg_c = vg.clamp(0.0, 31.0);
                        let vb_c = vb.clamp(0.0, 31.0);
                        let v_rgb = (vr_c as u16) | ((vg_c as u16) << 5) | ((vb_c as u16) << 10);
                        if textured {
                            let s = (l0 * s0 + l1 * s1 + l2 * s2) * inv_rw;
                            let t = (l0 * t0 + l1 * t1 + l2 * t2) * inv_rw;
                            match sample_tex(m, &tex, s, t) {
                                None => (v_rgb, poly_alpha),
                                Some((tc, ta)) if ta >= 31 => (tc, poly_alpha),
                                Some((tc, ta)) => {
                                    let tr = (tc & 0x1F) as u32;
                                    let tg = ((tc >> 5) & 0x1F) as u32;
                                    let tb = ((tc >> 10) & 0x1F) as u32;
                                    let dec = |tc_ch: u32, vc_ch: f64| -> u16 {
                                        ((tc_ch * (ta + 1) + (vc_ch as u32) * (31 - ta)) >> 5)
                                            .min(31) as u16
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
                        // Toon / Highlight shading driven by vertex R index into TOON_TABLE.
                        let toon_idx = (vr.clamp(0.0, 31.0).round() as usize).min(31);
                        let toon_c = m.toon_table[toon_idx];
                        let tr_n = (toon_c & 0x1F) as u32;
                        let tg_n = ((toon_c >> 5) & 0x1F) as u32;
                        let tb_n = ((toon_c >> 10) & 0x1F) as u32;
                        if !highlight_mode {
                            if textured {
                                let s = (l0 * s0 + l1 * s1 + l2 * s2) * inv_rw;
                                let t = (l0 * t0 + l1 * t1 + l2 * t2) * inv_rw;
                                let Some((tc, ta)) = sample_tex(m, &tex, s, t) else {
                                    continue;
                                };
                                let rgb = (blend_modulate((tc & 0x1F) as u32, tr_n as f64) as u16)
                                    | ((blend_modulate(((tc >> 5) & 0x1F) as u32, tg_n as f64)
                                        as u16)
                                        << 5)
                                    | ((blend_modulate(((tc >> 10) & 0x1F) as u32, tb_n as f64)
                                        as u16)
                                        << 10);
                                (rgb, poly_alpha * ta / 31)
                            } else {
                                (toon_c & 0x7FFF, poly_alpha)
                            }
                        } else {
                            let (base_r, base_g, base_b, ta) = if textured {
                                let s = (l0 * s0 + l1 * s1 + l2 * s2) * inv_rw;
                                let t = (l0 * t0 + l1 * t1 + l2 * t2) * inv_rw;
                                let Some((tc, ta)) = sample_tex(m, &tex, s, t) else {
                                    continue;
                                };
                                (
                                    blend_modulate((tc & 0x1F) as u32, vr),
                                    blend_modulate(((tc >> 5) & 0x1F) as u32, vr),
                                    blend_modulate(((tc >> 10) & 0x1F) as u32, vr),
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
                        // Mode 0 (Modulation) and Mode 3 (Shadow volume fill).
                        if textured {
                            let s = (l0 * s0 + l1 * s1 + l2 * s2) * inv_rw;
                            let t = (l0 * t0 + l1 * t1 + l2 * t2) * inv_rw;
                            match sample_tex(m, &tex, s, t) {
                                None => {
                                    if dbg {
                                        eprintln!("[pix]   texel transparent at s={s:.2} t={t:.2}");
                                    }
                                    continue;
                                }
                                Some((tc, ta)) => {
                                    if dbg {
                                        eprintln!(
                                            "[pix]   texel s={s:.3} t={t:.3} -> rgb15={tc:#06X} a={ta}"
                                        );
                                    }
                                    let rgb = (blend_modulate((tc & 0x1F) as u32, vr) as u16)
                                        | ((blend_modulate(((tc >> 5) & 0x1F) as u32, vg) as u16)
                                            << 5)
                                        | ((blend_modulate(((tc >> 10) & 0x1F) as u32, vb) as u16)
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
                        continue; // translucent pixel: second pass
                    }
                    depth[i] = d;
                    fb[i] = rgb555_to_rgb(rgb);
                    poly_id[i] = pid;
                } else {
                    if alpha >= 31 && !shadow_draw {
                        continue; // already drawn in pass 0
                    }
                    let src = rgb555_to_rgb(rgb);
                    let dst = fb[i];
                    fb[i] = if dst == TRANSPARENT {
                        src
                    } else {
                        let bl = |sh: u32| {
                            let s = src >> sh & 0xFF;
                            let dv = dst >> sh & 0xFF;
                            (s * alpha + dv * (31 - alpha)) / 31
                        };
                        bl(16) << 16 | bl(8) << 8 | bl(0)
                    };
                    if depth_write_translucent {
                        depth[i] = d;
                    }
                    if shadow_draw {
                        stencil[i] = false;
                    } else {
                        poly_id[i] = pid;
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
        // Map Bank A -> tex slot 0, Bank B -> tex slot 1, Bank C -> tex slot 2, Bank E -> texpal.
        m.vramcnt[0] = 0x83 | (0 << 3);
        m.vramcnt[1] = 0x83 | (1 << 3);
        m.vramcnt[2] = 0x83 | (2 << 3);
        m.vramcnt[4] = 0x83;

        // Palette colors at slot 0: c0 = red (31,0,0), c1 = blue (0,0,31), c2 = green (0,31,0), c3 = white
        let c0: u16 = 31;
        let c1: u16 = 31 << 10;
        let c2: u16 = 31 << 5;
        let c3: u16 = 0x7FFF;
        m.vram[4][0..2].copy_from_slice(&c0.to_le_bytes());
        m.vram[4][2..4].copy_from_slice(&c1.to_le_bytes());
        m.vram[4][4..6].copy_from_slice(&c2.to_le_bytes());
        m.vram[4][6..8].copy_from_slice(&c3.to_le_bytes());

        // Slot 0 test: 8x8 format 5 texture at base 0 (4 blocks total), mode 2 (4 explicit colors).
        // Row 0 of block 0: texel 0=0 (red), 1=1 (blue), 2=2 (green), 3=3 (white) -> 0b11_10_01_00 = 0xE4
        m.vram[0][0] = 0xE4;
        // Slot 1 lower half holds index pairs for Slot 0 (mode 2 => bits 14..15 = 2 => 0x8000)
        m.vram[1][0..2].copy_from_slice(&0x8000u16.to_le_bytes());

        let tp_slot0 = 5 << 26; // format 5, 8x8, offset 0 (Slot 0)
        let tex0 = TexInfo::from_poly(tp_slot0, 0, 1.0 / 1024.0);
        assert_eq!(sample_tex(&m, &tex0, 0.0, 0.0), Some((c0, 31)));
        assert_eq!(sample_tex(&m, &tex0, 1.0, 0.0), Some((c1, 31)));
        assert_eq!(sample_tex(&m, &tex0, 2.0, 0.0), Some((c2, 31)));
        assert_eq!(sample_tex(&m, &tex0, 3.0, 0.0), Some((c3, 31)));

        // Slot 2 test: base = 0x4_0000 (tp offset = 0x8000), mode 1 (50/50 avg at v=2, transparent at v=3).
        // Row 0 of block 0 in Bank C (Slot 2): texel 0=0, 1=1, 2=2 (50% red/blue), 3=3 (transparent) -> 0xE4
        m.vram[2][0] = 0xE4;
        // Slot 1 upper half (offset 0x1_0000 inside Bank B) holds index pairs for Slot 2:
        // Mode 1 => 0x4000
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
        m.vramcnt[0] = 0x83; // tex slot 0
        m.vramcnt[4] = 0x83; // texpal

        // 8x8 Format 4 (256-col pal) with color0 transparent (bit 29 set): left half transparent (0), right solid (1).
        for y in 0..8 {
            for x in 0..8 {
                m.vram[0][y * 8 + x] = if x < 2 { 0 } else { 1 };
            }
        }
        // Palette entry 1 = bright green (0, 31, 0)
        m.vram[4][2..4].copy_from_slice(&(31u16 << 5).to_le_bytes());

        // 1. Decal mode (mode 1) with opaque vertex blue (0, 0, 31) and transparent left half -> must show blue vertex color, not hole!
        let decal_attr = (1 << 4) | (3 << 6) | (31 << 16);
        let decal_tp = (4 << 26) | (1 << 29);
        setup_fullscreen_tri(&mut m, decal_attr, decal_tp, 0, 31 << 10);
        let fb_decal = render(&m);
        let left_px = fb_decal[96 * WIDTH + 20];
        let right_px = fb_decal[96 * WIDTH + 220];
        assert_eq!(left_px & 0xFF, 0xFF); // pure blue from vertex color
        assert_eq!((right_px >> 8) & 0xFF, 0xFF); // bright green from decal texture

        // 2. Toon mode (mode 2, DISP3DCNT bit 1 = 0): vertex R = 10 indexes toon_table[10] = red (31,0,0).
        m.io2d[0][0x60] = 0;
        m.toon_table[10] = 31; // R=31
        let toon_attr = (2 << 4) | (3 << 6) | (31 << 16);
        setup_fullscreen_tri(&mut m, toon_attr, 0, 0, 10);
        let fb_toon = render(&m);
        let center = fb_toon[96 * WIDTH + 128];
        assert_eq!(center >> 16 & 0xFF, 0xFF);
        assert_eq!(center & 0xFFFF, 0);

        // 3. Highlight mode (mode 2, DISP3DCNT bit 1 = 1): adds toon_table[10] (blue 31) onto greyscale intensity vr=10.
        m.io2d[0][0x60] = 2;
        m.toon_table[10] = 31 << 10; // bright blue specular highlight
        let fb_hi = render(&m);
        let hi_center = fb_hi[96 * WIDTH + 128];
        assert!(hi_center >> 16 & 0xFF > 60); // greyscale base
        assert_eq!(hi_center & 0xFF, 0xFF); // + blue highlight
    }
}
