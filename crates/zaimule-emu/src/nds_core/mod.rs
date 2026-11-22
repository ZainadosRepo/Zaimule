//! Adapted MIT-licensed pure-Rust Nintendo DS core (`x0cero/nds`), extended
//! with `NdsMachine` headless stepping, RGBA framebuffer compositing, stylus
//! input, backup RAM save handling, memory/CPU inspection, and RLE savestates.

#[allow(clippy::all, dead_code)]
mod bus;
#[allow(clippy::all, dead_code)]
mod cpu;
#[allow(clippy::all, dead_code)]
mod gpu3d;
#[allow(clippy::all, dead_code)]
mod key1;
#[allow(clippy::all, dead_code)]
mod ppu;
#[allow(clippy::all, dead_code)]
mod render3d;
mod spu;
pub mod state;

use bus::{Bus, Machine, MachineHandle, View, DMA_HBLANK, DMA_VBLANK, IRQ_HBLANK, IRQ_VBLANK};
use cpu::Cpu;
use ppu::Ppu;
use std::path::Path;
use zaimule_core::keymap::{
    BTN_A, BTN_B, BTN_DOWN, BTN_L, BTN_LEFT, BTN_R, BTN_RIGHT, BTN_SELECT, BTN_START, BTN_UP,
    BTN_X, BTN_Y,
};

pub const NDS_WIDTH: usize = ppu::WIDTH;
pub const NDS_HEIGHT: usize = ppu::HEIGHT * 2;
pub const NDS_SAVE_LEN: usize = 0x8_0000;

const SCANLINES_PER_FRAME: u32 = 263;
const ARM9_INSTR_PER_LINE: u32 = 2000;
const ARM7_INSTR_PER_LINE: u32 = 500;
const SPU_CYCLES_PER_LINE: u32 = 1062;
const PEN_RELEASE_FRAMES: u32 = 3;

/// Optional user-supplied Nintendo DS system images loaded from `system_dir`.
#[derive(Debug, Clone, Default)]
pub struct NdsBiosAssets {
    pub bios7: Option<Vec<u8>>,
    pub bios9: Option<Vec<u8>>,
    pub firmware: Option<Vec<u8>>,
}

impl NdsBiosAssets {
    /// Loads valid `bios7.bin` (16 KiB), `bios9.bin` (4 KiB), and `firmware.bin` (128/256/512 KiB)
    /// from `system_dir`, silently ignoring missing or invalidly sized files so HLE fallback remains active.
    pub fn load_from_dir(system_dir: &Path) -> Self {
        let bios7 = std::fs::read(system_dir.join("bios7.bin"))
            .ok()
            .filter(|b| b.len() == 0x4000);
        let bios9 = std::fs::read(system_dir.join("bios9.bin"))
            .ok()
            .filter(|b| b.len() == 0x1000);
        let firmware = std::fs::read(system_dir.join("firmware.bin"))
            .ok()
            .filter(|b| matches!(b.len(), 0x2_0000 | 0x4_0000 | 0x8_0000));
        Self {
            bios7,
            bios9,
            firmware,
        }
    }
}

pub struct NdsMachine {
    cpu9: Cpu<View<0>>,
    cpu7: Cpu<View<1>>,
    ppu: Ppu,
    rom: Vec<u8>,
    frame_count: u32,
    pen_up_frames: u32,
    cpu_overclock_pct: u16,
    scale_3d: u8,
    frame_width: usize,
    frame_height: usize,
    framebuffer_rgba: Vec<u8>,
    // Keep `MachineHandle` LAST so `cpu9` and `cpu7` (holding `MachinePtr`) drop first.
    machine: MachineHandle,
}

impl NdsMachine {
    pub fn from_rom(rom_bytes: &[u8]) -> Result<Self, String> {
        Self::from_rom_with_bios(rom_bytes, NdsBiosAssets::default())
    }

    pub fn from_rom_with_bios(rom_bytes: &[u8], bios: NdsBiosAssets) -> Result<Self, String> {
        if rom_bytes.len() < 0x170 {
            return Err("ROM Nintendo DS invalide : en-tête trop court (< 0x170 octets)".into());
        }
        let mut rom = rom_bytes.to_vec();
        if rom.len() > 0x4800 {
            let gamecode = u32::from_le_bytes(rom[0x0C..0x10].try_into().unwrap());
            key1::decrypt_secure_area_with_bios7(
                gamecode,
                &mut rom[0x4000..0x4800],
                bios.bios7.as_deref(),
            );
        }

        let r32 = |off: usize| u32::from_le_bytes(rom[off..off + 4].try_into().unwrap());
        let arm9_off = r32(0x20) as usize;
        let arm9_entry = r32(0x24);
        let arm9_addr = r32(0x28);
        let arm9_size = r32(0x2C) as usize;
        let arm7_off = r32(0x30) as usize;
        let arm7_entry = r32(0x34);
        let arm7_addr = r32(0x38);
        let arm7_size = r32(0x3C) as usize;

        let arm9_end = arm9_off
            .checked_add(arm9_size)
            .ok_or_else(|| "Section ARM9 DS invalide (dépassement)".to_string())?;
        let arm7_end = arm7_off
            .checked_add(arm7_size)
            .ok_or_else(|| "Section ARM7 DS invalide (dépassement)".to_string())?;
        if arm9_end > rom.len() {
            return Err(format!(
                "Section ARM9 DS hors limites ({arm9_end} > {} octets)",
                rom.len()
            ));
        }
        if arm7_end > rom.len() {
            return Err(format!(
                "Section ARM7 DS hors limites ({arm7_end} > {} octets)",
                rom.len()
            ));
        }

        let mut machine_inner = Machine::new();
        machine_inner.rom = rom.clone();
        let us = machine_inner.install_bios_and_firmware(bios.bios7, bios.bios9, bios.firmware);
        let machine = MachineHandle::new(machine_inner);

        let mut cpu9 = Cpu::new(
            View::<0>::new(machine.ptr()),
            true,
            arm9_entry,
            0x0300_2F7C,
            0x0300_3F80,
            0x0300_3FC0,
        );
        let mut cpu7 = Cpu::new(
            View::<1>::new(machine.ptr()),
            false,
            arm7_entry,
            0x0380_FD80,
            0x0380_FF80,
            0x0380_FFC0,
        );

        for i in 0..arm9_size {
            cpu9.bus
                .write8(arm9_addr.wrapping_add(i as u32), rom[arm9_off + i]);
        }
        for i in 0..arm7_size {
            cpu7.bus
                .write8(arm7_addr.wrapping_add(i as u32), rom[arm7_off + i]);
        }
        for (i, &b) in rom[..0x170.min(rom.len())].iter().enumerate() {
            cpu9.bus.write8(0x027F_FE00 + i as u32, b);
        }

        cpu9.bus.write32(0x027F_F800, 0x0000_1FC2);
        cpu9.bus.write32(0x027F_F804, 0x0000_1FC2);
        cpu9.bus.write32(0x027F_FC00, 0x0000_1FC2);
        cpu9.bus.write32(0x027F_FC04, 0x0000_1FC2);
        cpu9.bus.write16(0x027F_F850, 0x5835);
        cpu9.bus.write16(0x027F_FC10, 0x5835);
        cpu9.bus.write32(0x027F_FC40, 1);

        for (i, &b) in us.iter().enumerate() {
            cpu9.bus.write8(0x027F_FC80 + i as u32, b);
        }

        Ok(Self {
            cpu9,
            cpu7,
            ppu: Ppu::new(),
            rom,
            frame_count: 0,
            pen_up_frames: u32::MAX,
            cpu_overclock_pct: 100,
            scale_3d: 1,
            frame_width: NDS_WIDTH,
            frame_height: NDS_HEIGHT,
            framebuffer_rgba: vec![0; NDS_WIDTH * NDS_HEIGHT * 4],
            machine,
        })
    }

    pub fn set_cpu_overclock_pct(&mut self, pct: u16) {
        self.cpu_overclock_pct = pct.clamp(50, 400);
    }

    pub fn cpu_overclock_pct(&self) -> u16 {
        self.cpu_overclock_pct
    }

    pub fn set_3d_scale(&mut self, scale: u8) {
        self.scale_3d = scale.clamp(1, 2);
    }

    pub fn scale_3d(&self) -> u8 {
        self.scale_3d
    }

    pub fn frame_dimensions(&self) -> (u16, u16) {
        (self.frame_width as u16, self.frame_height as u16)
    }

    pub fn has_custom_bios(&self) -> (bool, bool) {
        let mm = self.machine.borrow();
        (mm.bios7.is_some(), mm.bios9.is_some())
    }

    pub fn has_custom_firmware(&self) -> bool {
        self.machine.borrow().custom_firmware
    }

    pub fn game_code(&self) -> String {
        if self.rom.len() >= 0x10 {
            String::from_utf8_lossy(&self.rom[0x0C..0x10])
                .trim_matches('\0')
                .to_string()
        } else {
            String::new()
        }
    }

    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }

    pub fn vcount(&self) -> u16 {
        self.machine.borrow().vcount
    }

    pub fn scanline_clock(&self) -> u64 {
        self.machine.borrow().now
    }

    pub fn step_frame(&mut self, buttons_p1: u16, touch: Option<(u32, u32, bool)>) -> &[u8] {
        self.step_frame_ex(buttons_p1, touch, false)
    }

    /// Steps one full Nintendo DS frame. When `skip_render` is `true` (used during
    /// fast-forward intermediate frames), skips PPU/3D/RGBA output unless Display
    /// Capture (`DISPCAPCNT` bit 31) is armed so VRAM feedback effects remain exact.
    pub fn step_frame_ex(
        &mut self,
        buttons_p1: u16,
        touch: Option<(u32, u32, bool)>,
        skip_render: bool,
    ) -> &[u8] {
        {
            let mut mm = self.machine.borrow_mut();
            mm.keyinput = map_keyinput(buttons_p1);
            let not_x = u16::from(buttons_p1 & BTN_X == 0);
            let not_y = u16::from(buttons_p1 & BTN_Y == 0);
            mm.extkeyin = 0x7C | not_x | (not_y << 1);

            if let Some((tx, ty, true)) = touch {
                mm.touch_x = tx.min(255);
                mm.touch_y = ty.min(191);
                self.pen_up_frames = 0;
            } else {
                self.pen_up_frames = self.pen_up_frames.saturating_add(1);
            }
            mm.touch_down = self.pen_up_frames < PEN_RELEASE_FRAMES;
            if mm.touch_down {
                mm.extkeyin &= !0x40;
            }
        }

        const ARM7_SLICE_QUANTUM: u32 = 32;
        let sub_slices = (ARM7_INSTR_PER_LINE / ARM7_SLICE_QUANTUM).max(4);
        let hdraw_slices = (sub_slices * 3) / 4;
        let hblank_slices = sub_slices - hdraw_slices;
        let arm9_per_line = ((u64::from(ARM9_INSTR_PER_LINE) * u64::from(self.cpu_overclock_pct))
            / 100)
            .max(500) as u32;
        let arm9_per_slice = (arm9_per_line / sub_slices).max(1);

        for line in 0..SCANLINES_PER_FRAME {
            {
                let mut mm = self.machine.borrow_mut();
                mm.now += 1;
                mm.vcount = line as u16;
                for cpu in 0..2 {
                    let in_vblank = (192..262).contains(&line);
                    let stat = &mut mm.dispstat[cpu];
                    // Clear HBlank bit (bit 1) at start of scanline; update VBlank (bit 0) & VMatch (bit 2).
                    *stat = (*stat & !3) | u16::from(in_vblank);
                    let target = (*stat >> 8) | ((*stat & 0x80) << 1);
                    let matched = line as u16 == target;
                    *stat = (*stat & !4) | (u16::from(matched) << 2);
                    if matched && *stat & 0x20 != 0 {
                        mm.if_[cpu] |= 1 << 2;
                    }
                }
                if line == 192 {
                    for cpu in 0..2 {
                        if mm.dispstat[cpu] & 8 != 0 {
                            mm.if_[cpu] |= IRQ_VBLANK;
                        }
                    }
                }
                if mm.gxstat_irq != 0 {
                    mm.if_[0] |= 1 << 21;
                }
            }
            if line == 192 {
                self.cpu9.bus.dma_service(DMA_VBLANK);
                self.cpu7.bus.dma_service(DMA_VBLANK);
            }
            self.machine.borrow_mut().tick_timers(2124);
            // HDraw phase (~74% of scanline)
            for _ in 0..hdraw_slices {
                if self.cpu9.is_halted_idle() && self.cpu7.is_halted_idle() {
                    break;
                }
                self.cpu9.run_slice(arm9_per_slice);
                self.cpu7.run_slice(ARM7_SLICE_QUANTUM);
            }
            // Enter HBlank: assert DISPSTAT bit 1, fire HBlank IRQ (if enabled), and service ARM9 HBlank DMA on visible lines.
            let hblank_dma = {
                let mut mm = self.machine.borrow_mut();
                for cpu in 0..2 {
                    mm.dispstat[cpu] |= 2;
                    if mm.dispstat[cpu] & 0x10 != 0 {
                        mm.if_[cpu] |= IRQ_HBLANK;
                    }
                }
                line < 192 && mm.dma_hblank_armed
            };
            if hblank_dma {
                self.cpu9.bus.dma_service(DMA_HBLANK);
            }
            // HBlank phase (~26% of scanline)
            for _ in 0..hblank_slices {
                if self.cpu9.is_halted_idle() && self.cpu7.is_halted_idle() {
                    break;
                }
                self.cpu9.run_slice(arm9_per_slice);
                self.cpu7.run_slice(ARM7_SLICE_QUANTUM);
            }
            self.machine
                .borrow_mut()
                .tick_spu_scanline(SPU_CYCLES_PER_LINE);
        }

        self.frame_count = self.frame_count.wrapping_add(1);

        if skip_render {
            let cap_armed = self.machine.borrow().io2d[0][0x67] & 0x80 != 0;
            if cap_armed {
                self.ppu
                    .render_frame_scaled(&mut self.machine.borrow_mut(), 1);
            }
            return &self.framebuffer_rgba;
        }

        self.ppu
            .render_frame_scaled(&mut self.machine.borrow_mut(), self.scale_3d);

        let powcnt1 = self.machine.borrow().powcnt1;
        let a_on_top = powcnt1 & 0x8000 != 0;

        if self.ppu.hd_active {
            const HD_W: usize = ppu::WIDTH * 2;
            const HD_H_SINGLE: usize = ppu::HEIGHT * 2;
            self.frame_width = HD_W;
            self.frame_height = HD_H_SINGLE * 2;
            let total_bytes = HD_W * (HD_H_SINGLE * 2) * 4;
            if self.framebuffer_rgba.len() != total_bytes {
                self.framebuffer_rgba.resize(total_bytes, 0xFF);
            }
            let half_bytes = HD_W * HD_H_SINGLE * 4;
            let (top_dst, bot_dst) = self.framebuffer_rgba.split_at_mut(half_bytes);
            let (a_dst, b_dst) = if a_on_top {
                (top_dst, bot_dst)
            } else {
                (bot_dst, top_dst)
            };
            // Engine A: true 512×384 HD 3D + 2D overlay
            for (dst, &px) in a_dst
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(self.ppu.fb_a_hd.iter())
            {
                *dst = [(px >> 16) as u8, (px >> 8) as u8, px as u8, 0xFF];
            }
            // Engine B: crisp 2× nearest-neighbor expansion to match 512×384 half-frame
            let b_chunks = b_dst.as_chunks_mut::<4>().0;
            for y in 0..ppu::HEIGHT {
                let src_row = &self.ppu.fb_b[y * ppu::WIDTH..(y + 1) * ppu::WIDTH];
                let dst_row0 = (y * 2) * HD_W;
                let dst_row1 = (y * 2 + 1) * HD_W;
                for (x, &px) in src_row.iter().enumerate() {
                    let rgba = [(px >> 16) as u8, (px >> 8) as u8, px as u8, 0xFF];
                    let x2 = x * 2;
                    b_chunks[dst_row0 + x2] = rgba;
                    b_chunks[dst_row0 + x2 + 1] = rgba;
                    b_chunks[dst_row1 + x2] = rgba;
                    b_chunks[dst_row1 + x2 + 1] = rgba;
                }
            }
        } else {
            self.frame_width = NDS_WIDTH;
            self.frame_height = NDS_HEIGHT;
            let total_bytes = NDS_WIDTH * NDS_HEIGHT * 4;
            if self.framebuffer_rgba.len() != total_bytes {
                self.framebuffer_rgba.resize(total_bytes, 0xFF);
            }
            let [upper, lower] = if a_on_top {
                [&self.ppu.fb_a, &self.ppu.fb_b]
            } else {
                [&self.ppu.fb_b, &self.ppu.fb_a]
            };

            let half_bytes = ppu::WIDTH * ppu::HEIGHT * 4;
            let (top_dst, bot_dst) = self.framebuffer_rgba.split_at_mut(half_bytes);
            for (dst, &px) in top_dst.as_chunks_mut::<4>().0.iter_mut().zip(upper.iter()) {
                *dst = [(px >> 16) as u8, (px >> 8) as u8, px as u8, 0xFF];
            }
            for (dst, &px) in bot_dst.as_chunks_mut::<4>().0.iter_mut().zip(lower.iter()) {
                *dst = [(px >> 16) as u8, (px >> 8) as u8, px as u8, 0xFF];
            }
        }

        &self.framebuffer_rgba
    }

    pub fn set_audio_rate(&mut self, hz: u32) {
        self.machine.borrow_mut().spu.set_sample_rate(hz);
    }

    pub fn drain_audio(&mut self, out: &mut Vec<f32>) -> u16 {
        self.machine.borrow_mut().spu.drain_audio(out)
    }

    pub fn framebuffer_rgba(&self) -> &[u8] {
        &self.framebuffer_rgba
    }

    /// Packs the latest rendered dual-screen frame directly into little-endian RGB565 bytes
    /// (2 bytes/pixel, lossless for NDS 15-bit colors and half the IPC/GPU upload bandwidth).
    pub fn encode_rgb565_into(&self, dst: &mut Vec<u8>) -> (u16, u16) {
        #[inline(always)]
        fn pack_rgb24_to_565(px: u32) -> [u8; 2] {
            let r5 = (px >> 19) & 0x1F;
            let g6 = (px >> 10) & 0x3F;
            let b5 = (px >> 3) & 0x1F;
            (((r5 << 11) | (g6 << 5) | b5) as u16).to_le_bytes()
        }

        if self.frame_count == 0 {
            let w = self.frame_width as u16;
            let h = self.frame_height as u16;
            dst.reserve(self.frame_width * self.frame_height * 2);
            for px in self.framebuffer_rgba.as_chunks::<4>().0 {
                let r5 = u16::from(px[0] >> 3);
                let g6 = u16::from(px[1] >> 2);
                let b5 = u16::from(px[2] >> 3);
                dst.extend_from_slice(&((r5 << 11) | (g6 << 5) | b5).to_le_bytes());
            }
            return (w, h);
        }

        let a_on_top = self.machine.borrow().powcnt1 & 0x8000 != 0;
        if self.ppu.hd_active {
            const HD_W: usize = ppu::WIDTH * 2;
            const HD_H_SINGLE: usize = ppu::HEIGHT * 2;
            let total_bytes = HD_W * (HD_H_SINGLE * 2) * 2;
            let start = dst.len();
            dst.resize(start + total_bytes, 0);
            let half_bytes = HD_W * HD_H_SINGLE * 2;
            let (top_dst, bot_dst) = dst[start..].split_at_mut(half_bytes);
            let (a_dst, b_dst) = if a_on_top {
                (top_dst, bot_dst)
            } else {
                (bot_dst, top_dst)
            };
            for (slot, &px) in a_dst
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .zip(self.ppu.fb_a_hd.iter())
            {
                *slot = pack_rgb24_to_565(px);
            }
            let b_chunks = b_dst.as_chunks_mut::<2>().0;
            for y in 0..ppu::HEIGHT {
                let src_row = &self.ppu.fb_b[y * ppu::WIDTH..(y + 1) * ppu::WIDTH];
                let dst_row0 = (y * 2) * HD_W;
                let dst_row1 = (y * 2 + 1) * HD_W;
                for (x, &px) in src_row.iter().enumerate() {
                    let c = pack_rgb24_to_565(px);
                    let x2 = x * 2;
                    b_chunks[dst_row0 + x2] = c;
                    b_chunks[dst_row0 + x2 + 1] = c;
                    b_chunks[dst_row1 + x2] = c;
                    b_chunks[dst_row1 + x2 + 1] = c;
                }
            }
            (HD_W as u16, (HD_H_SINGLE * 2) as u16)
        } else {
            let total_bytes = NDS_WIDTH * NDS_HEIGHT * 2;
            let start = dst.len();
            dst.resize(start + total_bytes, 0);
            let half_bytes = ppu::WIDTH * ppu::HEIGHT * 2;
            let (top_dst, bot_dst) = dst[start..].split_at_mut(half_bytes);
            let [upper, lower] = if a_on_top {
                [&self.ppu.fb_a, &self.ppu.fb_b]
            } else {
                [&self.ppu.fb_b, &self.ppu.fb_a]
            };
            for (slot, &px) in top_dst.as_chunks_mut::<2>().0.iter_mut().zip(upper.iter()) {
                *slot = pack_rgb24_to_565(px);
            }
            for (slot, &px) in bot_dst.as_chunks_mut::<2>().0.iter_mut().zip(lower.iter()) {
                *slot = pack_rgb24_to_565(px);
            }
            (NDS_WIDTH as u16, NDS_HEIGHT as u16)
        }
    }

    pub fn load_sram(&mut self, data: &[u8]) {
        let mut mm = self.machine.borrow_mut();
        mm.save.fill(0xFF);
        let n = data.len().min(mm.save.len());
        mm.save[..n].copy_from_slice(&data[..n]);
        mm.save_dirty = false;
    }

    pub fn export_sram(&self) -> Vec<u8> {
        self.machine.borrow().save.clone()
    }

    pub fn is_sram_dirty(&self) -> bool {
        self.machine.borrow().save_dirty
    }

    pub fn clear_sram_dirty(&mut self) {
        self.machine.borrow_mut().save_dirty = false;
    }

    pub fn save_state_raw_bytes(&self) -> Result<Vec<u8>, String> {
        state::save_snapshot_raw(
            &self.machine,
            &self.cpu9,
            &self.cpu7,
            &self.ppu,
            self.frame_count,
        )
    }

    pub fn save_state_bytes(&self) -> Result<Vec<u8>, String> {
        state::save_snapshot(
            &self.machine,
            &self.cpu9,
            &self.cpu7,
            &self.ppu,
            self.frame_count,
        )
    }

    pub fn load_state_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        let frame = state::restore_snapshot(
            bytes,
            &mut self.machine,
            &mut self.cpu9,
            &mut self.cpu7,
            &mut self.ppu,
            &self.rom,
        )?;
        self.frame_count = frame;
        Ok(())
    }

    pub fn arm9_regs(&self) -> [u32; 16] {
        self.cpu9.st.regs
    }

    pub fn arm9_cpsr(&self) -> u32 {
        self.cpu9.st.cpsr
    }

    pub fn arm9_halted(&self) -> bool {
        self.cpu9.st.halted
    }

    pub fn arm7_regs(&self) -> [u32; 16] {
        self.cpu7.st.regs
    }

    pub fn arm7_cpsr(&self) -> u32 {
        self.cpu7.st.cpsr
    }

    pub fn arm7_halted(&self) -> bool {
        self.cpu7.st.halted
    }

    pub fn gx_stats(&self) -> (u64, usize) {
        let mm = self.machine.borrow();
        (mm.gx_cmds, mm.gx.last_poly_count)
    }

    pub fn read_mem8(&mut self, addr: u32) -> u8 {
        if addr >> 24 == 0x04 {
            let mm = self.machine.borrow();
            let off = (addr & 0xFFFF) as usize;
            return if off < 0x70 {
                mm.io2d[0][off]
            } else if (0x1000..0x1070).contains(&off) {
                mm.io2d[1][off - 0x1000]
            } else if off == 0x0130 {
                mm.keyinput as u8
            } else if off == 0x0131 {
                (mm.keyinput >> 8) as u8
            } else if off == 0x0136 {
                mm.extkeyin as u8
            } else {
                0
            };
        }
        self.cpu9.bus.read8(addr)
    }

    pub fn read_mem16(&mut self, addr: u32) -> u16 {
        let a = addr & !1;
        if a >> 24 == 0x04 {
            let mm = self.machine.borrow();
            return match a & 0xFFFF {
                0x0130 => mm.keyinput,
                0x0136 => mm.extkeyin,
                _ => 0,
            };
        }
        self.cpu9.bus.read16(a)
    }

    pub fn read_mem32(&mut self, addr: u32) -> u32 {
        let a = addr & !3;
        if a >> 24 == 0x04 {
            let lo = u32::from(self.read_mem16(a));
            let hi = u32::from(self.read_mem16(a.wrapping_add(2)));
            return lo | (hi << 16);
        }
        self.cpu9.bus.read32(a)
    }

    pub fn write_mem8(&mut self, addr: u32, val: u8) {
        if addr >> 24 == 0x04 {
            return;
        }
        self.cpu9.bus.write8(addr, val);
    }

    pub fn write_mem16(&mut self, addr: u32, val: u16) {
        if addr >> 24 == 0x04 {
            return;
        }
        self.cpu9.bus.write16(addr & !1, val);
    }

    pub fn write_mem32(&mut self, addr: u32, val: u32) {
        if addr >> 24 == 0x04 {
            return;
        }
        self.cpu9.bus.write32(addr & !3, val);
    }

    /// Executes a 32-bit Nintendo DS Action Replay code list (`[(w0, w1), ...]`).
    ///
    /// Supports standard AR DS opcodes `0..=A` (32/16/8-bit writes & conditionals including
    /// keypad checks at `0x0400_0130`), `B` (pointer offset), `C`/`D1` (loops), `D0`/`D2`
    /// (end-if/reset), `D3..=DC` (offset/data register ops), and `E` (block patch).
    /// All RAM writes go through `cpu9.bus`, automatically invalidating cached code pages.
    pub fn apply_action_replay_code(&mut self, ops: &[(u32, u32)]) {
        let mut offset: u32 = 0;
        let mut data_reg: u32 = 0;
        let mut cond_stack: Vec<bool> = Vec::with_capacity(8);
        let mut loop_stack: Vec<(usize, u32)> = Vec::with_capacity(4);
        let mut pc: usize = 0;
        let mut steps: usize = 0;
        const MAX_AR_STEPS: usize = 1024;

        while pc < ops.len() && steps < MAX_AR_STEPS {
            steps += 1;
            let (w0, w1) = ops[pc];
            pc += 1;
            let op_type = (w0 >> 28) as u8;
            let addr28 = w0 & 0x0FFF_FFFF;
            let active = cond_stack.iter().all(|&b| b);

            if !active {
                match op_type {
                    0x3..=0xA => cond_stack.push(false),
                    0xD => match (w0 >> 24) & 0xF {
                        0x0 => {
                            cond_stack.pop();
                        }
                        0x2 => {
                            cond_stack.clear();
                            loop_stack.clear();
                            offset = 0;
                            data_reg = 0;
                        }
                        _ => {}
                    },
                    0xE => {
                        let skip_lines = (w1 as usize).div_ceil(8);
                        pc = pc.saturating_add(skip_lines);
                    }
                    _ => {}
                }
                continue;
            }

            let cond_addr = if addr28 == 0 { offset } else { addr28 };
            match op_type {
                0x0 => self.write_mem32(addr28.wrapping_add(offset), w1),
                0x1 => self.write_mem16(addr28.wrapping_add(offset), w1 as u16),
                0x2 => self.write_mem8(addr28.wrapping_add(offset), w1 as u8),
                0x3 => cond_stack.push(w1 > self.read_mem32(cond_addr)),
                0x4 => cond_stack.push(w1 < self.read_mem32(cond_addr)),
                0x5 => cond_stack.push(w1 == self.read_mem32(cond_addr)),
                0x6 => cond_stack.push(w1 != self.read_mem32(cond_addr)),
                0x7..=0xA => {
                    let mask = !((w1 >> 16) as u16);
                    let target = (w1 & 0xFFFF) as u16;
                    let cur = self.read_mem16(cond_addr) & mask;
                    let pass = match op_type {
                        0x7 => target > cur,
                        0x8 => target < cur,
                        0x9 => target == cur,
                        _ => target != cur,
                    };
                    cond_stack.push(pass);
                }
                0xB => offset = self.read_mem32(addr28.wrapping_add(offset)),
                0xC => {
                    loop_stack.push((pc, w1));
                }
                0xD => match (w0 >> 24) & 0xF {
                    0x0 => {
                        cond_stack.pop();
                    }
                    0x1 => {
                        if let Some((start_pc, rem)) = loop_stack.last_mut() {
                            if *rem > 0 {
                                *rem -= 1;
                                pc = *start_pc;
                            } else {
                                loop_stack.pop();
                            }
                        }
                    }
                    0x2 => {
                        cond_stack.clear();
                        loop_stack.clear();
                        offset = 0;
                        data_reg = 0;
                    }
                    0x3 => offset = w1,
                    0x4 => data_reg = data_reg.wrapping_add(w1),
                    0x5 => data_reg = w1,
                    0x6 => {
                        self.write_mem32(w1.wrapping_add(offset), data_reg);
                        offset = offset.wrapping_add(4);
                    }
                    0x7 => {
                        self.write_mem16(w1.wrapping_add(offset), data_reg as u16);
                        offset = offset.wrapping_add(2);
                    }
                    0x8 => {
                        self.write_mem8(w1.wrapping_add(offset), data_reg as u8);
                        offset = offset.wrapping_add(1);
                    }
                    0x9 => data_reg = self.read_mem32(w1.wrapping_add(offset)),
                    0xA => data_reg = u32::from(self.read_mem16(w1.wrapping_add(offset))),
                    0xB => data_reg = u32::from(self.read_mem8(w1.wrapping_add(offset))),
                    0xC => offset = offset.wrapping_add(w1),
                    _ => {}
                },
                0xE => {
                    let byte_count = (w1 as usize).min(4096);
                    let mut dst = addr28.wrapping_add(offset);
                    let mut written = 0usize;
                    while written < byte_count && pc < ops.len() {
                        let (p0, p1) = ops[pc];
                        pc += 1;
                        for b in p0.to_le_bytes().into_iter().chain(p1.to_le_bytes()) {
                            if written >= byte_count {
                                break;
                            }
                            self.write_mem8(dst, b);
                            dst = dst.wrapping_add(1);
                            written += 1;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn vram_bank_copy(&self, bank: usize, offset: usize, len: usize) -> Vec<u8> {
        let mm = self.machine.borrow();
        let idx = bank.min(8);
        let bank_buf = &mm.vram[idx];
        let start = offset.min(bank_buf.len());
        let end = start.saturating_add(len).min(bank_buf.len());
        bank_buf[start..end].to_vec()
    }

    pub fn vram_bank_write(&mut self, bank: usize, offset: usize, val: u8) {
        let mut mm = self.machine.borrow_mut();
        let idx = bank.min(8);
        if let Some(slot) = mm.vram[idx].get_mut(offset) {
            *slot = val;
        }
    }

    pub fn main_ram_copy(&self, offset: usize, len: usize) -> Vec<u8> {
        let mm = self.machine.borrow();
        let start = offset.min(mm.main_ram.len());
        let end = start.saturating_add(len).min(mm.main_ram.len());
        mm.main_ram[start..end].to_vec()
    }
}

fn map_keyinput(buttons_p1: u16) -> u16 {
    let mut pressed = 0u16;
    if buttons_p1 & BTN_A != 0 {
        pressed |= 1 << 0;
    }
    if buttons_p1 & BTN_B != 0 {
        pressed |= 1 << 1;
    }
    if buttons_p1 & BTN_SELECT != 0 {
        pressed |= 1 << 2;
    }
    if buttons_p1 & BTN_START != 0 {
        pressed |= 1 << 3;
    }
    if buttons_p1 & BTN_RIGHT != 0 {
        pressed |= 1 << 4;
    }
    if buttons_p1 & BTN_LEFT != 0 {
        pressed |= 1 << 5;
    }
    if buttons_p1 & BTN_UP != 0 {
        pressed |= 1 << 6;
    }
    if buttons_p1 & BTN_DOWN != 0 {
        pressed |= 1 << 7;
    }
    if buttons_p1 & BTN_R != 0 {
        pressed |= 1 << 8;
    }
    if buttons_p1 & BTN_L != 0 {
        pressed |= 1 << 9;
    }
    !pressed & 0x3FF
}
