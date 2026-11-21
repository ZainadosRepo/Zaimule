use super::bus::{Machine, BANK_SIZE};

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 192;

#[inline(always)]
fn rgb555(c: u16) -> u32 {
    let r = (c & 0x1F) as u32;
    let g = (c >> 5 & 0x1F) as u32;
    let b = (c >> 10 & 0x1F) as u32;
    (r << 19 | r >> 2 << 16) | (g << 11 | g >> 2 << 8) | (b << 3 | b >> 2)
}

/// Fast 16 KiB VRAM page lookup helper built once per engine per frame using direct slice pointers.
#[inline(always)]
fn vram_page_read8(pages: &[Option<&[u8]>], off: u32) -> u8 {
    let page = (off >> 14) as usize;
    if let Some(Some(slice)) = pages.get(page) {
        slice[(off & 0x3FFF) as usize]
    } else {
        0
    }
}

#[inline(always)]
fn vram_page_read16(pages: &[Option<&[u8]>], off: u32) -> u16 {
    let page = (off >> 14) as usize;
    if let Some(Some(slice)) = pages.get(page) {
        let o = (off & 0x3FFE) as usize;
        u16::from_le_bytes([slice[o], slice[o + 1]])
    } else {
        0
    }
}

/// Frame renderer for both 2D engines: text/affine/extended/bitmap backgrounds,
/// 1D/2D sprites, windows, color effects, 3D BG0, Display Capture, and LCDC mode.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Ppu {
    pub fb_a: Vec<u32>,
    pub fb_b: Vec<u32>,
    #[serde(skip)]
    obj_col: Vec<u16>,
    #[serde(skip)]
    obj_prio: Vec<u8>,
    #[serde(skip)]
    obj_win: Vec<bool>,
    #[serde(skip)]
    obj_semi: Vec<bool>,
    #[serde(skip)]
    obj_bmp_alpha: Vec<u8>,
    #[serde(skip)]
    render3d: super::render3d::Render3dScratch,
}

impl Ppu {
    pub fn new() -> Self {
        Self {
            fb_a: vec![0; WIDTH * HEIGHT],
            fb_b: vec![0; WIDTH * HEIGHT],
            obj_col: vec![0; WIDTH * HEIGHT],
            obj_prio: vec![0xFF; WIDTH * HEIGHT],
            obj_win: vec![false; WIDTH * HEIGHT],
            obj_semi: vec![false; WIDTH * HEIGHT],
            obj_bmp_alpha: vec![0; WIDTH * HEIGHT],
            render3d: super::render3d::Render3dScratch::new(),
        }
    }

    fn ensure_scratch(&mut self) {
        let len = WIDTH * HEIGHT;
        if self.obj_col.len() != len {
            self.obj_col.resize(len, 0);
            self.obj_prio.resize(len, 0xFF);
            self.obj_win.resize(len, false);
            self.obj_semi.resize(len, false);
            self.obj_bmp_alpha.resize(len, 0);
        }
    }

    pub fn render_frame(&mut self, m: &mut Machine) {
        self.ensure_scratch();
        for eng in 0..2 {
            let io = &m.io2d[eng];
            let dispcnt = u32::from_le_bytes([io[0], io[1], io[2], io[3]]);
            let fb = if eng == 0 {
                &mut self.fb_a
            } else {
                &mut self.fb_b
            };
            match dispcnt >> 16 & 3 {
                0 => fb.fill(0xFFFFFF), // display off: white
                2 => {
                    // LCDC: show the selected 128KB VRAM bank directly.
                    let bank = (dispcnt >> 18 & 3) as usize;
                    let src = &m.vram[bank];
                    let limit = BANK_SIZE[bank].min(src.len());
                    for (i, dst) in fb.iter_mut().enumerate() {
                        let off = i * 2;
                        let c = if off + 1 < limit {
                            u16::from_le_bytes([src[off], src[off + 1]])
                        } else {
                            0
                        };
                        *dst = rgb555(c);
                    }
                }
                _ => Self::render_graphics(
                    m,
                    eng,
                    dispcnt,
                    fb,
                    &mut self.obj_col,
                    &mut self.obj_prio,
                    &mut self.obj_win,
                    &mut self.obj_semi,
                    &mut self.obj_bmp_alpha,
                    &mut self.render3d,
                ),
            }
        }
        self.display_capture(m);
        // Master brightness (0x6C, per engine) applies after capture: the
        // capture unit taps the pre-brightness picture.
        for eng in 0..2 {
            let brt = u16::from_le_bytes([m.io2d[eng][0x6C], m.io2d[eng][0x6D]]);
            let factor = (brt & 0x1F).min(16) as u32;
            let mode = brt >> 14 & 3;
            if factor == 0 || mode == 0 || mode == 3 {
                continue;
            }
            let fb = if eng == 0 {
                &mut self.fb_a
            } else {
                &mut self.fb_b
            };
            for px in fb.iter_mut() {
                let (r, g, b) = (*px >> 16 & 0xFF, *px >> 8 & 0xFF, *px & 0xFF);
                let f = |c: u32| -> u32 {
                    if mode == 1 {
                        c + (255 - c) * factor / 16 // brighten toward white
                    } else {
                        c - c * factor / 16 // darken toward black
                    }
                };
                *px = f(r) << 16 | f(g) << 8 | f(b);
            }
        }
    }

    /// DISPCAPCNT (0x04000064): capture engine A's output (and/or a VRAM
    /// source) into a VRAM bank once per frame while the enable bit is set.
    /// Games re-display the captured bank (LCDC mode or as a BG) for TV
    /// static, motion blur, and screen-transition effects; without this the
    /// displayed bank holds stale garbage.
    fn display_capture(&mut self, m: &mut Machine) {
        let cap = u32::from_le_bytes([
            m.io2d[0][0x64],
            m.io2d[0][0x65],
            m.io2d[0][0x66],
            m.io2d[0][0x67],
        ]);
        if cap >> 31 == 0 {
            return;
        }
        let (w, h) = match cap >> 20 & 3 {
            0 => (128usize, 128usize),
            1 => (256, 64),
            2 => (256, 128),
            _ => (256, 192),
        };
        let wbank = (cap >> 16 & 3) as usize;
        let wofs = (cap >> 18 & 3) as usize * 0x8000;
        let rbank = {
            let dispcnt =
                u32::from_le_bytes([m.io2d[0][0], m.io2d[0][1], m.io2d[0][2], m.io2d[0][3]]);
            (dispcnt >> 18 & 3) as usize
        };
        let rofs = (cap >> 26 & 3) as usize * 0x8000;
        let eva = (cap & 0x1F).min(16);
        let evb = (cap >> 8 & 0x1F).min(16);
        let mode = cap >> 29 & 3; // 0 = source A, 1 = source B, 2/3 = blend
                                  // Source A: engine A's composited output (u32 -> RGB15; the u32 was
                                  // expanded from 5-bit channels, so >>3 recovers them). Bit 24 selects
                                  // "3D only", which we approximate with the same composited frame.
        let src_a = |fb: &[u32], i: usize| -> u16 {
            let px = fb[i];
            let (r, g, b) = (px >> 19 & 0x1F, px >> 11 & 0x1F, px >> 3 & 0x1F);
            (r | g << 5 | b << 10) as u16
        };
        let src_b = |m: &Machine, i: usize| -> u16 {
            let off = rofs + i * 2;
            if off + 1 < BANK_SIZE[rbank] {
                u16::from_le_bytes([m.vram[rbank][off], m.vram[rbank][off + 1]])
            } else {
                0
            }
        };
        let mut out = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let si = y * WIDTH + x; // source A is always a 256-wide frame
                let a = src_a(&self.fb_a, si);
                let b = src_b(m, i);
                let c = match mode {
                    0 => a,
                    1 => b,
                    _ => {
                        let ch = |sa: u16, sb: u16| -> u16 {
                            ((sa as u32 * eva + sb as u32 * evb) / 16).min(31) as u16
                        };
                        ch(a & 0x1F, b & 0x1F)
                            | ch(a >> 5 & 0x1F, b >> 5 & 0x1F) << 5
                            | ch(a >> 10 & 0x1F, b >> 10 & 0x1F) << 10
                    }
                };
                out[i] = c | 0x8000; // captured pixels have the alpha bit set
            }
        }
        for (i, c) in out.iter().enumerate() {
            let off = (wofs + i * 2) & (BANK_SIZE[wbank] - 1);
            m.vram[wbank][off] = *c as u8;
            m.vram[wbank][off + 1] = (*c >> 8) as u8;
        }
        // Capture-enable reads back 0 once the capture completes.
        m.io2d[0][0x67] &= 0x7F;
    }

    #[allow(clippy::too_many_arguments)]
    fn render_graphics(
        m: &Machine,
        eng: usize,
        dispcnt: u32,
        fb: &mut [u32],
        obj_col: &mut [u16],
        obj_prio: &mut [u8],
        obj_win: &mut [bool],
        obj_semi: &mut [bool],
        obj_bmp_alpha: &mut [u8],
        render3d: &mut super::render3d::Render3dScratch,
    ) {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum BgKind {
            None,
            Text,
            Affine,
            Extended,
            LargeBitmap,
        }

        let io = &m.io2d[eng];
        let r16 = |off: usize| u16::from_le_bytes([io[off], io[off + 1]]) as u32;
        let ri16 = |off: usize| i16::from_le_bytes([io[off], io[off + 1]]) as i64;
        let r28s = |off: usize| {
            let raw = u32::from_le_bytes([io[off], io[off + 1], io[off + 2], io[off + 3]]);
            (((raw as i32) << 4) >> 4) as i64
        };
        let pal_base = eng * 0x400;
        let mut bg_pal_rgb = [0u32; 256];
        for (idx, dst) in bg_pal_rgb.iter_mut().enumerate() {
            let p = pal_base + idx * 2;
            *dst = rgb555(u16::from_le_bytes([m.pal[p], m.pal[p + 1]]));
        }
        let backdrop_rgb = bg_pal_rgb[0];

        // BG VRAM window for this engine (512 KiB = 32 × 16 KiB direct slices).
        let bg_vram_base: u32 = if eng == 0 { 0x0600_0000 } else { 0x0620_0000 };
        let mut bg_pages: [Option<&[u8]>; 32] = [None; 32];
        for (p, slot) in bg_pages.iter_mut().enumerate() {
            if let Some((bank, bank_off)) = m.vram_slot(bg_vram_base + ((p as u32) << 14)) {
                if bank_off + 0x4000 <= m.vram[bank].len() {
                    *slot = Some(&m.vram[bank][bank_off..bank_off + 0x4000]);
                }
            }
        }
        let vram8 = |off: u32| vram_page_read8(&bg_pages, off);
        let vram16 = |off: u32| vram_page_read16(&bg_pages, off);
        let char_bank_off = if eng == 0 {
            (dispcnt >> 24 & 7) * 0x10000
        } else {
            0
        };
        let screen_bank_off = if eng == 0 {
            (dispcnt >> 27 & 7) * 0x10000
        } else {
            0
        };

        // 3D layer: engine A's BG0 shows the rasterized 3D frame when
        // DISPCNT bit 3 is set. Rendered once per frame into reusable scratch.
        let has_3d = eng == 0 && dispcnt & 8 != 0 && dispcnt & 0x100 != 0;
        if has_3d {
            super::render3d::render_into(m, render3d);
        }

        let objs_enabled = dispcnt & (1 << 12) != 0;
        let (any_obj_drawn, any_semi_obj) = if objs_enabled {
            obj_prio.fill(0xFF);
            obj_win.fill(false);
            Self::render_objs(
                m,
                eng,
                dispcnt,
                obj_col,
                obj_prio,
                obj_win,
                obj_semi,
                obj_bmp_alpha,
            )
        } else {
            (false, false)
        };

        // Color special effects (BLDCNT/BLDALPHA/BLDY). Layer ids: BG0-3 =
        // 0-3, OBJ = 4, backdrop = 5.
        let bldcnt = r16(0x50);
        let bld_mode = bldcnt >> 6 & 3;
        let eva = (r16(0x52) & 0x1F).min(16);
        let evb = (r16(0x52) >> 8 & 0x1F).min(16);
        let evy = (r16(0x54) & 0x1F).min(16);
        let need_color_fx = bld_mode != 0 || any_semi_obj;

        // Windows clip layers to a rectangle.
        let bg_extpal_on = dispcnt & 1 << 30 != 0;
        let win0_on = dispcnt & 1 << 13 != 0;
        let win1_on = dispcnt & 1 << 14 != 0;
        let objwin_on = objs_enabled && (dispcnt & (1 << 15) != 0);
        let windows_on = dispcnt & 0xE000 != 0;
        let winin = r16(0x48);
        let winout = r16(0x4A);
        let span = |reg: u32, max: u32| {
            let (a, b) = (reg >> 8 & 0xFF, reg & 0xFF);
            (a, if b < a || b > max { max } else { b })
        };
        let (w0x1, w0x2) = span(r16(0x40), WIDTH as u32);
        let (w0y1, w0y2) = span(r16(0x44), HEIGHT as u32);
        let (w1x1, w1x2) = span(r16(0x42), WIDTH as u32);
        let (w1y1, w1y2) = span(r16(0x46), HEIGHT as u32);

        // Painter's order: higher priority value first, and for ties the
        // higher BG number first, so the lower one ends up on top.
        let mut order = [0u32, 1, 2, 3];
        order.sort_by_key(|&bg| std::cmp::Reverse((r16(0x8 + bg as usize * 2) & 3, bg)));
        let mode = dispcnt & 7;

        for y in 0..HEIGHT as u32 {
            let w0_row = win0_on && y >= w0y1 && y < w0y2;
            let w1_row = win1_on && y >= w1y1 && y < w1y2;
            let row_off = y as usize * WIDTH;
            let layers_at = |x: u32| -> u32 {
                if w0_row && x >= w0x1 && x < w0x2 {
                    winin
                } else if w1_row && x >= w1x1 && x < w1x2 {
                    winin >> 8
                } else if objwin_on && obj_win[row_off + x as usize] {
                    winout >> 8
                } else {
                    winout
                }
            };
            let mut top_prio = [4u8; WIDTH];
            let mut top_col = [backdrop_rgb; WIDTH];
            let mut top_layer = [5u8; WIDTH];
            let mut sec_col = [backdrop_rgb; WIDTH];
            let mut sec_layer = [5u8; WIDTH];

            for bg in order {
                if dispcnt & (1 << (8 + bg)) == 0 {
                    continue;
                }
                // BG0 with 3D enabled: composite the rasterized 3D frame at BG0's priority.
                if eng == 0 && bg == 0 && dispcnt & 8 != 0 {
                    if has_3d {
                        let buf = &render3d.fb[row_off..row_off + WIDTH];
                        let prio = (r16(0x8) & 3) as u8;
                        if !windows_on {
                            for x in 0..WIDTH {
                                let px = buf[x];
                                if px != super::render3d::TRANSPARENT {
                                    sec_col[x] = top_col[x];
                                    sec_layer[x] = top_layer[x];
                                    top_col[x] = px;
                                    top_layer[x] = 0;
                                    top_prio[x] = prio;
                                }
                            }
                        } else {
                            for x in 0..WIDTH {
                                if layers_at(x as u32) & 1 == 0 {
                                    continue;
                                }
                                let px = buf[x];
                                if px != super::render3d::TRANSPARENT {
                                    sec_col[x] = top_col[x];
                                    sec_layer[x] = top_layer[x];
                                    top_col[x] = px;
                                    top_layer[x] = 0;
                                    top_prio[x] = prio;
                                }
                            }
                        }
                    }
                    continue;
                }

                let kind = match (mode, bg) {
                    (0, 0..=3) => BgKind::Text,
                    (1, 0..=2) => BgKind::Text,
                    (1, 3) => BgKind::Affine,
                    (2, 0..=1) => BgKind::Text,
                    (2, 2..=3) => BgKind::Affine,
                    (3, 0..=2) => BgKind::Text,
                    (3, 3) => BgKind::Extended,
                    (4, 0..=1) => BgKind::Text,
                    (4, 2) => BgKind::Affine,
                    (4, 3) => BgKind::Extended,
                    (5, 0..=1) => BgKind::Text,
                    (5, 2..=3) => BgKind::Extended,
                    (6, 2) if eng == 0 => BgKind::LargeBitmap,
                    _ => BgKind::None,
                };
                if kind == BgKind::None {
                    continue;
                }

                let bgcnt = r16(0x8 + bg as usize * 2);
                let prio = (bgcnt & 3) as u8;

                if kind == BgKind::Text {
                    let char_base = (char_bank_off + (bgcnt >> 2 & 0xF) * 0x4000) as u32;
                    let screen_base = (screen_bank_off + (bgcnt >> 8 & 0x1F) * 0x800) as u32;
                    let eight_bpp = bgcnt & 0x80 != 0;
                    let size = bgcnt >> 14 & 3;
                    let hofs = r16(0x10 + bg as usize * 4) & 0x1FF;
                    let vofs = r16(0x12 + bg as usize * 4) & 0x1FF;
                    let w_mask = if size & 1 != 0 { 511u32 } else { 255u32 };
                    let h_mask = if size & 2 != 0 { 511u32 } else { 255u32 };
                    let py = (y + vofs) & h_mask;
                    let ty = (py >> 3) & 31;
                    let fy_raw = py & 7;
                    let sbb_y = match size {
                        2 => (py >> 8) * 0x800,
                        3 => (py >> 8) * 0x1000,
                        _ => 0,
                    };
                    let row_screen_base = screen_base + sbb_y + ty * 64;
                    let extpal_slot = if bg < 2 && bgcnt & 0x2000 != 0 {
                        bg as usize + 2
                    } else {
                        bg as usize
                    };
                    let bg_bit = 1u32 << bg;
                    let bg_u8 = bg as u8;

                    // Tile-at-a-time rendering: read each 16-bit screen entry once per 8-pixel tile.
                    let mut x = 0u32;
                    while x < WIDTH as u32 {
                        let px = (x + hofs) & w_mask;
                        let fx_start = px & 7;
                        let run = (8 - fx_start).min(WIDTH as u32 - x);
                        let sbb_x = if size & 1 != 0 { (px >> 8) * 0x800 } else { 0 };
                        let tx = (px >> 3) & 31;
                        let entry = vram16(row_screen_base + sbb_x + tx * 2) as u32;
                        let tile = entry & 0x3FF;
                        let hflip = entry & 0x400 != 0;
                        let fy = if entry & 0x800 != 0 {
                            7 - fy_raw
                        } else {
                            fy_raw
                        };
                        let pal_hi = entry >> 12;

                        if eight_bpp {
                            let tile_row = char_base + tile * 64 + fy * 8;
                            for step in 0..run {
                                let cx = x + step;
                                if windows_on && layers_at(cx) & bg_bit == 0 {
                                    continue;
                                }
                                let fx = if hflip {
                                    7 - (fx_start + step)
                                } else {
                                    fx_start + step
                                };
                                let color = vram8(tile_row + fx) as u32;
                                if color != 0 {
                                    let rgb = if bg_extpal_on {
                                        rgb555(m.bg_extpal(eng, extpal_slot, pal_hi, color))
                                    } else {
                                        bg_pal_rgb[color as usize]
                                    };
                                    let xi = cx as usize;
                                    sec_col[xi] = top_col[xi];
                                    sec_layer[xi] = top_layer[xi];
                                    top_col[xi] = rgb;
                                    top_layer[xi] = bg_u8;
                                    top_prio[xi] = prio;
                                }
                            }
                        } else {
                            let tile_row = char_base + tile * 32 + fy * 4;
                            let pal_off = (pal_hi as usize) * 16;
                            for step in 0..run {
                                let cx = x + step;
                                if windows_on && layers_at(cx) & bg_bit == 0 {
                                    continue;
                                }
                                let fx = if hflip {
                                    7 - (fx_start + step)
                                } else {
                                    fx_start + step
                                };
                                let b = vram8(tile_row + (fx >> 1)) as u32;
                                let color = if fx & 1 == 0 { b & 0xF } else { b >> 4 };
                                if color != 0 {
                                    let xi = cx as usize;
                                    sec_col[xi] = top_col[xi];
                                    sec_layer[xi] = top_layer[xi];
                                    top_col[xi] = bg_pal_rgb[pal_off + color as usize];
                                    top_layer[xi] = bg_u8;
                                    top_prio[xi] = prio;
                                }
                            }
                        }
                        x += run;
                    }
                    continue;
                }

                // Affine, Extended, or Mode 6 Large Bitmap BG (`bg` is 2 or 3).
                let mat_base = if bg == 2 { 0x20usize } else { 0x30usize };
                let pa = ri16(mat_base);
                let pb = ri16(mat_base + 2);
                let pc = ri16(mat_base + 4);
                let pd = ri16(mat_base + 6);
                let ref_x = r28s(mat_base + 8);
                let ref_y = r28s(mat_base + 12);
                let size = bgcnt >> 14 & 3;
                let overflow_wrap = bgcnt & (1 << 13) != 0;

                let mut sx = ref_x + pb * (y as i64);
                let mut sy = ref_y + pd * (y as i64);

                let (map_w, map_h): (i64, i64) = match kind {
                    BgKind::Affine => {
                        let s = 128i64 << size;
                        (s, s)
                    }
                    BgKind::Extended if bgcnt & 0x80 == 0 => {
                        let s = 128i64 << size;
                        (s, s)
                    }
                    BgKind::Extended => match size {
                        0 => (128, 128),
                        1 => (256, 256),
                        2 => (512, 256),
                        _ => (512, 512),
                    },
                    BgKind::LargeBitmap => match size {
                        0 => (512, 1024),
                        1 => (1024, 512),
                        _ => (0, 0),
                    },
                    _ => (0, 0),
                };
                if map_w == 0 || map_h == 0 {
                    continue;
                }
                let mask_w = map_w - 1;
                let mask_h = map_h - 1;

                let char_base = (char_bank_off + (bgcnt >> 2 & 0xF) * 0x4000) as u32;
                let screen_base_tile = (screen_bank_off + (bgcnt >> 8 & 0x1F) * 0x800) as u32;
                let bmp_base = if kind == BgKind::LargeBitmap {
                    0
                } else {
                    ((bgcnt >> 8) & 0x1F) * 0x4000
                };
                let tiles_w = (map_w >> 3) as u32;

                #[derive(Clone, Copy)]
                enum AffMode {
                    Affine8bpp,
                    ExtTile16,
                    Bitmap8bpp,
                    BitmapDirect16,
                }
                let aff_mode = match kind {
                    BgKind::Affine => AffMode::Affine8bpp,
                    BgKind::LargeBitmap => AffMode::Bitmap8bpp,
                    BgKind::Extended if bgcnt & 0x80 == 0 => AffMode::ExtTile16,
                    BgKind::Extended if bgcnt & 0x04 == 0 => AffMode::Bitmap8bpp,
                    BgKind::Extended => AffMode::BitmapDirect16,
                    _ => unreachable!(),
                };

                for x in 0..WIDTH as u32 {
                    let cur_x = sx >> 8;
                    let cur_y = sy >> 8;
                    sx += pa;
                    sy += pc;

                    if windows_on && layers_at(x) & (1 << bg) == 0 {
                        continue;
                    }
                    let (px, py) = if cur_x < 0 || cur_x >= map_w || cur_y < 0 || cur_y >= map_h {
                        if overflow_wrap {
                            ((cur_x & mask_w) as u32, (cur_y & mask_h) as u32)
                        } else {
                            continue;
                        }
                    } else {
                        (cur_x as u32, cur_y as u32)
                    };

                    let rgb_opt: Option<u32> = match aff_mode {
                        AffMode::Affine8bpp => {
                            let tx = px >> 3;
                            let ty = py >> 3;
                            let tile = vram8(screen_base_tile + ty * tiles_w + tx) as u32;
                            let color =
                                vram8(char_base + tile * 64 + (py & 7) * 8 + (px & 7)) as usize;
                            if color == 0 {
                                None
                            } else {
                                Some(bg_pal_rgb[color])
                            }
                        }
                        AffMode::ExtTile16 => {
                            let tx = px >> 3;
                            let ty = py >> 3;
                            let eoff = screen_base_tile + (ty * tiles_w + tx) * 2;
                            let entry = vram16(eoff) as u32;
                            let tile = entry & 0x3FF;
                            let mut fx = px & 7;
                            let mut fy = py & 7;
                            if entry & 0x400 != 0 {
                                fx = 7 - fx;
                            }
                            if entry & 0x800 != 0 {
                                fy = 7 - fy;
                            }
                            let color = vram8(char_base + tile * 64 + fy * 8 + fx) as u32;
                            if color == 0 {
                                None
                            } else if bg_extpal_on {
                                Some(rgb555(m.bg_extpal(eng, bg as usize, entry >> 12, color)))
                            } else {
                                Some(bg_pal_rgb[color as usize])
                            }
                        }
                        AffMode::Bitmap8bpp => {
                            let color = vram8(bmp_base + py * (map_w as u32) + px) as usize;
                            if color == 0 {
                                None
                            } else {
                                Some(bg_pal_rgb[color])
                            }
                        }
                        AffMode::BitmapDirect16 => {
                            let off = bmp_base + (py * (map_w as u32) + px) * 2;
                            let c = vram16(off);
                            if c & 0x8000 == 0 {
                                None
                            } else {
                                Some(rgb555(c & 0x7FFF))
                            }
                        }
                    };

                    if let Some(rgb) = rgb_opt {
                        let xi = x as usize;
                        sec_col[xi] = top_col[xi];
                        sec_layer[xi] = top_layer[xi];
                        top_col[xi] = rgb;
                        top_layer[xi] = bg as u8;
                        top_prio[xi] = prio;
                    }
                }
            }

            // Composite sprites: a sprite pixel shows over a BG pixel of the
            // same or lower priority (higher or equal value).
            let mut semi_top = [false; WIDTH];
            let mut bmp_alpha_top = [0u8; WIDTH];
            let mut row_has_semi = false;
            if any_obj_drawn {
                for x in 0..WIDTH {
                    let i = row_off + x;
                    if obj_prio[i] == 0xFF {
                        continue;
                    }
                    if windows_on && layers_at(x as u32) & 0x10 == 0 {
                        continue;
                    }
                    if obj_prio[i] <= top_prio[x] {
                        sec_col[x] = top_col[x];
                        sec_layer[x] = top_layer[x];
                        top_col[x] = rgb555(obj_col[i]);
                        top_layer[x] = 4;
                        if any_semi_obj {
                            let s = obj_semi[i];
                            let ba = obj_bmp_alpha[i];
                            semi_top[x] = s;
                            bmp_alpha_top[x] = ba;
                            if s || ba > 0 {
                                row_has_semi = true;
                            }
                        }
                    }
                }
            }

            // Fast path: when no blend mode and no semi-transparent sprite pixel touched this row,
            // commit `top_col` directly with a single memcpy.
            if bld_mode == 0 && !row_has_semi {
                fb[row_off..row_off + WIDTH].copy_from_slice(&top_col);
                continue;
            }

            // Color special effects, then commit the row.
            let _ = need_color_fx;
            for x in 0..WIDTH {
                let (top, sec) = (top_col[x], sec_col[x]);
                let first_ok = bldcnt >> top_layer[x] & 1 != 0;
                let second_ok = bldcnt >> (8 + sec_layer[x]) & 1 != 0;
                let alpha = |eva_c: u32, evb_c: u32| {
                    let bl = |sh: u32| {
                        (((top >> sh & 0xFF) * eva_c + (sec >> sh & 0xFF) * evb_c) >> 4).min(255)
                    };
                    bl(16) << 16 | bl(8) << 8 | bl(0)
                };
                let out = if row_has_semi && bmp_alpha_top[x] > 0 && second_ok {
                    let ba = bmp_alpha_top[x] as u32;
                    alpha(ba, 16 - ba)
                } else if row_has_semi && semi_top[x] && second_ok {
                    alpha(eva, evb)
                } else {
                    match bld_mode {
                        1 if first_ok && second_ok => alpha(eva, evb),
                        2 if first_ok => {
                            let bl = |sh: u32| {
                                let c = top >> sh & 0xFF;
                                c + (((255 - c) * evy) >> 4)
                            };
                            bl(16) << 16 | bl(8) << 8 | bl(0)
                        }
                        3 if first_ok => {
                            let bl = |sh: u32| {
                                let c = top >> sh & 0xFF;
                                c - ((c * evy) >> 4)
                            };
                            bl(16) << 16 | bl(8) << 8 | bl(0)
                        }
                        _ => top,
                    }
                };
                fb[row_off + x] = out;
            }
        }
    }

    /// Render all OAM sprites for one engine into per-pixel color/priority
    /// buffers (lower OAM index wins), bitmap per-OBJ alpha, and the OBJ-window mask.
    /// Returns `(any_obj_drawn, any_semi_or_bmp_alpha)`.
    #[allow(clippy::too_many_arguments)]
    fn render_objs(
        m: &Machine,
        eng: usize,
        dispcnt: u32,
        obj_col: &mut [u16],
        obj_prio: &mut [u8],
        obj_win: &mut [bool],
        obj_semi: &mut [bool],
        obj_bmp_alpha: &mut [u8],
    ) -> (bool, bool) {
        let oam_base = eng * 0x400;
        let obj_vram_base: u32 = if eng == 0 { 0x0640_0000 } else { 0x0660_0000 };
        let mut obj_pages: [Option<&[u8]>; 16] = [None; 16];
        for (p, slot) in obj_pages.iter_mut().enumerate() {
            if let Some((bank, bank_off)) = m.vram_slot(obj_vram_base + ((p as u32) << 14)) {
                if bank_off + 0x4000 <= m.vram[bank].len() {
                    *slot = Some(&m.vram[bank][bank_off..bank_off + 0x4000]);
                }
            }
        }
        let obj_vram8 = |addr: u32| vram_page_read8(&obj_pages, addr.wrapping_sub(obj_vram_base));
        let pal_base = eng * 0x400 + 0x200;
        let one_d = dispcnt & 0x10 != 0;
        let bmp_one_d = dispcnt & 0x40 != 0;
        let bmp_2d_w: u32 = if dispcnt & 0x20 != 0 { 256 } else { 128 };
        let obj_extpal_on = dispcnt & 1 << 31 != 0;
        let boundary = 32u32 << (dispcnt >> 20 & 3);
        let bmp_boundary = if eng == 0 {
            128u32 << ((dispcnt >> 22) & 1)
        } else {
            128u32
        };
        let mut any_drawn = false;
        let mut any_semi = false;

        // Painted highest index first so lower OAM indices overwrite (win).
        for idx in (0..128).rev() {
            let at = |o: usize| {
                u16::from_le_bytes([
                    m.oam[oam_base + idx * 8 + o],
                    m.oam[oam_base + idx * 8 + o + 1],
                ])
            };
            let (a0, a1, a2) = (at(0), at(2), at(4));
            let rot = a0 & 0x100 != 0;
            if !rot && a0 & 0x200 != 0 {
                continue; // disabled
            }
            let mode = a0 >> 10 & 3;
            let palnum = (a2 >> 12) as u32;
            if mode == 3 && palnum == 0 {
                continue; // Bitmap OBJ with alpha 0 is invisible on hardware
            }
            let eight_bpp = a0 & 0x2000 != 0;
            let (w, h): (i32, i32) = match (a0 >> 14 & 3, a1 >> 14 & 3) {
                (0, 0) => (8, 8),
                (0, 1) => (16, 16),
                (0, 2) => (32, 32),
                (0, 3) => (64, 64),
                (1, 0) => (16, 8),
                (1, 1) => (32, 8),
                (1, 2) => (32, 16),
                (1, 3) => (64, 32),
                (2, 0) => (8, 16),
                (2, 1) => (8, 32),
                (2, 2) => (16, 32),
                (2, 3) => (32, 64),
                _ => continue, // prohibited shape
            };
            let double = rot && a0 & 0x200 != 0;
            let (bw, bh) = if double { (w * 2, h * 2) } else { (w, h) };
            let mut top = (a0 & 0xFF) as i32;
            if top + bh > 256 {
                top -= 256; // Y wraps within 0..255
            }
            let mut left = (a1 & 0x1FF) as i32;
            if left >= 256 {
                left -= 512; // X is 9-bit signed
            }
            let sy_start = (-top).max(0);
            let sy_end = (HEIGHT as i32 - top).min(bh);
            let sx_start = (-left).max(0);
            let sx_end = (WIDTH as i32 - left).min(bw);
            if sy_start >= sy_end || sx_start >= sx_end {
                continue;
            }
            let (pa, pb, pc, pd) = if rot {
                let grp = oam_base + (a1 >> 9 & 0x1F) as usize * 32;
                let p = |o: usize| i16::from_le_bytes([m.oam[grp + o], m.oam[grp + o + 1]]) as i32;
                (p(6), p(14), p(22), p(30))
            } else {
                (0x100, 0, 0, 0x100)
            };
            let tile = (a2 & 0x3FF) as u32;
            let prio = (a2 >> 10 & 3) as u8;
            let tsz: u32 = if eight_bpp { 64 } else { 32 };
            let w_tiles = (w / 8) as u32;
            if mode == 1 || (mode == 3 && palnum < 15) {
                any_semi = true;
            }
            for sy in sy_start..sy_end {
                let y = (top + sy) as usize;
                let row_y = y * WIDTH;
                for sx in sx_start..sx_end {
                    let x = (left + sx) as usize;
                    // Texture coordinates within the w*h sprite.
                    let (tx, ty) = if rot {
                        let dx = sx - bw / 2;
                        let dy = sy - bh / 2;
                        (
                            ((w / 2 << 8) + pa * dx + pb * dy) >> 8,
                            ((h / 2 << 8) + pc * dx + pd * dy) >> 8,
                        )
                    } else {
                        let mut tx = sx;
                        let mut ty = sy;
                        if a1 & 0x1000 != 0 {
                            tx = w - 1 - tx;
                        }
                        if a1 & 0x2000 != 0 {
                            ty = h - 1 - ty;
                        }
                        (tx, ty)
                    };
                    if tx < 0 || tx >= w || ty < 0 || ty >= h {
                        continue;
                    }
                    let i = row_y + x;
                    if mode == 3 {
                        let addr = if bmp_one_d {
                            obj_vram_base
                                + tile * bmp_boundary
                                + (ty as u32 * w as u32 + tx as u32) * 2
                        } else if bmp_2d_w == 256 {
                            let px = (tile & 0x1F) * 8 + tx as u32;
                            let py = (tile >> 5) * 8 + ty as u32;
                            obj_vram_base + (py * 256 + px) * 2
                        } else {
                            let px = (tile & 0xF) * 8 + tx as u32;
                            let py = (tile >> 4) * 8 + ty as u32;
                            obj_vram_base + (py * 128 + px) * 2
                        };
                        let raw = u16::from_le_bytes([obj_vram8(addr), obj_vram8(addr + 1)]);
                        if raw & 0x8000 == 0 {
                            continue;
                        }
                        obj_col[i] = raw & 0x7FFF;
                        obj_prio[i] = prio;
                        obj_semi[i] = false;
                        obj_bmp_alpha[i] = if palnum < 15 { (palnum + 1) as u8 } else { 0 };
                        any_drawn = true;
                        continue;
                    }
                    let (ctx, cty) = ((tx >> 3) as u32, (ty >> 3) as u32);
                    let (fx, fy) = ((tx & 7) as u32, (ty & 7) as u32);
                    let tile_addr = if one_d {
                        obj_vram_base + tile * boundary + (cty * w_tiles + ctx) * tsz
                    } else if eight_bpp {
                        obj_vram_base + (tile & !1) * 32 + cty * 1024 + ctx * 64
                    } else {
                        obj_vram_base + (tile + cty * 32 + ctx) * 32
                    };
                    let color = if eight_bpp {
                        obj_vram8(tile_addr + fy * 8 + fx) as u32
                    } else {
                        let b = obj_vram8(tile_addr + fy * 4 + (fx >> 1)) as u32;
                        if fx & 1 == 0 {
                            b & 0xF
                        } else {
                            b >> 4
                        }
                    };
                    if color == 0 {
                        continue;
                    }
                    if mode == 2 {
                        obj_win[i] = true;
                        continue;
                    }
                    obj_semi[i] = mode == 1;
                    obj_bmp_alpha[i] = 0;
                    obj_col[i] = if eight_bpp && obj_extpal_on {
                        m.obj_extpal(eng, palnum, color)
                    } else {
                        let poff = if eight_bpp {
                            pal_base + color as usize * 2
                        } else {
                            pal_base + (palnum * 32 + color * 2) as usize
                        };
                        u16::from_le_bytes([m.pal[poff], m.pal[poff + 1]])
                    };
                    obj_prio[i] = prio;
                    any_drawn = true;
                }
            }
        }
        (any_drawn, any_semi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_io16(m: &mut Machine, eng: usize, off: usize, v: u16) {
        m.io2d[eng][off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn set_io32(m: &mut Machine, eng: usize, off: usize, v: u32) {
        m.io2d[eng][off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn renders_affine_and_extended_tilemap_and_bitmap_backgrounds() {
        let mut m = Machine::new();
        // Map Bank A (128 KiB) -> Engine A BG VRAM (0x0600_0000), Bank E -> BG ExtPal.
        m.vramcnt[0] = 0x81; // MST=1, OFS=0 -> 0x0600_0000
        m.vramcnt[4] = 0x84; // MST=4 -> Engine A BG ExtPal slots 0..3

        // Palette colors: idx 1 = red, idx 2 = blue. Backdrop = black.
        m.pal[2..4].copy_from_slice(&31u16.to_le_bytes());
        m.pal[4..6].copy_from_slice(&(31u16 << 10).to_le_bytes());

        // Tile 1 in char_base 0x4000 (char block 1): filled with color idx 1.
        for b in &mut m.vram[0][0x4000 + 64..0x4000 + 128] {
            *b = 1;
        }
        // Affine tilemap at screen_base 0 (16x16 tiles = 128x128): tile (1, 1) -> tile 1.
        m.vram[0][16 + 1] = 1;

        // 1. Mode 2 with BG1 Text + BG2 Affine (char block 1, size 128x128, no wrap).
        // Also configure identity matrix shifted by (-4, -4) or identity with 2x zoom.
        set_io32(&mut m, 0, 0x00, 0x0001_0402); // Display graphics, BG2 on, Mode 2
        set_io16(&mut m, 0, 0x0C, 1 << 2); // BG2CNT: char_base=1, size=0 (128x128), no wrap
        set_io16(&mut m, 0, 0x20, 0x0100); // PA = 1.0
        set_io16(&mut m, 0, 0x26, 0x0100); // PD = 1.0
        let mut ppu = Ppu::new();
        ppu.render_frame(&mut m);
        // Pixel (9, 9) lands in tile (1,1) -> red; Pixel (0,0) -> black; Pixel (200,10) out-of-bounds -> black.
        assert_eq!(ppu.fb_a[9 * WIDTH + 9], rgb555(31));
        assert_eq!(ppu.fb_a[0], 0);
        assert_eq!(ppu.fb_a[10 * WIDTH + 200], 0);

        // Enable Area Overflow wrap (bit 13): (128 + 9, 128 + 9) = (137, 137) wraps to tile (1,1) -> red!
        set_io16(&mut m, 0, 0x0C, (1 << 2) | (1 << 13));
        ppu.render_frame(&mut m);
        assert_eq!(ppu.fb_a[137 * WIDTH + 137], rgb555(31));

        // 2. Mode 5: BG2 Extended 16-bit tilemap (extpal slot 2, palette 1, H-flip) + BG3 Direct Color RGB555 Bitmap.
        set_io32(&mut m, 0, 0x00, 0x4001_0C05); // DISPCNT: BG extpal (bit 30), BG2+BG3 on, Mode 5
                                                // BG2: Extended 16-bit tilemap (bit7=0), char block 1 (0x4000), screen base block 1 (0x0800), prio 1.
        set_io16(&mut m, 0, 0x0C, (1 << 2) | (1 << 8) | 1);
        // Tile 2 at char block 1: only x=0 pixel is idx 1 -> with H-flip (0x400) appears at x=7!
        m.vram[0][0x4000 + 2 * 64] = 1;
        // Map entry (0, 0) at 0x0800: tile 2, H-flip (0x0400), extpal palette 1 (0x1000) -> 0x1402
        m.vram[0][0x0800..0x0802].copy_from_slice(&0x1402u16.to_le_bytes());
        // ExtPal slot 2 (BG2), palette 1, color 1 -> green (0, 31, 0)
        let ext_off = 2 * 0x2000 + 0x200 + 2;
        m.vram[4][ext_off..ext_off + 2].copy_from_slice(&(31u16 << 5).to_le_bytes());

        // BG3: Extended Direct-Color Bitmap (bit7=1, bit2=1), bmp_base block 4 (0x10000), size 128x128, prio 2 (behind BG2).
        set_io16(&mut m, 0, 0x0E, 0x80 | 0x04 | (4 << 8) | 2);
        set_io16(&mut m, 0, 0x30, 0x0100);
        set_io16(&mut m, 0, 0x36, 0x0100);
        // Write opaque blue (0x8000 | (31<<10)) at bitmap pixel (20, 20) in VRAM 0x10000
        let px_off = 0x10000 + (20 * 128 + 20) * 2;
        m.vram[0][px_off..px_off + 2].copy_from_slice(&(0x8000u16 | (31u16 << 10)).to_le_bytes());

        ppu.render_frame(&mut m);
        // BG2 flipped pixel lands at (7, 0) in green from BG ExtPal!
        assert_eq!(ppu.fb_a[7], rgb555(31 << 5));
        assert_eq!(ppu.fb_a[0], 0);
        // BG3 direct-color bitmap pixel at (20, 20) -> pure blue!
        assert_eq!(ppu.fb_a[20 * WIDTH + 20], rgb555(31 << 10));

        // 3. Mode 3: BG3 Extended 256-color Bitmap (bit7=1, bit2=0), bmp_base block 4 (0x10000), size 128x128.
        set_io32(&mut m, 0, 0x00, 0x0001_0803); // DISPCNT: BG3 on, Mode 3
        set_io16(&mut m, 0, 0x0E, 0x80 | (4 << 8)); // 256-color bitmap (bit2=0)
        m.vram[0][0x10000 + 5 * 128 + 15] = 2; // palette index 2 -> blue
        ppu.render_frame(&mut m);
        assert_eq!(ppu.fb_a[5 * WIDTH + 15], rgb555(31 << 10));
    }

    #[test]
    fn renders_bitmap_objs_1d_and_2d_with_alpha() {
        let mut m = Machine::new();
        // Map Bank B -> Engine A OBJ VRAM (0x0640_0000)
        m.vramcnt[1] = 0x82; // MST=2, OFS=0 -> 0x0640_0000
                             // Backdrop = pure green (0, 31, 0)
        m.pal[0..2].copy_from_slice(&(31u16 << 5).to_le_bytes());

        // 1. 1D Bitmap OBJ (DISPCNT bit 6 = 1, OBJ on bit 12), 8x8 at (10, 10), tile=1, boundary=128 -> offset 128.
        set_io32(&mut m, 0, 0x00, 0x0001_1040);
        // OAM[0]: mode=3 (0x0C00), y=10, 8x8 (a0 shape=0, a1 size=0), x=10, tile=1, alpha=15 (0xF000 = opaque).
        m.oam[0..2].copy_from_slice(&(0x0C00u16 | 10).to_le_bytes());
        m.oam[2..4].copy_from_slice(&10u16.to_le_bytes());
        m.oam[4..6].copy_from_slice(&(0xF001u16).to_le_bytes());
        // Write opaque red (0x8000 | 31) at pixel (2, 1) of tile 1 (offset 128 + (1*8 + 2)*2 = 148)
        m.vram[1][148..150].copy_from_slice(&(0x8000u16 | 31).to_le_bytes());

        // OAM[1]: same bitmap tile but semi-transparent alpha=7 (eva=8, evb=8) at (30, 10) blended over backdrop (BLDCNT second target = backdrop bit 13)
        set_io16(&mut m, 0, 0x50, 1 << 13);
        m.oam[8..10].copy_from_slice(&(0x0C00u16 | 10).to_le_bytes());
        m.oam[10..12].copy_from_slice(&30u16.to_le_bytes());
        m.oam[12..14].copy_from_slice(&(0x7001u16).to_le_bytes());

        // OAM[2]: disabled other sprites stay unused (0x0200 on non-rot sprites)
        for idx in 2..128 {
            m.oam[idx * 8..idx * 8 + 2].copy_from_slice(&0x0200u16.to_le_bytes());
        }

        let mut ppu = Ppu::new();
        ppu.render_frame(&mut m);
        // Opaque bitmap OBJ pixel at (12, 11) -> pure red
        assert_eq!(ppu.fb_a[11 * WIDTH + 12], rgb555(31));
        // Transparent bit-15=0 pixel at (10, 10) -> green backdrop
        assert_eq!(ppu.fb_a[10 * WIDTH + 10], rgb555(31 << 5));
        // Semi-transparent alpha=7 (8/16 red + 8/16 green backdrop) at (32, 11)
        let blend_px = ppu.fb_a[11 * WIDTH + 32];
        assert_eq!(blend_px >> 16 & 0xFF, 127);
        assert_eq!(blend_px >> 8 & 0xFF, 127);

        // 2. 2D 256-wide Bitmap OBJ mapping (DISPCNT bit 6 = 0, bit 5 = 1) at tile 32 -> row y=8, x=0 -> offset 8*256*2 = 4096.
        set_io32(&mut m, 0, 0x00, 0x0001_1020);
        m.oam[4..6].copy_from_slice(&(0xF000u16 | 32).to_le_bytes());
        // Disable OAM[1]
        m.oam[8..10].copy_from_slice(&0x0200u16.to_le_bytes());
        let off_2d = (8 * 256 + 1) * 2;
        m.vram[1][off_2d..off_2d + 2].copy_from_slice(&(0x8000u16 | (31 << 10)).to_le_bytes());
        ppu.render_frame(&mut m);
        // Pixel (11, 10) reads (tx=1, ty=0) in tile 32 -> pure blue!
        assert_eq!(ppu.fb_a[10 * WIDTH + 11], rgb555(31 << 10));
    }
}
