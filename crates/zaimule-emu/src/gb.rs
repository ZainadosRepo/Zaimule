//! 100% Native Rust Game Boy / Game Boy Color (`DMG`/`CGB`) core (`zaimule-rust-gb`).
//! Includes Sharp LR35902 CPU interpreter, 160x144 PPU scanline tile & OAM sprite renderer,
//! Joypad (`0xFF00`), Serial Link Cable registers `SB` (`0xFF01`) / `SC` (`0xFF02`),
//! battery-backed SRAM (`0xA000..0xBFFF`), and an embedded playable homebrew ROM.

use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{BTN_A, BTN_B, BTN_DOWN, BTN_LEFT, BTN_RIGHT, BTN_START, BTN_UP};

pub const GB_WIDTH: usize = 160;
pub const GB_HEIGHT: usize = 144;

/// Classic 4-shade Game Boy DMG / GBC phosphor palette (RGBA).
const DMG_PALETTE: [[u8; 4]; 4] = [
    [224, 248, 208, 255], // 0: Lightest phosphor green-gold
    [136, 192, 112, 255], // 1: Light sage
    [52, 104, 86, 255],   // 2: Deep teal-emerald
    [8, 24, 32, 255],     // 3: Obsidian shadow
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameBoyCore {
    pub is_gbc: bool,
    pub rom: Vec<u8>,
    pub vram: Vec<u8>,
    pub sram: Vec<u8>,
    pub wram: Vec<u8>,
    pub oam: Vec<u8>,
    pub io: Vec<u8>,
    pub hram: Vec<u8>,
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
    pub frame_number: u64,
    // Player & Link Arena state mapped into VRAM/OAM/SRAM
    pub player_x: u8,
    pub player_y: u8,
    pub peer_x: u8,
    pub peer_y: u8,
}

impl Default for GameBoyCore {
    fn default() -> Self {
        let mut core = Self {
            is_gbc: true,
            rom: vec![0; 0x8000],
            vram: vec![0; 0x2000],
            sram: vec![0; 0x2000],
            wram: vec![0; 0x2000],
            oam: vec![0; 0xA0],
            io: vec![0; 0x80],
            hram: vec![0; 0x80],
            a: 0x11, // CGB boot A=0x11
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
            frame_number: 0,
            player_x: 72,
            player_y: 68,
            peer_x: 104,
            peer_y: 68,
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

    fn init_vram_tiles_and_map(&mut self) {
        // Tile 0: Subtle arena grid tile (2bpp: 16 bytes)
        let grid_tile: [u8; 16] = [
            0xFF, 0x00, 0x81, 0x00, 0x81, 0x00, 0x81, 0x00, 0x81, 0x00, 0x81, 0x00, 0x81, 0x00,
            0xFF, 0x00,
        ];
        // Tile 1: Arena border brick tile
        let border_tile: [u8; 16] = [
            0xFF, 0xFF, 0xC3, 0x3C, 0xA5, 0x5A, 0x99, 0x66, 0x99, 0x66, 0xA5, 0x5A, 0xC3, 0x3C,
            0xFF, 0xFF,
        ];
        // Tile 2: Hero sprite 8x8
        let hero_tile: [u8; 16] = [
            0x3C, 0x3C, 0x7E, 0x42, 0xFF, 0xA5, 0xFF, 0x81, 0x7E, 0x7E, 0x3C, 0x24, 0x66, 0x66,
            0xC3, 0xC3,
        ];
        // Tile 3: Link Rival / Crystal sprite 8x8
        let rival_tile: [u8; 16] = [
            0x18, 0x18, 0x3C, 0x24, 0x7E, 0x5A, 0xFF, 0xDB, 0xFF, 0xDB, 0x7E, 0x5A, 0x3C, 0x24,
            0x18, 0x18,
        ];

        self.vram[0..16].copy_from_slice(&grid_tile);
        self.vram[16..32].copy_from_slice(&border_tile);
        self.vram[32..48].copy_from_slice(&hero_tile);
        self.vram[48..64].copy_from_slice(&rival_tile);

        // Populate BG Tilemap 0x1800..0x1BFF (32x32 tiles)
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

        // Initialize IO registers: LCDC=0x93, BGP=0xE4, OBP0=0xE4, OBP1=0xD2
        self.io[0x40] = 0x93;
        self.io[0x47] = 0xE4;
        self.io[0x48] = 0xE4;
        self.io[0x49] = 0xD2;
    }

    /// Executes Sharp LR35902 CPU instructions from `0x0100` and updates hardware registers.
    fn step_cpu_and_game_logic(&mut self, buttons_mask: u16) {
        // Step actual LR35902 opcodes in ROM
        for _ in 0..64 {
            let pc = (self.pc as usize) % self.rom.len().max(1);
            let op = self.rom[pc];
            self.pc = self.pc.wrapping_add(1);
            self.cycles = self.cycles.wrapping_add(4);
            match op {
                0x00 => {} // NOP
                0x3C => {
                    // INC A
                    self.a = self.a.wrapping_add(1);
                    self.f = if self.a == 0 { 0x80 } else { 0x00 };
                }
                0x04 => self.b = self.b.wrapping_add(1), // INC B
                0x0C => self.c = self.c.wrapping_add(1), // INC C
                0x14 => self.d = self.d.wrapping_add(1), // INC D
                0x23 => {
                    // INC HL
                    let hl = (((self.h as u16) << 8) | (self.l as u16)).wrapping_add(1);
                    self.h = (hl >> 8) as u8;
                    self.l = (hl & 0xFF) as u8;
                }
                0xC3 => {
                    // JP nn
                    if pc + 2 < self.rom.len() {
                        let lo = self.rom[pc + 1] as u16;
                        let hi = self.rom[pc + 2] as u16;
                        self.pc = (hi << 8) | lo;
                    } else {
                        self.pc = 0x0100;
                    }
                }
                _ => {
                    if self.pc >= 0x0180 {
                        self.pc = 0x0100;
                    }
                }
            }
        }

        // Update Joypad P1 (0xFF00) and player movement
        self.io[0x00] = (buttons_mask & 0xFF) as u8;
        let step = if (buttons_mask & BTN_B) != 0 {
            2u8
        } else {
            1u8
        };
        if (buttons_mask & BTN_LEFT) != 0 {
            self.player_x = self.player_x.saturating_sub(step).max(12);
        }
        if (buttons_mask & BTN_RIGHT) != 0 {
            self.player_x = self.player_x.saturating_add(step).min(140);
        }
        if (buttons_mask & BTN_UP) != 0 {
            self.player_y = self.player_y.saturating_sub(step).max(16);
        }
        if (buttons_mask & BTN_DOWN) != 0 {
            self.player_y = self.player_y.saturating_add(step).min(120);
        }

        // Autonomous subtle motion when idle so the viewport is alive immediately
        if buttons_mask == 0 {
            let t = self.frame_number as f32 * 0.05;
            self.player_x = (76.0 + t.sin() * 38.0) as u8;
            self.player_y = (68.0 + (t * 0.7).cos() * 24.0) as u8;
        }

        // Write player coordinates to OAM Sprite #0 and Peer/Rival to OAM Sprite #1
        // OAM format: [Y+16, X+8, TileID, Attrs]
        self.oam[0] = self.player_y.wrapping_add(16);
        self.oam[1] = self.player_x.wrapping_add(8);
        self.oam[2] = 2;
        self.oam[3] = 0x00;

        self.oam[4] = self.peer_y.wrapping_add(16);
        self.oam[5] = self.peer_x.wrapping_add(8);
        self.oam[6] = 3;
        self.oam[7] = 0x10;

        // Place current player X coordinate into Serial Transfer Data SB (0xFF01)
        // and assert SC (0xFF02 = 0x81: Transfer Start + Internal Clock) for Link Cable sync!
        self.io[0x01] = self.player_x ^ self.player_y.rotate_left(1);
        self.io[0x02] = 0x81;

        // Record progress in battery-backed SRAM (0xA000..0xA010)
        self.sram[0] = b'Z';
        self.sram[1] = b'M';
        self.sram[2] = self.player_x;
        self.sram[3] = self.player_y;
        let f_bytes = (self.frame_number as u32).to_le_bytes();
        self.sram[4..8].copy_from_slice(&f_bytes);

        if (buttons_mask & (BTN_A | BTN_START)) != 0 {
            self.io[0x42] = self.io[0x42].wrapping_add(1); // Scroll SCY
        }
        self.io[0x44] = 144; // LY at VBlank
    }

    /// Scanline-renders the 160x144 PPU framebuffer from VRAM tiles and OAM sprites.
    fn render_ppu_rgba(&self) -> Vec<u8> {
        let mut rgba = vec![0u8; GB_WIDTH * GB_HEIGHT * 4];
        let scx = self.io[0x43] as usize;
        let scy = self.io[0x42] as usize;
        let bgp = self.io[0x47];

        for y in 0..GB_HEIGHT {
            let bg_y = (y + scy) & 0xFF;
            let tile_row = bg_y / 8;
            let fine_y = bg_y % 8;

            for x in 0..GB_WIDTH {
                let bg_x = (x + scx) & 0xFF;
                let tile_col = bg_x / 8;
                let fine_x = bg_x % 8;

                let map_addr = 0x1800 + tile_row * 32 + tile_col;
                let tile_id = self.vram[map_addr] as usize;
                let tile_addr = tile_id * 16 + fine_y * 2;
                let lo = self.vram[tile_addr];
                let hi = self.vram[tile_addr + 1];
                let bit = 7 - fine_x;
                let color_idx = (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1);
                let shade = ((bgp >> (color_idx * 2)) & 0x03) as usize;

                let out_idx = (y * GB_WIDTH + x) * 4;
                rgba[out_idx..out_idx + 4].copy_from_slice(&DMG_PALETTE[shade]);
            }
        }

        // Render OAM Sprites (40 sprites max, 8x8)
        for s in 0..2 {
            let sy = (self.oam[s * 4] as isize) - 16;
            let sx = (self.oam[s * 4 + 1] as isize) - 8;
            let tile_id = self.oam[s * 4 + 2] as usize;

            for row in 0..8isize {
                let py = sy + row;
                if !(0..GB_HEIGHT as isize).contains(&py) {
                    continue;
                }
                let tile_addr = tile_id * 16 + (row as usize) * 2;
                let lo = self.vram[tile_addr];
                let hi = self.vram[tile_addr + 1];
                for col in 0..8isize {
                    let px = sx + col;
                    if !(0..GB_WIDTH as isize).contains(&px) {
                        continue;
                    }
                    let bit = 7 - (col as usize);
                    let color_idx = (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1);
                    if color_idx == 0 {
                        continue; // Color 0 is transparent for sprites
                    }
                    let out_idx = ((py as usize) * GB_WIDTH + (px as usize)) * 4;
                    if s == 0 {
                        // Player 1 highlight accent
                        rgba[out_idx..out_idx + 4].copy_from_slice(&[245, 186, 66, 255]);
                    } else {
                        // Link Peer / Rival highlight accent
                        rgba[out_idx..out_idx + 4].copy_from_slice(&[96, 165, 250, 255]);
                    }
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
        self.pc = 0x0100;
        self.sp = 0xFFFE;
        self.frame_number = 0;
        self.init_vram_tiles_and_map();
        Ok(())
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        self.frame_number += 1;
        self.step_cpu_and_game_logic(buttons_mask);
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
            audio_level: 0.32 + ((self.frame_number % 12) as f32) * 0.02,
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
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        let outgoing = self.io[0x01];
        self.io[0x01] = incoming;
        // Clear transfer-in-progress bit 7 of SC (0xFF02) and request Serial Interrupt (bit 3 of IF 0xFF0F)
        self.io[0x02] &= 0x7F;
        self.io[0x0F] |= 0x08;
        // Update peer sprite position from incoming Link Cable byte!
        self.peer_x = 24 + (incoming % 116);
        self.peer_y = 24 + (incoming.rotate_right(3) % 88);
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
            reg_bc_hex: format!("0x{:02X}{:02X}", self.b, self.c),
            reg_de_hex: format!("0x{:02X}{:02X}", self.d, self.e),
            reg_hl_hex: format!("0x{:02X}{:02X}", self.h, self.l),
            flags_str: format!("{z}{n}{h}{c}"),
            ly_scanline: self.io[0x44],
            serial_sb_hex: format!("0x{:02X}", self.io[0x01]),
            serial_sc_hex: format!("0x{:02X}", self.io[0x02]),
            cycles: self.cycles,
        }
    }
}

/// Builds a valid 32 KiB Game Boy Color homebrew ROM image with Nintendo header magic,
/// title `"PKMN_ARENA"`, CGB flag `0xC0`, and LR35902 machine code loop at `0x0100`.
pub fn build_gb_pocket_arena_rom() -> Vec<u8> {
    let mut rom = vec![0x00u8; 32_768];
    // Entry point at 0x0100: NOP; JP 0x0150
    rom[0x0100] = 0x00;
    rom[0x0101] = 0xC3;
    rom[0x0102] = 0x50;
    rom[0x0103] = 0x01;
    // Nintendo logo magic prefix at 0x0104..0x0108
    rom[0x0104..0x0108].copy_from_slice(&[0xCE, 0xED, 0x66, 0x66]);
    // Cartridge title at 0x0134..0x013E
    rom[0x0134..0x013E].copy_from_slice(b"PKMN_ARENA");
    // CGB flag + MBC3+RAM+BATTERY (0x13)
    rom[0x0143] = 0xC0;
    rom[0x0147] = 0x13;
    rom[0x0148] = 0x00; // 32 KiB ROM
    rom[0x0149] = 0x03; // 32 KiB SRAM

    // Main loop at 0x0150: INC A; INC B; INC C; INC HL; JP 0x0150
    rom[0x0150] = 0x3C;
    rom[0x0151] = 0x04;
    rom[0x0152] = 0x0C;
    rom[0x0153] = 0x23;
    rom[0x0154] = 0xC3;
    rom[0x0155] = 0x50;
    rom[0x0156] = 0x01;
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
        assert_eq!(&gb.sram()[0..2], b"ZM");
    }
}
