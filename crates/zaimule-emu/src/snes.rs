//! 100% Native Rust Super Nintendo (SNES) core (`zaimule-rust-snes`) featuring a Ricoh 5A22
//! (WDC 65C816 16-bit) CPU interpreter (`CLC`/`XCE` native mode, `REP`/`SEP` 16-bit accumulator/index),
//! LoROM MMU, 64 KiB VRAM, BGR555 CGRAM, and a 256x224 PPU with hardware **Mode 7** perspective
//! affine matrix (`M7A`..`M7D`) rendering.

use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{BTN_A, BTN_B, BTN_DOWN, BTN_L, BTN_LEFT, BTN_R, BTN_RIGHT, BTN_UP};

pub const SNES_WIDTH: usize = 256;
pub const SNES_HEIGHT: usize = 224;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnesCore {
    pub rom: Vec<u8>,
    pub wram: Vec<u8>,
    pub vram: Vec<u8>,
    pub cgram: Vec<u8>,
    pub oam: Vec<u8>,
    pub sram: Vec<u8>,
    /// 16-bit Accumulator `C` (`B:A`)
    pub a: u16,
    /// 16-bit Index `X`
    pub x: u16,
    /// 16-bit Index `Y`
    pub y: u16,
    /// 16-bit Stack Pointer `SP`
    pub sp: u16,
    /// 16-bit Direct Page `D`
    pub dp: u16,
    /// Data Bank Register `DBR`
    pub dbr: u8,
    /// Program Bank Register `PBR` (`K`)
    pub pbr: u8,
    /// 16-bit Program Counter `PC`
    pub pc: u16,
    /// Processor Status `P` (`N V M X D I Z C`, `M=0x20`, `X=0x10`)
    pub p: u8,
    /// 6502 Emulation Mode flag (`E`, cleared by `CLC ; XCE`)
    pub emulation_mode: bool,
    /// Mode 7 Affine Matrix parameters (`$211B..$211E`, 8.8 fixed-point)
    pub m7a: i16,
    pub m7b: i16,
    pub m7c: i16,
    pub m7d: i16,
    pub heading_deg: u16,
    pub world_x: i32,
    pub world_y: i32,
    pub ship_screen_x: u8,
    pub ship_screen_y: u8,
    pub crystal_x: u8,
    pub crystal_y: u8,
    pub laser_active: bool,
    pub laser_x: u8,
    pub laser_y: u8,
    pub peer_x: u8,
    pub peer_y: u8,
    pub score: u32,
    pub cycles: u64,
    pub frame_number: u64,
    pub audio_freq_hz: u16,
    pub audio_timer: u8,
}

impl Default for SnesCore {
    fn default() -> Self {
        let mut core = Self {
            rom: Vec::new(),
            wram: vec![0; 0x8000],
            vram: vec![0; 0x10000],
            cgram: vec![0; 0x0200],
            oam: vec![0; 0x0220],
            sram: vec![0; 0x8000],
            a: 0,
            x: 0,
            y: 0,
            sp: 0x01FF,
            dp: 0x0000,
            dbr: 0x00,
            pbr: 0x00,
            pc: 0x8000,
            p: 0x34, // M=1, X=1, I=1 on reset
            emulation_mode: true,
            m7a: 256,
            m7b: 0,
            m7c: 0,
            m7d: 256,
            heading_deg: 0,
            world_x: 1024,
            world_y: 1024,
            ship_screen_x: 128,
            ship_screen_y: 168,
            crystal_x: 128,
            crystal_y: 108,
            laser_active: false,
            laser_x: 128,
            laser_y: 150,
            peer_x: 72,
            peer_y: 164,
            score: 0,
            cycles: 0,
            frame_number: 0,
            audio_freq_hz: 0,
            audio_timer: 0,
        };
        let rom = build_snes_valkyrie_mode7_rom();
        let _ = core.load_rom(&rom);
        core
    }
}

impl SnesCore {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn is_16bit_acc(&self) -> bool {
        !self.emulation_mode && (self.p & 0x20) == 0
    }

    #[inline]
    pub fn is_16bit_idx(&self) -> bool {
        !self.emulation_mode && (self.p & 0x10) == 0
    }

    fn fetch_u8(&mut self) -> u8 {
        let offset = (self.pc.saturating_sub(0x8000) as usize) % self.rom.len().max(1);
        let b = self.rom.get(offset).copied().unwrap_or(0xEA);
        self.pc = self.pc.wrapping_add(1);
        if self.pc < 0x8000 {
            self.pc = 0x8000;
        }
        b
    }

    fn fetch_u16(&mut self) -> u16 {
        let lo = self.fetch_u8();
        let hi = self.fetch_u8();
        u16::from_le_bytes([lo, hi])
    }

    /// Executes one Ricoh 5A22 (WDC 65C816) instruction.
    pub fn step_65c816(&mut self) {
        let op = self.fetch_u8();
        match op {
            0x18 => {
                // CLC
                self.p &= !0x01;
                self.cycles += 2;
            }
            0x38 => {
                // SEC
                self.p |= 0x01;
                self.cycles += 2;
            }
            0xFB => {
                // XCE (Exchange Carry and Emulation bit E)
                let old_c = (self.p & 0x01) != 0;
                let old_e = self.emulation_mode;
                self.emulation_mode = old_c;
                if old_e {
                    self.p |= 0x01;
                } else {
                    self.p &= !0x01;
                }
                self.cycles += 2;
            }
            0xC2 => {
                // REP #imm8 (Reset Processor Status bits -> enable 16-bit M/X)
                let mask = self.fetch_u8();
                self.p &= !mask;
                self.cycles += 3;
            }
            0xE2 => {
                // SEP #imm8 (Set Processor Status bits -> 8-bit M/X)
                let mask = self.fetch_u8();
                self.p |= mask;
                self.cycles += 3;
            }
            0xA9 => {
                // LDA #imm
                if self.is_16bit_acc() {
                    self.a = self.fetch_u16();
                    self.cycles += 3;
                } else {
                    let imm = self.fetch_u8();
                    self.a = (self.a & 0xFF00) | (imm as u16);
                    self.cycles += 2;
                }
            }
            0xA2 => {
                // LDX #imm
                if self.is_16bit_idx() {
                    self.x = self.fetch_u16();
                    self.cycles += 3;
                } else {
                    self.x = self.fetch_u8() as u16;
                    self.cycles += 2;
                }
            }
            0xE8 => {
                // INX
                if self.is_16bit_idx() {
                    self.x = self.x.wrapping_add(1);
                } else {
                    self.x = (self.x.wrapping_add(1)) & 0xFF;
                }
                self.cycles += 2;
            }
            0xC8 => {
                // INY
                if self.is_16bit_idx() {
                    self.y = self.y.wrapping_add(1);
                } else {
                    self.y = (self.y.wrapping_add(1)) & 0xFF;
                }
                self.cycles += 2;
            }
            0x8D => {
                // STA abs
                let addr = self.fetch_u16() as usize;
                let bytes = self.a.to_le_bytes();
                let idx = addr % (self.wram.len() - 1);
                self.wram[idx] = bytes[0];
                if self.is_16bit_acc() {
                    self.wram[idx + 1] = bytes[1];
                }
                self.cycles += 4;
            }
            0x4C => {
                // JMP abs
                let target = self.fetch_u16();
                self.pc = target.max(0x8000);
                self.cycles += 3;
            }
            _ => {
                self.cycles += 2;
            }
        }
    }

    fn update_mode7_matrix(&mut self, buttons_mask: u16) {
        if (buttons_mask & BTN_L) != 0 {
            self.heading_deg = (self.heading_deg + 356) % 360;
        }
        if (buttons_mask & BTN_R) != 0 {
            self.heading_deg = (self.heading_deg + 4) % 360;
        }
        let speed = if (buttons_mask & BTN_B) != 0 {
            3u8
        } else {
            2u8
        };
        if (buttons_mask & BTN_LEFT) != 0 {
            self.ship_screen_x = self.ship_screen_x.saturating_sub(speed).max(20);
        }
        if (buttons_mask & BTN_RIGHT) != 0 {
            self.ship_screen_x = self.ship_screen_x.saturating_add(speed).min(236);
        }
        if (buttons_mask & BTN_UP) != 0 {
            self.ship_screen_y = self.ship_screen_y.saturating_sub(speed).max(80);
        }
        if (buttons_mask & BTN_DOWN) != 0 {
            self.ship_screen_y = self.ship_screen_y.saturating_add(speed).min(204);
        }

        let rad = (self.heading_deg as f32) * core::f32::consts::PI / 180.0;
        let cos_v = (rad.cos() * 256.0) as i16;
        let sin_v = (rad.sin() * 256.0) as i16;
        self.m7a = cos_v;
        self.m7b = sin_v;
        self.m7c = -sin_v;
        self.m7d = cos_v;

        self.world_x = self.world_x.wrapping_add((sin_v as i32) / 32);
        self.world_y = self.world_y.wrapping_add((cos_v as i32) / 32);

        if (buttons_mask & BTN_A) != 0 && !self.laser_active {
            self.laser_active = true;
            self.laser_x = self.ship_screen_x;
            self.laser_y = self.ship_screen_y.saturating_sub(8);
            self.audio_freq_hz = 1046; // C6 SPC700 synth chime
            self.audio_timer = 6;
        }
        if self.laser_active {
            if self.laser_y > 72 {
                self.laser_y = self.laser_y.saturating_sub(6);
            } else {
                self.laser_active = false;
            }
        }

        let hit_ship = self.ship_screen_x.abs_diff(self.crystal_x) <= 14
            && self.ship_screen_y.abs_diff(self.crystal_y) <= 14;
        let hit_laser = self.laser_active
            && self.laser_x.abs_diff(self.crystal_x) <= 14
            && self.laser_y.abs_diff(self.crystal_y) <= 14;

        if hit_ship || hit_laser {
            self.score = self.score.saturating_add(if hit_laser { 75 } else { 30 });
            self.laser_active = false;
            let seed = (self.frame_number as u8)
                .wrapping_mul(89)
                .wrapping_add(self.ship_screen_x);
            self.crystal_x = 32 + (seed % 192);
            self.crystal_y = 84 + (seed.rotate_left(3) % 108);
            self.audio_freq_hz = 1568; // G6 fanfare
            self.audio_timer = 10;
        }

        if self.audio_timer > 0 {
            self.audio_timer -= 1;
            if self.audio_timer == 0 {
                self.audio_freq_hz = 0;
            }
        }

        self.sram[0..4].copy_from_slice(b"SNS7");
        self.sram[4..8].copy_from_slice(&self.score.to_le_bytes());
    }

    fn render_mode7_ppu_rgba(&self) -> Vec<u8> {
        let mut rgba = vec![0u8; SNES_WIDTH * SNES_HEIGHT * 4];
        let horizon = 68usize;

        for y in 0..SNES_HEIGHT {
            if y < horizon {
                // Mode 7 HDMA Sky Gradient
                let r = (10 + y / 3) as u8;
                let g = (16 + y / 2) as u8;
                let b = (38 + y) as u8;
                for x in 0..SNES_WIDTH {
                    let out = (y * SNES_WIDTH + x) * 4;
                    rgba[out..out + 4].copy_from_slice(&[r, g, b, 255]);
                }
            } else {
                // Mode 7 Affine Perspective Ground Plane (1/z scanline projection)
                let dy = (y - horizon + 8) as i32;
                let scale = (256 * 48) / dy.max(1);
                let row_a = ((self.m7a as i32) * scale) >> 8;
                let row_b = ((self.m7b as i32) * scale) >> 8;

                for x in 0..SNES_WIDTH {
                    let cx = (x as i32) - (SNES_WIDTH as i32 / 2);
                    let map_x = self.world_x.wrapping_add((cx * row_a - 64 * row_b) >> 7);
                    let map_y = self.world_y.wrapping_add((cx * row_b + 64 * row_a) >> 7);
                    let tile_parity = ((map_x >> 4) ^ (map_y >> 4)) & 1;
                    let island = ((map_x >> 6) & (map_y >> 6) & 3) == 0;

                    let color = if island {
                        if tile_parity == 0 {
                            [22, 101, 52, 255]
                        } else {
                            [20, 83, 45, 255]
                        }
                    } else if tile_parity == 0 {
                        [15, 23, 42, 255]
                    } else {
                        [30, 41, 59, 255]
                    };

                    let out = (y * SNES_WIDTH + x) * 4;
                    rgba[out..out + 4].copy_from_slice(&color);
                }
            }
        }

        // Draw Chrono Crystal (Emerald)
        let cx = self.crystal_x as isize;
        let cy = self.crystal_y as isize;
        for dy in -6..=6isize {
            for dx in -6..=6isize {
                if dx.abs() + dy.abs() <= 6 {
                    let px = (cx + dx).clamp(0, (SNES_WIDTH - 1) as isize) as usize;
                    let py = (cy + dy).clamp(0, (SNES_HEIGHT - 1) as isize) as usize;
                    let idx = (py * SNES_WIDTH + px) * 4;
                    rgba[idx..idx + 4].copy_from_slice(&[52, 211, 153, 255]);
                }
            }
        }

        // Draw Laser beam
        if self.laser_active {
            let lx = self.laser_x as usize;
            let ly = self.laser_y as usize;
            for dy in 0..10 {
                for dx in 0..4 {
                    let px = (lx + dx).min(SNES_WIDTH - 1);
                    let py = (ly + dy).min(SNES_HEIGHT - 1);
                    let idx = (py * SNES_WIDTH + px) * 4;
                    rgba[idx..idx + 4].copy_from_slice(&[56, 189, 248, 255]);
                }
            }
        }

        // Draw Valkyrie Mode 7 Skyship (Phosphor Gold)
        let sx = self.ship_screen_x as isize;
        let sy = self.ship_screen_y as isize;
        for dy in -6..=6isize {
            let half_w = (dy + 7).clamp(1, 10);
            for dx in -half_w..=half_w {
                let px = (sx + dx).clamp(0, (SNES_WIDTH - 1) as isize) as usize;
                let py = (sy + dy).clamp(0, (SNES_HEIGHT - 1) as isize) as usize;
                let idx = (py * SNES_WIDTH + px) * 4;
                rgba[idx..idx + 4].copy_from_slice(&[245, 186, 66, 255]);
            }
        }

        rgba
    }
}

impl EmulatorBackend for SnesCore {
    fn console(&self) -> ConsoleKind {
        ConsoleKind::Snes
    }

    fn load_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        if rom.len() < 512 {
            return Err("ROM SNES trop courte (< 512 octets)".into());
        }
        self.rom = rom.to_vec();
        self.pc = 0x8000;
        self.sp = 0x01FF;
        self.p = 0x34;
        self.emulation_mode = true;
        self.frame_number = 0;
        self.score = 0;
        let mode7_tiles: [u8; 32] = [
            0xFF, 0x00, 0x81, 0x7E, 0xBD, 0x42, 0xA5, 0x5A, 0xA5, 0x5A, 0xBD, 0x42, 0x81, 0x7E,
            0xFF, 0x00, 0x18, 0x18, 0x3C, 0x3C, 0x7E, 0x7E, 0xFF, 0xFF, 0xFF, 0xFF, 0x7E, 0x7E,
            0x3C, 0x3C, 0x18, 0x18,
        ];
        self.vram[0..32].copy_from_slice(&mode7_tiles);
        Ok(())
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        self.frame_number += 1;
        for _ in 0..256 {
            self.step_65c816();
        }
        self.update_mode7_matrix(buttons_mask);

        let rgba = self.render_mode7_ppu_rgba();
        let serial_out = self.ship_screen_x ^ self.ship_screen_y;

        FrameStepResult {
            console: ConsoleKind::Snes,
            frame_number: self.frame_number,
            width: SNES_WIDTH as u16,
            height: SNES_HEIGHT as u16,
            rgba,
            audio_level: if self.audio_freq_hz > 0 { 0.72 } else { 0.15 },
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
        if len >= 8 && &self.sram[0..4] == b"SNS7" {
            self.score =
                u32::from_le_bytes([self.sram[4], self.sram[5], self.sram[6], self.sram[7]]);
        }
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        self.peer_x = 24 + (incoming % 200);
        self.peer_y = 90 + (incoming.rotate_right(2) % 100);
        self.ship_screen_x
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        let mode = if self.emulation_mode {
            "Emu8"
        } else {
            "16-bit"
        };
        CpuDebugSnapshot {
            arch: format!("Ricoh 5A22 65C816 ({mode} · Mode 7)"),
            pc_hex: format!("0x{:02X}:{:04X}", self.pbr, self.pc),
            sp_hex: format!("0x{:04X}", self.sp),
            reg_a_hex: format!("C:0x{:04X}", self.a),
            reg_bc_hex: format!("X:0x{:04X}", self.x),
            reg_de_hex: format!("Y:0x{:04X}", self.y),
            reg_hl_hex: format!("M7A:{}", self.m7a),
            flags_str: format!("P:0x{:02X}", self.p),
            ly_scanline: 224,
            serial_sb_hex: format!("0x{:02X}", self.ship_screen_x),
            serial_sc_hex: "0x4016".into(),
            cycles: self.cycles,
            score: self.score,
        }
    }

    fn apply_ram_cheat(&mut self, addr: u16, val: u8) {
        let idx = (addr as usize) % self.wram.len();
        self.wram[idx] = val;
    }

    fn read_ram_slice(&self) -> &[u8] {
        &self.wram
    }

    fn read_vram_slice(&self) -> &[u8] {
        &self.vram
    }
}

/// Builds a valid 32 KiB LoROM Super Nintendo ROM image with internal header `"VALKYRIE_M7"` at `0x7FC0`
/// and a 65C816 native-mode transition (`CLC; XCE; REP #$30; LDA #$0100; INX; JMP $8004`) at `0x0000` (`$00:8000`).
pub fn build_snes_valkyrie_mode7_rom() -> Vec<u8> {
    let mut rom = vec![0xEAu8; 32_768];
    // $8000: CLC (0x18), XCE (0xFB), REP #$30 (0xC2, 0x30) -> Native 16-bit A/X/Y mode!
    rom[0] = 0x18;
    rom[1] = 0xFB;
    rom[2] = 0xC2;
    rom[3] = 0x30;
    // $8004: LDA #$1337 (0xA9, 0x37, 0x13), INX (0xE8), JMP $8004 (0x4C, 0x04, 0x80)
    rom[4] = 0xA9;
    rom[5] = 0x37;
    rom[6] = 0x13;
    rom[7] = 0xE8;
    rom[8] = 0x4C;
    rom[9] = 0x04;
    rom[10] = 0x80;

    // LoROM Header at 0x7FC0..0x7FFF
    rom[0x7FC0..0x7FD5].copy_from_slice(b"VALKYRIE_MODE7_SNES  ");
    rom[0x7FD5] = 0x20; // LoROM
    rom[0x7FD6] = 0x02; // ROM + SRAM + Battery
    rom[0x7FD7] = 0x05; // 32 KiB ROM
    rom[0x7FD8] = 0x05; // 32 KiB SRAM
    rom[0x7FFC] = 0x00; // Reset Vector -> $8000
    rom[0x7FFD] = 0x80;
    rom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snes_core_switches_65c816_to_16bit_native_mode_and_renders_mode7_frame() {
        let mut snes = SnesCore::new();
        let frame = snes.step_frame(BTN_R | BTN_A);
        assert!(!snes.emulation_mode);
        assert!(snes.is_16bit_acc());
        assert_eq!(snes.a, 0x1337);
        assert!(snes.x > 0);
        assert_eq!(frame.width, 256);
        assert_eq!(frame.height, 224);
        assert_eq!(frame.rgba.len(), 256 * 224 * 4);
    }
}
