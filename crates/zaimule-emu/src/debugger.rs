//! Visual VRAM Tilemap / Sprite Atlas Inspector & Step-by-Step Disassembler
//! for supported pure-Rust architectures: Sharp LR35902 (GB/GBC) and ARM7TDMI/ARM946E-S (GBA/NDS).

use crate::traits::CpuDebugSnapshot;
use serde::{Deserialize, Serialize};
use zaimule_core::catalog::ConsoleKind;

/// Single disassembled instruction row for the BGB/No$GBA-style step-by-step debugger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisassembledLine {
    pub addr: u32,
    pub addr_hex: String,
    pub bytes_hex: String,
    pub mnemonic: String,
    pub is_pc: bool,
    pub has_breakpoint: bool,
}

/// 128x64 RGBA8888 2bpp Tile Atlas (16x8 tiles of 8x8 pixels) + palette inspector snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VramInspectorSnapshot {
    pub atlas_width: u16,
    pub atlas_height: u16,
    pub tile_atlas_rgba: Vec<u8>,
    pub palette_hex: Vec<String>,
    pub active_tiles_count: usize,
}

/// Combined payload returned to the UI for the live VRAM & Disassembler drawer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebuggerPayload {
    pub cpu: CpuDebugSnapshot,
    pub disassembly: Vec<DisassembledLine>,
    pub breakpoints: Vec<u32>,
    pub vram: VramInspectorSnapshot,
}

/// Disassembles `count` instructions starting at `pc` for the active console architecture.
pub fn disassemble_window(
    console: ConsoleKind,
    rom: &[u8],
    pc: u32,
    breakpoints: &[u32],
    count: usize,
) -> Vec<DisassembledLine> {
    if !matches!(
        console,
        ConsoleKind::Gb | ConsoleKind::Gbc | ConsoleKind::Gba | ConsoleKind::Nds
    ) {
        return Vec::new();
    }

    let mut lines = Vec::with_capacity(count);
    let mut cursor = pc;

    for i in 0..count {
        let (step_len, bytes_hex, mnemonic) = match console {
            ConsoleKind::Gba => decode_arm32_gba(rom, cursor),
            ConsoleKind::Nds => decode_arm32_nds(rom, cursor),
            ConsoleKind::Gb | ConsoleKind::Gbc => decode_lr35902(rom, cursor),
            _ => unreachable!(),
        };

        let addr_hex = match console {
            ConsoleKind::Gba | ConsoleKind::Nds => format!("0x{cursor:08X}"),
            _ => format!("0x{:04X}", cursor & 0xFFFF),
        };

        lines.push(DisassembledLine {
            addr: cursor,
            addr_hex,
            bytes_hex,
            mnemonic,
            is_pc: i == 0,
            has_breakpoint: breakpoints.contains(&cursor),
        });

        cursor = cursor.wrapping_add(step_len);
    }

    lines
}

fn read_rom_byte(rom: &[u8], idx: usize) -> u8 {
    if rom.is_empty() {
        0x00
    } else {
        rom[idx % rom.len()]
    }
}

fn decode_lr35902(rom: &[u8], pc: u32) -> (u32, String, String) {
    let idx = (pc as usize) & 0x7FFF;
    let op = read_rom_byte(rom, idx);
    let b1 = read_rom_byte(rom, idx + 1);
    let b2 = read_rom_byte(rom, idx + 2);
    let u16_imm = u16::from_le_bytes([b1, b2]);

    match op {
        0x00 => (1, "00".into(), "NOP".into()),
        0x01 => (
            3,
            format!("01 {b1:02X} {b2:02X}"),
            format!("LD BC, ${u16_imm:04X}"),
        ),
        0x04 => (1, "04".into(), "INC B".into()),
        0x05 => (1, "05".into(), "DEC B".into()),
        0x06 => (2, format!("06 {b1:02X}"), format!("LD B, #${b1:02X}")),
        0x0E => (2, format!("0E {b1:02X}"), format!("LD C, #${b1:02X}")),
        0x11 => (
            3,
            format!("11 {b1:02X} {b2:02X}"),
            format!("LD DE, ${u16_imm:04X}"),
        ),
        0x18 => {
            let rel = b1 as i8;
            let dest = (pc as i32 + 2 + rel as i32) as u16;
            (2, format!("18 {b1:02X}"), format!("JR ${dest:04X}"))
        }
        0x20 => {
            let rel = b1 as i8;
            let dest = (pc as i32 + 2 + rel as i32) as u16;
            (2, format!("20 {b1:02X}"), format!("JR NZ, ${dest:04X}"))
        }
        0x21 => (
            3,
            format!("21 {b1:02X} {b2:02X}"),
            format!("LD HL, ${u16_imm:04X}"),
        ),
        0x23 => (1, "23".into(), "INC HL".into()),
        0x31 => (
            3,
            format!("31 {b1:02X} {b2:02X}"),
            format!("LD SP, ${u16_imm:04X}"),
        ),
        0x3E => (2, format!("3E {b1:02X}"), format!("LD A, #${b1:02X}")),
        0x76 => (1, "76".into(), "HALT".into()),
        0xAF => (1, "AF".into(), "XOR A, A".into()),
        0xC3 => (
            3,
            format!("C3 {b1:02X} {b2:02X}"),
            format!("JP ${u16_imm:04X}"),
        ),
        0xC9 => (1, "C9".into(), "RET".into()),
        0xCB => {
            let cb_mnem = match b1 {
                0x37 => "SWAP A".into(),
                0x87 => "RES 0, A".into(),
                0xC7 => "SET 0, A".into(),
                _ => format!("CB ${b1:02X}"),
            };
            (2, format!("CB {b1:02X}"), cb_mnem)
        }
        0xCD => (
            3,
            format!("CD {b1:02X} {b2:02X}"),
            format!("CALL ${u16_imm:04X}"),
        ),
        0xE0 => (2, format!("E0 {b1:02X}"), format!("LDH ($FF{b1:02X}), A")),
        0xEA => (
            3,
            format!("EA {b1:02X} {b2:02X}"),
            format!("LD (${u16_imm:04X}), A"),
        ),
        0xF0 => (2, format!("F0 {b1:02X}"), format!("LDH A, ($FF{b1:02X})")),
        0xF3 => (1, "F3".into(), "DI".into()),
        0xFB => (1, "FB".into(), "EI".into()),
        0xFE => (2, format!("FE {b1:02X}"), format!("CP #${b1:02X}")),
        _ => (1, format!("{op:02X}"), format!("DB ${op:02X}")),
    }
}

fn decode_arm32_word(word: u32, pc: u32) -> String {
    if (word & 0x0E00_0000) == 0x0A00_0000 {
        let link = if (word & 0x0100_0000) != 0 { "BL" } else { "B" };
        let raw24 = (word & 0x00FF_FFFF) as i32;
        let signed_offset = (raw24 << 8) >> 6;
        let target = (pc as i32).wrapping_add(8).wrapping_add(signed_offset) as u32;
        return format!("{link} 0x{target:08X}");
    }

    if (word & 0x0C00_0000) == 0x0000_0000 {
        let opcode = (word >> 21) & 0x0F;
        let rd = (word >> 12) & 0x0F;
        let rn = (word >> 16) & 0x0F;
        let imm = word & 0xFF;
        return match opcode {
            0x04 => format!("ADD R{rd}, R{rn}, #{imm}"),
            0x02 => format!("SUB R{rd}, R{rn}, #{imm}"),
            0x0D => format!("MOV R{rd}, #{imm}"),
            _ => format!("ALU R{rd}, R{rn}, #{imm}"),
        };
    }

    format!("DCD 0x{word:08X}")
}

fn decode_arm32_gba(rom: &[u8], pc: u32) -> (u32, String, String) {
    let offset = (pc.saturating_sub(0x0800_0000) as usize) & !3;
    let b0 = read_rom_byte(rom, offset);
    let b1 = read_rom_byte(rom, offset + 1);
    let b2 = read_rom_byte(rom, offset + 2);
    let b3 = read_rom_byte(rom, offset + 3);
    let word = u32::from_le_bytes([b0, b1, b2, b3]);
    let hex = format!("{b3:02X}{b2:02X}{b1:02X}{b0:02X}");
    (4, hex, decode_arm32_word(word, pc))
}

fn decode_arm32_nds(rom: &[u8], pc: u32) -> (u32, String, String) {
    let offset = if rom.len() >= 0x40 {
        let arm9_off = u32::from_le_bytes([rom[0x20], rom[0x21], rom[0x22], rom[0x23]]);
        let arm9_addr = u32::from_le_bytes([rom[0x28], rom[0x29], rom[0x2A], rom[0x2B]]);
        let arm9_size = u32::from_le_bytes([rom[0x2C], rom[0x2D], rom[0x2E], rom[0x2F]]);
        if pc >= arm9_addr && pc < arm9_addr.saturating_add(arm9_size) {
            (arm9_off.saturating_add(pc - arm9_addr) as usize) & !3
        } else {
            (pc.saturating_sub(0x0200_0000) as usize) & !3
        }
    } else {
        (pc.saturating_sub(0x0200_0000) as usize) & !3
    };
    let b0 = read_rom_byte(rom, offset);
    let b1 = read_rom_byte(rom, offset + 1);
    let b2 = read_rom_byte(rom, offset + 2);
    let b3 = read_rom_byte(rom, offset + 3);
    let word = u32::from_le_bytes([b0, b1, b2, b3]);
    let hex = format!("{b3:02X}{b2:02X}{b1:02X}{b0:02X}");
    (4, hex, decode_arm32_word(word, pc))
}

const INSPECTOR_PALETTE: [[u8; 4]; 4] = [
    [9, 12, 16, 255],
    [56, 189, 248, 255],
    [52, 211, 153, 255],
    [245, 186, 66, 255],
];

/// Renders a `128x64` RGBA8888 2bpp tile sheet (16 columns x 8 rows = 128 tiles of 8x8px)
/// from the active core's VRAM buffer.
pub fn build_vram_inspector_snapshot(vram: &[u8]) -> VramInspectorSnapshot {
    let atlas_width: usize = 128;
    let atlas_height: usize = 64;
    let mut rgba = vec![0u8; atlas_width * atlas_height * 4];
    let mut active_tiles_count = 0usize;

    for tile_idx in 0..128usize {
        let tile_col = tile_idx % 16;
        let tile_row = tile_idx / 16;
        let base_addr = tile_idx * 16;

        let mut non_zero = false;
        for row in 0..8usize {
            let (lo, hi) = if base_addr + row * 2 + 1 < vram.len() {
                (vram[base_addr + row * 2], vram[base_addr + row * 2 + 1])
            } else {
                (0, 0)
            };
            if lo != 0 || hi != 0 {
                non_zero = true;
            }
            for col in 0..8usize {
                let bit = 7 - col;
                let color_idx = ((((hi >> bit) & 1) << 1) | ((lo >> bit) & 1)) as usize;
                let px = tile_col * 8 + col;
                let py = tile_row * 8 + row;
                let out_idx = (py * atlas_width + px) * 4;
                rgba[out_idx..out_idx + 4].copy_from_slice(&INSPECTOR_PALETTE[color_idx & 3]);
            }
        }
        if non_zero {
            active_tiles_count += 1;
        }
    }

    VramInspectorSnapshot {
        atlas_width: atlas_width as u16,
        atlas_height: atlas_height as u16,
        tile_atlas_rgba: rgba,
        palette_hex: vec![
            "#090C10".into(),
            "#38BDF8".into(),
            "#34D399".into(),
            "#F5BA42".into(),
        ],
        active_tiles_count,
    }
}

/// Computes the two `2bpp` planar bitplane byte mutations `[(lo_addr, lo_byte), (hi_addr, hi_byte)]`
/// required to paint pixel `(atlas_x, atlas_y)` (`0..128`, `0..64`) with `color_idx` (`0..=3`).
pub fn compute_2bpp_pixel_mutation(
    vram: &[u8],
    atlas_x: u16,
    atlas_y: u16,
    color_idx: u8,
) -> Option<[(usize, u8); 2]> {
    if atlas_x >= 128 || atlas_y >= 64 {
        return None;
    }
    let tile_col = (atlas_x / 8) as usize;
    let tile_row = (atlas_y / 8) as usize;
    let fine_x = (atlas_x % 8) as usize;
    let fine_y = (atlas_y % 8) as usize;

    let tile_idx = tile_row * 16 + tile_col;
    let lo_addr = tile_idx * 16 + fine_y * 2;
    let hi_addr = lo_addr + 1;
    if hi_addr >= vram.len() {
        return None;
    }

    let bit_mask = 1u8 << (7 - fine_x);
    let mut lo = vram[lo_addr] & !bit_mask;
    let mut hi = vram[hi_addr] & !bit_mask;
    if (color_idx & 1) != 0 {
        lo |= bit_mask;
    }
    if (color_idx & 2) != 0 {
        hi |= bit_mask;
    }
    Some([(lo_addr, lo), (hi_addr, hi)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disassembler_decodes_lr35902_and_arm7tdmi_mnemonics() {
        let mut gb_rom = vec![0u8; 0x8000];
        gb_rom[0x0150..0x0155].copy_from_slice(&[0xF0, 0x00, 0x04, 0x18, 0xFE]);
        let gb_lines = disassemble_window(ConsoleKind::Gbc, &gb_rom, 0x0150, &[0x0152], 3);
        assert_eq!(gb_lines.len(), 3);
        assert!(gb_lines[0].is_pc);
        assert_eq!(gb_lines[0].mnemonic, "LDH A, ($FF00)");
        assert_eq!(gb_lines[1].mnemonic, "INC B");
        assert!(gb_lines[1].has_breakpoint);
        assert_eq!(gb_lines[2].mnemonic, "JR $0153");

        let mut gba_rom = vec![0u8; 0x100];
        gba_rom[0xC0..0xC4].copy_from_slice(&0xE282_2001u32.to_le_bytes()); // ADD R2, R2, #1
        gba_rom[0xC4..0xC8].copy_from_slice(&0xEAFF_FFFDu32.to_le_bytes()); // B 0x0800_00C0
        let gba_lines = disassemble_window(ConsoleKind::Gba, &gba_rom, 0x0800_00C0, &[], 2);
        assert_eq!(gba_lines[0].mnemonic, "ADD R2, R2, #1");
        assert_eq!(gba_lines[1].mnemonic, "B 0x080000C0");

        let mut nds_rom = vec![0u8; 0x300];
        nds_rom[0x20..0x24].copy_from_slice(&0x200u32.to_le_bytes());
        nds_rom[0x28..0x2C].copy_from_slice(&0x0200_0800u32.to_le_bytes());
        nds_rom[0x2C..0x30].copy_from_slice(&0x20u32.to_le_bytes());
        nds_rom[0x200..0x204].copy_from_slice(&0xE3A0_1055u32.to_le_bytes()); // MOV R1, #85
        let nds_lines = disassemble_window(ConsoleKind::Nds, &nds_rom, 0x0200_0800, &[], 1);
        assert_eq!(nds_lines[0].mnemonic, "MOV R1, #85");
    }

    #[test]
    fn vram_inspector_renders_128x64_rgba_tile_atlas() {
        let mut vram = vec![0u8; 2048];
        vram[0..16].copy_from_slice(&[0xFF; 16]);
        let snap = build_vram_inspector_snapshot(&vram);
        assert_eq!(snap.atlas_width, 128);
        assert_eq!(snap.atlas_height, 64);
        assert_eq!(snap.tile_atlas_rgba.len(), 128 * 64 * 4);
        assert_eq!(snap.active_tiles_count, 1);
    }

    #[test]
    fn vram_pixel_mutation_updates_2bpp_bitplanes_accurately() {
        let mut vram = vec![0u8; 2048];
        let edits = compute_2bpp_pixel_mutation(&vram, 0, 0, 3).unwrap();
        for (addr, val) in edits {
            vram[addr] = val;
        }
        assert_eq!(vram[0], 0x80);
        assert_eq!(vram[1], 0x80);
    }
}
