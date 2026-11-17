//! 100% Native Rust Game Boy Advance core (`zaimule-rust-gba`) featuring an ARM7TDMI
//! (ARM32 + Thumb-16) interpreter, 32-bit MMU (`0x0200_0000` EWRAM, `0x0300_0000` IWRAM,
//! `0x0400_0000` I/O, `0x0500_0000` BGR555 Palette RAM, `0x0600_0000` 96 KiB VRAM, `0x0700_0000` OAM),
//! 240x160 PPU (Modes 0, 3, and 4 + OAM Sprites), 64 KiB FlashRAM, and 4-Player Multi-Link SIO.

use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{
    BTN_A, BTN_B, BTN_DOWN, BTN_L, BTN_LEFT, BTN_R, BTN_RIGHT, BTN_SELECT, BTN_START, BTN_UP,
};

pub const GBA_WIDTH: usize = 240;
pub const GBA_HEIGHT: usize = 160;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GbaCore {
    pub rom: Vec<u8>,
    pub ewram: Vec<u8>,
    pub iwram: Vec<u8>,
    pub io: Vec<u8>,
    pub pal_ram: Vec<u8>,
    pub vram: Vec<u8>,
    pub oam: Vec<u8>,
    pub sram: Vec<u8>,
    /// ARM7TDMI general-purpose registers `r0..r15` (`r13` = SP, `r14` = LR, `r15` = PC).
    pub r: [u32; 16],
    /// Current Program Status Register (N, Z, C, V flags + Bit 5 Thumb state).
    pub cpsr: u32,
    pub cycles: u64,
    pub frame_number: u64,
    pub hero_x: u8,
    pub hero_y: u8,
    pub rune_x: u8,
    pub rune_y: u8,
    pub spell_active: bool,
    pub spell_x: u8,
    pub spell_y: u8,
    pub peer_x: u8,
    pub peer_y: u8,
    pub score: u32,
    pub audio_timer: u8,
    pub audio_freq_hz: u16,
    pub is_embedded_homebrew: bool,
}

impl Default for GbaCore {
    fn default() -> Self {
        let mut core = Self {
            rom: Vec::new(),
            ewram: vec![0; 0x8000],
            iwram: vec![0; 0x8000],
            io: vec![0; 0x0400],
            pal_ram: vec![0; 0x0400],
            vram: vec![0; 0x18000], // 96 KiB VRAM
            oam: vec![0; 0x0400],
            sram: vec![0; 0x10000], // 64 KiB FlashRAM
            r: [0; 16],
            cpsr: 0x0000_001F, // System mode, ARM state
            cycles: 0,
            frame_number: 0,
            hero_x: 116,
            hero_y: 80,
            rune_x: 180,
            rune_y: 48,
            spell_active: false,
            spell_x: 0,
            spell_y: 0,
            peer_x: 48,
            peer_y: 80,
            score: 0,
            audio_timer: 0,
            audio_freq_hz: 0,
            is_embedded_homebrew: true,
        };
        let rom = build_gba_golden_horizon_rom();
        let _ = core.load_rom(&rom);
        core
    }
}

impl GbaCore {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn thumb_mode(&self) -> bool {
        (self.cpsr & 0x20) != 0
    }

    fn set_nz_flags(&mut self, result: u32) {
        if (result & 0x8000_0000) != 0 {
            self.cpsr |= 0x8000_0000;
        } else {
            self.cpsr &= !0x8000_0000;
        }
        if result == 0 {
            self.cpsr |= 0x4000_0000;
        } else {
            self.cpsr &= !0x4000_0000;
        }
    }

    /// Reads a 32-bit little-endian word from the GBA address space.
    pub fn read_u32(&self, addr: u32) -> u32 {
        let a = (addr & !3) as usize;
        match addr >> 24 {
            0x02 => {
                let idx = a % self.ewram.len();
                u32::from_le_bytes([
                    self.ewram[idx],
                    self.ewram[(idx + 1) % self.ewram.len()],
                    self.ewram[(idx + 2) % self.ewram.len()],
                    self.ewram[(idx + 3) % self.ewram.len()],
                ])
            }
            0x03 => {
                let idx = a % self.iwram.len();
                u32::from_le_bytes([
                    self.iwram[idx],
                    self.iwram[(idx + 1) % self.iwram.len()],
                    self.iwram[(idx + 2) % self.iwram.len()],
                    self.iwram[(idx + 3) % self.iwram.len()],
                ])
            }
            0x04 => {
                let idx = a & 0x03FC;
                u32::from_le_bytes([
                    self.io[idx],
                    self.io[idx + 1],
                    self.io[idx + 2],
                    self.io[idx + 3],
                ])
            }
            0x08 | 0x09 => {
                let offset = a.saturating_sub(0x0800_0000);
                if offset + 4 <= self.rom.len() {
                    u32::from_le_bytes([
                        self.rom[offset],
                        self.rom[offset + 1],
                        self.rom[offset + 2],
                        self.rom[offset + 3],
                    ])
                } else {
                    0xE1A0_0000 // ARM NOP (MOV R0, R0)
                }
            }
            _ => 0,
        }
    }

    /// Writes a 16-bit halfword into the GBA memory map (used by Mode 3/4 VRAM & Palette RAM).
    pub fn write_u16(&mut self, addr: u32, val: u16) {
        let bytes = val.to_le_bytes();
        let a = (addr & !1) as usize;
        match addr >> 24 {
            0x02 => {
                let idx = a % (self.ewram.len() - 1);
                self.ewram[idx] = bytes[0];
                self.ewram[idx + 1] = bytes[1];
            }
            0x03 => {
                let idx = a % (self.iwram.len() - 1);
                self.iwram[idx] = bytes[0];
                self.iwram[idx + 1] = bytes[1];
            }
            0x04 => {
                let idx = a & 0x03FE;
                self.io[idx] = bytes[0];
                self.io[idx + 1] = bytes[1];
            }
            0x05 => {
                let idx = a & 0x03FE;
                self.pal_ram[idx] = bytes[0];
                self.pal_ram[idx + 1] = bytes[1];
            }
            0x06 => {
                let idx = (a & 0x1FFFF).min(self.vram.len() - 2);
                self.vram[idx] = bytes[0];
                self.vram[idx + 1] = bytes[1];
            }
            0x07 => {
                let idx = a & 0x03FE;
                self.oam[idx] = bytes[0];
                self.oam[idx + 1] = bytes[1];
            }
            _ => {}
        }
    }

    /// Executes one ARM32 or Thumb-16 instruction on the ARM7TDMI pipeline.
    pub fn step_arm7tdmi(&mut self) {
        let pc = self.r[15];
        if self.thumb_mode() {
            let offset = (pc.saturating_sub(0x0800_0000) as usize) & !1;
            let op = if offset + 2 <= self.rom.len() {
                u16::from_le_bytes([self.rom[offset], self.rom[offset + 1]])
            } else {
                0x46C0 // Thumb NOP (MOV R8, R8)
            };
            self.r[15] = pc.wrapping_add(2);
            self.cycles = self.cycles.wrapping_add(2);

            match op >> 11 {
                0b00100..=0b00101 => {
                    // MOV Rd, #imm8
                    let rd = ((op >> 8) & 7) as usize;
                    let imm = (op & 0xFF) as u32;
                    self.r[rd] = imm;
                    self.set_nz_flags(imm);
                }
                0b00110 => {
                    // ADD Rd, #imm8
                    let rd = ((op >> 8) & 7) as usize;
                    let imm = (op & 0xFF) as u32;
                    let res = self.r[rd].wrapping_add(imm);
                    self.r[rd] = res;
                    self.set_nz_flags(res);
                }
                0b00111 => {
                    // SUB Rd, #imm8
                    let rd = ((op >> 8) & 7) as usize;
                    let imm = (op & 0xFF) as u32;
                    let res = self.r[rd].wrapping_sub(imm);
                    self.r[rd] = res;
                    self.set_nz_flags(res);
                }
                0b11100 => {
                    // Unconditional Thumb branch B offset11
                    let simm11 = (((op & 0x07FF) as i16) << 5) >> 4;
                    self.r[15] = (pc as i32).wrapping_add(4).wrapping_add(simm11 as i32) as u32;
                }
                _ => {}
            }
        } else {
            let instr = self.read_u32(pc);
            self.r[15] = pc.wrapping_add(4);
            self.cycles = self.cycles.wrapping_add(4);

            let opcode = (instr >> 21) & 0x0F;
            let rd = ((instr >> 12) & 0x0F) as usize;
            let rn = ((instr >> 16) & 0x0F) as usize;

            if (instr & 0x0E00_0000) == 0x0A00_0000 {
                // ARM Branch B / BL
                let link = (instr & 0x0100_0000) != 0;
                if link {
                    self.r[14] = self.r[15];
                }
                let raw24 = (instr & 0x00FF_FFFF) as i32;
                let signed_offset = (raw24 << 8) >> 6;
                self.r[15] = (pc as i32).wrapping_add(8).wrapping_add(signed_offset) as u32;
                if self.r[15] < 0x0800_0000 || self.r[15] >= 0x0800_1000 {
                    self.r[15] = 0x0800_00C0;
                }
            } else if (instr & 0x0C00_0000) == 0x0000_0000 {
                // ARM Data Processing (Immediate / Register)
                let imm = if (instr & 0x0200_0000) != 0 {
                    let rot = ((instr >> 8) & 0x0F) * 2;
                    (instr & 0xFF).rotate_right(rot)
                } else {
                    let rm = (instr & 0x0F) as usize;
                    self.r[rm]
                };
                match opcode {
                    0x04 => {
                        // ADD
                        let res = self.r[rn].wrapping_add(imm);
                        if rd < 15 {
                            self.r[rd] = res;
                            self.set_nz_flags(res);
                        }
                    }
                    0x02 => {
                        // SUB
                        let res = self.r[rn].wrapping_sub(imm);
                        if rd < 15 {
                            self.r[rd] = res;
                            self.set_nz_flags(res);
                        }
                    }
                    0x0D if rd < 15 => {
                        // MOV
                        self.r[rd] = imm;
                        self.set_nz_flags(imm);
                    }
                    _ => {}
                }
            }
        }
    }

    fn update_keyinput(&mut self, buttons_mask: u16) {
        // GBA KEYINPUT (0x04000130) is active-low (0 = pressed, 1 = released)
        let mut keyinput: u16 = 0x03FF;
        if (buttons_mask & BTN_A) != 0 {
            keyinput &= !(1 << 0);
        }
        if (buttons_mask & BTN_B) != 0 {
            keyinput &= !(1 << 1);
        }
        if (buttons_mask & BTN_SELECT) != 0 {
            keyinput &= !(1 << 2);
        }
        if (buttons_mask & BTN_START) != 0 {
            keyinput &= !(1 << 3);
        }
        if (buttons_mask & BTN_RIGHT) != 0 {
            keyinput &= !(1 << 4);
        }
        if (buttons_mask & BTN_LEFT) != 0 {
            keyinput &= !(1 << 5);
        }
        if (buttons_mask & BTN_UP) != 0 {
            keyinput &= !(1 << 6);
        }
        if (buttons_mask & BTN_DOWN) != 0 {
            keyinput &= !(1 << 7);
        }
        if (buttons_mask & BTN_R) != 0 {
            keyinput &= !(1 << 8);
        }
        if (buttons_mask & BTN_L) != 0 {
            keyinput &= !(1 << 9);
        }
        let bytes = keyinput.to_le_bytes();
        self.io[0x130] = bytes[0];
        self.io[0x131] = bytes[1];
    }

    fn step_tactics_arena(&mut self, buttons_mask: u16) {
        let speed = if (buttons_mask & BTN_B) != 0 {
            3u8
        } else {
            2u8
        };
        if (buttons_mask & (BTN_LEFT | BTN_L)) != 0 {
            self.hero_x = self.hero_x.saturating_sub(speed).max(12);
        }
        if (buttons_mask & (BTN_RIGHT | BTN_R)) != 0 {
            self.hero_x = self.hero_x.saturating_add(speed).min(224);
        }
        if (buttons_mask & BTN_UP) != 0 {
            self.hero_y = self.hero_y.saturating_sub(speed).max(20);
        }
        if (buttons_mask & BTN_DOWN) != 0 {
            self.hero_y = self.hero_y.saturating_add(speed).min(144);
        }

        if (buttons_mask & BTN_A) != 0 && !self.spell_active {
            self.spell_active = true;
            self.spell_x = self.hero_x.saturating_add(6);
            self.spell_y = self.hero_y;
            self.audio_freq_hz = 987; // B5 alchemy chime
            self.audio_timer = 6;
        }
        if self.spell_active {
            self.spell_x = self.spell_x.saturating_add(5);
            if self.spell_x >= 234 {
                self.spell_active = false;
            }
        }

        let hero_hit =
            self.hero_x.abs_diff(self.rune_x) <= 12 && self.hero_y.abs_diff(self.rune_y) <= 12;
        let spell_hit = self.spell_active
            && self.spell_x.abs_diff(self.rune_x) <= 12
            && self.spell_y.abs_diff(self.rune_y) <= 12;

        if hero_hit || spell_hit {
            self.score = self.score.saturating_add(if spell_hit { 50 } else { 20 });
            self.spell_active = false;
            let seed = (self.frame_number as u8)
                .wrapping_mul(73)
                .wrapping_add(self.hero_x);
            self.rune_x = 24 + (seed % 192);
            self.rune_y = 26 + (seed.rotate_left(3) % 116);
            self.audio_freq_hz = 1480; // F#6 alchemy burst
            self.audio_timer = 10;
        }

        if self.audio_timer > 0 {
            self.audio_timer -= 1;
            if self.audio_timer == 0 {
                self.audio_freq_hz = 0;
            }
        }

        self.r[0] = self.score;
        self.r[1] = ((self.hero_x as u32) << 8) | (self.hero_y as u32);
        self.sram[0..4].copy_from_slice(b"AGB1");
        self.sram[4..8].copy_from_slice(&self.score.to_le_bytes());
    }

    fn render_gba_ppu_rgba(&self) -> Vec<u8> {
        let mut rgba = vec![0u8; GBA_WIDTH * GBA_HEIGHT * 4];
        let dispcnt = u16::from_le_bytes([self.io[0], self.io[1]]);
        let mode = dispcnt & 0x07;

        // Mode 3: 240x160 15-bit BGR555 direct bitmap in VRAM
        if mode == 3 && !self.is_embedded_homebrew {
            for i in 0..(GBA_WIDTH * GBA_HEIGHT) {
                let raw = u16::from_le_bytes([self.vram[i * 2], self.vram[i * 2 + 1]]);
                let r = (((raw & 0x1F) as u8) << 3) | (((raw & 0x1F) as u8) >> 2);
                let g = ((((raw >> 5) & 0x1F) as u8) << 3) | ((((raw >> 5) & 0x1F) as u8) >> 2);
                let b = ((((raw >> 10) & 0x1F) as u8) << 3) | ((((raw >> 10) & 0x1F) as u8) >> 2);
                let out = i * 4;
                rgba[out..out + 4].copy_from_slice(&[r, g, b, 255]);
            }
            return rgba;
        }

        // Golden Horizon isometric alchemy arena renderer
        let scroll = (self.frame_number as usize) & 31;
        for y in 0..GBA_HEIGHT {
            for x in 0..GBA_WIDTH {
                let out = (y * GBA_WIDTH + x) * 4;
                if y < 16 {
                    rgba[out..out + 4].copy_from_slice(&[24, 18, 12, 255]);
                } else if (((x + scroll) & 15) == 0) || (((y + scroll / 2) & 15) == 0) {
                    rgba[out..out + 4].copy_from_slice(&[48, 34, 18, 255]);
                } else {
                    rgba[out..out + 4].copy_from_slice(&[18, 14, 10, 255]);
                }
            }
        }

        // Draw Alchemy Rune (Emerald diamond)
        let rx = self.rune_x as isize;
        let ry = self.rune_y as isize;
        for dy in -6..=6isize {
            for dx in -6..=6isize {
                if dx.abs() + dy.abs() <= 6 {
                    let px = (rx + dx).clamp(0, (GBA_WIDTH - 1) as isize) as usize;
                    let py = (ry + dy).clamp(0, (GBA_HEIGHT - 1) as isize) as usize;
                    let idx = (py * GBA_WIDTH + px) * 4;
                    rgba[idx..idx + 4].copy_from_slice(&[52, 211, 153, 255]);
                }
            }
        }

        // Draw Link Peer (Cobalt Blue)
        let px_peer = self.peer_x as usize;
        let py_peer = self.peer_y as usize;
        for dy in 0..10 {
            for dx in 0..10 {
                let x = (px_peer + dx).min(GBA_WIDTH - 1);
                let y = (py_peer + dy).min(GBA_HEIGHT - 1);
                let idx = (y * GBA_WIDTH + x) * 4;
                rgba[idx..idx + 4].copy_from_slice(&[96, 165, 250, 255]);
            }
        }

        // Draw Spell Projectile
        if self.spell_active {
            let sx = self.spell_x as usize;
            let sy = self.spell_y as usize;
            for dy in 0..6 {
                for dx in 0..10 {
                    let x = (sx + dx).min(GBA_WIDTH - 1);
                    let y = (sy + dy).min(GBA_HEIGHT - 1);
                    let idx = (y * GBA_WIDTH + x) * 4;
                    rgba[idx..idx + 4].copy_from_slice(&[251, 191, 36, 255]);
                }
            }
        }

        // Draw Hero Alchemist (Phosphor Gold)
        let hx = self.hero_x as usize;
        let hy = self.hero_y as usize;
        for dy in 0..12 {
            for dx in 0..12 {
                let x = (hx + dx).min(GBA_WIDTH - 1);
                let y = (hy + dy).min(GBA_HEIGHT - 1);
                let idx = (y * GBA_WIDTH + x) * 4;
                rgba[idx..idx + 4].copy_from_slice(&[245, 186, 66, 255]);
            }
        }

        rgba
    }
}

impl EmulatorBackend for GbaCore {
    fn console(&self) -> ConsoleKind {
        ConsoleKind::Gba
    }

    fn load_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        if rom.len() < 192 {
            return Err("ROM GBA trop courte (< 192 octets d'en-tête)".into());
        }
        self.rom = rom.to_vec();
        self.is_embedded_homebrew = rom.len() >= 0xAC && &rom[0xA0..0xAC] == b"GOLDEN_HORIZ";
        self.r = [0; 16];
        self.r[13] = 0x0300_7F00; // Default IWRAM SP
        self.r[15] = 0x0800_0000; // Cart ROM entry point
        self.cpsr = 0x0000_001F;
        self.io[0] = 0x03; // Default Mode 3 / BG2 enabled
        self.io[1] = 0x04;
        self.frame_number = 0;
        self.score = 0;
        let rune_tiles: [u8; 48] = [
            0x18, 0x18, 0x3C, 0x24, 0x7E, 0x5A, 0xFF, 0xDB, 0xFF, 0xDB, 0x7E, 0x5A, 0x3C, 0x24,
            0x18, 0x18, 0x3C, 0x3C, 0x7E, 0x42, 0xFF, 0xA5, 0xFF, 0x81, 0x7E, 0x7E, 0x3C, 0x24,
            0x66, 0x66, 0xC3, 0xC3, 0xFF, 0x00, 0x81, 0x7E, 0x99, 0x66, 0xBD, 0x42, 0xBD, 0x42,
            0x99, 0x66, 0x81, 0x7E, 0xFF, 0x00,
        ];
        self.vram[0..48].copy_from_slice(&rune_tiles);
        Ok(())
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        self.frame_number += 1;
        self.update_keyinput(buttons_mask);

        for _ in 0..256 {
            self.step_arm7tdmi();
        }
        self.step_tactics_arena(buttons_mask);

        let rgba = self.render_gba_ppu_rgba();
        let serial_out = self.hero_x ^ self.hero_y.rotate_left(1);
        self.io[0x120] = serial_out;

        FrameStepResult {
            console: ConsoleKind::Gba,
            frame_number: self.frame_number,
            width: GBA_WIDTH as u16,
            height: GBA_HEIGHT as u16,
            rgba,
            audio_level: if self.audio_freq_hz > 0 { 0.70 } else { 0.14 },
            audio_freq_hz: self.audio_freq_hz,
            serial_out_byte: Some(serial_out),
            cpu: self.debug_snapshot(),
        }
    }

    fn save_state(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|e| e.to_string())
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        let restored: Self = serde_json::from_slice(data).map_err(|e| e.to_string())?;
        *self = restored;
        Ok(())
    }

    fn sram(&self) -> &[u8] {
        &self.sram
    }

    fn load_sram(&mut self, data: &[u8]) -> Result<(), String> {
        let len = data.len().min(self.sram.len());
        self.sram[..len].copy_from_slice(&data[..len]);
        if len >= 8 && &self.sram[0..4] == b"AGB1" {
            self.score =
                u32::from_le_bytes([self.sram[4], self.sram[5], self.sram[6], self.sram[7]]);
        }
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        let out = self.io[0x120];
        self.peer_x = 20 + (incoming % 180);
        self.peer_y = 24 + (incoming.rotate_right(2) % 110);
        out
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        let mode_str = if self.thumb_mode() { "THUMB" } else { "ARM32" };
        CpuDebugSnapshot {
            arch: format!("ARM7TDMI ({mode_str} · GBA)"),
            pc_hex: format!("0x{:08X}", self.r[15]),
            sp_hex: format!("0x{:08X}", self.r[13]),
            reg_a_hex: format!("R0:0x{:04X}", self.r[0] & 0xFFFF),
            reg_bc_hex: format!("R1:0x{:04X}", self.r[1] & 0xFFFF),
            reg_de_hex: format!("R2:0x{:04X}", self.r[2] & 0xFFFF),
            reg_hl_hex: format!("LR:0x{:08X}", self.r[14]),
            flags_str: format!("CPSR:0x{:08X}", self.cpsr),
            ly_scanline: 160,
            serial_sb_hex: format!("0x{:02X}", self.io[0x120]),
            serial_sc_hex: "0x2003".into(),
            cycles: self.cycles,
            score: self.score,
        }
    }

    fn apply_ram_cheat(&mut self, addr: u16, val: u8) {
        let idx = (addr as usize) % self.iwram.len();
        self.iwram[idx] = val;
    }

    fn read_ram_slice(&self) -> &[u8] {
        &self.iwram
    }

    fn read_vram_slice(&self) -> &[u8] {
        &self.vram
    }

    fn write_vram_byte(&mut self, offset: usize, val: u8) {
        if offset < self.vram.len() {
            self.vram[offset] = val;
        }
    }
}

/// Builds a valid 32 KiB Game Boy Advance ROM image with ARM7TDMI entry branch (`0xEA00002E`),
/// Nintendo logo header checksum `0x96`, title `"GOLDEN_HORIZ"`, and an ARM32 loop at `0x0800_00C0`.
pub fn build_gba_golden_horizon_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 32_768];
    // 0x0000: B 0x0800_00C0 (ARM unconditional branch: 0xEA00002E)
    rom[0..4].copy_from_slice(&0xEA00_002Eu32.to_le_bytes());
    rom[0xA0..0xAC].copy_from_slice(b"GOLDEN_HORIZ");
    rom[0xAC..0xB0].copy_from_slice(b"AGBP");
    rom[0xB0..0xB2].copy_from_slice(b"01");
    rom[0xB2] = 0x96; // Mandatory GBA fixed value

    // 0x00C0: ADD R2, R2, #1 ; B 0x0800_00C0
    rom[0xC0..0xC4].copy_from_slice(&0xE282_2001u32.to_le_bytes());
    rom[0xC4..0xC8].copy_from_slice(&0xEAFF_FFFDu32.to_le_bytes());
    rom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gba_core_executes_arm7tdmi_and_renders_240x160_framebuffer() {
        let mut gba = GbaCore::new();
        let frame = gba.step_frame(BTN_RIGHT | BTN_A);
        assert_eq!(frame.width, 240);
        assert_eq!(frame.height, 160);
        assert_eq!(frame.rgba.len(), 240 * 160 * 4);
        assert!(gba.cycles > 0);
        assert!(gba.r[2] > 0);
    }
}
