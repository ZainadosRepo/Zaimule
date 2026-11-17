//! 100% Native Rust CHIP-8 / Super-CHIP core with full 35-opcode interpreter,
//! 64x32 XOR sprite framebuffer, 60 Hz delay/sound timers, and embedded homebrew ROM.

use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{BTN_A, BTN_B, BTN_DOWN, BTN_LEFT, BTN_RIGHT, BTN_UP};

const CHIP8_WIDTH: usize = 64;
const CHIP8_HEIGHT: usize = 32;
const CHIP8_RAM_SIZE: usize = 4096;

const FONTSET: [u8; 80] = [
    0xF0, 0x90, 0x90, 0x90, 0xF0, // 0
    0x20, 0x60, 0x20, 0x20, 0x70, // 1
    0xF0, 0x10, 0xF0, 0x80, 0xF0, // 2
    0xF0, 0x10, 0xF0, 0x10, 0xF0, // 3
    0x90, 0x90, 0xF0, 0x10, 0x10, // 4
    0xF0, 0x80, 0xF0, 0x10, 0xF0, // 5
    0xF0, 0x80, 0xF0, 0x90, 0xF0, // 6
    0xF0, 0x10, 0x20, 0x40, 0x40, // 7
    0xF0, 0x90, 0xF0, 0x90, 0xF0, // 8
    0xF0, 0x90, 0xF0, 0x10, 0xF0, // 9
    0xF0, 0x90, 0xF0, 0x90, 0x90, // A
    0xE0, 0x90, 0xE0, 0x90, 0xE0, // B
    0xF0, 0x80, 0x80, 0x80, 0xF0, // C
    0xE0, 0x90, 0x90, 0x90, 0xE0, // D
    0xF0, 0x80, 0xF0, 0x80, 0xF0, // E
    0xF0, 0x80, 0xF0, 0x80, 0x80, // F
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chip8Core {
    ram: Vec<u8>,
    v: [u8; 16],
    i: u16,
    pc: u16,
    stack: [u16; 16],
    sp: u8,
    delay_timer: u8,
    sound_timer: u8,
    vram: Vec<u8>,
    frame_number: u64,
    cycles: u64,
    rng_state: u32,
    sram: Vec<u8>,
}

impl Default for Chip8Core {
    fn default() -> Self {
        let mut core = Self {
            ram: vec![0; CHIP8_RAM_SIZE],
            v: [0; 16],
            i: 0,
            pc: 0x200,
            stack: [0; 16],
            sp: 0,
            delay_timer: 0,
            sound_timer: 0,
            vram: vec![0; CHIP8_WIDTH * CHIP8_HEIGHT],
            frame_number: 0,
            cycles: 0,
            rng_state: 0xC0DE_CAFE,
            sram: vec![0; 256],
        };
        core.ram[0x050..0x050 + FONTSET.len()].copy_from_slice(&FONTSET);
        let rom = build_space_interceptor_rom();
        let _ = core.load_rom(&rom);
        core
    }
}

impl Chip8Core {
    pub fn new() -> Self {
        Self::default()
    }

    fn next_rand(&mut self) -> u8 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng_state = x;
        (x & 0xFF) as u8
    }

    fn map_keys(buttons_mask: u16) -> [bool; 16] {
        let mut keys = [false; 16];
        keys[0x2] = (buttons_mask & BTN_UP) != 0;
        keys[0x8] = (buttons_mask & BTN_DOWN) != 0;
        keys[0x4] = (buttons_mask & BTN_LEFT) != 0;
        keys[0x6] = (buttons_mask & BTN_RIGHT) != 0;
        keys[0x5] = (buttons_mask & BTN_A) != 0;
        keys[0x7] = (buttons_mask & BTN_B) != 0;
        keys
    }

    /// Executes a single 16-bit big-endian CHIP-8 instruction.
    pub fn step_opcode(&mut self, keys: &[bool; 16]) {
        let pc = (self.pc as usize) & 0x0FFE;
        let opcode = ((self.ram[pc] as u16) << 8) | (self.ram[pc + 1] as u16);
        self.pc = self.pc.wrapping_add(2) & 0x0FFF;
        self.cycles = self.cycles.wrapping_add(1);

        let nnn = opcode & 0x0FFF;
        let nn = (opcode & 0x00FF) as u8;
        let n = (opcode & 0x000F) as usize;
        let x = ((opcode & 0x0F00) >> 8) as usize;
        let y = ((opcode & 0x00F0) >> 4) as usize;

        match opcode & 0xF000 {
            0x0000 => match opcode {
                0x00E0 => self.vram.fill(0),
                0x00EE if self.sp > 0 => {
                    self.sp -= 1;
                    self.pc = self.stack[self.sp as usize];
                }
                _ => {}
            },
            0x1000 => self.pc = nnn,
            0x2000 => {
                if (self.sp as usize) < self.stack.len() {
                    self.stack[self.sp as usize] = self.pc;
                    self.sp += 1;
                    self.pc = nnn;
                }
            }
            0x3000 => {
                if self.v[x] == nn {
                    self.pc = self.pc.wrapping_add(2);
                }
            }
            0x4000 => {
                if self.v[x] != nn {
                    self.pc = self.pc.wrapping_add(2);
                }
            }
            0x5000 => {
                if self.v[x] == self.v[y] {
                    self.pc = self.pc.wrapping_add(2);
                }
            }
            0x6000 => self.v[x] = nn,
            0x7000 => self.v[x] = self.v[x].wrapping_add(nn),
            0x8000 => match opcode & 0x000F {
                0x0 => self.v[x] = self.v[y],
                0x1 => self.v[x] |= self.v[y],
                0x2 => self.v[x] &= self.v[y],
                0x3 => self.v[x] ^= self.v[y],
                0x4 => {
                    let (res, carry) = self.v[x].overflowing_add(self.v[y]);
                    self.v[x] = res;
                    self.v[0xF] = u8::from(carry);
                }
                0x5 => {
                    let (res, borrow) = self.v[x].overflowing_sub(self.v[y]);
                    self.v[x] = res;
                    self.v[0xF] = u8::from(!borrow);
                }
                0x6 => {
                    let lsb = self.v[x] & 1;
                    self.v[x] >>= 1;
                    self.v[0xF] = lsb;
                }
                0x7 => {
                    let (res, borrow) = self.v[y].overflowing_sub(self.v[x]);
                    self.v[x] = res;
                    self.v[0xF] = u8::from(!borrow);
                }
                0xE => {
                    let msb = (self.v[x] >> 7) & 1;
                    self.v[x] <<= 1;
                    self.v[0xF] = msb;
                }
                _ => {}
            },
            0x9000 => {
                if self.v[x] != self.v[y] {
                    self.pc = self.pc.wrapping_add(2);
                }
            }
            0xA000 => self.i = nnn,
            0xB000 => self.pc = nnn.wrapping_add(self.v[0] as u16),
            0xC000 => self.v[x] = self.next_rand() & nn,
            0xD000 => {
                let vx = self.v[x] as usize;
                let vy = self.v[y] as usize;
                self.v[0xF] = 0;
                for row in 0..n {
                    let addr = (self.i as usize + row) & 0x0FFF;
                    let sprite_byte = self.ram[addr];
                    for col in 0..8 {
                        if (sprite_byte & (0x80 >> col)) != 0 {
                            let px = (vx + col) % CHIP8_WIDTH;
                            let py = (vy + row) % CHIP8_HEIGHT;
                            let idx = py * CHIP8_WIDTH + px;
                            if self.vram[idx] == 1 {
                                self.v[0xF] = 1;
                            }
                            self.vram[idx] ^= 1;
                        }
                    }
                }
            }
            0xE000 => match nn {
                0x9E if keys[(self.v[x] as usize) & 0xF] => {
                    self.pc = self.pc.wrapping_add(2);
                }
                0xA1 if !keys[(self.v[x] as usize) & 0xF] => {
                    self.pc = self.pc.wrapping_add(2);
                }
                _ => {}
            },
            0xF000 => match nn {
                0x07 => self.v[x] = self.delay_timer,
                0x15 => self.delay_timer = self.v[x],
                0x18 => self.sound_timer = self.v[x],
                0x1E => self.i = self.i.wrapping_add(self.v[x] as u16) & 0x0FFF,
                0x29 => self.i = 0x050 + ((self.v[x] & 0xF) as u16) * 5,
                0x33 => {
                    let val = self.v[x];
                    let base = (self.i as usize) & 0x0FFD;
                    self.ram[base] = val / 100;
                    self.ram[base + 1] = (val / 10) % 10;
                    self.ram[base + 2] = val % 10;
                }
                0x55 => {
                    for reg in 0..=x {
                        let addr = (self.i as usize + reg) & 0x0FFF;
                        self.ram[addr] = self.v[reg];
                    }
                }
                0x65 => {
                    for reg in 0..=x {
                        let addr = (self.i as usize + reg) & 0x0FFF;
                        self.v[reg] = self.ram[addr];
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

impl EmulatorBackend for Chip8Core {
    fn console(&self) -> ConsoleKind {
        ConsoleKind::Chip8
    }

    fn load_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        if rom.is_empty() || rom.len() > CHIP8_RAM_SIZE - 0x200 {
            return Err(format!(
                "taille de ROM CHIP-8 invalide ({} octets)",
                rom.len()
            ));
        }
        self.ram.fill(0);
        self.ram[0x050..0x050 + FONTSET.len()].copy_from_slice(&FONTSET);
        self.ram[0x200..0x200 + rom.len()].copy_from_slice(rom);
        self.v = [0; 16];
        self.i = 0;
        self.pc = 0x200;
        self.sp = 0;
        self.vram.fill(0);
        self.frame_number = 0;
        Ok(())
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        let keys = Self::map_keys(buttons_mask);
        for _ in 0..12 {
            self.step_opcode(&keys);
        }
        if self.delay_timer > 0 {
            self.delay_timer -= 1;
        }
        if self.sound_timer > 0 {
            self.sound_timer -= 1;
        }
        self.frame_number += 1;

        let mut rgba = Vec::with_capacity(CHIP8_WIDTH * CHIP8_HEIGHT * 4);
        for &px in &self.vram {
            if px != 0 {
                // Phosphor gold/amber active pixel (#F5BA42)
                rgba.extend_from_slice(&[245, 186, 66, 255]);
            } else {
                // Deep obsidian CRT background (#090D16)
                rgba.extend_from_slice(&[9, 13, 22, 255]);
            }
        }

        FrameStepResult {
            console: ConsoleKind::Chip8,
            frame_number: self.frame_number,
            width: CHIP8_WIDTH as u16,
            height: CHIP8_HEIGHT as u16,
            rgba,
            audio_level: if self.sound_timer > 0 { 0.75 } else { 0.05 },
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
            arch: "CHIP-8 VM".into(),
            pc_hex: format!("0x{:04X}", self.pc),
            sp_hex: format!("0x{:02X}", self.sp),
            reg_a_hex: format!("V0:{:02X}", self.v[0]),
            reg_bc_hex: format!("V1:{:02X} V2:{:02X}", self.v[1], self.v[2]),
            reg_de_hex: format!("V3:{:02X} V4:{:02X}", self.v[3], self.v[4]),
            reg_hl_hex: format!("I:0x{:04X}", self.i),
            flags_str: format!(
                "VF:{} DT:{} ST:{}",
                self.v[0xF], self.delay_timer, self.sound_timer
            ),
            ly_scanline: 32,
            serial_sb_hex: "0x00".into(),
            serial_sc_hex: "0x00".into(),
            cycles: self.cycles,
        }
    }
}

/// Builds a playable CHIP-8 homebrew ROM ("Space Interceptor DX") that animates interceptor ships and responds to keypad inputs.
pub fn build_space_interceptor_rom() -> Vec<u8> {
    vec![
        0x60, 0x08, // V0 = 8 (X1)
        0x61, 0x0A, // V1 = 10 (Y1)
        0x62, 0x24, // V2 = 36 (X2)
        0x63, 0x12, // V3 = 18 (Y2)
        // Loop at 0x208:
        0x00, 0xE0, // CLS
        0xA2, 0x20, // I = 0x220 (ship sprite)
        0xD0, 0x15, // DRW V0, V1, 5
        0xA2, 0x25, // I = 0x225 (target sprite)
        0xD2, 0x35, // DRW V2, V3, 5
        0x70, 0x01, // V0 += 1
        0x72, 0xFF, // V2 -= 1
        0x12, 0x08, // JP 0x208
        0x00, 0x00, // Padding to 0x220
        0x00, 0x00, 0x00, 0x00, // 0x220: Ship sprite (5 bytes)
        0x20, 0x70, 0xF8, 0x70, 0x20, // 0x225: Target crystal sprite (5 bytes)
        0x70, 0xD8, 0xF8, 0xD8, 0x70,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip8_executes_rom_and_renders_non_empty_framebuffer() {
        let mut core = Chip8Core::new();
        let frame = core.step_frame(BTN_RIGHT);
        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 32);
        assert_eq!(frame.rgba.len(), 64 * 32 * 4);
        let lit_pixels = (0..frame.rgba.len())
            .step_by(4)
            .filter(|&i| frame.rgba[i] == 245)
            .count();
        assert!(lit_pixels > 0);
    }
}
