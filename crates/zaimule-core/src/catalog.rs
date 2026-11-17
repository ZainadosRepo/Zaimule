//! Multi-console ROM catalog, IEEE 802.3 CRC32 hashing, cartridge header parser,
//! and accent-insensitive search filter.

use serde::{Deserialize, Serialize};

/// Supported console platforms across native Rust cores and Libretro C-ABI cores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ConsoleKind {
    Chip8,
    Gb,
    Gbc,
    Gba,
    Nes,
    Snes,
    Nds,
    Ps1,
    N64,
    Psp,
    Megadrive,
}

impl ConsoleKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chip8 => "CHIP-8 / Super-CHIP",
            Self::Gb => "Game Boy (DMG)",
            Self::Gbc => "Game Boy Color",
            Self::Gba => "Game Boy Advance",
            Self::Nes => "Nintendo Entertainment System",
            Self::Snes => "Super Nintendo (SNES)",
            Self::Nds => "Nintendo DS",
            Self::Ps1 => "PlayStation 1",
            Self::N64 => "Nintendo 64",
            Self::Psp => "PlayStation Portable",
            Self::Megadrive => "Sega Mega Drive",
        }
    }

    pub fn default_core_id(self) -> &'static str {
        match self {
            Self::Chip8 => "zaimule-rust-chip8",
            Self::Gb | Self::Gbc => "zaimule-rust-gb",
            Self::Nes => "zaimule-rust-nes",
            Self::Gba => "mgba_libretro",
            Self::Snes => "snes9x_libretro",
            Self::Nds => "melonds_libretro",
            Self::Ps1 => "swanstation_libretro",
            Self::N64 => "mupen64plus_next_libretro",
            Self::Psp => "ppsspp_libretro",
            Self::Megadrive => "genesis_plus_gx_libretro",
        }
    }

    pub fn is_native_rust(self) -> bool {
        matches!(self, Self::Chip8 | Self::Gb | Self::Gbc | Self::Nes)
    }

    pub fn supports_link_cable(self) -> bool {
        matches!(self, Self::Gb | Self::Gbc | Self::Gba | Self::Nds)
    }
}

/// Execution engine family for a game entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CoreEngineKind {
    NativeRust,
    LibretroAbi,
}

/// Cartridge/disc metadata entry in the user's library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameEntry {
    pub id: String,
    pub title: String,
    pub console: ConsoleKind,
    pub engine_kind: CoreEngineKind,
    pub core_id: String,
    pub region: String,
    pub crc32_hex: String,
    pub size_bytes: u64,
    pub sram_summary: String,
    pub playtime_seconds: u64,
    pub favorite: bool,
    pub link_cable_ready: bool,
    pub embedded_rom_key: Option<String>,
    pub cover_accent: String,
    pub cover_symbol: String,
}

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// Computes the standard IEEE 802.3 CRC32 checksum of a ROM buffer.
pub fn compute_crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        let idx = ((crc ^ (b as u32)) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    !crc
}

/// Formats a CRC32 integer as an 8-character uppercase hex string.
pub fn format_crc32_hex(crc: u32) -> String {
    format!("{crc:08X}")
}

/// Normalizes text for fast accent-insensitive and case-insensitive search.
pub fn normalize_for_search(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let folded = match ch {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => {
                'a'
            }
            'ç' | 'Ç' => 'c',
            'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => 'i',
            'ñ' | 'Ñ' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => 'u',
            'ý' | 'ÿ' | 'Ý' => 'y',
            other => other.to_ascii_lowercase(),
        };
        out.push(folded);
    }
    out
}

/// Detects the console platform from a file name extension and/or ROM magic header bytes.
pub fn detect_console(file_name: &str, rom_bytes: &[u8]) -> ConsoleKind {
    // 1. Check iNES magic header ("NES\x1A")
    if rom_bytes.len() >= 16 && &rom_bytes[0..4] == b"NES\x1A" {
        return ConsoleKind::Nes;
    }
    // 2. Check Game Boy / GBC Nintendo logo magic bytes at 0x0104..0x010C
    if rom_bytes.len() >= 0x0150 && rom_bytes[0x0104..0x0108] == [0xCE, 0xED, 0x66, 0x66] {
        let cgb_flag = rom_bytes[0x0143];
        if cgb_flag == 0x80 || cgb_flag == 0xC0 {
            return ConsoleKind::Gbc;
        }
        return ConsoleKind::Gb;
    }

    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".gbc") {
        ConsoleKind::Gbc
    } else if lower.ends_with(".gb") || lower.ends_with(".dmg") {
        ConsoleKind::Gb
    } else if lower.ends_with(".gba") {
        ConsoleKind::Gba
    } else if lower.ends_with(".nes") {
        ConsoleKind::Nes
    } else if lower.ends_with(".sfc") || lower.ends_with(".smc") {
        ConsoleKind::Snes
    } else if lower.ends_with(".nds") {
        ConsoleKind::Nds
    } else if lower.ends_with(".bin") || lower.ends_with(".cue") || lower.ends_with(".chd") {
        ConsoleKind::Ps1
    } else if lower.ends_with(".z64") || lower.ends_with(".n64") {
        ConsoleKind::N64
    } else if lower.ends_with(".cso") || lower.ends_with(".iso") {
        ConsoleKind::Psp
    } else if lower.ends_with(".md") || lower.ends_with(".gen") {
        ConsoleKind::Megadrive
    } else {
        ConsoleKind::Chip8
    }
}

/// Extracts an internal ASCII title from a GB/GBC/GBA header when available, or falls back to the filename stem.
pub fn extract_rom_title(file_name: &str, console: ConsoleKind, rom_bytes: &[u8]) -> String {
    if matches!(console, ConsoleKind::Gb | ConsoleKind::Gbc) && rom_bytes.len() >= 0x0144 {
        let raw = &rom_bytes[0x0134..0x0143];
        let cleaned: String = raw
            .iter()
            .copied()
            .take_while(|&b| b != 0)
            .filter(|b| b.is_ascii_graphic() || *b == b' ')
            .map(char::from)
            .collect();
        let trimmed = cleaned.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if console == ConsoleKind::Gba && rom_bytes.len() >= 0x00AC {
        let raw = &rom_bytes[0x00A0..0x00AC];
        let cleaned: String = raw
            .iter()
            .copied()
            .take_while(|&b| b != 0)
            .filter(|b| b.is_ascii_graphic() || *b == b' ')
            .map(char::from)
            .collect();
        let trimmed = cleaned.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    match base.rfind('.') {
        Some(idx) if idx > 0 => base[..idx].to_string(),
        _ => base.to_string(),
    }
}

/// Builds the default curated library including playable embedded homebrew ROMs and showcase entries.
pub fn default_catalog() -> Vec<GameEntry> {
    vec![
        GameEntry {
            id: "gb-pocket-arena".into(),
            title: "Pocket Monster Link Arena (DMG/GBC)".into(),
            console: ConsoleKind::Gbc,
            engine_kind: CoreEngineKind::NativeRust,
            core_id: "zaimule-rust-gb".into(),
            region: "EUR · FR (Homebrew Inclus)".into(),
            crc32_hex: "DCE0682B".into(),
            size_bytes: 32_768,
            sram_summary: "32 Ko SRAM Atomique + Câble Link SIO".into(),
            playtime_seconds: 15_420,
            favorite: true,
            link_cable_ready: true,
            embedded_rom_key: Some("gb_pocket_arena".into()),
            cover_accent: "#1e3a8a".into(),
            cover_symbol: "🐉".into(),
        },
        GameEntry {
            id: "nes-cyber-knight".into(),
            title: "Chrono Knight 6502 (NES)".into(),
            console: ConsoleKind::Nes,
            engine_kind: CoreEngineKind::NativeRust,
            core_id: "zaimule-rust-nes".into(),
            region: "WORLD (Homebrew Inclus)".into(),
            crc32_hex: "A482910E".into(),
            size_bytes: 40_976,
            sram_summary: "8 Ko PRG-RAM + PPU 2C02".into(),
            playtime_seconds: 8_190,
            favorite: true,
            link_cable_ready: false,
            embedded_rom_key: Some("nes_chrono_knight".into()),
            cover_accent: "#78350f".into(),
            cover_symbol: "🗡️".into(),
        },
        GameEntry {
            id: "chip8-space-interceptor".into(),
            title: "Space Interceptor DX (CHIP-8)".into(),
            console: ConsoleKind::Chip8,
            engine_kind: CoreEngineKind::NativeRust,
            core_id: "zaimule-rust-chip8".into(),
            region: "WORLD (Homebrew Inclus)".into(),
            crc32_hex: "C800FA11".into(),
            size_bytes: 4_096,
            sram_summary: "Save State .zst Instantané".into(),
            playtime_seconds: 3_720,
            favorite: false,
            link_cable_ready: false,
            embedded_rom_key: Some("chip8_interceptor".into()),
            cover_accent: "#334155".into(),
            cover_symbol: "👾".into(),
        },
        GameEntry {
            id: "gba-emerald-fr".into(),
            title: "Pokémon Version Émeraude".into(),
            console: ConsoleKind::Gba,
            engine_kind: CoreEngineKind::LibretroAbi,
            core_id: "mgba_libretro".into(),
            region: "EUR · FR".into(),
            crc32_hex: "1F1C08FB".into(),
            size_bytes: 16_777_216,
            sram_summary: "128 Ko Flash (5 Backups .bak)".into(),
            playtime_seconds: 152_280,
            favorite: true,
            link_cable_ready: true,
            embedded_rom_key: Some("gb_pocket_arena".into()),
            cover_accent: "#064e3b".into(),
            cover_symbol: "💎".into(),
        },
        GameEntry {
            id: "snes-alttp-fr".into(),
            title: "The Legend of Zelda : A Link to the Past".into(),
            console: ConsoleKind::Snes,
            engine_kind: CoreEngineKind::LibretroAbi,
            core_id: "snes9x_libretro".into(),
            region: "EUR · FR".into(),
            crc32_hex: "D3974B58".into(),
            size_bytes: 1_048_576,
            sram_summary: "8 Ko SRAM Atomique".into(),
            playtime_seconds: 67_200,
            favorite: true,
            link_cable_ready: false,
            embedded_rom_key: Some("nes_chrono_knight".into()),
            cover_accent: "#14532d".into(),
            cover_symbol: "🛡️".into(),
        },
        GameEntry {
            id: "nds-mariokart".into(),
            title: "Mario Kart DS".into(),
            console: ConsoleKind::Nds,
            engine_kind: CoreEngineKind::LibretroAbi,
            core_id: "melonds_libretro".into(),
            region: "EUR · Multi5".into(),
            crc32_hex: "4A99C10E".into(),
            size_bytes: 33_554_432,
            sram_summary: "256 Ko EEPROM + Écran Tactile".into(),
            playtime_seconds: 57_000,
            favorite: false,
            link_cable_ready: true,
            embedded_rom_key: Some("gb_pocket_arena".into()),
            cover_accent: "#7f1d1d".into(),
            cover_symbol: "🏎️".into(),
        },
        GameEntry {
            id: "ps1-ff7-fr".into(),
            title: "Final Fantasy VII (Disque 1)".into(),
            console: ConsoleKind::Ps1,
            engine_kind: CoreEngineKind::LibretroAbi,
            core_id: "swanstation_libretro".into(),
            region: "EUR · FR".into(),
            crc32_hex: "9B8E71A2".into(),
            size_bytes: 681_574_400,
            sram_summary: "Carte Mémoire .mcr (Slots 1 & 2)".into(),
            playtime_seconds: 231_120,
            favorite: true,
            link_cable_ready: false,
            embedded_rom_key: Some("nes_chrono_knight".into()),
            cover_accent: "#134e4a".into(),
            cover_symbol: "☄️".into(),
        },
    ]
}

/// Filters games by console, engine type, Link Cable capability, and accent-insensitive query.
pub fn filter_catalog<'a>(
    entries: &'a [GameEntry],
    console_filter: Option<ConsoleKind>,
    only_native_rust: bool,
    only_link_ready: bool,
    query: &str,
) -> Vec<&'a GameEntry> {
    let q = normalize_for_search(query.trim());
    entries
        .iter()
        .filter(|g| {
            if let Some(c) = console_filter {
                if g.console != c {
                    return false;
                }
            }
            if only_native_rust && g.engine_kind != CoreEngineKind::NativeRust {
                return false;
            }
            if only_link_ready && !g.link_cable_ready {
                return false;
            }
            if q.is_empty() {
                return true;
            }
            normalize_for_search(&g.title).contains(&q)
                || normalize_for_search(&g.crc32_hex).contains(&q)
                || normalize_for_search(g.console.label()).contains(&q)
                || normalize_for_search(&g.region).contains(&q)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_ieee_standard_check_value() {
        assert_eq!(compute_crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(format_crc32_hex(compute_crc32(b"123456789")), "CBF43926");
    }

    #[test]
    fn detects_nes_and_gb_headers_accurately() {
        let mut nes_rom = vec![0u8; 32];
        nes_rom[0..4].copy_from_slice(b"NES\x1A");
        assert_eq!(detect_console("unknown.bin", &nes_rom), ConsoleKind::Nes);

        let mut gbc_rom = vec![0u8; 0x0160];
        gbc_rom[0x0104..0x0108].copy_from_slice(&[0xCE, 0xED, 0x66, 0x66]);
        gbc_rom[0x0134..0x013F].copy_from_slice(b"POKEMON_GLD");
        gbc_rom[0x0143] = 0xC0;
        assert_eq!(detect_console("cart.rom", &gbc_rom), ConsoleKind::Gbc);
        assert_eq!(
            extract_rom_title("cart.rom", ConsoleKind::Gbc, &gbc_rom),
            "POKEMON_GLD"
        );
    }

    #[test]
    fn accent_insensitive_search_finds_pokemon_emeraude() {
        let catalog = default_catalog();
        let hits = filter_catalog(&catalog, None, false, false, "emeraude");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "gba-emerald-fr");
    }
}
