//! Multi-console ROM catalog for supported Zaimule platforms (`GB`, `GBC`, `GBA`, `NDS`,
//! `SWITCH`, `SWITCH2`), IEEE 802.3 CRC32 hashing, cartridge header parser, and search filter.

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const FULL_CRC_MAX_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB
const SAMPLE_WINDOW_BYTES: usize = 2 * 1024 * 1024; // 2 MiB per sample slice for multi-GB Switch files

/// Supported console platforms across pure-Rust native cores and Option-A Switch vault launchers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ConsoleKind {
    Gb,
    Gbc,
    Gba,
    Nds,
    Switch,
    Switch2,
}

impl ConsoleKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Gb => "Game Boy (DMG)",
            Self::Gbc => "Game Boy Color",
            Self::Gba => "Game Boy Advance",
            Self::Nds => "Nintendo DS",
            Self::Switch => "Nintendo Switch (Torzu)",
            Self::Switch2 => "Switch 2 (Fritchii)",
        }
    }

    pub fn default_core_id(self) -> &'static str {
        match self {
            Self::Gb | Self::Gbc => "zaimule-rust-gb",
            Self::Gba => "zaimule-rust-gba",
            Self::Nds => "zaimule-rust-nds",
            Self::Switch => "torzu-external",
            Self::Switch2 => "fritchii-external",
        }
    }

    pub fn is_native_rust(self) -> bool {
        matches!(self, Self::Gb | Self::Gbc | Self::Gba | Self::Nds)
    }

    pub fn is_external_only(self) -> bool {
        matches!(self, Self::Switch | Self::Switch2)
    }

    pub fn supports_link_cable(self) -> bool {
        matches!(self, Self::Gb | Self::Gbc | Self::Gba)
    }
}

/// Execution engine family for a game entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CoreEngineKind {
    NativeRust,
    ExternalLauncher,
}

/// Per-game Switch engine preference inside Option-A Vault (`Auto` picks Torzu for Switch 1, Fritchii for Switch 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SwitchEngineOverride {
    #[default]
    Auto,
    Torzu,
    Fritchii,
}

/// Cartridge/image metadata entry in the user's library.
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
    #[serde(default)]
    pub rom_path: Option<String>,
    #[serde(default)]
    pub archive_entry: Option<String>,
    #[serde(default)]
    pub title_id: Option<String>,
    #[serde(default)]
    pub fast_fingerprint: bool,
    #[serde(default)]
    pub switch_engine: SwitchEngineOverride,
}

/// Rich inspection metadata extracted from a ROM header + streaming CRC32 fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedRomFile {
    pub console: ConsoleKind,
    pub title: String,
    pub region: String,
    pub crc32_hex: String,
    pub size_bytes: u64,
    pub title_id: Option<String>,
    pub fast_fingerprint: bool,
}

/// Computes the standard IEEE 802.3 CRC32 checksum of a ROM buffer using SIMD-accelerated hashing.
pub fn compute_crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
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

/// Returns true when the header looks like a Nintendo DS Nitro cartridge header.
pub fn is_nds_nitro_header(rom_bytes: &[u8]) -> bool {
    if rom_bytes.len() >= 0x160 {
        let logo_crc_ok = rom_bytes[0x15C] == 0x56 && rom_bytes[0x15D] == 0xCF;
        let logo_prefix_ok = rom_bytes[0x00C0..0x00C4] == [0x24, 0xFF, 0xAE, 0x51];
        let code_ok = rom_bytes[0x000C..0x0010]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        if (logo_crc_ok || logo_prefix_ok) && code_ok {
            return true;
        }
    }
    false
}

/// Extracts a 4-character Nintendo DS Gamecode (e.g. `CPUE` / `CPUF` for Pokémon Platinum).
pub fn extract_nds_game_code(rom_bytes: &[u8]) -> Option<String> {
    if rom_bytes.len() < 0x10 {
        return None;
    }
    let slice = &rom_bytes[0x000C..0x0010];
    if slice
        .iter()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        Some(String::from_utf8_lossy(slice).to_string())
    } else {
        None
    }
}

/// Extracts a Nintendo Switch Title ID (`0100XXXXXXXXXXXX`) if present in a filename.
pub fn extract_switch_title_id(file_name: &str) -> Option<String> {
    let bytes = file_name.as_bytes();
    if bytes.len() < 16 {
        return None;
    }
    for i in 0..=(bytes.len() - 16) {
        if &bytes[i..i + 4] == b"0100" && bytes[i..i + 16].iter().all(|b| b.is_ascii_hexdigit()) {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_hexdigit();
            let after_ok = i + 16 == bytes.len() || !bytes[i + 16].is_ascii_hexdigit();
            if before_ok && after_ok {
                return Some(String::from_utf8_lossy(&bytes[i..i + 16]).to_ascii_uppercase());
            }
        }
    }
    None
}

/// Returns `true` if a file name is a firmware, encryption key, or executable binary rather than a playable ROM.
pub fn is_known_system_or_bios_filename(file_name: &str) -> bool {
    let lower = file_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_name)
        .to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "prod.keys"
            | "title.keys"
            | "console.keys"
            | "dev.keys"
            | "bios7.bin"
            | "bios9.bin"
            | "firmware.bin"
            | "gba_bios.bin"
            | "dmg_boot.bin"
            | "cgb_boot.bin"
    ) || lower.ends_with(".keys")
        || lower.ends_with(".exe")
        || lower.ends_with(".dll")
        || lower.ends_with(".so")
        || lower.ends_with(".dylib")
        || lower.ends_with(".appimage")
}

/// Classifies an explicit ROM file extension among supported consoles only.
pub fn classify_rom_extension(file_name: &str) -> Option<ConsoleKind> {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".gbc") {
        Some(ConsoleKind::Gbc)
    } else if lower.ends_with(".gb") || lower.ends_with(".dmg") {
        Some(ConsoleKind::Gb)
    } else if lower.ends_with(".gba") {
        Some(ConsoleKind::Gba)
    } else if lower.ends_with(".nds") || lower.ends_with(".srl") {
        Some(ConsoleKind::Nds)
    } else if lower.ends_with(".nx") || lower.ends_with(".nup") {
        Some(ConsoleKind::Switch2)
    } else if lower.ends_with(".nsp")
        || lower.ends_with(".xci")
        || lower.ends_with(".nsz")
        || lower.ends_with(".xcz")
        || lower.ends_with(".nro")
        || lower.ends_with(".nso")
    {
        Some(ConsoleKind::Switch)
    } else {
        None
    }
}

/// Detects the console platform from a file name extension and/or ROM magic header bytes.
pub fn detect_console(file_name: &str, rom_bytes: &[u8]) -> Option<ConsoleKind> {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".nx") || lower.ends_with(".nup") {
        return Some(ConsoleKind::Switch2);
    }

    // 1. Check Nintendo Switch PFS0 (.nsp), XCI ("HEAD" @ 0x100), Homebrew NRO0/NSO0
    if rom_bytes.len() >= 4 && &rom_bytes[0..4] == b"PFS0" {
        return Some(ConsoleKind::Switch);
    }
    if rom_bytes.len() >= 0x0104 && &rom_bytes[0x0100..0x0104] == b"HEAD" {
        return Some(ConsoleKind::Switch);
    }
    if rom_bytes.len() >= 0x14 && &rom_bytes[0x10..0x14] == b"NRO0" {
        return Some(ConsoleKind::Switch);
    }
    if rom_bytes.len() >= 4 && &rom_bytes[0..4] == b"NSO0" {
        return Some(ConsoleKind::Switch);
    }

    // 2. Check Nintendo DS Nitro header
    if is_nds_nitro_header(rom_bytes) {
        return Some(ConsoleKind::Nds);
    }

    // 3. Check Game Boy / GBC Nintendo logo magic bytes at 0x0104..0x0108
    if rom_bytes.len() >= 0x0150 && rom_bytes[0x0104..0x0108] == [0xCE, 0xED, 0x66, 0x66] {
        let cgb_flag = rom_bytes[0x0143];
        if cgb_flag == 0x80 || cgb_flag == 0xC0 {
            return Some(ConsoleKind::Gbc);
        }
        return Some(ConsoleKind::Gb);
    }

    // 4. Check GBA Nintendo logo prefix + fixed byte 0x96 at offset 0x00B2
    if rom_bytes.len() >= 0x00C0
        && rom_bytes[0x0004..0x0008] == [0x24, 0xFF, 0xAE, 0x51]
        && rom_bytes[0x00B2] == 0x96
    {
        return Some(ConsoleKind::Gba);
    }

    classify_rom_extension(file_name)
}

fn stem_from_filename(file_name: &str) -> String {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    match base.rfind('.') {
        Some(idx) if idx > 0 => base[..idx].to_string(),
        _ => base.to_string(),
    }
}

fn clean_switch_title_stem(stem: &str) -> String {
    let mut out = String::with_capacity(stem.len());
    let mut in_bracket = false;
    for ch in stem.chars() {
        match ch {
            '[' => in_bracket = true,
            ']' => in_bracket = false,
            _ if !in_bracket => out.push(ch),
            _ => {}
        }
    }
    let normalized: String = out
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = normalized.trim_matches(['-', ' ', '_']).to_string();
    if trimmed.is_empty() {
        stem.to_string()
    } else {
        trimmed
    }
}

/// Extracts an internal title from GB/GBC/GBA/NDS/Switch metadata or falls back to a clean filename stem.
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

    let stem = stem_from_filename(file_name);
    if matches!(console, ConsoleKind::Switch | ConsoleKind::Switch2) {
        let clean = clean_switch_title_stem(&stem);
        let upper_stem = stem.to_ascii_uppercase();
        if upper_stem.contains("[UPD]") || upper_stem.contains("UPDATE") {
            return format!("{clean} [Mise à jour]");
        }
        if upper_stem.contains("[DLC]") {
            return format!("{clean} [DLC]");
        }
        return clean;
    }

    if console == ConsoleKind::Nds {
        let readable_stem = stem.replace('_', " ").trim().to_string();
        let generic_stem = readable_stem.is_empty()
            || readable_stem.eq_ignore_ascii_case("rom")
            || readable_stem.eq_ignore_ascii_case("game");
        if !generic_stem {
            return readable_stem;
        }
        if let Some(code) = extract_nds_game_code(rom_bytes) {
            if code.starts_with("CPU") {
                return "Pokémon Version Platine".into();
            }
        }
        if rom_bytes.len() >= 12 {
            let raw = &rom_bytes[0x0000..0x000C];
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
    }

    stem
}

fn infer_nds_region(game_code: Option<&str>) -> String {
    match game_code.and_then(|c| c.as_bytes().get(3).copied()) {
        Some(b'F') => "EUR · FR (Nitro NTR)".into(),
        Some(b'P') | Some(b'X') | Some(b'Y') => "EUR · Multi (Nitro NTR)".into(),
        Some(b'E') => "USA (Nitro NTR)".into(),
        Some(b'J') => "JPN (Nitro NTR)".into(),
        Some(b'D') => "EUR · DE (Nitro NTR)".into(),
        Some(b'I') => "EUR · IT (Nitro NTR)".into(),
        Some(b'S') => "EUR · ES (Nitro NTR)".into(),
        Some(b'K') => "KOR (Nitro NTR)".into(),
        _ => "Nintendo DS (NTR)".into(),
    }
}

/// Builds an `InspectedRomFile` from an already-read header slice and streaming CRC32 checksum.
pub fn inspect_rom_header_and_stats(
    file_name: &str,
    header_bytes: &[u8],
    size_bytes: u64,
    crc32: u32,
    fast_fingerprint: bool,
) -> Result<InspectedRomFile, String> {
    let console = detect_console(file_name, header_bytes)
        .ok_or_else(|| format!("format de ROM non pris en charge : {file_name}"))?;
    let title = extract_rom_title(file_name, console, header_bytes);
    let crc32_hex = format_crc32_hex(crc32);

    let (region, title_id) = match console {
        ConsoleKind::Nds => {
            let code = extract_nds_game_code(header_bytes);
            let reg = infer_nds_region(code.as_deref());
            (reg, code)
        }
        ConsoleKind::Switch => {
            let tid = extract_switch_title_id(file_name);
            let lower = file_name.to_ascii_lowercase();
            let fmt = if lower.ends_with(".xci") || lower.ends_with(".xcz") {
                "Cartouche XCI · Torzu (Switch 1)"
            } else if lower.ends_with(".nro") || lower.ends_with(".nso") {
                "Homebrew NRO · Torzu (Switch 1)"
            } else {
                "Image Switch NSP · Torzu (Switch 1)"
            };
            let tag = tid
                .as_deref()
                .map(|id| format!("{fmt} ({id})"))
                .unwrap_or_else(|| fmt.to_string());
            (tag, tid)
        }
        ConsoleKind::Switch2 => {
            let tid = extract_switch_title_id(file_name);
            let fmt = "Image Switch 2 · Fritchii / Lichii";
            let tag = tid
                .as_deref()
                .map(|id| format!("{fmt} ({id})"))
                .unwrap_or_else(|| fmt.to_string());
            (tag, tid)
        }
        _ => ("Import Local · CRC32 Vérifié".into(), None),
    };

    Ok(InspectedRomFile {
        console,
        title,
        region,
        crc32_hex,
        size_bytes,
        title_id,
        fast_fingerprint,
    })
}

/// Computes either a full streaming IEEE 802.3 CRC32 (for files <= 256 MiB)
/// or a fast sampled fingerprint (for huge multi-GB Switch `.nsp`/`.xci`/`.nx` images).
pub fn compute_file_fingerprint(path: &Path, size_bytes: u64) -> Result<(u32, bool), String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = crc32fast::Hasher::new();

    if size_bytes <= FULL_CRC_MAX_BYTES {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        return Ok((hasher.finalize(), false));
    }

    hasher.update(&size_bytes.to_le_bytes());
    let mut buf = vec![0u8; SAMPLE_WINDOW_BYTES];

    let n_head = file.read(&mut buf).map_err(|e| e.to_string())?;
    hasher.update(&buf[..n_head]);

    let mid_off = (size_bytes / 2).saturating_sub((SAMPLE_WINDOW_BYTES as u64) / 2);
    file.seek(SeekFrom::Start(mid_off))
        .map_err(|e| e.to_string())?;
    let n_mid = file.read(&mut buf).map_err(|e| e.to_string())?;
    hasher.update(&buf[..n_mid]);

    let tail_off = size_bytes.saturating_sub(SAMPLE_WINDOW_BYTES as u64);
    file.seek(SeekFrom::Start(tail_off))
        .map_err(|e| e.to_string())?;
    let n_tail = file.read(&mut buf).map_err(|e| e.to_string())?;
    hasher.update(&buf[..n_tail]);

    Ok((hasher.finalize(), true))
}

/// Inspects a ROM file on disk without loading large multi-MB/GB binaries into memory.
pub fn inspect_rom_file(path: &Path) -> Result<InspectedRomFile, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size_bytes = meta.len();
    if size_bytes == 0 {
        return Err(format!("fichier ROM vide : {}", path.display()));
    }

    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let header_len = size_bytes.min(64 * 1024) as usize;
    let mut header = vec![0u8; header_len];
    file.read_exact(&mut header).map_err(|e| e.to_string())?;
    drop(file);

    let (crc32, fast_fingerprint) = compute_file_fingerprint(path, size_bytes)?;
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    inspect_rom_header_and_stats(&file_name, &header, size_bytes, crc32, fast_fingerprint)
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
                || g.title_id
                    .as_deref()
                    .map(|tid| normalize_for_search(tid).contains(&q))
                    .unwrap_or(false)
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
    fn detects_gbc_gba_nds_switch_and_switch2_accurately() {
        let mut gbc_rom = vec![0u8; 0x0160];
        gbc_rom[0x0104..0x0108].copy_from_slice(&[0xCE, 0xED, 0x66, 0x66]);
        gbc_rom[0x0134..0x013F].copy_from_slice(b"POKEMON_GLD");
        gbc_rom[0x0143] = 0xC0;
        assert_eq!(detect_console("cart.rom", &gbc_rom), Some(ConsoleKind::Gbc));
        assert_eq!(
            extract_rom_title("cart.rom", ConsoleKind::Gbc, &gbc_rom),
            "POKEMON_GLD"
        );

        let mut gba_rom = vec![0u8; 0xC0];
        gba_rom[0x04..0x08].copy_from_slice(&[0x24, 0xFF, 0xAE, 0x51]);
        gba_rom[0xA0..0xA7].copy_from_slice(b"EMERALD");
        gba_rom[0xB2] = 0x96;
        assert_eq!(detect_console("game.gba", &gba_rom), Some(ConsoleKind::Gba));
        assert_eq!(
            extract_rom_title("game.gba", ConsoleKind::Gba, &gba_rom),
            "EMERALD"
        );

        let mut nsp = vec![0u8; 256];
        nsp[0..4].copy_from_slice(b"PFS0");
        let nsp_info = inspect_rom_header_and_stats(
            "Super Mario Odyssey [0100000000010000][v0].nsp",
            &nsp,
            3_800_000_000,
            0x1234_5678,
            true,
        )
        .unwrap();
        assert_eq!(nsp_info.console, ConsoleKind::Switch);
        assert_eq!(nsp_info.title, "Super Mario Odyssey");
        assert_eq!(nsp_info.title_id.as_deref(), Some("0100000000010000"));
        assert!(nsp_info.fast_fingerprint);

        let nx_info = inspect_rom_header_and_stats(
            "NextGenTitle [0100ABCD00001000].nx",
            &nsp,
            4_200_000_000,
            0x8765_4321,
            true,
        )
        .unwrap();
        assert_eq!(nx_info.console, ConsoleKind::Switch2);
        assert_eq!(nx_info.title, "NextGenTitle");

        let mut xci = vec![0u8; 0x120];
        xci[0x0100..0x0104].copy_from_slice(b"HEAD");
        assert_eq!(detect_console("cart.xci", &xci), Some(ConsoleKind::Switch));

        let mut nds = vec![0u8; 0x200];
        nds[0x0000..0x000A].copy_from_slice(b"POKEMON PL");
        nds[0x000C..0x0010].copy_from_slice(b"CPUF");
        nds[0x015C] = 0x56;
        nds[0x015D] = 0xCF;
        let nds_info = inspect_rom_header_and_stats(
            "Pokemon - Version Platine (France).nds",
            &nds,
            128 * 1024 * 1024,
            0xAABB_CCDD,
            false,
        )
        .unwrap();
        assert_eq!(nds_info.console, ConsoleKind::Nds);
        assert_eq!(nds_info.title, "Pokemon - Version Platine (France)");
        assert_eq!(nds_info.title_id.as_deref(), Some("CPUF"));
        assert!(nds_info.region.contains("FR"));
        assert_eq!(
            extract_rom_title("rom.nds", ConsoleKind::Nds, &nds),
            "Pokémon Version Platine"
        );
    }

    #[test]
    fn rejects_unknown_non_rom_extension_and_filters_accents() {
        assert!(inspect_rom_header_and_stats("readme.txt", b"hello", 5, 1, false).is_err());

        let sample = vec![GameEntry {
            id: "gba-emeraude".into(),
            title: "Pokémon Version Émeraude".into(),
            console: ConsoleKind::Gba,
            engine_kind: CoreEngineKind::NativeRust,
            core_id: "zaimule-rust-gba".into(),
            region: "EUR · FR".into(),
            crc32_hex: "1F1C08FB".into(),
            size_bytes: 16_777_216,
            sram_summary: "128 Ko Flash".into(),
            playtime_seconds: 0,
            favorite: true,
            link_cable_ready: true,
            embedded_rom_key: None,
            cover_accent: "#3A271C".into(),
            cover_symbol: "GBA".into(),
            rom_path: Some("/roms/emeraude.gba".into()),
            archive_entry: None,
            title_id: None,
            fast_fingerprint: false,
            switch_engine: SwitchEngineOverride::Auto,
        }];
        let hits = filter_catalog(&sample, Some(ConsoleKind::Gba), true, true, "emeraude");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn inspects_real_file_on_disk_without_full_ram_allocation() {
        let dir = std::env::temp_dir().join(format!("zaimule-catalog-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("Mario Kart 8 Deluxe [0100152000022000].nsp");
        let mut data = vec![0u8; 70_000];
        data[0..4].copy_from_slice(b"PFS0");
        std::fs::write(&file_path, &data).unwrap();

        let meta = inspect_rom_file(&file_path).unwrap();
        assert_eq!(meta.console, ConsoleKind::Switch);
        assert_eq!(meta.title, "Mario Kart 8 Deluxe");
        assert_eq!(meta.title_id.as_deref(), Some("0100152000022000"));
        assert_eq!(meta.size_bytes, 70_000);
        assert!(!meta.fast_fingerprint);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
