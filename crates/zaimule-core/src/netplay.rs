//! P2P Rollback Netplay rooms (RomStation CloudPlay without ads/queues) and `zaimule://` deep links.

use serde::{Deserialize, Serialize};

/// Active or public multiplayer lobby entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetplayRoom {
    pub room_code: String,
    pub title: String,
    pub game_title: String,
    pub console: String,
    pub rom_crc32_hex: String,
    pub host_name: String,
    pub players_current: u8,
    pub players_max: u8,
    pub ping_ms: u16,
    pub rollback_frames: u8,
    pub link_cable_mode: bool,
    pub deep_link_uri: String,
}

/// Parsed `zaimule://` deep-link invitation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepLinkInvite {
    pub room_code: String,
    pub rom_crc32_hex: String,
    pub core_id: String,
}

/// Formats a shareable `zaimule://room/<CODE>?crc=<CRC32>&core=<CORE>` URI.
pub fn format_deep_link(room_code: &str, rom_crc32_hex: &str, core_id: &str) -> String {
    format!(
        "zaimule://room/{}?crc={}&core={}",
        room_code.trim().to_ascii_uppercase(),
        rom_crc32_hex.trim().to_ascii_uppercase(),
        core_id.trim()
    )
}

/// Parses a `zaimule://room/...` invitation link and validates its parameters.
pub fn parse_deep_link(uri: &str) -> Result<DeepLinkInvite, String> {
    let trimmed = uri.trim();
    let Some(rest) = trimmed.strip_prefix("zaimule://room/") else {
        return Err("le lien doit commencer par 'zaimule://room/'".into());
    };
    let (code_part, query_part) = match rest.split_once('?') {
        Some((c, q)) => (c, q),
        None => (rest, ""),
    };
    let room_code = code_part.trim().to_ascii_uppercase();
    if room_code.is_empty() {
        return Err("code de salon manquant dans le lien zaimule://".into());
    }

    let mut rom_crc32_hex = String::from("DCE0682B");
    let mut core_id = String::from("zaimule-rust-gb");
    for kv in query_part.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            match k.trim() {
                "crc" if !v.trim().is_empty() => rom_crc32_hex = v.trim().to_ascii_uppercase(),
                "core" if !v.trim().is_empty() => core_id = v.trim().to_string(),
                _ => {}
            }
        }
    }

    Ok(DeepLinkInvite {
        room_code,
        rom_crc32_hex,
        core_id,
    })
}

/// Default Netplay rooms available in the hub.
pub fn default_netplay_rooms() -> Vec<NetplayRoom> {
    vec![
        NetplayRoom {
            room_code: "PKMN-LINK-01".into(),
            title: "Colisée & Échange Câble Link (SIO 256 Kbps)".into(),
            game_title: "Pocket Monster Link Arena (DMG/GBC)".into(),
            console: "GBC".into(),
            rom_crc32_hex: "DCE0682B".into(),
            host_name: "Zainados".into(),
            players_current: 2,
            players_max: 2,
            ping_ms: 9,
            rollback_frames: 1,
            link_cable_mode: true,
            deep_link_uri: format_deep_link("PKMN-LINK-01", "DCE0682B", "zaimule-rust-gb"),
        },
        NetplayRoom {
            room_code: "SNES-CUP-88".into(),
            title: "Tournoi Rétro 60 FPS Sans File d'Attente".into(),
            game_title: "The Legend of Zelda : A Link to the Past".into(),
            console: "SNES".into(),
            rom_crc32_hex: "D3974B58".into(),
            host_name: "Kaelen_FR".into(),
            players_current: 1,
            players_max: 2,
            ping_ms: 14,
            rollback_frames: 1,
            link_cable_mode: false,
            deep_link_uri: format_deep_link("SNES-CUP-88", "D3974B58", "snes9x_libretro"),
        },
        NetplayRoom {
            room_code: "NDS-MK-42".into(),
            title: "Grand Prix Wi-Fi Local Émulé".into(),
            game_title: "Mario Kart DS".into(),
            console: "NDS".into(),
            rom_crc32_hex: "4A99C10E".into(),
            host_name: "RetroPulse".into(),
            players_current: 3,
            players_max: 4,
            ping_ms: 18,
            rollback_frames: 2,
            link_cable_mode: true,
            deep_link_uri: format_deep_link("NDS-MK-42", "4A99C10E", "melonds_libretro"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_link_roundtrips_room_crc_and_core() {
        let uri = format_deep_link("pkmn-link-01", "dce0682b", "zaimule-rust-gb");
        assert_eq!(
            uri,
            "zaimule://room/PKMN-LINK-01?crc=DCE0682B&core=zaimule-rust-gb"
        );
        let parsed = parse_deep_link(&uri).unwrap();
        assert_eq!(parsed.room_code, "PKMN-LINK-01");
        assert_eq!(parsed.rom_crc32_hex, "DCE0682B");
        assert_eq!(parsed.core_id, "zaimule-rust-gb");
    }
}
