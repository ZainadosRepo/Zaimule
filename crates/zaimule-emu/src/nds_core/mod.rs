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

use bus::{user_settings_block, Bus, Machine, MachineHandle, View, DMA_VBLANK, IRQ_VBLANK};
use cpu::Cpu;
use ppu::Ppu;
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

pub struct NdsMachine {
    cpu9: Cpu<View>,
    cpu7: Cpu<View>,
    ppu: Ppu,
    rom: Vec<u8>,
    frame_count: u32,
    pen_up_frames: u32,
    framebuffer_rgba: Vec<u8>,
    // Keep `MachineHandle` LAST so `cpu9` and `cpu7` (holding `MachinePtr`) drop first.
    machine: MachineHandle,
}

impl NdsMachine {
    pub fn from_rom(rom_bytes: &[u8]) -> Result<Self, String> {
        if rom_bytes.len() < 0x170 {
            return Err("ROM Nintendo DS invalide : en-tête trop court (< 0x170 octets)".into());
        }
        let mut rom = rom_bytes.to_vec();
        if rom.len() > 0x4800 {
            let gamecode = u32::from_le_bytes(rom[0x0C..0x10].try_into().unwrap());
            key1::decrypt_secure_area(gamecode, &mut rom[0x4000..0x4800]);
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
        let machine = MachineHandle::new(machine_inner);

        let mut cpu9 = Cpu::new(
            View {
                m: machine.ptr(),
                cpu: 0,
                in_dma: false,
            },
            true,
            arm9_entry,
            0x0300_2F7C,
            0x0300_3F80,
            0x0300_3FC0,
        );
        let mut cpu7 = Cpu::new(
            View {
                m: machine.ptr(),
                cpu: 1,
                in_dma: false,
            },
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

        let us = user_settings_block();
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
            framebuffer_rgba: vec![0; NDS_WIDTH * NDS_HEIGHT * 4],
            machine,
        })
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

    pub fn step_frame(&mut self, buttons_p1: u16, touch: Option<(u32, u32, bool)>) -> &[u8] {
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

        for line in 0..SCANLINES_PER_FRAME {
            {
                let mut mm = self.machine.borrow_mut();
                mm.now += 1;
                mm.vcount = line as u16;
                for cpu in 0..2 {
                    let in_vblank = (192..262).contains(&line);
                    let stat = &mut mm.dispstat[cpu];
                    *stat = (*stat & !1) | u16::from(in_vblank);
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
            let sub_slices = ARM7_INSTR_PER_LINE / 8;
            for _ in 0..sub_slices {
                self.cpu9.run_slice(ARM9_INSTR_PER_LINE / sub_slices);
                self.cpu7.run_slice(8);
            }
            self.machine
                .borrow_mut()
                .tick_spu_scanline(SPU_CYCLES_PER_LINE);
        }

        self.ppu.render_frame(&mut self.machine.borrow_mut());
        self.frame_count = self.frame_count.wrapping_add(1);

        let powcnt1 = self.machine.borrow().powcnt1;
        let [upper, lower] = if powcnt1 & 0x8000 != 0 {
            [&self.ppu.fb_a, &self.ppu.fb_b]
        } else {
            [&self.ppu.fb_b, &self.ppu.fb_a]
        };

        let half_pixels = ppu::WIDTH * ppu::HEIGHT;
        for (idx, &px) in upper.iter().enumerate() {
            let off = idx * 4;
            self.framebuffer_rgba[off] = (px >> 16) as u8;
            self.framebuffer_rgba[off + 1] = (px >> 8) as u8;
            self.framebuffer_rgba[off + 2] = px as u8;
            self.framebuffer_rgba[off + 3] = 0xFF;
        }
        for (idx, &px) in lower.iter().enumerate() {
            let off = (half_pixels + idx) * 4;
            self.framebuffer_rgba[off] = (px >> 16) as u8;
            self.framebuffer_rgba[off + 1] = (px >> 8) as u8;
            self.framebuffer_rgba[off + 2] = px as u8;
            self.framebuffer_rgba[off + 3] = 0xFF;
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
            } else {
                0
            };
        }
        self.cpu9.bus.read8(addr)
    }

    pub fn write_mem8(&mut self, addr: u32, val: u8) {
        if addr >> 24 == 0x04 {
            return;
        }
        self.cpu9.bus.write8(addr, val);
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
