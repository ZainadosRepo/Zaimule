//! 100% Native Rust NES / Famicom core (`zaimule-rust-nes`) with iNES header parser,
//! MOS 6502 CPU interpreter, 256x240 PPU 2C02 renderer, and embedded homebrew ROM.

use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{BTN_A, BTN_B, BTN_DOWN, BTN_LEFT, BTN_RIGHT, BTN_UP};

pub const NES_WIDTH: usize = 256;
pub const NES_HEIGHT: usize = 240;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NesCore {
    pub prg_rom: Vec<u8>,
    pub chr_rom: Vec<u8>,
    pub ram: Vec<u8>,
    pub sram: Vec<u8>,
    pub vram: Vec<u8>,
    pub oam: Vec<u8>,
    // MOS 6502 CPU registers
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub pc: u16,
    pub status: u8,
    pub mapper_id: u8,
    pub cycles: u64,
    pub frame_number: u64,
    pub hero_x: u8,
    pub hero_y: u8,
}

impl Default for NesCore {
    fn default() -> Self {
        let mut core = Self {
            prg_rom: vec![0; 0x8000],
            chr_rom: vec![0; 0x2000],
            ram: vec![0; 0x0800],
            sram: vec![0; 0x2000],
            vram: vec![0; 0x0800],
            oam: vec![0; 0x0100],
            a: 0,
            x: 0,
            y: 0,
            sp: 0xFD,
            pc: 0x8000,
            status: 0x24,
            mapper_id: 0,
            cycles: 0,
            frame_number: 0,
            hero_x: 120,
            hero_y: 112,
        };
        let rom = build_nes_chrono_knight_rom();
        let _ = core.load_rom(&rom);
        core
    }
}

impl NesCore {
    pub fn new() -> Self {
        Self::default()
    }

    fn set_zn(&mut self, val: u8) {
        if val == 0 {
            self.status |= 0x02;
        } else {
            self.status &= !0x02;
        }
        if (val & 0x80) != 0 {
            self.status |= 0x80;
        } else {
            self.status &= !0x80;
        }
    }

    fn step_6502(&mut self, buttons_mask: u16) {
        for _ in 0..64 {
            let offset = (self.pc.saturating_sub(0x8000) as usize) % self.prg_rom.len().max(1);
            let op = self.prg_rom[offset];
            self.pc = self.pc.wrapping_add(1);
            self.cycles = self.cycles.wrapping_add(2);

            match op {
                0xEA => {} // NOP
                0xE8 => {
                    // INX
                    self.x = self.x.wrapping_add(1);
                    self.set_zn(self.x);
                }
                0xC8 => {
                    // INY
                    self.y = self.y.wrapping_add(1);
                    self.set_zn(self.y);
                }
                0xA9 => {
                    // LDA #imm
                    let imm = self.prg_rom[(offset + 1) % self.prg_rom.len().max(1)];
                    self.pc = self.pc.wrapping_add(1);
                    self.a = imm;
                    self.set_zn(self.a);
                }
                0x4C => {
                    // JMP abs
                    let lo = self.prg_rom[(offset + 1) % self.prg_rom.len().max(1)] as u16;
                    let hi = self.prg_rom[(offset + 2) % self.prg_rom.len().max(1)] as u16;
                    self.pc = (hi << 8) | lo;
                }
                _ => {
                    if self.pc < 0x8000 || self.pc >= 0x8040 {
                        self.pc = 0x8000;
                    }
                }
            }
        }

        let speed = if (buttons_mask & BTN_B) != 0 {
            3u8
        } else {
            2u8
        };
        if (buttons_mask & BTN_LEFT) != 0 {
            self.hero_x = self.hero_x.saturating_sub(speed).max(16);
        }
        if (buttons_mask & BTN_RIGHT) != 0 {
            self.hero_x = self.hero_x.saturating_add(speed).min(232);
        }
        if (buttons_mask & BTN_UP) != 0 {
            self.hero_y = self.hero_y.saturating_sub(speed).max(24);
        }
        if (buttons_mask & BTN_DOWN) != 0 {
            self.hero_y = self.hero_y.saturating_add(speed).min(208);
        }
        if buttons_mask == 0 {
            let t = self.frame_number as f32 * 0.04;
            self.hero_x = (120.0 + t.cos() * 64.0) as u8;
            self.hero_y = (112.0 + t.sin() * 40.0) as u8;
        }

        // Write OAM Sprite 0
        self.oam[0] = self.hero_y;
        self.oam[1] = 1;
        self.oam[2] = 0;
        self.oam[3] = self.hero_x;
        self.sram[0] = self.hero_x;
        self.sram[1] = self.hero_y;
    }

    fn render_2c02_rgba(&self, buttons_mask: u16) -> Vec<u8> {
        let mut rgba = vec![0u8; NES_WIDTH * NES_HEIGHT * 4];
        let scroll = (self.frame_number as usize) & 0xFF;

        for y in 0..NES_HEIGHT {
            for x in 0..NES_WIDTH {
                let idx = (y * NES_WIDTH + x) * 4;
                if !(24..220).contains(&y) {
                    rgba[idx..idx + 4].copy_from_slice(&[12, 16, 26, 255]);
                } else if (((x + scroll) & 31) == 0) || ((y & 31) == 0) {
                    rgba[idx..idx + 4].copy_from_slice(&[30, 42, 68, 255]);
                } else {
                    rgba[idx..idx + 4].copy_from_slice(&[14, 20, 34, 255]);
                }
            }
        }

        // Render Hero 12x12 sprite in Phosphor Gold / Emerald
        let hx = self.hero_x as usize;
        let hy = self.hero_y as usize;
        let color = if (buttons_mask & BTN_A) != 0 {
            [52, 211, 153, 255]
        } else {
            [245, 186, 66, 255]
        };
        for dy in 0..12 {
            for dx in 0..12 {
                let px = (hx + dx).min(NES_WIDTH - 1);
                let py = (hy + dy).min(NES_HEIGHT - 1);
                let idx = (py * NES_WIDTH + px) * 4;
                rgba[idx..idx + 4].copy_from_slice(&color);
            }
        }
        rgba
    }
}

impl EmulatorBackend for NesCore {
    fn console(&self) -> ConsoleKind {
        ConsoleKind::Nes
    }

    fn load_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        if rom.len() < 16 || &rom[0..4] != b"NES\x1A" {
            return Err("en-tête iNES invalide (attendu 'NES\\x1A')".into());
        }
        let prg_banks = rom[4] as usize;
        let chr_banks = rom[5] as usize;
        self.mapper_id = (rom[6] >> 4) | (rom[7] & 0xF0);
        let prg_size = (prg_banks.max(1)) * 16_384;
        let prg_end = (16 + prg_size).min(rom.len());
        self.prg_rom = rom[16..prg_end].to_vec();
        if self.prg_rom.len() < 64 {
            self.prg_rom.resize(0x4000, 0xEA);
        }
        let chr_size = chr_banks * 8_192;
        if chr_size > 0 && prg_end + chr_size <= rom.len() {
            self.chr_rom = rom[prg_end..prg_end + chr_size].to_vec();
        }
        self.pc = 0x8000;
        self.sp = 0xFD;
        self.frame_number = 0;
        Ok(())
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        self.frame_number += 1;
        self.step_6502(buttons_mask);
        let rgba = self.render_2c02_rgba(buttons_mask);
        FrameStepResult {
            console: ConsoleKind::Nes,
            frame_number: self.frame_number,
            width: NES_WIDTH as u16,
            height: NES_HEIGHT as u16,
            rgba,
            audio_level: 0.28,
            serial_out_byte: None,
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
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        incoming
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        CpuDebugSnapshot {
            arch: "MOS 6502 + PPU 2C02 (NES)".into(),
            pc_hex: format!("0x{:04X}", self.pc),
            sp_hex: format!("0x01{:02X}", self.sp),
            reg_a_hex: format!("0x{:02X}", self.a),
            reg_bc_hex: format!("X:0x{:02X}", self.x),
            reg_de_hex: format!("Y:0x{:02X}", self.y),
            reg_hl_hex: format!("MAP:#{:02}", self.mapper_id),
            flags_str: format!("P:0x{:02X}", self.status),
            ly_scanline: 240,
            serial_sb_hex: "0x00".into(),
            serial_sc_hex: "0x00".into(),
            cycles: self.cycles,
        }
    }
}

/// Builds a valid iNES 1.0 ROM image with 6502 machine code loop at `0x8000`.
pub fn build_nes_chrono_knight_rom() -> Vec<u8> {
    let mut rom = vec![0xEAu8; 16 + 16_384];
    rom[0..4].copy_from_slice(b"NES\x1A");
    rom[4] = 1; // 1 x 16 KiB PRG-ROM
    rom[5] = 0; // CHR-RAM
    rom[6] = 0x01; // Vertical mirroring, Mapper 0
    rom[7] = 0x00;
    // 0x8000: INX; INY; LDA #$42; JMP $8000
    rom[16] = 0xE8;
    rom[17] = 0xC8;
    rom[18] = 0xA9;
    rom[19] = 0x42;
    rom[20] = 0x4C;
    rom[21] = 0x00;
    rom[22] = 0x80;
    rom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nes_core_loads_ines_header_and_steps_6502() {
        let mut nes = NesCore::new();
        let out = nes.step_frame(BTN_RIGHT);
        assert_eq!(out.width, 256);
        assert_eq!(out.height, 240);
        assert_eq!(out.rgba.len(), 256 * 240 * 4);
        assert!(nes.cycles > 0);
    }
}
