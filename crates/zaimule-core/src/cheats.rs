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

/// Built-in GameShark codes ready to toggle in Zaimule.
pub fn default_cheat_codes() -> Vec<CheatCodeEntry> {
    vec![
        CheatCodeEntry {
            id: "cheat-c104-ff".into(),
            label: "⚡ Tir Plasma Continu & Cadence Maximale".into(),
            code_text: "010104C1".into(),
            addr: 0xC104,
            value: 0x01,
            enabled: false,
        },
        CheatCodeEntry {
            id: "cheat-c108-64".into(),
            label: "💎 Bonus Score Instantané (+100 pts SRAM)".into(),
            code_text: "016404A0".into(),
            addr: 0xA004,
            value: 0x64,
            enabled: false,
        },
    ]
}

/// Built-in RetroAchievements tracked live during gameplay.
pub fn default_achievements() -> Vec<AchievementEntry> {
    vec![
        AchievementEntry {
            id: "ach-first-orb".into(),
            title: "Premier Contact Phosphore".into(),
            description: "Atteindre 10 points dans l'arène ou sur une ROM active.".into(),
            points: 10,
            target_score: 10,
            unlocked: false,
            badge_symbol: "🟢".into(),
        },
        AchievementEntry {
            id: "ach-plasma-hunter".into(),
            title: "Chasseur Plasma 60 FPS".into(),
            description: "Dépasser 50 points en détruisant les orbes au tir A.".into(),
            points: 25,
            target_score: 50,
            unlocked: false,
            badge_symbol: "🔥".into(),
        },
        AchievementEntry {
            id: "ach-link-master".into(),
            title: "Maître du Câble Link".into(),
            description: "Atteindre 100 points ou synchroniser une session 2P.".into(),
            points: 50,
            target_score: 100,
            unlocked: false,
            badge_symbol: "🏆".into(),
        },
    ]
}

/// Resolves a deterministic Libretro No-Intro thumbnail URL for a given console and game title.
pub fn resolve_cover_thumbnail_url(console_badge: &str, title: &str) -> String {
    let repo_dir = match console_badge {
        "GB" => "Nintendo - Game Boy",
        "GBC" => "Nintendo - Game Boy Color",
        "GBA" => "Nintendo - Game Boy Advance",
        "NES" => "Nintendo - Nintendo Entertainment System",
        "SNES" => "Nintendo - Super Nintendo Entertainment System",
        "NDS" => "Nintendo - Nintendo DS",
        "PS1" => "Sony - PlayStation",
        "N64" => "Nintendo - Nintendo 64",
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
    fn scans_ram_for_matching_bytes() {
        let ram = [0x00, 0x42, 0x10, 0x42, 0xFF];
        let hits = scan_ram_for_exact_byte(&ram, 0xC000, 0x42, 10);
        assert_eq!(hits, vec![0xC001, 0xC003]);
    }
}
