//! GameShark / Action Replay RAM cheat code parser, live RAM value scanner (Cheat Engine style),
//! RetroAchievements condition evaluator, and Libretro No-Intro thumbnail resolver.

use serde::{Deserialize, Serialize};

/// A parsed GameShark / Action Replay / Raw RAM cheat code (`01VVAAAA` or `AAAA:VV`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheatCodeEntry {
    pub id: String,
    pub label: String,
    pub code_text: String,
    pub addr: u16,
    pub value: u8,
    pub enabled: bool,
}

/// RetroAchievement definition evaluated against live emulator state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementEntry {
    pub id: String,
    pub title: String,
    pub description: String,
    pub points: u32,
    pub target_score: u32,
    pub unlocked: bool,
    pub badge_symbol: String,
}

/// Parses a Game Boy GameShark code (`01VVLLHH`, e.g. `016300C1` -> write `0x63` to `0xC100`)
/// or a direct `AAAA:VV` / `0xAAAA=0xVV` hex expression.
pub fn parse_cheat_code(raw_code: &str, label: &str) -> Result<CheatCodeEntry, String> {
    let cleaned: String = raw_code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();

    if cleaned.len() == 8 && cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        // Game Boy GameShark format: 01 [VV] [LL] [HH]
        let prefix = u8::from_str_radix(&cleaned[0..2], 16).map_err(|e| e.to_string())?;
        if prefix != 0x01 && prefix != 0x00 && prefix != 0x91 {
            return Err(format!(
                "préfixe GameShark non supporté (attendu 01/91, reçu {prefix:02X})"
            ));
        }
        let value = u8::from_str_radix(&cleaned[2..4], 16).map_err(|e| e.to_string())?;
        let lo = u8::from_str_radix(&cleaned[4..6], 16).map_err(|e| e.to_string())?;
        let hi = u8::from_str_radix(&cleaned[6..8], 16).map_err(|e| e.to_string())?;
        let addr = ((hi as u16) << 8) | (lo as u16);

        return Ok(CheatCodeEntry {
            id: format!("cheat-{addr:04x}-{value:02x}"),
            label: if label.trim().is_empty() {
                format!("GameShark @{addr:04X} = {value:02X}")
            } else {
                label.trim().to_string()
            },
            code_text: cleaned.to_ascii_uppercase(),
            addr,
            value,
            enabled: true,
        });
    }

    if let Some((left, right)) = cleaned.split_once(':').or_else(|| cleaned.split_once('=')) {
        let l = left.trim_start_matches("0x").trim_start_matches("0X");
        let r = right.trim_start_matches("0x").trim_start_matches("0X");
        let addr = u16::from_str_radix(l, 16)
            .map_err(|_| format!("adresse hexadécimale invalide: {l}"))?;
        let value =
            u8::from_str_radix(r, 16).map_err(|_| format!("valeur hexadécimale invalide: {r}"))?;
        return Ok(CheatCodeEntry {
            id: format!("cheat-{addr:04x}-{value:02x}"),
            label: if label.trim().is_empty() {
                format!("Patch RAM @{addr:04X} = {value:02X}")
            } else {
                label.trim().to_string()
            },
            code_text: format!("{addr:04X}:{value:02X}"),
            addr,
            value,
            enabled: true,
        });
    }

    Err("format de code invalide (utilisez GameShark '01FF00C1' ou 'C100:FF')".into())
}

/// Scans a RAM slice for all offsets matching `target_value` (up to `max_results` hits).
pub fn scan_ram_for_exact_byte(
    ram: &[u8],
    base_addr: u16,
    target_value: u8,
    max_results: usize,
) -> Vec<u16> {
    ram.iter()
        .enumerate()
        .filter(|(_, &b)| b == target_value)
        .take(max_results)
        .map(|(idx, _)| base_addr.wrapping_add(idx as u16))
        .collect()
}

/// Starts empty so only user-added GameShark/RAM codes appear.
pub fn default_cheat_codes() -> Vec<CheatCodeEntry> {
    Vec::new()
}

/// Starts empty with zero pre-populated fake achievements.
pub fn default_achievements() -> Vec<AchievementEntry> {
    Vec::new()
}

/// Live RAM Watchpoint with delta tracking (`previous_value -> current_value`) and Freeze/Pin support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryWatchpoint {
    pub id: String,
    pub label: String,
    pub addr: u16,
    pub addr_hex: String,
    pub current_value: u8,
    pub previous_value: u8,
    pub frozen: bool,
    pub frozen_value: u8,
}

/// Parses a 16-bit hexadecimal RAM watchpoint address (e.g. `"0xC104"` or `"C104"`).
pub fn parse_watchpoint_address(raw_addr: &str) -> Result<u16, String> {
    let cleaned = raw_addr
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if cleaned.is_empty() || cleaned.len() > 4 {
        return Err("adresse Watchpoint invalide (ex: 0xC104)".into());
    }
    u16::from_str_radix(cleaned, 16)
        .map_err(|_| format!("adresse hexadécimale Watchpoint invalide : {raw_addr}"))
}

/// Starts empty so only user-created watchpoints exist.
pub fn default_memory_watchpoints() -> Vec<MemoryWatchpoint> {
    Vec::new()
}

/// Resolves a deterministic Libretro No-Intro thumbnail URL for a supported console and game title.
pub fn resolve_cover_thumbnail_url(console_badge: &str, title: &str) -> String {
    let repo_dir = match console_badge {
        "GB" => "Nintendo - Game Boy",
        "GBC" => "Nintendo - Game Boy Color",
        "GBA" => "Nintendo - Game Boy Advance",
        "NDS" => "Nintendo - Nintendo DS",
        "SWITCH" | "SWITCH2" => "Nintendo - Nintendo Switch",
        _ => "Nintendo - Game Boy Color",
    };
    let sanitized: String = title
        .chars()
        .map(|c| match c {
            '&' | '*' | '/' | ':' | '`' | '<' | '>' | '?' | '\\' | '|' => '_',
            _ => c,
        })
        .collect();
    format!(
        "https://thumbnails.libretro.com/{}/Named_Boxarts/{}.png",
        repo_dir.replace(' ', "%20"),
        sanitized.trim().replace(' ', "%20")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gameshark_and_colon_cheat_codes_accurately() {
        let gs = parse_cheat_code("0163-00C1", "Inf HP").unwrap();
        assert_eq!(gs.addr, 0xC100);
        assert_eq!(gs.value, 0x63);
        assert_eq!(gs.label, "Inf HP");

        let raw = parse_cheat_code("0xC102:0x2A", "").unwrap();
        assert_eq!(raw.addr, 0xC102);
        assert_eq!(raw.value, 0x2A);
    }

    #[test]
    fn scans_ram_for_matching_bytes_and_parses_watchpoints() {
        let ram = [0x00, 0x42, 0x10, 0x42, 0xFF];
        let hits = scan_ram_for_exact_byte(&ram, 0xC000, 0x42, 10);
        assert_eq!(hits, vec![0xC001, 0xC003]);

        assert_eq!(parse_watchpoint_address("0xC108").unwrap(), 0xC108);
        assert!(default_cheat_codes().is_empty());
        assert!(default_achievements().is_empty());
        assert!(default_memory_watchpoints().is_empty());
    }
}
