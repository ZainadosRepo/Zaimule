use serde::{Deserialize, Serialize};

/// Version du protocole de contrôle et de relais binaire `ZMR1`.
pub const PROTOCOL_VERSION: u8 = 1;

/// Taille minimale d'une trame binaire relais : `[channel: u8, peer_id: u8, payload...]`.
pub const BINARY_HEADER_LEN: usize = 2;

/// Taille maximale d'un message JSON de contrôle.
pub const MAX_CONTROL_TEXT_BYTES: usize = 4096;

/// Canaux logiques multiplexés dans le tunnel chiffré E2EE.
/// Le relais ne déchiffre jamais la charge utile : il vérifie uniquement que le canal est connu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RelayChannel {
    GbLink = 1,
    Chat = 2,
    SwitchLdn = 3,
    TradeHub = 4,
    GbaLink = 5,
    NdsWifi = 6,
}

impl RelayChannel {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::GbLink),
            2 => Some(Self::Chat),
            3 => Some(Self::SwitchLdn),
            4 => Some(Self::TradeHub),
            5 => Some(Self::GbaLink),
            6 => Some(Self::NdsWifi),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub peer_id: u8,
    pub name: String,
    pub is_host: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    CreateRoom {
        console: String,
        game_title: String,
        #[serde(default)]
        rom_crc32: Option<String>,
        player_name: String,
        #[serde(default)]
        password_hash: Option<String>,
        #[serde(default)]
        max_players: Option<u8>,
    },
    JoinRoom {
        room_code: String,
        #[serde(default)]
        rom_crc32: Option<String>,
        player_name: String,
        #[serde(default)]
        password_hash: Option<String>,
    },
    LeaveRoom,
    Ping {
        ts_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        protocol_version: u8,
        max_frame_bytes: usize,
    },
    RoomCreated {
        room_code: String,
        peer_id: u8,
        peers: Vec<PeerInfo>,
        console: String,
        game_title: String,
        rom_crc32: Option<String>,
        max_players: u8,
    },
    RoomJoined {
        room_code: String,
        peer_id: u8,
        peers: Vec<PeerInfo>,
        console: String,
        game_title: String,
        rom_crc32: Option<String>,
        max_players: u8,
    },
    PeerJoined {
        peer: PeerInfo,
    },
    PeerLeft {
        peer_id: u8,
        reason: String,
    },
    HostMigrated {
        new_host_peer_id: u8,
    },
    Pong {
        ts_ms: u64,
        server_ts_ms: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

/// Trame interne envoyée vers la tâche d'écriture WebSocket d'un pair.
#[derive(Debug, Clone)]
pub enum OutboundFrame {
    Control(ServerMessage),
    Binary(Vec<u8>),
}

/// Décode l'en-tête binaire client `[channel: u8, target_peer_id: u8, payload...]`.
pub fn parse_client_binary_frame(raw: &[u8]) -> Option<(RelayChannel, u8, &[u8])> {
    if raw.len() <= BINARY_HEADER_LEN {
        return None;
    }
    let channel = RelayChannel::from_byte(raw[0])?;
    let target_peer_id = raw[1];
    Some((channel, target_peer_id, &raw[BINARY_HEADER_LEN..]))
}

/// Construit la trame binaire relayée `[channel: u8, sender_peer_id: u8, payload...]`.
/// L'identifiant expéditeur est estampillé par le relais pour empêcher toute usurpation.
pub fn encode_server_binary_frame(
    channel: RelayChannel,
    sender_peer_id: u8,
    opaque_payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(BINARY_HEADER_LEN + opaque_payload.len());
    out.push(channel as u8);
    out.push(sender_peer_id);
    out.extend_from_slice(opaque_payload);
    out
}

pub fn sanitize_label(raw: &str, max_chars: usize, fallback: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

pub fn normalize_crc32(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(16)
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_ascii_uppercase())
    }
}

pub fn normalize_room_code(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>()
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_frame_round_trip_stamps_sender() {
        let mut client_frame = vec![RelayChannel::GbLink as u8, 0u8];
        client_frame.extend_from_slice(b"ZMR1-opaque-ciphertext");

        let (ch, target, payload) = parse_client_binary_frame(&client_frame).expect("valid");
        assert_eq!(ch, RelayChannel::GbLink);
        assert_eq!(target, 0);
        assert_eq!(payload, b"ZMR1-opaque-ciphertext");

        let relayed = encode_server_binary_frame(ch, 2, payload);
        assert_eq!(relayed[0], RelayChannel::GbLink as u8);
        assert_eq!(relayed[1], 2);
        assert_eq!(&relayed[2..], b"ZMR1-opaque-ciphertext");
    }

    #[test]
    fn rejects_unknown_channel_or_empty_payload() {
        assert!(parse_client_binary_frame(&[1, 0]).is_none());
        assert!(parse_client_binary_frame(&[99, 0, 1, 2]).is_none());
        assert!(parse_client_binary_frame(&[]).is_none());
    }
}
