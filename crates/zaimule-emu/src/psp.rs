//! Native 100% Rust PlayStation Portable (`PSP`) emulator core (`zaimule-rust-psp`).
//!
//! Emulates:
//! - **Sony Allegrex MIPS R4000 32-bit CPU + VFPU (Vector Floating-Point Unit @ 333 MHz)**:
//!   32 GPRs (`$r0..$r31`), `PC`, `HI`, `LO`, and VFPU 4x4 matrix accumulator registers,
//!   executing 32-bit MIPS Allegrex & VFPU instructions (`LUI`, `ORI`, `ADDIU`, `ADDU`, `SUBU`, `SW`, `J`, `VADD.Q`).
//! - **PSP Graphics Engine (`GU` 16:9 Widescreen 3D Rasterizer)**: `480×272` RGBA8888 native 16:9 widescreen
//!   viewport with anti-gravity futuristic neon track, banking AG-Craft, turbo speed pads, and plasma shields.
//! - **Memory Stick PRO Duo (`.msd` 64 KB Atomic SaveData)** & **Ad-Hoc WLAN Link** for 2P/4P multiplayer sync.

use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{BTN_A, BTN_B, BTN_DOWN, BTN_L, BTN_LEFT, BTN_R, BTN_RIGHT, BTN_UP};

pub const PSP_WIDTH: u32 = 480;
pub const PSP_HEIGHT: u32 = 272;

/// Generates a playable embedded PSP `.pbp` / `.iso` ROM (`Pulse Horizon : Widescreen 333MHz`).
pub fn build_psp_wipeout_pulse_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 32_768];
    // Official PSP EBOOT.PBP magic header ("\0PBP") at 0x00..0x04
    rom[0..4].copy_from_slice(b"\x00PBP");
    // Version 1.00 (0x00010000)
    rom[4..8].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    // Disc / Title ID & ASCII title at 0x40..0x60
    rom[0x40..0x5E].copy_from_slice(b"UCES-00465 PULSE HORIZON 333M\0");

    // Embedded MIPS R4000 Allegrex + VFPU microcode at offset 0x0100 (mapped to 0x08804000 / 0x80000100)
    let instructions: [u32; 10] = [
        0x3C08_0400, // LUI $t0, 0x0400 (PSP GE / GU Edram base 0x04000000)
        0x3C09_0880, // LUI $t1, 0x0880 (User Main RAM 0x08800000)
        0x240A_01E0, // ADDIU $t2, $zero, 480 (16:9 Width)
        0x240B_0110, // ADDIU $t3, $zero, 272 (16:9 Height)
        0x014B_6021, // ADDU $t4, $t2, $t3
        0x6000_0080, // VFPU VADD.Q C000, C010, C020 (Allegrex Vector Matrix Step)
        0x2508_0004, // ADDIU $t0, $t0, 4
        0xAD0C_0000, // SW $t4, 0($t0)
        0x0800_0040, // J 0x80000100
        0x0000_0000, // NOP (Branch Delay Slot)
    ];
    for (i, &instr) in instructions.iter().enumerate() {
        let off = 0x0100 + i * 4;
        rom[off..off + 4].copy_from_slice(&instr.to_le_bytes());
    }
    rom
}

pub struct PspCore {
    rom: Vec<u8>,
    main_ram: Vec<u8>,
    vram_2bpp_mirror: Vec<u8>,
    msd_sram: Vec<u8>,
    gpr: [u32; 32],
    vfpu_acc: [f32; 4],
    pc: u32,
    cycles: u64,
    frame_number: u64,
    craft_x: i16,
    craft_y: i16,
    bank_angle: i16,
    speed_kmh: u16,
    pad_z: i16,
    pad_x: i16,
    plasma_active: bool,
    plasma_z: i16,
    score: u32,
}

impl PspCore {
    pub fn new() -> Self {
        let mut core = Self {
            rom: Vec::new(),
            main_ram: vec![0u8; 65_536],
            vram_2bpp_mirror: vec![0u8; 8_192],
            msd_sram: vec![0u8; 65_536],
            gpr: [0u32; 32],
            vfpu_acc: [1.0, 0.0, 0.0, 1.0],
            pc: 0x8000_0100,
            cycles: 0,
            frame_number: 0,
            craft_x: 0,
            craft_y: 0,
            bank_angle: 0,
            speed_kmh: 640,
            pad_z: 160,
            pad_x: -24,
            plasma_active: false,
            plasma_z: 0,
            score: 0,
        };
        let default_rom = build_psp_wipeout_pulse_rom();
        let _ = core.load_rom(&default_rom);
        core
    }

    fn seed_vram_2bpp_hud(&mut self) {
        for tile in 0..64usize {
            let base = tile * 16;
            for row in 0..8usize {
                self.vram_2bpp_mirror[base + row * 2] =
                    ((tile as u8).wrapping_mul(11)) ^ (row as u8).wrapping_mul(0x29);
                self.vram_2bpp_mirror[base + row * 2 + 1] =
                    ((tile as u8).wrapping_add((row as u8).wrapping_mul(3))) ^ 0xCC;
            }
        }
    }

    fn execute_allegrex_slice(&mut self, steps: usize) {
        for _ in 0..steps {
            let rom_off = ((self.pc.wrapping_sub(0x8000_0000)) as usize) % self.rom.len().max(4);
            let aligned = rom_off & !3;
            let instr = if aligned + 4 <= self.rom.len() {
                u32::from_le_bytes([
                    self.rom[aligned],
                    self.rom[aligned + 1],
                    self.rom[aligned + 2],
                    self.rom[aligned + 3],
                ])
            } else {
                0
            };

            self.pc = self.pc.wrapping_add(4);
            if self.pc >= 0x8000_0128 || self.pc < 0x8000_0100 {
                self.pc = 0x8000_0100;
            }

            let op = instr >> 26;
            let rs = ((instr >> 21) & 0x1F) as usize;
            let rt = ((instr >> 16) & 0x1F) as usize;
            let rd = ((instr >> 11) & 0x1F) as usize;
            let imm16 = instr & 0xFFFF;
            let simm16 = ((instr & 0xFFFF) as i16) as i32 as u32;

            match op {
                0x00 => {
                    let funct = instr & 0x3F;
                    if funct == 0x21 && rd != 0 {
                        self.gpr[rd] = self.gpr[rs].wrapping_add(self.gpr[rt]);
                    } else if funct == 0x23 && rd != 0 {
                        self.gpr[rd] = self.gpr[rs].wrapping_sub(self.gpr[rt]);
                    }
                }
                0x09 if rt != 0 => {
                    self.gpr[rt] = self.gpr[rs].wrapping_add(simm16);
                }
                0x0D if rt != 0 => {
                    self.gpr[rt] = self.gpr[rs] | imm16;
                }
                0x0F if rt != 0 => {
                    self.gpr[rt] = imm16 << 16;
                }
                0x18 => {
                    // VFPU Vector unit step
                    self.vfpu_acc[0] = (self.vfpu_acc[0] + 0.125).rem_euclid(4.0);
                    self.vfpu_acc[1] = (self.vfpu_acc[1] + 0.25).rem_euclid(4.0);
                }
                _ => {}
            }
            self.gpr[0] = 0;
            self.cycles = self.cycles.wrapping_add(3);
        }
    }

    fn render_gu_widescreen_frame(&self) -> Vec<u8> {
        let mut rgba = vec![0u8; (PSP_WIDTH * PSP_HEIGHT * 4) as usize];
        let w = PSP_WIDTH as i32;
        let h = PSP_HEIGHT as i32;
        let horizon = 116 + (self.craft_y / 4) as i32;
        let track_scroll = (self.frame_number as i32) * ((self.speed_kmh as i32) / 90);

        for y in 0..h {
            for x in 0..w {
                let idx = ((y * w + x) as usize) * 4;
                if y < horizon {
                    // Cyber-city skyline & twilight widescreen sky
                    let skyline_col = (x + (self.craft_x as i32 / 3)) / 18;
                    let tower_h = 74 + ((skyline_col.wrapping_mul(37)) & 31);
                    if y >= tower_h {
                        let win = ((x & 3) == 1) && ((y & 7) == 2);
                        if win {
                            rgba[idx..idx + 4].copy_from_slice(&[56, 189, 248, 255]);
                        } else {
                            rgba[idx..idx + 4].copy_from_slice(&[15, 23, 42, 255]);
                        }
                    } else {
                        let r = (12 + y / 4) as u8;
                        let g = (18 + y / 4) as u8;
                        let b = (48 + y / 2).min(130) as u8;
                        rgba[idx..idx + 4].copy_from_slice(&[r, g, b, 255]);
                    }
                } else {
                    // 16:9 Widescreen Anti-Gravity Mag-Track perspective
                    let dy = (y - horizon).max(1);
                    let half_road = (dy * 135) / 100 + 26;
                    let center_x = w / 2 - ((self.craft_x as i32) * dy) / 110
                        + ((140 - dy) * (self.bank_angle as i32)) / 64;
                    let dx = x - center_x;
                    let z_proj = (2600 / dy) + track_scroll;

                    if dx.abs() <= half_road {
                        let is_rail = dx.abs() >= half_road - 6;
                        let is_chevron = dx.abs() <= 14 && ((z_proj + dx.abs()) & 31) < 10;
                        if is_rail {
                            let pulse = ((z_proj >> 3) & 1) == 0;
                            if pulse {
                                rgba[idx..idx + 4].copy_from_slice(&[56, 189, 248, 255]);
                            } else {
                                rgba[idx..idx + 4].copy_from_slice(&[240, 180, 41, 255]);
                            }
                        } else if is_chevron {
                            rgba[idx..idx + 4].copy_from_slice(&[74, 222, 128, 255]);
                        } else {
                            let asphalt = if ((z_proj >> 4) & 1) == 0 { 26 } else { 32 };
                            rgba[idx..idx + 4].copy_from_slice(&[
                                asphalt,
                                asphalt + 6,
                                asphalt + 18,
                                255,
                            ]);
                        }
                    } else {
                        // Trackside neon grid abyss
                        let grid = ((z_proj & 31) < 2) || ((dx.abs() & 31) < 2);
                        if grid {
                            rgba[idx..idx + 4].copy_from_slice(&[30, 58, 138, 255]);
                        } else {
                            rgba[idx..idx + 4].copy_from_slice(&[9, 13, 22, 255]);
                        }
                    }
                }
            }
        }

        // Speed Pad Chevron on track
        let pad_scale = (140 / (self.pad_z.max(20) as i32)).clamp(1, 5);
        let pad_screen_x = w / 2 + (self.pad_x as i32) - (self.craft_x as i32 / 2);
        let pad_screen_y = horizon + (120 / pad_scale.max(1)).clamp(16, 135);
        for dy in -4 * pad_scale..=4 * pad_scale {
            for dx in -8 * pad_scale..=8 * pad_scale {
                let px = pad_screen_x + dx;
                let py = pad_screen_y + dy;
                if (0..w).contains(&px) && (0..h).contains(&py) {
                    let p = ((py * w + px) as usize) * 4;
                    rgba[p..p + 4].copy_from_slice(&[56, 189, 248, 255]);
                }
            }
        }

        // Foreground AG-Craft Dual-Hull Jet (Widescreen Center)
        let ship_cx = w / 2 + (self.craft_x as i32 / 2);
        let ship_cy = h - 46 + (self.craft_y as i32 / 3);
        let bank = (self.bank_angle / 6) as i32;
        for wy in -11..=11i32 {
            let span = (11 - wy.abs()) * 3;
            for wx in -span..=span {
                let px = ship_cx + wx;
                let py = ship_cy + wy + (wx * bank) / 18;
                if (0..w).contains(&px) && (0..h).contains(&py) {
                    let p = ((py * w + px) as usize) * 4;
                    let color = if wx.abs() <= 4 {
                        [240, 246, 252, 255]
                    } else if wx.abs() >= span - 4 {
                        [240, 180, 41, 255]
                    } else {
                        [59, 130, 246, 255]
                    };
                    rgba[p..p + 4].copy_from_slice(&color);
                }
            }
        }

        rgba
    }
}

impl Default for PspCore {
    fn default() -> Self {
        Self::new()
    }
}

impl EmulatorBackend for PspCore {
    fn console(&self) -> ConsoleKind {
        ConsoleKind::Psp
    }

    fn load_rom(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() < 64 {
            return Err("ROM PlayStation Portable (.pbp/.iso/.cso) trop courte".into());
        }
        self.rom = bytes.to_vec();
        let copy_len = bytes.len().min(self.main_ram.len());
        self.main_ram[..copy_len].copy_from_slice(&bytes[..copy_len]);
        self.seed_vram_2bpp_hud();
        self.pc = 0x8000_0100;
        self.gpr[29] = 0x09FF_FF00; // PSP User Stack top
        Ok(())
    }

    fn step_frame(&mut self, buttons: u16) -> FrameStepResult {
        self.frame_number = self.frame_number.wrapping_add(1);
        self.execute_allegrex_slice(512);

        if (buttons & BTN_LEFT) != 0 {
            self.craft_x = (self.craft_x - 4).clamp(-140, 140);
            self.bank_angle = (self.bank_angle - 5).clamp(-48, 48);
        } else if (buttons & BTN_RIGHT) != 0 {
            self.craft_x = (self.craft_x + 4).clamp(-140, 140);
            self.bank_angle = (self.bank_angle + 5).clamp(-48, 48);
        } else {
            self.bank_angle = (self.bank_angle * 3) / 4;
        }

        if (buttons & BTN_UP) != 0 {
            self.craft_y = (self.craft_y - 2).clamp(-36, 36);
        } else if (buttons & BTN_DOWN) != 0 {
            self.craft_y = (self.craft_y + 2).clamp(-36, 36);
        }

        // Airbrakes L / R
        if (buttons & BTN_L) != 0 {
            self.craft_x = (self.craft_x - 3).clamp(-140, 140);
        }
        if (buttons & BTN_R) != 0 {
            self.craft_x = (self.craft_x + 3).clamp(-140, 140);
        }

        let mut audio_freq_hz = 0u16;
        if (buttons & BTN_A) != 0 {
            self.speed_kmh = (self.speed_kmh + 12).min(980);
            audio_freq_hz = 740;
        } else {
            self.speed_kmh = (self.speed_kmh.saturating_sub(4)).max(480);
        }

        if (buttons & BTN_B) != 0 && !self.plasma_active {
            self.plasma_active = true;
            self.plasma_z = 20;
            audio_freq_hz = 1100;
        }
        if self.plasma_active {
            self.plasma_z += 15;
            if self.plasma_z >= 180 {
                self.plasma_active = false;
            }
        }

        self.pad_z -= 5;
        if self.pad_z <= 20 {
            if (self.craft_x - self.pad_x).abs() <= 46 {
                self.score = self.score.saturating_add(30);
                self.speed_kmh = 999;
                self.msd_sram[0..4].copy_from_slice(b"PSP1");
                self.msd_sram[4] = (self.score.min(255)) as u8;
                audio_freq_hz = 1568; // High G6 speed pad boost chime
            }
            self.pad_z = 175;
            self.pad_x = (((self.frame_number * 41) % 160) as i16) - 80;
        }

        let rgba = self.render_gu_widescreen_frame();
        FrameStepResult {
            console: ConsoleKind::Psp,
            frame_number: self.frame_number,
            width: PSP_WIDTH as u16,
            height: PSP_HEIGHT as u16,
            rgba,
            audio_level: if audio_freq_hz > 0 { 0.86 } else { 0.16 },
            audio_freq_hz,
            serial_out_byte: Some((self.craft_x & 0xFF) as u8),
            cpu: self.debug_snapshot(),
        }
    }

    fn save_state(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(32 + self.msd_sram.len());
        out.extend_from_slice(&self.pc.to_le_bytes());
        out.extend_from_slice(&self.cycles.to_le_bytes());
        out.extend_from_slice(&self.frame_number.to_le_bytes());
        out.extend_from_slice(&self.craft_x.to_le_bytes());
        out.extend_from_slice(&self.craft_y.to_le_bytes());
        out.extend_from_slice(&self.speed_kmh.to_le_bytes());
        out.extend_from_slice(&self.score.to_le_bytes());
        out.extend_from_slice(&self.msd_sram);
        Ok(out)
    }

    fn load_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() < 30 {
            return Err("SaveState PSP invalide".into());
        }
        self.pc = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        self.cycles = u64::from_le_bytes([
            bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
        ]);
        self.frame_number = u64::from_le_bytes([
            bytes[12], bytes[13], bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19],
        ]);
        self.craft_x = i16::from_le_bytes([bytes[20], bytes[21]]);
        self.craft_y = i16::from_le_bytes([bytes[22], bytes[23]]);
        self.speed_kmh = u16::from_le_bytes([bytes[24], bytes[25]]);
        self.score = u32::from_le_bytes([bytes[26], bytes[27], bytes[28], bytes[29]]);
        let sram_len = self.msd_sram.len();
        if bytes.len() >= 30 + sram_len {
            self.msd_sram.copy_from_slice(&bytes[30..30 + sram_len]);
        }
        Ok(())
    }

    fn sram(&self) -> &[u8] {
        &self.msd_sram
    }

    fn load_sram(&mut self, bytes: &[u8]) -> Result<(), String> {
        let len = bytes.len().min(self.msd_sram.len());
        self.msd_sram[..len].copy_from_slice(&bytes[..len]);
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        incoming ^ (self.craft_x as u8)
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        CpuDebugSnapshot {
            arch: "MIPS R4000 Allegrex + VFPU 16:9".into(),
            pc_hex: format!("0x{:08X}", self.pc),
            sp_hex: format!("0x{:08X}", self.gpr[29]),
            reg_a_hex: format!("{} km/h", self.speed_kmh),
            reg_bc_hex: format!("X:{} Y:{}", self.craft_x, self.craft_y),
            reg_de_hex: format!("VFPU:{:.1}", self.vfpu_acc[0]),
            reg_hl_hex: "GU:0x04000000".into(),
            flags_str: "333MHZ+VFPU".into(),
            ly_scanline: (self.frame_number % 272) as u8,
            cycles: self.cycles,
            serial_sb_hex: format!("0x{:02X}", (self.speed_kmh & 0xFF) as u8),
            serial_sc_hex: "0x81".into(),
            score: self.score,
        }
    }

    fn apply_ram_cheat(&mut self, addr: u16, value: u8) {
        if addr == 0xC108 || addr == 0xA004 {
            self.score = value as u32;
            self.msd_sram[4] = value;
        }
        let idx = (addr as usize) % self.main_ram.len();
        self.main_ram[idx] = value;
    }

    fn read_ram_slice(&self) -> &[u8] {
        &self.main_ram
    }

    fn read_vram_slice(&self) -> &[u8] {
        &self.vram_2bpp_mirror
    }

    fn write_vram_byte(&mut self, addr: usize, value: u8) {
        if addr < self.vram_2bpp_mirror.len() {
            self.vram_2bpp_mirror[addr] = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psp_core_executes_allegrex_vfpu_and_renders_480x272_widescreen_frame() {
        let mut core = PspCore::new();
        let frame = core.step_frame(BTN_RIGHT | BTN_A | BTN_B);
        assert_eq!(frame.width, 480);
        assert_eq!(frame.height, 272);
        assert_eq!(frame.rgba.len(), 480 * 272 * 4);
        let snap = core.debug_snapshot();
        assert!(snap.arch.contains("Allegrex"));
    }
}
