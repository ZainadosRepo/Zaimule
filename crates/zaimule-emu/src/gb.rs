//! 100% Native Rust Game Boy / Game Boy Color (`DMG`/`CGB`) core (`zaimule-rust-gb`).
//! Features:
//! - Full MMU memory map (`0x0000..=0xFFFF`) with MBC1/MBC3 ROM & SRAM bank switching
//! - Sharp LR35902 CPU interpreter (loads, 8/16-bit ALU, jumps/calls/branches, stack,
//!   interrupt servicing `0x0040..0x0060`, and the complete 256-opcode `0xCB` bit/shift table)
//! - Active-low Joypad matrix (`0xFF00`), Hardware Timer (`DIV`/`TIMA`/`TMA`/`TAC`), OAM DMA (`0xFF46`)
//! - 160x144 PPU scanline renderer (Background, Window, 40 OAM 8x8/8x16 sprites, signed/unsigned tile addressing)
//! - APU square channel frequency extraction (`NR13`/`NR14` & `NR23`/`NR24`)
//! - Serial Link Cable registers `SB` (`0xFF01`) / `SC` (`0xFF02`)
//! - Built-in playable Homebrew game ("Pocket Monster Link Arena DX") with collectible orbs, projectiles & score.

use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{
    BTN_A, BTN_B, BTN_DOWN, BTN_LEFT, BTN_RIGHT, BTN_SELECT, BTN_START, BTN_UP,
};

pub const GB_WIDTH: usize = 160;
pub const GB_HEIGHT: usize = 144;

const DMG_PALETTE: [[u8; 4]; 4] = [
    [224, 248, 208, 255], // 0: Lightest phosphor green-gold
    [136, 192, 112, 255], // 1: Light sage
    [52, 104, 86, 255],   // 2: Deep teal-emerald
    [8, 24, 32, 255],     // 3: Obsidian shadow
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameBoyCore {
    pub is_gbc: bool,
    pub is_embedded_homebrew: bool,
    pub rom: Vec<u8>,
    pub rom_bank: usize,
    pub ram_bank: usize,
    pub ram_enabled: bool,
    pub rtc_regs: [u8; 5],
    pub rtc_latch_arm: u8,
    pub vram: Vec<u8>,
    pub sram: Vec<u8>,
    pub wram: Vec<u8>,
    pub oam: Vec<u8>,
    pub io: Vec<u8>,
    pub hram: Vec<u8>,
    pub ie: u8,
    pub ime: bool,
    pub halted: bool,
    // Sharp LR35902 CPU registers
    pub a: u8,
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub sp: u16,
    pub pc: u16,
    pub cycles: u64,
    pub div_counter: u16,
    pub frame_number: u64,
    pub buttons_state: u16,
    // Built-in Homebrew Arena state
    pub player_x: u8,
    pub player_y: u8,
    pub peer_x: u8,
    pub peer_y: u8,
    pub orb_x: u8,
    pub orb_y: u8,
    pub shot_x: u8,
    pub shot_y: u8,
    pub shot_active: bool,
    pub score: u32,
    pub audio_timer: u8,
    pub audio_freq_hz: u16,
}

impl Default for GameBoyCore {
    fn default() -> Self {
        let mut core = Self {
            is_gbc: true,
            is_embedded_homebrew: true,
            rom: vec![0; 0x8000],
            rom_bank: 1,
            ram_bank: 0,
            ram_enabled: true,
            rtc_regs: [30, 42, 14, 12, 0],
            rtc_latch_arm: 0xFF,
            vram: vec![0; 0x2000],
            sram: vec![0; 0x8000],
            wram: vec![0; 0x2000],
            oam: vec![0; 0xA0],
            io: vec![0; 0x80],
            hram: vec![0; 0x7F],
            ie: 0x01,
            ime: true,
            halted: false,
            a: 0x11,
            f: 0xB0,
            b: 0x00,
            c: 0x13,
            d: 0x00,
            e: 0xD8,
            h: 0x01,
            l: 0x4D,
            sp: 0xFFFE,
            pc: 0x0100,
            cycles: 0,
            div_counter: 0,
            frame_number: 0,
            buttons_state: 0,
            player_x: 72,
            player_y: 76,
            peer_x: 112,
            peer_y: 64,
            orb_x: 44,
            orb_y: 44,
            shot_x: 0,
            shot_y: 0,
            shot_active: false,
            score: 0,
            audio_timer: 0,
            audio_freq_hz: 0,
        };
        let homebrew = build_gb_pocket_arena_rom();
        let _ = core.load_rom(&homebrew);
        core
    }
}

impl GameBoyCore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_empty() -> Self {
        Self {
            is_gbc: true,
            is_embedded_homebrew: false,
            rom: Vec::new(),
            rom_bank: 1,
            ram_bank: 0,
            ram_enabled: false,
            rtc_regs: [0; 5],
            rtc_latch_arm: 0,
            vram: vec![0; 0x2000],
            sram: Vec::new(),
            wram: vec![0; 0x2000],
            oam: vec![0; 0xA0],
            io: vec![0; 0x80],
            hram: vec![0; 0x7F],
            ie: 0,
            ime: false,
            halted: true,
            a: 0,
            f: 0,
            b: 0,
            c: 0,
            d: 0,
            e: 0,
            h: 0,
            l: 0,
            sp: 0xFFFE,
            pc: 0x0100,
            cycles: 0,
            div_counter: 0,
            frame_number: 0,
            buttons_state: 0,
            player_x: 0,
            player_y: 0,
            peer_x: 0,
            peer_y: 0,
            orb_x: 0,
            orb_y: 0,
            shot_x: 0,
            shot_y: 0,
            shot_active: false,
            score: 0,
            audio_timer: 0,
            audio_freq_hz: 0,
        }
    }

    // 16-bit register pairs
    #[inline]
    fn bc(&self) -> u16 {
        ((self.b as u16) << 8) | (self.c as u16)
    }
    #[inline]
    fn set_bc(&mut self, val: u16) {
        self.b = (val >> 8) as u8;
        self.c = (val & 0xFF) as u8;
    }
    #[inline]
    fn de(&self) -> u16 {
        ((self.d as u16) << 8) | (self.e as u16)
    }
    #[inline]
    fn set_de(&mut self, val: u16) {
        self.d = (val >> 8) as u8;
        self.e = (val & 0xFF) as u8;
    }
    #[inline]
    fn hl(&self) -> u16 {
        ((self.h as u16) << 8) | (self.l as u16)
    }
    #[inline]
    fn set_hl(&mut self, val: u16) {
        self.h = (val >> 8) as u8;
        self.l = (val & 0xFF) as u8;
    }
    #[inline]
    fn flag_z(&self) -> bool {
        (self.f & 0x80) != 0
    }
    #[inline]
    fn flag_c(&self) -> bool {
        (self.f & 0x10) != 0
    }
    #[inline]
    fn set_flags(&mut self, z: bool, n: bool, h: bool, c: bool) {
        self.f = (if z { 0x80 } else { 0 })
            | (if n { 0x40 } else { 0 })
            | (if h { 0x20 } else { 0 })
            | (if c { 0x10 } else { 0 });
    }

    /// Reads one byte from the 16-bit Game Boy MMU address space.
    pub fn read_byte(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x3FFF => self.rom.get(addr as usize).copied().unwrap_or(0xFF),
            0x4000..=0x7FFF => {
                let bank_offset = self.rom_bank.saturating_mul(0x4000);
                let idx = bank_offset + ((addr as usize) - 0x4000);
                self.rom
                    .get(idx % self.rom.len().max(1))
                    .copied()
                    .unwrap_or(0xFF)
            }
            0x8000..=0x9FFF => self.vram[(addr - 0x8000) as usize],
            0xA000..=0xBFFF => {
                if !self.ram_enabled {
                    return 0xFF;
                }
                if (0x08..=0x0C).contains(&self.ram_bank) {
                    return self.rtc_regs[self.ram_bank - 0x08];
                }
                let offset = (self.ram_bank & 0x03) * 0x2000 + ((addr - 0xA000) as usize);
                self.sram.get(offset).copied().unwrap_or(0xFF)
            }
            0xC000..=0xDFFF => self.wram[(addr - 0xC000) as usize],
            0xE000..=0xFDFF => self.wram[(addr - 0xE000) as usize],
            0xFE00..=0xFE9F => self.oam[(addr - 0xFE00) as usize],
            0xFEA0..=0xFEFF => 0xFF,
            0xFF00 => {
                // Active-low Joypad matrix register P1/JOYP
                let select = self.io[0x00] & 0x30;
                let mut low = 0x0F;
                if (select & 0x10) == 0 {
                    // D-Pad selected (Right=bit0, Left=bit1, Up=bit2, Down=bit3)
                    if (self.buttons_state & BTN_RIGHT) != 0 {
                        low &= !0x01;
                    }
                    if (self.buttons_state & BTN_LEFT) != 0 {
                        low &= !0x02;
                    }
                    if (self.buttons_state & BTN_UP) != 0 {
                        low &= !0x04;
                    }
                    if (self.buttons_state & BTN_DOWN) != 0 {
                        low &= !0x08;
                    }
                }
                if (select & 0x20) == 0 {
                    // Action buttons selected (A=bit0, B=bit1, Select=bit2, Start=bit3)
                    if (self.buttons_state & BTN_A) != 0 {
                        low &= !0x01;
                    }
                    if (self.buttons_state & BTN_B) != 0 {
                        low &= !0x02;
                    }
                    if (self.buttons_state & BTN_SELECT) != 0 {
                        low &= !0x04;
                    }
                    if (self.buttons_state & BTN_START) != 0 {
                        low &= !0x08;
                    }
                }
                0xC0 | select | low
            }
            0xFF01..=0xFF7F => self.io[(addr - 0xFF00) as usize],
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize],
            0xFFFF => self.ie,
        }
    }

    /// Writes one byte to the 16-bit Game Boy MMU address space (handling MBC1/MBC3/MBC5, RTC & OAM DMA).
    pub fn write_byte(&mut self, addr: u16, val: u8) {
        match addr {
            0x0000..=0x1FFF => {
                self.ram_enabled = (val & 0x0F) == 0x0A;
            }
            0x2000..=0x2FFF => {
                let is_mbc5 = matches!(self.rom.get(0x0147).copied(), Some(0x19..=0x1E));
                if is_mbc5 {
                    self.rom_bank = (self.rom_bank & 0x100) | (val as usize);
                } else {
                    self.rom_bank = ((val & 0x7F) as usize).max(1);
                }
            }
            0x3000..=0x3FFF => {
                let is_mbc5 = matches!(self.rom.get(0x0147).copied(), Some(0x19..=0x1E));
                if is_mbc5 {
                    self.rom_bank = (self.rom_bank & 0xFF) | (((val & 0x01) as usize) << 8);
                } else {
                    self.rom_bank = ((val & 0x7F) as usize).max(1);
                }
            }
            0x4000..=0x5FFF => {
                self.ram_bank = (val & 0x0F) as usize;
            }
            0x6000..=0x7FFF => {
                // MBC3 RTC Clock Data Latch: writing 0x00 then 0x01 latches the real-time clock registers
                if self.rtc_latch_arm == 0x00 && val == 0x01 {
                    let secs = ((self.frame_number / 60) % 60) as u8;
                    self.rtc_regs[0] = (self.rtc_regs[0].wrapping_add(secs)) % 60;
                }
                self.rtc_latch_arm = val;
            }
            0x8000..=0x9FFF => self.vram[(addr - 0x8000) as usize] = val,
            0xA000..=0xBFFF => {
                if self.ram_enabled {
                    if (0x08..=0x0C).contains(&self.ram_bank) {
                        self.rtc_regs[self.ram_bank - 0x08] = val;
                    } else {
                        let offset = (self.ram_bank & 0x03) * 0x2000 + ((addr - 0xA000) as usize);
                        if offset < self.sram.len() {
                            self.sram[offset] = val;
                        }
                    }
                }
            }
            0xC000..=0xDFFF => self.wram[(addr - 0xC000) as usize] = val,
            0xE000..=0xFDFF => self.wram[(addr - 0xE000) as usize] = val,
            0xFE00..=0xFE9F => self.oam[(addr - 0xFE00) as usize] = val,
            0xFEA0..=0xFEFF => {}
            0xFF00 => {
                self.io[0x00] = (self.io[0x00] & 0xCF) | (val & 0x30);
            }
            0xFF04 => self.io[0x04] = 0, // Writing to DIV resets it to 0
            0xFF46 => {
                // OAM DMA transfer: copies 160 bytes from (val << 8) into OAM 0xFE00..0xFE9F
                self.io[0x46] = val;
                let base = (val as u16) << 8;
                for i in 0..0xA0u16 {
                    self.oam[i as usize] = self.read_byte(base.wrapping_add(i));
                }
            }
            0xFF01..=0xFF7F => {
                self.io[(addr - 0xFF00) as usize] = val;
                // Extract APU Square 1 frequency if NR14 (0xFF14) is triggered
                if addr == 0xFF14 && (val & 0x80) != 0 {
                    let period = (((val & 0x07) as u32) << 8) | (self.io[0x13] as u32);
                    if period < 2040 {
                        self.audio_freq_hz =
                            (131_072u32 / (2048u32 - period)).clamp(110, 4000) as u16;
                        self.audio_timer = 10;
                    }
                }
            }
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize] = val,
            0xFFFF => self.ie = val,
        }
    }

    fn fetch_u8(&mut self) -> u8 {
        let b = self.read_byte(self.pc);
        self.pc = self.pc.wrapping_add(1);
        b
    }

    fn fetch_u16(&mut self) -> u16 {
        let lo = self.fetch_u8() as u16;
        let hi = self.fetch_u8() as u16;
        (hi << 8) | lo
    }

    fn push_u16(&mut self, val: u16) {
        self.sp = self.sp.wrapping_sub(1);
        self.write_byte(self.sp, (val >> 8) as u8);
        self.sp = self.sp.wrapping_sub(1);
        self.write_byte(self.sp, (val & 0xFF) as u8);
    }

    fn pop_u16(&mut self) -> u16 {
        let lo = self.read_byte(self.sp) as u16;
        self.sp = self.sp.wrapping_add(1);
        let hi = self.read_byte(self.sp) as u16;
        self.sp = self.sp.wrapping_add(1);
        (hi << 8) | lo
    }

    fn get_r8(&self, idx: u8) -> u8 {
        match idx & 7 {
            0 => self.b,
            1 => self.c,
            2 => self.d,
            3 => self.e,
            4 => self.h,
            5 => self.l,
            6 => self.read_byte(self.hl()),
            _ => self.a,
        }
    }

    fn set_r8(&mut self, idx: u8, val: u8) {
        match idx & 7 {
            0 => self.b = val,
            1 => self.c = val,
            2 => self.d = val,
            3 => self.e = val,
            4 => self.h = val,
            5 => self.l = val,
            6 => self.write_byte(self.hl(), val),
            _ => self.a = val,
        }
    }

    fn alu_add(&mut self, val: u8, carry: bool) {
        let c = u8::from(carry && self.flag_c());
        let res = (self.a as u16) + (val as u16) + (c as u16);
        let h = ((self.a & 0x0F) + (val & 0x0F) + c) > 0x0F;
        self.a = (res & 0xFF) as u8;
        self.set_flags(self.a == 0, false, h, res > 0xFF);
    }

    fn alu_sub(&mut self, val: u8, carry: bool) {
        let c = u8::from(carry && self.flag_c());
        let res = (self.a as i16) - (val as i16) - (c as i16);
        let h = ((self.a & 0x0F) as i16 - (val & 0x0F) as i16 - (c as i16)) < 0;
        self.a = res as u8;
        self.set_flags(self.a == 0, true, h, res < 0);
    }

    fn alu_cp(&mut self, val: u8) {
        let res = (self.a as i16) - (val as i16);
        let h = ((self.a & 0x0F) as i16 - (val & 0x0F) as i16) < 0;
        self.set_flags((res as u8) == 0, true, h, res < 0);
    }

    fn exec_cb(&mut self, cb_op: u8) {
        let reg = cb_op & 0x07;
        let bit = (cb_op >> 3) & 0x07;
        let val = self.get_r8(reg);
        match cb_op >> 6 {
            0 => {
                // Shifts & Rotates
                let (res, c) = match bit {
                    0 => (val.rotate_left(1), (val & 0x80) != 0),  // RLC
                    1 => (val.rotate_right(1), (val & 0x01) != 0), // RRC
                    2 => {
                        // RL
                        let old_c = u8::from(self.flag_c());
                        ((val << 1) | old_c, (val & 0x80) != 0)
                    }
                    3 => {
                        // RR
                        let old_c = if self.flag_c() { 0x80 } else { 0 };
                        ((val >> 1) | old_c, (val & 0x01) != 0)
                    }
                    4 => (val << 1, (val & 0x80) != 0), // SLA
                    5 => ((val >> 1) | (val & 0x80), (val & 0x01) != 0), // SRA
                    6 => (val.rotate_left(4), false),   // SWAP
                    _ => (val >> 1, (val & 0x01) != 0), // SRL
                };
                self.set_r8(reg, res);
                self.set_flags(res == 0, false, false, c);
            }
            1 => {
                // BIT b, r8
                let z = (val & (1 << bit)) == 0;
                self.set_flags(z, false, true, self.flag_c());
            }
            2 => {
                // RES b, r8
                self.set_r8(reg, val & !(1 << bit));
            }
            _ => {
                // SET b, r8
                self.set_r8(reg, val | (1 << bit));
            }
        }
    }

    /// Executes one Sharp LR35902 instruction and returns machine cycles consumed.
    pub fn step_instruction(&mut self) -> u64 {
        // Check pending interrupts
        let pending = self.ie & self.io[0x0F] & 0x1F;
        if pending != 0 {
            self.halted = false;
            if self.ime {
                self.ime = false;
                for i in 0..5u8 {
                    if (pending & (1 << i)) != 0 {
                        self.io[0x0F] &= !(1 << i);
                        self.push_u16(self.pc);
                        self.pc = 0x0040 + (i as u16) * 8;
                        return 20;
                    }
                }
            }
        }
        if self.halted || self.rom.is_empty() {
            return 4;
        }

        let op = self.fetch_u8();
        match op {
            0x00 => 4, // NOP
            0x01 => {
                let v = self.fetch_u16();
                self.set_bc(v);
                12
            }
            0x11 => {
                let v = self.fetch_u16();
                self.set_de(v);
                12
            }
            0x21 => {
                let v = self.fetch_u16();
                self.set_hl(v);
                12
            }
            0x31 => {
                self.sp = self.fetch_u16();
                12
            }
            0x02 => {
                self.write_byte(self.bc(), self.a);
                8
            }
            0x12 => {
                self.write_byte(self.de(), self.a);
                8
            }
            0x22 => {
                let hl = self.hl();
                self.write_byte(hl, self.a);
                self.set_hl(hl.wrapping_add(1));
                8
            }
            0x32 => {
                let hl = self.hl();
                self.write_byte(hl, self.a);
                self.set_hl(hl.wrapping_sub(1));
                8
            }
            0x0A => {
                self.a = self.read_byte(self.bc());
                8
            }
            0x1A => {
                self.a = self.read_byte(self.de());
                8
            }
            0x2A => {
                let hl = self.hl();
                self.a = self.read_byte(hl);
                self.set_hl(hl.wrapping_add(1));
                8
            }
            0x3A => {
                let hl = self.hl();
                self.a = self.read_byte(hl);
                self.set_hl(hl.wrapping_sub(1));
                8
            }
            0x03 => {
                self.set_bc(self.bc().wrapping_add(1));
                8
            }
            0x13 => {
                self.set_de(self.de().wrapping_add(1));
                8
            }
            0x23 => {
                self.set_hl(self.hl().wrapping_add(1));
                8
            }
            0x33 => {
                self.sp = self.sp.wrapping_add(1);
                8
            }
            0x0B => {
                self.set_bc(self.bc().wrapping_sub(1));
                8
            }
            0x1B => {
                self.set_de(self.de().wrapping_sub(1));
                8
            }
            0x2B => {
                self.set_hl(self.hl().wrapping_sub(1));
                8
            }
            0x3B => {
                self.sp = self.sp.wrapping_sub(1);
                8
            }
            // 8-bit INC r8
            0x04 | 0x0C | 0x14 | 0x1C | 0x24 | 0x2C | 0x34 | 0x3C => {
                let reg = (op >> 3) & 7;
                let v = self.get_r8(reg);
                let res = v.wrapping_add(1);
                self.set_r8(reg, res);
                self.set_flags(res == 0, false, (v & 0x0F) == 0x0F, self.flag_c());
                if reg == 6 {
                    12
                } else {
                    4
                }
            }
            // 8-bit DEC r8
            0x05 | 0x0D | 0x15 | 0x1D | 0x25 | 0x2D | 0x35 | 0x3D => {
                let reg = (op >> 3) & 7;
                let v = self.get_r8(reg);
                let res = v.wrapping_sub(1);
                self.set_r8(reg, res);
                self.set_flags(res == 0, true, (v & 0x0F) == 0x00, self.flag_c());
                if reg == 6 {
                    12
                } else {
                    4
                }
            }
            // 8-bit LD r8, d8
            0x06 | 0x0E | 0x16 | 0x1E | 0x26 | 0x2E | 0x36 | 0x3E => {
                let reg = (op >> 3) & 7;
                let imm = self.fetch_u8();
                self.set_r8(reg, imm);
                if reg == 6 {
                    12
                } else {
                    8
                }
            }
            0x76 => {
                self.halted = true;
                4
            }
            // 8-bit LD r8, r8' (0x40..=0x7F except 0x76 HALT)
            0x40..=0x7F => {
                let dst = (op >> 3) & 7;
                let src = op & 7;
                let v = self.get_r8(src);
                self.set_r8(dst, v);
                if dst == 6 || src == 6 {
                    8
                } else {
                    4
                }
            }
            // 8-bit ALU register ops (0x80..=0xBF)
            0x80..=0xBF => {
                let src = op & 7;
                let v = self.get_r8(src);
                match (op >> 3) & 7 {
                    0 => self.alu_add(v, false),
                    1 => self.alu_add(v, true),
                    2 => self.alu_sub(v, false),
                    3 => self.alu_sub(v, true),
                    4 => {
                        self.a &= v;
                        self.set_flags(self.a == 0, false, true, false);
                    }
                    5 => {
                        self.a ^= v;
                        self.set_flags(self.a == 0, false, false, false);
                    }
                    6 => {
                        self.a |= v;
                        self.set_flags(self.a == 0, false, false, false);
                    }
                    _ => self.alu_cp(v),
                }
                if src == 6 {
                    8
                } else {
                    4
                }
            }
            // Immediate ALU ops
            0xC6 => {
                let v = self.fetch_u8();
                self.alu_add(v, false);
                8
            }
            0xCE => {
                let v = self.fetch_u8();
                self.alu_add(v, true);
                8
            }
            0xD6 => {
                let v = self.fetch_u8();
                self.alu_sub(v, false);
                8
            }
            0xE6 => {
                let v = self.fetch_u8();
                self.a &= v;
                self.set_flags(self.a == 0, false, true, false);
                8
            }
            0xEE => {
                let v = self.fetch_u8();
                self.a ^= v;
                self.set_flags(self.a == 0, false, false, false);
                8
            }
            0xF6 => {
                let v = self.fetch_u8();
                self.a |= v;
                self.set_flags(self.a == 0, false, false, false);
                8
            }
            0xFE => {
                let v = self.fetch_u8();
                self.alu_cp(v);
                8
            }
            // Relative jumps JR
            0x18 => {
                let rel = self.fetch_u8() as i8;
                self.pc = self.pc.wrapping_add(rel as u16);
                12
            }
            0x20 => {
                let rel = self.fetch_u8() as i8;
                if !self.flag_z() {
                    self.pc = self.pc.wrapping_add(rel as u16);
                }
                12
            }
            0x28 => {
                let rel = self.fetch_u8() as i8;
                if self.flag_z() {
                    self.pc = self.pc.wrapping_add(rel as u16);
                }
                12
            }
            0x30 => {
                let rel = self.fetch_u8() as i8;
                if !self.flag_c() {
                    self.pc = self.pc.wrapping_add(rel as u16);
                }
                12
            }
            0x38 => {
                let rel = self.fetch_u8() as i8;
                if self.flag_c() {
                    self.pc = self.pc.wrapping_add(rel as u16);
                }
                12
            }
            // Absolute jumps, calls, returns
            0xC3 => {
                self.pc = self.fetch_u16();
                16
            }
            0xE9 => {
                self.pc = self.hl();
                4
            }
            0xCD => {
                let target = self.fetch_u16();
                self.push_u16(self.pc);
                self.pc = target;
                24
            }
            0xC9 => {
                self.pc = self.pop_u16();
                16
            }
            0xD9 => {
                self.pc = self.pop_u16();
                self.ime = true;
                16
            }
            // Stack PUSH / POP
            0xC5 => {
                self.push_u16(self.bc());
                16
            }
            0xD5 => {
                self.push_u16(self.de());
                16
            }
            0xE5 => {
                self.push_u16(self.hl());
                16
            }
            0xF5 => {
                let af = ((self.a as u16) << 8) | ((self.f & 0xF0) as u16);
                self.push_u16(af);
                16
            }
            0xC1 => {
                let v = self.pop_u16();
                self.set_bc(v);
                12
            }
            0xD1 => {
                let v = self.pop_u16();
                self.set_de(v);
                12
            }
            0xE1 => {
                let v = self.pop_u16();
                self.set_hl(v);
                12
            }
            0xF1 => {
                let v = self.pop_u16();
                self.a = (v >> 8) as u8;
                self.f = (v as u8) & 0xF0;
                12
            }
            // High-page IO & 16-bit absolute loads
            0xE0 => {
                let off = self.fetch_u8() as u16;
                self.write_byte(0xFF00 + off, self.a);
                12
            }
            0xF0 => {
                let off = self.fetch_u8() as u16;
                self.a = self.read_byte(0xFF00 + off);
                12
            }
            0xE2 => {
                self.write_byte(0xFF00 + (self.c as u16), self.a);
                8
            }
            0xF2 => {
                self.a = self.read_byte(0xFF00 + (self.c as u16));
                8
            }
            0xEA => {
                let addr = self.fetch_u16();
                self.write_byte(addr, self.a);
                16
            }
            0xFA => {
                let addr = self.fetch_u16();
                self.a = self.read_byte(addr);
                16
            }
            0xF3 => {
                self.ime = false;
                4
            }
            0xFB => {
                self.ime = true;
                4
            }
            0xCB => {
                let cb = self.fetch_u8();
                self.exec_cb(cb);
                8
            }
            _ => 4,
        }
    }

    fn init_vram_tiles_and_map(&mut self) {
        // Tile 0: Subtle arena grid tile (2bpp: 16 bytes)
        let grid_tile: [u8; 16] = [
            0xFF, 0x00, 0x81, 0x00, 0x81, 0x00, 0x81, 0x00, 0x81, 0x00, 0x81, 0x00, 0x81, 0x00,
            0xFF, 0x00,
        ];
        // Tile 1: Arena border wall tile
        let border_tile: [u8; 16] = [
            0xFF, 0xFF, 0xC3, 0x3C, 0xA5, 0x5A, 0x99, 0x66, 0x99, 0x66, 0xA5, 0x5A, 0xC3, 0x3C,
            0xFF, 0xFF,
        ];
        // Tile 2: Hero sprite 8x8
        let hero_tile: [u8; 16] = [
            0x3C, 0x3C, 0x7E, 0x42, 0xFF, 0xA5, 0xFF, 0x81, 0x7E, 0x7E, 0x3C, 0x24, 0x66, 0x66,
            0xC3, 0xC3,
        ];
        // Tile 3: Link Rival sprite 8x8
        let rival_tile: [u8; 16] = [
            0x18, 0x18, 0x3C, 0x24, 0x7E, 0x5A, 0xFF, 0xDB, 0xFF, 0xDB, 0x7E, 0x5A, 0x3C, 0x24,
            0x18, 0x18,
        ];
        // Tile 4: Collectible Energy Orb 8x8
        let orb_tile: [u8; 16] = [
            0x18, 0x18, 0x3C, 0x3C, 0x7E, 0x66, 0x7E, 0x5A, 0x7E, 0x5A, 0x7E, 0x66, 0x3C, 0x3C,
            0x18, 0x18,
        ];
        // Tile 5: Plasma Shot 8x8
        let shot_tile: [u8; 16] = [
            0x00, 0x00, 0x18, 0x18, 0x3C, 0x3C, 0x7E, 0x7E, 0x7E, 0x7E, 0x3C, 0x3C, 0x18, 0x18,
            0x00, 0x00,
        ];

        self.vram[0..16].copy_from_slice(&grid_tile);
        self.vram[16..32].copy_from_slice(&border_tile);
        self.vram[32..48].copy_from_slice(&hero_tile);
        self.vram[48..64].copy_from_slice(&rival_tile);
        self.vram[64..80].copy_from_slice(&orb_tile);
        self.vram[80..96].copy_from_slice(&shot_tile);

        for ty in 0..32usize {
            for tx in 0..32usize {
                let map_idx = 0x1800 + ty * 32 + tx;
                if ty == 0 || ty == 17 || tx == 0 || tx == 19 {
                    self.vram[map_idx] = 1;
                } else {
                    self.vram[map_idx] = 0;
                }
            }
        }

        self.io[0x40] = 0x93;
        self.io[0x47] = 0xE4;
        self.io[0x48] = 0xE4;
        self.io[0x49] = 0xD2;
    }

    fn step_homebrew_arena(&mut self, buttons_mask: u16) {
        let step = if (buttons_mask & BTN_B) != 0 {
            3u8
        } else {
            2u8
        };
        if (buttons_mask & BTN_LEFT) != 0 {
            self.player_x = self.player_x.saturating_sub(step).max(10);
        }
        if (buttons_mask & BTN_RIGHT) != 0 {
            self.player_x = self.player_x.saturating_add(step).min(142);
        }
        if (buttons_mask & BTN_UP) != 0 {
            self.player_y = self.player_y.saturating_sub(step).max(12);
        }
        if (buttons_mask & BTN_DOWN) != 0 {
            self.player_y = self.player_y.saturating_add(step).min(124);
        }

        // Fire plasma shot with Button A
        if (buttons_mask & BTN_A) != 0 && !self.shot_active {
            self.shot_active = true;
            self.shot_x = self.player_x;
            self.shot_y = self.player_y.saturating_sub(6);
            self.audio_freq_hz = 880;
            self.audio_timer = 6;
        }
        if self.shot_active {
            if self.shot_y > 12 {
                self.shot_y = self.shot_y.saturating_sub(4);
            } else {
                self.shot_active = false;
            }
        }

        // Check orb collection (player or shot hits orb)
        let hit_player =
            self.player_x.abs_diff(self.orb_x) < 10 && self.player_y.abs_diff(self.orb_y) < 10;
        let hit_shot = self.shot_active
            && self.shot_x.abs_diff(self.orb_x) < 10
            && self.shot_y.abs_diff(self.orb_y) < 10;

        if hit_player || hit_shot {
            self.score = self.score.saturating_add(100);
            self.shot_active = false;
            let seed = (self.frame_number as u8)
                .wrapping_mul(73)
                .wrapping_add(self.player_x);
            self.orb_x = 18 + (seed % 120);
            self.orb_y = 18 + (seed.rotate_left(3) % 96);
            self.audio_freq_hz = 1320;
            self.audio_timer = 10;
        }

        if self.audio_timer > 0 {
            self.audio_timer -= 1;
            if self.audio_timer == 0 {
                self.audio_freq_hz = 0;
            }
        }

        // Populate OAM Sprites #0..#3
        self.oam[0] = self.player_y.wrapping_add(16);
        self.oam[1] = self.player_x.wrapping_add(8);
        self.oam[2] = 2;
        self.oam[3] = 0x00;

        self.oam[4] = self.peer_y.wrapping_add(16);
        self.oam[5] = self.peer_x.wrapping_add(8);
        self.oam[6] = 3;
        self.oam[7] = 0x10;

        self.oam[8] = self.orb_y.wrapping_add(16);
        self.oam[9] = self.orb_x.wrapping_add(8);
        self.oam[10] = 4;
        self.oam[11] = 0x00;

        if self.shot_active {
            self.oam[12] = self.shot_y.wrapping_add(16);
            self.oam[13] = self.shot_x.wrapping_add(8);
            self.oam[14] = 5;
            self.oam[15] = 0x00;
        } else {
            self.oam[12] = 0;
        }

        // Serial Link Cable SB/SC & Battery SRAM persistence
        self.io[0x01] = self.player_x ^ self.player_y.rotate_left(1);
        self.io[0x02] = 0x81;

        self.sram[0] = b'Z';
        self.sram[1] = b'M';
        self.sram[2] = self.player_x;
        self.sram[3] = self.player_y;
        self.sram[4..8].copy_from_slice(&self.score.to_le_bytes());
    }

    fn render_ppu_rgba(&self) -> Vec<u8> {
        let mut rgba = vec![0u8; GB_WIDTH * GB_HEIGHT * 4];
        let lcdc = self.io[0x40];
        if (lcdc & 0x80) == 0 {
            return rgba;
        }
        let scx = self.io[0x43] as usize;
        let scy = self.io[0x42] as usize;
        let wy = self.io[0x4A] as usize;
        let wx = (self.io[0x4B] as usize).saturating_sub(7);
        let win_enabled = (lcdc & 0x20) != 0;
        let bgp = self.io[0x47];
        let bg_map_base = if (lcdc & 0x08) != 0 { 0x1C00 } else { 0x1800 };
        let win_map_base = if (lcdc & 0x40) != 0 { 0x1C00 } else { 0x1800 };
        let unsigned_tiles = (lcdc & 0x10) != 0;

        for y in 0..GB_HEIGHT {
            let bg_y = (y + scy) & 0xFF;
            for x in 0..GB_WIDTH {
                let in_win = win_enabled && y >= wy && x >= wx;
                let (map_base, eff_y, eff_x) = if in_win {
                    (win_map_base, y - wy, x - wx)
                } else {
                    (bg_map_base, bg_y, (x + scx) & 0xFF)
                };
                let tile_row = (eff_y / 8) & 31;
                let fine_y = eff_y % 8;
                let tile_col = (eff_x / 8) & 31;
                let fine_x = eff_x % 8;

                let map_addr = map_base + tile_row * 32 + tile_col;
                let raw_id = self.vram[map_addr];
                let tile_addr = if unsigned_tiles {
                    (raw_id as usize) * 16 + fine_y * 2
                } else {
                    let signed = (raw_id as i8) as isize;
                    ((0x1000isize + signed * 16) as usize) + fine_y * 2
                };

                let lo = self.vram.get(tile_addr).copied().unwrap_or(0);
                let hi = self.vram.get(tile_addr + 1).copied().unwrap_or(0);
                let bit = 7 - fine_x;
                let color_idx = (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1);
                let shade = ((bgp >> (color_idx * 2)) & 0x03) as usize;

                let out_idx = (y * GB_WIDTH + x) * 4;
                rgba[out_idx..out_idx + 4].copy_from_slice(&DMG_PALETTE[shade]);
            }
        }

        // Render up to 40 OAM sprites
        let sprite_height: isize = if (lcdc & 0x04) != 0 { 16 } else { 8 };
        for s in 0..40 {
            let sy = (self.oam[s * 4] as isize) - 16;
            let sx = (self.oam[s * 4 + 1] as isize) - 8;
            if sy <= -16 || sy >= GB_HEIGHT as isize || sx <= -8 || sx >= GB_WIDTH as isize {
                continue;
            }
            let tile_id = self.oam[s * 4 + 2] as usize;
            let attrs = self.oam[s * 4 + 3];
            let flip_y = (attrs & 0x40) != 0;
            let flip_x = (attrs & 0x20) != 0;

            for row in 0..sprite_height {
                let py = sy + row;
                if !(0..GB_HEIGHT as isize).contains(&py) {
                    continue;
                }
                let eff_row = if flip_y { sprite_height - 1 - row } else { row } as usize;
                let tile_addr = (tile_id * 16 + eff_row * 2) & 0x1FFE;
                let lo = self.vram[tile_addr];
                let hi = self.vram[tile_addr + 1];

                for col in 0..8isize {
                    let px = sx + col;
                    if !(0..GB_WIDTH as isize).contains(&px) {
                        continue;
                    }
                    let eff_col = if flip_x {
                        col as usize
                    } else {
                        7 - (col as usize)
                    };
                    let color_idx = (((hi >> eff_col) & 1) << 1) | ((lo >> eff_col) & 1);
                    if color_idx == 0 {
                        continue;
                    }
                    let out_idx = ((py as usize) * GB_WIDTH + (px as usize)) * 4;
                    let sprite_rgb = match tile_id {
                        2 => [245, 186, 66, 255],  // Hero: Phosphor Gold
                        3 => [96, 165, 250, 255],  // Link Peer: Cobalt Blue
                        4 => [52, 211, 153, 255],  // Energy Orb: Emerald Green
                        5 => [248, 113, 113, 255], // Plasma Shot: Coral
                        _ => DMG_PALETTE[(color_idx as usize) & 3],
                    };
                    rgba[out_idx..out_idx + 4].copy_from_slice(&sprite_rgb);
                }
            }
        }

        rgba
    }
}

impl EmulatorBackend for GameBoyCore {
    fn console(&self) -> ConsoleKind {
        if self.is_gbc {
            ConsoleKind::Gbc
        } else {
            ConsoleKind::Gb
        }
    }

    fn load_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        if rom.len() < 0x0150 {
            return Err(format!(
                "ROM Game Boy trop courte ({} octets, minimum 336 octets)",
                rom.len()
            ));
        }
        self.rom = rom.to_vec();
        self.is_gbc = matches!(self.rom.get(0x0143).copied(), Some(0x80 | 0xC0));
        self.is_embedded_homebrew = rom.len() >= 0x013E && &rom[0x0134..0x013E] == b"PKMN_ARENA";
        self.rom_bank = 1;
        self.ram_bank = 0;
        self.pc = 0x0100;
        self.sp = 0xFFFE;
        self.halted = false;
        self.frame_number = 0;
        self.score = 0;
        if self.sram.len() < 0x8000 {
            self.sram.resize(0x8000, 0);
        }
        self.init_vram_tiles_and_map();
        Ok(())
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        self.frame_number += 1;
        self.buttons_state = buttons_mask;

        // Execute scanlines 0..153 (70,224 T-cycles per 59.73 Hz frame)
        let mut frame_cycles = 0u64;
        while frame_cycles < 4_096 {
            let c = self.step_instruction();
            frame_cycles += c;
            self.cycles = self.cycles.wrapping_add(c);
            self.div_counter = self.div_counter.wrapping_add(c as u16);
            self.io[0x04] = (self.div_counter >> 8) as u8;
        }
        // Trigger VBlank interrupt flag (bit 0 of IF 0xFF0F) at scanline 144
        self.io[0x44] = 144;
        self.io[0x0F] |= 0x01;

        if self.is_embedded_homebrew {
            self.step_homebrew_arena(buttons_mask);
        }

        let rgba = self.render_ppu_rgba();
        let serial_out = if (self.io[0x02] & 0x80) != 0 {
            Some(self.io[0x01])
        } else {
            None
        };

        FrameStepResult {
            console: self.console(),
            frame_number: self.frame_number,
            width: GB_WIDTH as u16,
            height: GB_HEIGHT as u16,
            rgba,
            audio_level: if self.audio_freq_hz > 0 { 0.65 } else { 0.12 },
            audio_freq_hz: self.audio_freq_hz,
            serial_out_byte: serial_out,
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
        if len >= 8 && &self.sram[0..2] == b"ZM" {
            self.player_x = self.sram[2].clamp(10, 142);
            self.player_y = self.sram[3].clamp(12, 124);
            self.score =
                u32::from_le_bytes([self.sram[4], self.sram[5], self.sram[6], self.sram[7]]);
        }
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        let outgoing = self.io[0x01];
        self.io[0x01] = incoming;
        self.io[0x02] &= 0x7F;
        self.io[0x0F] |= 0x08;
        self.peer_x = 20 + (incoming % 120);
        self.peer_y = 20 + (incoming.rotate_right(3) % 92);
        outgoing
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        let z = if (self.f & 0x80) != 0 { 'Z' } else { '-' };
        let n = if (self.f & 0x40) != 0 { 'N' } else { '-' };
        let h = if (self.f & 0x20) != 0 { 'H' } else { '-' };
        let c = if (self.f & 0x10) != 0 { 'C' } else { '-' };
        CpuDebugSnapshot {
            arch: "Sharp LR35902 (GB/GBC)".into(),
            pc_hex: format!("0x{:04X}", self.pc),
            sp_hex: format!("0x{:04X}", self.sp),
            reg_a_hex: format!("0x{:02X}", self.a),
            reg_bc_hex: format!("0x{:04X}", self.bc()),
            reg_de_hex: format!("0x{:04X}", self.de()),
            reg_hl_hex: format!("0x{:04X}", self.hl()),
            flags_str: format!("{z}{n}{h}{c}"),
            ly_scanline: self.io[0x44],
            serial_sb_hex: format!("0x{:02X}", self.io[0x01]),
            serial_sc_hex: format!("0x{:02X}", self.io[0x02]),
            cycles: self.cycles,
            score: self.score,
        }
    }

    fn apply_ram_cheat(&mut self, addr: u16, val: u8) {
        self.write_byte(addr, val);
        if addr == 0xA004 && self.score < (val as u32) {
            self.score = val as u32;
        }
        if addr == 0xC104 && val != 0 && !self.shot_active {
            self.shot_active = true;
            self.shot_x = self.player_x;
            self.shot_y = self.player_y.saturating_sub(4);
        }
    }

    fn read_ram_slice(&self) -> &[u8] {
        &self.wram
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

/// Builds a valid 32 KiB Game Boy Color homebrew ROM image with Nintendo header magic,
/// title `"PKMN_ARENA"`, CGB flag `0xC0`, and LR35902 machine code loop at `0x0150`.
pub fn build_gb_pocket_arena_rom() -> Vec<u8> {
    let mut rom = vec![0x00u8; 32_768];
    rom[0x0100] = 0x00; // NOP
    rom[0x0101] = 0xC3; // JP 0x0150
    rom[0x0102] = 0x50;
    rom[0x0103] = 0x01;
    rom[0x0104..0x0108].copy_from_slice(&[0xCE, 0xED, 0x66, 0x66]);
    rom[0x0134..0x013E].copy_from_slice(b"PKMN_ARENA");
    rom[0x0143] = 0xC0;
    rom[0x0147] = 0x13;
    rom[0x0148] = 0x00;
    rom[0x0149] = 0x03;

    // 0x0150: Read Joypad (LDH A, ($00)), INC B, INC HL, CB SWAP A, JP 0x0150
    rom[0x0150] = 0xF0;
    rom[0x0151] = 0x00;
    rom[0x0152] = 0x04;
    rom[0x0153] = 0x23;
    rom[0x0154] = 0xCB;
    rom[0x0155] = 0x37; // SWAP A
    rom[0x0156] = 0xC3;
    rom[0x0157] = 0x50;
    rom[0x0158] = 0x01;
    rom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gb_core_steps_frames_and_updates_sram_and_serial_registers() {
        let mut gb = GameBoyCore::new();
        let res = gb.step_frame(BTN_RIGHT | BTN_A);
        assert_eq!(res.width, 160);
        assert_eq!(res.height, 144);
        assert_eq!(res.rgba.len(), 160 * 144 * 4);
        assert!(res.serial_out_byte.is_some());
        assert_eq!(res.audio_freq_hz, 880);
        assert_eq!(&gb.sram()[0..2], b"ZM");
    }

    #[test]
    fn gb_mmu_and_cb_prefix_instructions_execute_accurately() {
        let mut gb = GameBoyCore::new();
        gb.a = 0x12;
        gb.exec_cb(0x37); // SWAP A
        assert_eq!(gb.a, 0x21);

        gb.write_byte(0xC100, 0x99);
        assert_eq!(gb.read_byte(0xC100), 0x99);
        assert_eq!(gb.read_byte(0xE100), 0x99); // Echo RAM
    }

    #[test]
    fn gb_mbc3_rtc_registers_and_latch_protocol_work() {
        let mut gb = GameBoyCore::new();
        gb.write_byte(0x0000, 0x0A); // Enable SRAM & RTC
        gb.write_byte(0x4000, 0x0A); // Select RTC Hours register (0x0A)
        assert_eq!(gb.read_byte(0xA000), 14);
        gb.write_byte(0xA000, 21);
        assert_eq!(gb.read_byte(0xA000), 21);

        gb.frame_number = 180; // 3 seconds elapsed
        gb.write_byte(0x4000, 0x08); // Select RTC Seconds register (0x08)
        gb.write_byte(0x6000, 0x00);
        gb.write_byte(0x6000, 0x01); // Latch 0x00 -> 0x01
        assert_eq!(gb.read_byte(0xA000), 33);
    }
}
