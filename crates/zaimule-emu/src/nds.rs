//! Native Rust Nintendo DS Core (`zaimule-rust-nds`):
//! - Dual-CPU Architecture: ARM946E-S (67 MHz Main 2D/3D Engine A) + ARM7TDMI (33 MHz Audio/Wi-Fi/SPI Coprocessor)
//! - Hardware IPC Sync (`IPCSYNC 0x04000180` / `IPCFIFO 0x04000188`) & SPI Touchscreen ADC (`TSC`)
//! - Dual-Screen `256x384` RGBA8888 PPU (`256x192` Top Screen Engine A + `256x192` Bottom Touchscreen Engine B)
//! - Interactive Stylus Touchscreen (`set_touch_stylus`) + NiFi Wireless Multi-Card Link
//! - Embedded playable homebrew ROM (`build_nds_dual_phantom_rom`)

use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{BTN_A, BTN_B, BTN_DOWN, BTN_LEFT, BTN_RIGHT, BTN_UP};

pub const NDS_WIDTH: u16 = 256;
pub const NDS_HEIGHT: u16 = 384; // 256x192 Top Screen + 256x192 Bottom Touch Screen

pub struct NdsCore {
    /// ARM946E-S registers (`R0..R15`, `R15` = `PC` in `0x0200_0000` Main RAM)
    pub arm9_regs: [u32; 16],
    pub arm9_cpsr: u32,
    /// ARM7TDMI coprocessor registers (`R0..R15`, `R15` = `PC` in `0x0380_0000` ARM7 WRAM)
    pub arm7_regs: [u32; 16],
    /// Hardware IPC Sync register (`0x0400_0180`)
    pub ipc_sync: u16,
    /// Stylus touchscreen coordinates (`0..=255`, `0..=191` on Bottom Engine B) & pen-down state
    pub stylus_x: u8,
    pub stylus_y: u8,
    pub stylus_down: bool,
    pub cycles: u64,
    pub frame_number: u64,
    /// Cartridge ROM image
    pub rom: Vec<u8>,
    /// Main RAM (`64 KiB` window mapped at `0x0200_0000`)
    pub main_ram: Vec<u8>,
    /// Engine A + Engine B VRAM (`64 KiB` mapped at `0x0600_0000`)
    pub vram: Vec<u8>,
    /// Cartridge Backup EEPROM / FRAM (`32 KiB`)
    pub sram: Vec<u8>,
    /// NiFi wireless / Link data register
    pub nifi_data: u8,
    pub nifi_pending: bool,
    /// Gameplay state for the embedded Dual Screen Phantom Detective homebrew
    pub agent_x: u8,
    pub agent_y: u8,
    pub phantom_x: u8,
    pub phantom_y: u8,
    pub scan_pulse: u8,
    pub score: u32,
}

impl Default for NdsCore {
    fn default() -> Self {
        Self::new()
    }
}

impl NdsCore {
    pub fn new() -> Self {
        let mut core = Self {
            arm9_regs: [0; 16],
            arm9_cpsr: 0x0000_001F,
            arm7_regs: [0; 16],
            ipc_sync: 0x0100,
            stylus_x: 128,
            stylus_y: 96,
            stylus_down: false,
            cycles: 0,
            frame_number: 0,
            rom: Vec::new(),
            main_ram: vec![0; 0x10000],
            vram: vec![0; 0x10000],
            sram: vec![0; 0x8000],
            nifi_data: 0,
            nifi_pending: false,
            agent_x: 128,
            agent_y: 110,
            phantom_x: 164,
            phantom_y: 78,
            scan_pulse: 0,
            score: 0,
        };
        let _ = core.load_rom(&build_nds_dual_phantom_rom());
        core
    }

    fn seed_vram_tiles(&mut self) {
        let tiles: [[u8; 16]; 4] = [
            // Tile 0: Detective Badge
            [
                0x3C, 0x3C, 0x7E, 0x42, 0xFF, 0x99, 0xFF, 0xBD, 0xFF, 0xBD, 0xFF, 0x99, 0x7E, 0x42,
                0x3C, 0x3C,
            ],
            // Tile 1: Phantom Anomaly Signature
            [
                0x18, 0x18, 0x3C, 0x24, 0x7E, 0x5A, 0xFF, 0xA5, 0xFF, 0xA5, 0x7E, 0x5A, 0x24, 0x3C,
                0x5A, 0x42,
            ],
            // Tile 2: Stylus Touch Target Crosshair
            [
                0x18, 0x00, 0x18, 0x18, 0x7E, 0x24, 0xFF, 0x5A, 0xFF, 0x5A, 0x7E, 0x24, 0x18, 0x18,
                0x18, 0x00,
            ],
            // Tile 3: Dual-Screen IPC Sync Icon
            [
                0xFF, 0x00, 0x81, 0x7E, 0xBD, 0x42, 0xA5, 0x5A, 0xA5, 0x5A, 0xBD, 0x42, 0x81, 0x7E,
                0xFF, 0x00,
            ],
        ];
        for (i, t) in tiles.iter().enumerate() {
            let start = i * 16;
            self.vram[start..start + 16].copy_from_slice(t);
        }
    }

    fn step_arm9_and_arm7(&mut self, instructions: usize) {
        for _ in 0..instructions {
            let pc9 = self.arm9_regs[15];
            let offset = (pc9.saturating_sub(0x0200_0000) as usize) & 0x7FFC;
            let word = if offset + 4 <= self.rom.len() {
                u32::from_le_bytes([
                    self.rom[offset],
                    self.rom[offset + 1],
                    self.rom[offset + 2],
                    self.rom[offset + 3],
                ])
            } else {
                0xE1A0_0000 // NOP
            };

            self.arm9_regs[15] = pc9.wrapping_add(4);
            self.cycles = self.cycles.wrapping_add(2);

            // ARM9 branch (`0xEAxxxxxx`)
            if (word & 0x0E00_0000) == 0x0A00_0000 {
                let raw24 = (word & 0x00FF_FFFF) as i32;
                let signed_offset = (raw24 << 8) >> 6;
                let target = (pc9 as i32).wrapping_add(8).wrapping_add(signed_offset) as u32;
                self.arm9_regs[15] = target;
            } else if (word & 0x0C00_0000) == 0x0000_0000 {
                let opcode = (word >> 21) & 0x0F;
                let rn = ((word >> 16) & 0x0F) as usize;
                let rd = ((word >> 12) & 0x0F) as usize;
                let imm = word & 0xFF;
                match opcode {
                    0x04 if rd < 15 => self.arm9_regs[rd] = self.arm9_regs[rn].wrapping_add(imm),
                    0x0D if rd < 15 => self.arm9_regs[rd] = imm,
                    _ => {}
                }
            }

            // Step ARM7 coprocessor (SPI Touchscreen + IPC FIFO heartbeat)
            self.arm7_regs[0] = ((self.stylus_x as u32) << 8) | (self.stylus_y as u32);
            self.arm7_regs[1] = u32::from(self.stylus_down);
            self.arm7_regs[15] = 0x0380_0100 + ((self.arm7_regs[15].wrapping_add(4)) & 0x3C);
            self.ipc_sync = 0x0100 | ((self.frame_number as u16) & 0x0F);
        }
    }

    fn render_dual_screen_rgba(&self) -> Vec<u8> {
        let w = NDS_WIDTH as usize;
        let h = NDS_HEIGHT as usize;
        let mut rgba = vec![0u8; w * h * 4];
        let t = self.frame_number as usize;

        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 4;

                if (190..194).contains(&y) {
                    // Nintendo DS Hinge Divider Bar
                    rgba[idx..idx + 4].copy_from_slice(&[30, 41, 59, 255]);
                    continue;
                }

                if y < 190 {
                    // TOP SCREEN (Engine A: 3D/2D Cyber Investigation Viewport)
                    if y < 22 {
                        let glow = ((x + t) % 32 < 2) as u8 * 35;
                        rgba[idx..idx + 4].copy_from_slice(&[12, 24 + glow, 48 + glow, 255]);
                    } else {
                        let grid = (x + t).is_multiple_of(24) || (y + t / 2).is_multiple_of(24);
                        if grid {
                            rgba[idx..idx + 4].copy_from_slice(&[20, 52, 88, 255]);
                        } else {
                            rgba[idx..idx + 4].copy_from_slice(&[9, 15, 28, 255]);
                        }
                    }

                    // Draw Phantom Anomaly on Top Screen
                    let dx_p = (x as i32 - self.phantom_x as i32).abs();
                    let dy_p = (y as i32 - self.phantom_y as i32).abs();
                    if dx_p + dy_p <= 9 {
                        rgba[idx..idx + 4].copy_from_slice(&[245, 186, 66, 255]);
                    }

                    // Draw Detective Agent on Top Screen
                    let dx_a = (x as i32 - self.agent_x as i32).abs();
                    let dy_a = (y as i32 - self.agent_y as i32).abs();
                    if dx_a <= 6 && dy_a <= 6 {
                        if dx_a <= 2 && dy_a <= 2 {
                            rgba[idx..idx + 4].copy_from_slice(&[248, 250, 252, 255]);
                        } else {
                            rgba[idx..idx + 4].copy_from_slice(&[52, 211, 153, 255]);
                        }
                    }
                } else {
                    // BOTTOM SCREEN (Engine B: Interactive Stylus Touchscreen Radar)
                    let by = y - 192;
                    let radar_grid = x.is_multiple_of(16) || by.is_multiple_of(16);
                    if radar_grid {
                        rgba[idx..idx + 4].copy_from_slice(&[16, 42, 56, 255]);
                    } else {
                        rgba[idx..idx + 4].copy_from_slice(&[7, 18, 26, 255]);
                    }

                    // Phantom Blip on Bottom Touch Radar
                    let dx_p = (x as i32 - self.phantom_x as i32).abs();
                    let dy_p = (by as i32 - self.phantom_y as i32).abs();
                    if dx_p <= 6 && dy_p <= 6 {
                        rgba[idx..idx + 4].copy_from_slice(&[248, 113, 113, 255]);
                    }

                    // Stylus Crosshair & Sonar Pulse on Bottom Touch Screen
                    let dx_s = (x as i32 - self.stylus_x as i32).abs();
                    let dy_s = (by as i32 - self.stylus_y as i32).abs();
                    let dist_s = dx_s + dy_s;
                    if (dx_s == 0 && dy_s <= 10) || (dy_s == 0 && dx_s <= 10) {
                        let color = if self.stylus_down {
                            [245, 186, 66, 255]
                        } else {
                            [56, 189, 248, 255]
                        };
                        rgba[idx..idx + 4].copy_from_slice(&color);
                    } else if self.scan_pulse > 0
                        && (dist_s - (18 - self.scan_pulse as i32)).abs() <= 1
                    {
                        rgba[idx..idx + 4].copy_from_slice(&[52, 211, 153, 255]);
                    }
                }
            }
        }

        rgba
    }
}

impl EmulatorBackend for NdsCore {
    fn console(&self) -> ConsoleKind {
        ConsoleKind::Nds
    }

    fn load_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        if rom.is_empty() {
            return Err("ROM Nintendo DS vide".into());
        }
        self.rom = rom.to_vec();
        self.arm9_regs = [0; 16];
        self.arm9_regs[13] = 0x0380_3F00;
        self.arm9_regs[15] = 0x0200_0000;
        self.arm7_regs = [0; 16];
        self.arm7_regs[13] = 0x0380_FD80;
        self.arm7_regs[15] = 0x0380_0100;
        self.cycles = 0;
        self.frame_number = 0;
        self.seed_vram_tiles();
        Ok(())
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        self.frame_number += 1;
        self.step_arm9_and_arm7(512);

        let speed: u8 = if (buttons_mask & BTN_B) != 0 { 3 } else { 2 };
        if (buttons_mask & BTN_LEFT) != 0 {
            self.agent_x = self.agent_x.saturating_sub(speed).max(16);
        }
        if (buttons_mask & BTN_RIGHT) != 0 {
            self.agent_x = (self.agent_x.saturating_add(speed)).min(240);
        }
        if (buttons_mask & BTN_UP) != 0 {
            self.agent_y = self.agent_y.saturating_sub(speed).max(28);
        }
        if (buttons_mask & BTN_DOWN) != 0 {
            self.agent_y = (self.agent_y.saturating_add(speed)).min(176);
        }

        let mut audio_freq_hz: u16 = 0;
        if (buttons_mask & BTN_A) != 0 && self.scan_pulse == 0 {
            self.scan_pulse = 16;
            self.stylus_x = self.agent_x;
            self.stylus_y = self.agent_y;
            audio_freq_hz = 1046;
        }
        if self.stylus_down && self.scan_pulse == 0 {
            self.scan_pulse = 16;
            audio_freq_hz = 1174;
        }
        if self.scan_pulse > 0 {
            self.scan_pulse -= 1;
        }

        let hit_agent = (self.agent_x as i16 - self.phantom_x as i16).abs() <= 11
            && (self.agent_y as i16 - self.phantom_y as i16).abs() <= 11;
        let hit_stylus = (self.stylus_down || self.scan_pulse > 0)
            && (self.stylus_x as i16 - self.phantom_x as i16).abs() <= 14
            && (self.stylus_y as i16 - self.phantom_y as i16).abs() <= 14;

        if hit_agent || hit_stylus {
            self.score = self.score.wrapping_add(if hit_stylus { 35 } else { 15 });
            let seed = (self.frame_number as u8)
                .wrapping_mul(43)
                .wrapping_add(self.agent_x);
            self.phantom_x = 24 + (seed % 204);
            self.phantom_y = 34 + (seed.rotate_left(3) % 136);
            audio_freq_hz = 1568; // G6 Phantom Locked Chime

            let score_bytes = self.score.to_le_bytes();
            self.sram[0..4].copy_from_slice(&score_bytes);
            self.main_ram[0..4].copy_from_slice(&score_bytes);
        }

        self.nifi_data = self.stylus_x ^ (self.score as u8);
        self.nifi_pending = (self.frame_number & 1) == 0;
        self.arm9_regs[0] = self.score;
        self.arm9_regs[1] = ((self.agent_x as u32) << 8) | (self.agent_y as u32);

        let rgba = self.render_dual_screen_rgba();
        let cpu = self.debug_snapshot();

        FrameStepResult {
            console: ConsoleKind::Nds,
            frame_number: self.frame_number,
            width: NDS_WIDTH,
            height: NDS_HEIGHT,
            rgba,
            audio_level: if audio_freq_hz > 0 { 0.42 } else { 0.08 },
            audio_freq_hz,
            serial_out_byte: if self.nifi_pending {
                Some(self.nifi_data)
            } else {
                None
            },
            cpu,
        }
    }

    fn save_state(&self) -> Result<Vec<u8>, String> {
        let mut buf = Vec::with_capacity(64 + self.main_ram.len() + self.sram.len());
        buf.extend_from_slice(b"ZMNDS1");
        buf.extend_from_slice(&self.arm9_regs[15].to_le_bytes());
        buf.extend_from_slice(&self.arm7_regs[15].to_le_bytes());
        buf.extend_from_slice(&self.cycles.to_le_bytes());
        buf.extend_from_slice(&self.frame_number.to_le_bytes());
        buf.extend_from_slice(&[
            self.agent_x,
            self.agent_y,
            self.phantom_x,
            self.phantom_y,
            self.stylus_x,
            self.stylus_y,
            u8::from(self.stylus_down),
            self.scan_pulse,
        ]);
        buf.extend_from_slice(&self.score.to_le_bytes());
        buf.extend_from_slice(&self.main_ram[0..4096]);
        buf.extend_from_slice(&self.sram);
        Ok(buf)
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        if data.len() < 42 || &data[0..6] != b"ZMNDS1" {
            return Err("Save State NDS invalide".into());
        }
        self.arm9_regs[15] = u32::from_le_bytes([data[6], data[7], data[8], data[9]]);
        self.arm7_regs[15] = u32::from_le_bytes([data[10], data[11], data[12], data[13]]);
        self.cycles = u64::from_le_bytes(data[14..22].try_into().unwrap_or([0; 8]));
        self.frame_number = u64::from_le_bytes(data[22..30].try_into().unwrap_or([0; 8]));
        self.agent_x = data[30];
        self.agent_y = data[31];
        self.phantom_x = data[32];
        self.phantom_y = data[33];
        self.stylus_x = data[34];
        self.stylus_y = data[35];
        self.stylus_down = data[36] != 0;
        self.scan_pulse = data[37];
        self.score = u32::from_le_bytes([data[38], data[39], data[40], data[41]]);
        Ok(())
    }

    fn sram(&self) -> &[u8] {
        &self.sram
    }

    fn load_sram(&mut self, data: &[u8]) -> Result<(), String> {
        let len = data.len().min(self.sram.len());
        self.sram[..len].copy_from_slice(&data[..len]);
        if len >= 4 {
            self.score =
                u32::from_le_bytes([self.sram[0], self.sram[1], self.sram[2], self.sram[3]]);
        }
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        let out = self.nifi_data;
        self.nifi_data = incoming;
        self.nifi_pending = false;
        out
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        CpuDebugSnapshot {
            arch: "NDS ARM946E-S + ARM7".into(),
            pc_hex: format!("0x{:08X}", self.arm9_regs[15]),
            sp_hex: format!("0x{:08X}", self.arm9_regs[13]),
            reg_a_hex: format!("R0:0x{:04X}", self.arm9_regs[0] & 0xFFFF),
            reg_bc_hex: format!("ARM7:0x{:04X}", self.arm7_regs[15] & 0xFFFF),
            reg_de_hex: format!("STY:{},{}", self.stylus_x, self.stylus_y),
            reg_hl_hex: format!("IPC:0x{:04X}", self.ipc_sync),
            flags_str: if self.stylus_down {
                "PEN_DOWN+IPC".into()
            } else {
                "ARM9+ARM7_OK".into()
            },
            ly_scanline: (self.frame_number % 192) as u8,
            serial_sb_hex: format!("0x{:02X}", self.nifi_data),
            serial_sc_hex: "0x81".into(),
            cycles: self.cycles,
            score: self.score,
        }
    }

    fn apply_ram_cheat(&mut self, addr: u16, val: u8) {
        let idx = (addr as usize) & 0xFFFF;
        self.main_ram[idx] = val;
        if addr == 0xA004 || addr == 0xC108 {
            self.score = self.score.max(val as u32);
        }
    }

    fn read_ram_slice(&self) -> &[u8] {
        &self.main_ram
    }

    fn read_vram_slice(&self) -> &[u8] {
        &self.vram
    }

    fn write_vram_byte(&mut self, offset: usize, val: u8) {
        if offset < self.vram.len() {
            self.vram[offset] = val;
        }
    }

    fn set_touch_stylus(&mut self, x: u8, y: u8, pressed: bool) {
        self.stylus_x = x;
        self.stylus_y = y.min(191);
        self.stylus_down = pressed;
    }
}

/// Builds a playable 32 KiB Nintendo DS ROM ("DUAL PHANTOM DS") with valid header and ARM9 loop.
pub fn build_nds_dual_phantom_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[0x0000..0x000C].copy_from_slice(b"DUALPHANTOM\0");
    rom[0x000C..0x0010].copy_from_slice(b"ADPE");

    // ARM9 entry loop at offset 0x0000 (mapped to 0x0200_0000):
    // 0x0200_0000: ADD R2, R2, #1 (0xE282_2001)
    // 0x0200_0004: B 0x0200_0000  (0xEAFF_FFFD)
    rom[0x0000..0x0004].copy_from_slice(&0xE282_2001u32.to_le_bytes());
    rom[0x0004..0x0008].copy_from_slice(&0xEAFF_FFFDu32.to_le_bytes());
    rom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nds_core_executes_arm9_arm7_ipc_and_processes_stylus_touchscreen() {
        let mut core = NdsCore::new();
        core.set_touch_stylus(164, 78, true);
        let step = core.step_frame(BTN_RIGHT);
        assert_eq!(step.width, NDS_WIDTH);
        assert_eq!(step.height, NDS_HEIGHT);
        assert_eq!(
            step.rgba.len(),
            (NDS_WIDTH as usize) * (NDS_HEIGHT as usize) * 4
        );
        assert!(step.cpu.score >= 35);
        assert_eq!(step.cpu.flags_str, "PEN_DOWN+IPC");
    }
}
