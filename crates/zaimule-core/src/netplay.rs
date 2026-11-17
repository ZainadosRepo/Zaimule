//! P2P Rollback Netplay rooms (RomStation CloudPlay without ads/queues),
//! non-blocking UDP Rollback & Link Cable wire protocol (`ZML2`), and `zaimule://` deep links.

use crate::catalog::compute_crc32;
use serde::{Deserialize, Serialize};
use std::io::ErrorKind;
use std::net::UdpSocket;

const ZML2_MAGIC: [u8; 4] = *b"ZML2";

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

/// 16-byte deterministic UDP Rollback & Link Cable frame packet (`ZML2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetplayWirePacket {
    pub frame: u32,
    pub buttons_mask: u16,
    pub sio_sb: u8,
    pub sio_sc: u8,
}

impl NetplayWirePacket {
    pub fn to_bytes(self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&ZML2_MAGIC);
        buf[4..8].copy_from_slice(&self.frame.to_le_bytes());
        buf[8..10].copy_from_slice(&self.buttons_mask.to_le_bytes());
        buf[10] = self.sio_sb;
        buf[11] = self.sio_sc;
        let crc = compute_crc32(&buf[0..12]);
        buf[12..16].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 16 {
            return Err("paquet UDP ZML2 trop court (< 16 octets)".into());
        }
        if bytes[0..4] != ZML2_MAGIC {
            return Err("en-tête magique ZML2 invalide".into());
        }
        let expected_crc = compute_crc32(&bytes[0..12]);
        let actual_crc = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        if expected_crc != actual_crc {
            return Err("somme de contrôle CRC32 du paquet UDP ZML2 invalide".into());
        }
        Ok(Self {
            frame: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            buttons_mask: u16::from_le_bytes([bytes[8], bytes[9]]),
            sio_sb: bytes[10],
            sio_sc: bytes[11],
        })
    }
}

/// Non-blocking UDP P2P Rollback & Link Cable node.
pub struct UdpNetplayNode {
    socket: Option<UdpSocket>,
    pub local_port: u16,
    pub peer_addr: Option<String>,
    pub packets_sent: u64,
    pub packets_received: u64,
}

impl Default for UdpNetplayNode {
    fn default() -> Self {
        Self::bind_ephemeral()
    }
}

impl UdpNetplayNode {
    pub fn bind_ephemeral() -> Self {
        match UdpSocket::bind("127.0.0.1:0") {
            Ok(sock) => {
                let _ = sock.set_nonblocking(true);
                let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
                Self {
                    socket: Some(sock),
                    local_port: port,
                    peer_addr: None,
                    packets_sent: 0,
                    packets_received: 0,
                }
            }
            Err(_) => Self {
                socket: None,
                local_port: 0,
                peer_addr: None,
                packets_sent: 0,
                packets_received: 0,
            },
        }
    }

    pub fn connect_peer(&mut self, peer_addr: &str) {
        let trimmed = peer_addr.trim();
        if !trimmed.is_empty() {
            self.peer_addr = Some(trimmed.to_string());
        }
    }

    pub fn send_frame_packet(&mut self, pkt: NetplayWirePacket) -> bool {
        let (Some(ref sock), Some(ref peer)) = (&self.socket, &self.peer_addr) else {
            return false;
        };
        let raw = pkt.to_bytes();
        if sock.send_to(&raw, peer).is_ok() {
            self.packets_sent += 1;
            true
        } else {
            false
        }
    }

    pub fn poll_incoming_packet(&mut self) -> Option<NetplayWirePacket> {
        let sock = self.socket.as_ref()?;
        let mut buf = [0u8; 32];
        match sock.recv_from(&mut buf) {
            Ok((len, _src)) => {
                if let Ok(pkt) = NetplayWirePacket::from_bytes(&buf[..len]) {
                    self.packets_received += 1;
                    Some(pkt)
                } else {
                    None
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => None,
            Err(_) => None,
        }
    }
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

    #[test]
    fn zml2_wire_packet_encodes_and_transmits_over_local_udp_socket() {
        let mut node_a = UdpNetplayNode::bind_ephemeral();
        let mut node_b = UdpNetplayNode::bind_ephemeral();
        assert!(node_a.local_port > 0 && node_b.local_port > 0);

        node_a.connect_peer(&format!("127.0.0.1:{}", node_b.local_port));
        let pkt = NetplayWirePacket {
            frame: 420,
            buttons_mask: 0x0018,
            sio_sb: 0xA5,
            sio_sc: 0x81,
        };
        assert!(node_a.send_frame_packet(pkt));
        let received = node_b.poll_incoming_packet().unwrap();
        assert_eq!(received, pkt);
    }
}
