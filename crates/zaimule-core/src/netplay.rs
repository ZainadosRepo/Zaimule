//! P2P Rollback Netplay rooms (RomStation CloudPlay without ads/queues),
//! non-blocking UDP Rollback & Link Cable wire protocol (`ZML2`), and `zaimule://` deep links.

use crate::catalog::compute_crc32;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};

const ZML2_MAGIC: [u8; 4] = *b"ZML2";
const ZMR1_MAGIC: [u8; 4] = *b"ZMR1";
const ZMR1_NONCE_LEN: usize = 12;
const ZMR1_HEADER_LEN: usize = 4 + 1 + ZMR1_NONCE_LEN;
const ZMR1_TAG_LEN: usize = 16;

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
        let bound = UdpSocket::bind("0.0.0.0:0").or_else(|_| UdpSocket::bind("127.0.0.1:0"));
        match bound {
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

    /// Validates an `IP:PORT` socket address and attaches it as the active UDP P2P peer.
    pub fn connect_peer_validated(&mut self, peer_addr: &str) -> Result<String, String> {
        let trimmed = peer_addr.trim();
        if trimmed.is_empty() {
            return Err("adresse du pair UDP vide (attendu : IP:PORT)".into());
        }
        let parsed: std::net::SocketAddr = trimmed.parse().map_err(|_| {
            format!("adresse UDP invalide '{trimmed}' (format attendu : 192.168.1.20:54920)")
        })?;
        if parsed.port() == 0 {
            return Err("le port UDP du pair doit être supérieur à 0".into());
        }
        let normalized = parsed.to_string();
        self.peer_addr = Some(normalized.clone());
        Ok(normalized)
    }

    pub fn disconnect_peer(&mut self) {
        self.peer_addr = None;
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
    let without_fragment = trimmed
        .split_once('#')
        .map(|(head, _)| head)
        .unwrap_or(trimmed);
    let Some(rest) = without_fragment.strip_prefix("zaimule://room/") else {
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

/// Starts empty with no mock Netplay rooms.
pub fn default_netplay_rooms() -> Vec<NetplayRoom> {
    Vec::new()
}

/// Local network / LAN UDP P2P endpoint report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StunDiscoveryCandidate {
    pub public_endpoint: String,
    pub nat_type: String,
    pub hole_punch_ready: bool,
    pub stun_latency_ms: u16,
}

/// Queries the OS routing table via an unconnected-traffic UDP bind+connect (zero packets sent)
/// to determine the host's primary non-loopback LAN IPv4 address.
pub fn detect_local_lan_ip() -> Option<std::net::IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    for probe in ["192.0.2.1:80", "10.255.255.1:80", "192.168.1.1:80"] {
        if sock.connect(probe).is_ok() {
            if let Ok(local) = sock.local_addr() {
                let ip = local.ip();
                if !ip.is_loopback() && !ip.is_unspecified() {
                    return Some(ip);
                }
            }
        }
    }
    None
}

/// Reports the real local LAN UDP P2P endpoint (never fabricates a synthetic STUN IP).
pub fn discover_stun_candidate(local_port: u16) -> StunDiscoveryCandidate {
    if local_port == 0 {
        return StunDiscoveryCandidate {
            public_endpoint: "Indisponible".into(),
            nat_type: "Socket UDP inactif".into(),
            hole_punch_ready: false,
            stun_latency_ms: 0,
        };
    }
    match detect_local_lan_ip() {
        Some(lan_ip) => StunDiscoveryCandidate {
            public_endpoint: format!("{lan_ip}:{local_port}"),
            nat_type: "LAN UDP P2P (Direct)".into(),
            hole_punch_ready: true,
            stun_latency_ms: 0,
        },
        None => StunDiscoveryCandidate {
            public_endpoint: format!("127.0.0.1:{local_port}"),
            nat_type: "Localhost (Boucle locale)".into(),
            hole_punch_ready: false,
            stun_latency_ms: 0,
        },
    }
}

/// Live lobby chat message with optional 1-click Save State attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetplayChatMessage {
    pub id: String,
    pub room_code: String,
    pub sender: String,
    pub text: String,
    pub timestamp_ms: u64,
    pub shared_state_slot: Option<u8>,
    pub shared_state_crc32_hex: Option<String>,
}

/// Starts empty with zero fake lobby messages.
pub fn default_netplay_chat() -> Vec<NetplayChatMessage> {
    Vec::new()
}

/// A single match node in an automated 4P/8P elimination tournament bracket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TournamentMatch {
    pub id: String,
    pub round_label: String,
    pub player_a: String,
    pub player_b: String,
    pub score_a: u32,
    pub score_b: u32,
    pub winner: Option<String>,
    pub spectators_count: u16,
}

/// Automated elimination Tournament Bracket with live Spectator telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TournamentBracket {
    pub id: String,
    pub title: String,
    pub console: String,
    pub game_title: String,
    pub matches: Vec<TournamentMatch>,
    pub champion: Option<String>,
    pub spectator_mode_active: bool,
}

impl TournamentBracket {
    /// Records the score for `match_id`, crowns its winner, and automatically populates the Grand Final or Champion.
    pub fn advance_match(
        &mut self,
        match_id: &str,
        score_a: u32,
        score_b: u32,
    ) -> Result<(), String> {
        let Some(idx) = self.matches.iter().position(|m| m.id == match_id) else {
            return Err(format!("match de tournoi introuvable : {match_id}"));
        };
        let winner_name = if score_a >= score_b {
            self.matches[idx].player_a.clone()
        } else {
            self.matches[idx].player_b.clone()
        };
        self.matches[idx].score_a = score_a;
        self.matches[idx].score_b = score_b;
        self.matches[idx].winner = Some(winner_name.clone());

        if idx == 0 && self.matches.len() >= 3 {
            self.matches[2].player_a = winner_name;
        } else if idx == 1 && self.matches.len() >= 3 {
            self.matches[2].player_b = winner_name;
        } else if idx == 2 {
            self.champion = Some(winner_name);
        }
        Ok(())
    }
}

/// Starts with an empty tournament bracket (no pre-populated players or matches).
pub fn default_tournament_bracket() -> TournamentBracket {
    TournamentBracket {
        id: "tourney-empty".into(),
        title: "Tournoi P2P Zaimule".into(),
        console: "GB / GBC / GBA".into(),
        game_title: "En attente d'un salon actif".into(),
        matches: Vec::new(),
        champion: None,
        spectator_mode_active: false,
    }
}

// =============================================================================
// PROTOCOLE RELAIS CHIFFRÉ DE BOUT EN BOUT (ZMR1 · ChaCha20-Poly1305)
// =============================================================================

/// Canaux logiques multiplexés sur le relais WebSocket `zaimule-relay`.
/// Alignés 1:1 avec `apps/public/zaimule-relay/src/protocol.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Clé symétrique de session 256 bits (`ChaCha20-Poly1305`) partagée uniquement entre joueurs
/// dans le fragment `#KEY` du lien `zaimule://` (jamais transmise au serveur relais).
#[derive(Clone, PartialEq, Eq)]
pub struct RelaySessionKey([u8; 32]);

impl fmt::Debug for RelaySessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RelaySessionKey([REDACTED])")
    }
}

impl RelaySessionKey {
    pub fn generate() -> Result<Self, String> {
        let rng = SystemRandom::new();
        let mut bytes = [0u8; 32];
        rng.fill(&mut bytes)
            .map_err(|_| "générateur aléatoire cryptographique indisponible".to_string())?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_b64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    pub fn from_b64url(raw: &str) -> Result<Self, String> {
        let decoded = URL_SAFE_NO_PAD
            .decode(raw.trim())
            .map_err(|_| "clé E2EE de salon invalide (encodage base64url incorrect)".to_string())?;
        if decoded.len() != 32 {
            return Err(format!(
                "clé E2EE de salon invalide ({} octets au lieu de 32)",
                decoded.len()
            ));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&decoded);
        Ok(Self(out))
    }
}

/// Moteur de chiffrement authentifié `ZMR1` (`ChaCha20-Poly1305`).
/// Chaque trame utilise un nonce unique de 96 bits (`[sel_session_32b || compteur_64b]`) et
/// lie cryptographiquement le canal et le code de salon via l'AAD.
pub struct Zmr1Cipher {
    key: LessSafeKey,
    nonce_salt: [u8; 4],
    counter: AtomicU64,
}

impl fmt::Debug for Zmr1Cipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Zmr1Cipher")
            .field("cipher", &"ChaCha20-Poly1305")
            .finish()
    }
}

impl Zmr1Cipher {
    pub fn new(session_key: &RelaySessionKey) -> Result<Self, String> {
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, session_key.as_bytes())
            .map_err(|_| "échec d'initialisation ChaCha20-Poly1305".to_string())?;
        let rng = SystemRandom::new();
        let mut nonce_salt = [0u8; 4];
        rng.fill(&mut nonce_salt)
            .map_err(|_| "échec de génération du sel de nonce".to_string())?;
        Ok(Self {
            key: LessSafeKey::new(unbound),
            nonce_salt,
            counter: AtomicU64::new(1),
        })
    }

    fn build_aad(channel: RelayChannel, room_code: &str) -> Vec<u8> {
        let code = room_code.trim().to_ascii_uppercase();
        let mut aad = Vec::with_capacity(5 + code.len());
        aad.extend_from_slice(&ZMR1_MAGIC);
        aad.push(channel as u8);
        aad.extend_from_slice(code.as_bytes());
        aad
    }

    /// Chiffre et authentifie `plaintext` dans une enveloppe binaire `ZMR1`.
    pub fn seal(
        &self,
        channel: RelayChannel,
        room_code: &str,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, String> {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut nonce_bytes = [0u8; ZMR1_NONCE_LEN];
        nonce_bytes[0..4].copy_from_slice(&self.nonce_salt);
        nonce_bytes[4..12].copy_from_slice(&seq.to_le_bytes());
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let aad_bytes = Self::build_aad(channel, room_code);

        let mut in_out = Vec::with_capacity(plaintext.len() + ZMR1_TAG_LEN);
        in_out.extend_from_slice(plaintext);
        self.key
            .seal_in_place_append_tag(nonce, Aad::from(&aad_bytes), &mut in_out)
            .map_err(|_| "échec du chiffrement ChaCha20-Poly1305".to_string())?;

        let mut packet = Vec::with_capacity(ZMR1_HEADER_LEN + in_out.len());
        packet.extend_from_slice(&ZMR1_MAGIC);
        packet.push(channel as u8);
        packet.extend_from_slice(&nonce_bytes);
        packet.extend_from_slice(&in_out);
        Ok(packet)
    }

    /// Vérifie l'intégrité et déchiffre une enveloppe binaire `ZMR1`.
    pub fn open(
        &self,
        expected_channel: RelayChannel,
        room_code: &str,
        packet: &[u8],
    ) -> Result<Vec<u8>, String> {
        if packet.len() < ZMR1_HEADER_LEN + ZMR1_TAG_LEN {
            return Err("trame ZMR1 trop courte".into());
        }
        if packet[0..4] != ZMR1_MAGIC {
            return Err("en-tête magique ZMR1 invalide".into());
        }
        let channel =
            RelayChannel::from_byte(packet[4]).ok_or_else(|| "canal ZMR1 inconnu".to_string())?;
        if channel != expected_channel {
            return Err("canal ZMR1 inattendu".into());
        }
        let mut nonce_bytes = [0u8; ZMR1_NONCE_LEN];
        nonce_bytes.copy_from_slice(&packet[5..17]);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let aad_bytes = Self::build_aad(expected_channel, room_code);

        let mut buf = packet[ZMR1_HEADER_LEN..].to_vec();
        let plain = self
            .key
            .open_in_place(nonce, Aad::from(&aad_bytes), &mut buf)
            .map_err(|_| {
                "authentification E2EE ZMR1 échouée (clé ou intégrité invalide)".to_string()
            })?;
        Ok(plain.to_vec())
    }
}

/// Drapeaux d'échange série Câble Link GB/GBC dans `GbLinkPayload`.
pub const GB_LINK_FLAG_MASTER_TX: u8 = 0x01;
pub const GB_LINK_FLAG_SLAVE_REPLY: u8 = 0x02;

/// Trame binaire compacte (12 octets en clair, chiffrée dans `ZMR1`) pour le Câble Link GB/GBC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GbLinkPayload {
    pub frame: u32,
    pub buttons_mask: u16,
    pub sio_sb: u8,
    pub sio_sc: u8,
    pub flags: u8,
    pub seq: u16,
}

impl GbLinkPayload {
    pub const WIRE_LEN: usize = 12;

    pub fn to_bytes(self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..4].copy_from_slice(&self.frame.to_le_bytes());
        out[4..6].copy_from_slice(&self.buttons_mask.to_le_bytes());
        out[6] = self.sio_sb;
        out[7] = self.sio_sc;
        out[8] = self.flags;
        out[9..11].copy_from_slice(&self.seq.to_le_bytes());
        out[11] = 0;
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < Self::WIRE_LEN {
            return Err("charge utile GB Link trop courte".into());
        }
        Ok(Self {
            frame: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            buttons_mask: u16::from_le_bytes([bytes[4], bytes[5]]),
            sio_sb: bytes[6],
            sio_sc: bytes[7],
            flags: bytes[8],
            seq: u16::from_le_bytes([bytes[9], bytes[10]]),
        })
    }

    pub fn is_master_tx(self) -> bool {
        (self.flags & GB_LINK_FLAG_MASTER_TX) != 0
    }

    pub fn is_slave_reply(self) -> bool {
        (self.flags & GB_LINK_FLAG_SLAVE_REPLY) != 0
    }
}

/// Message de chat chiffré de bout en bout (transmis dans le canal `RelayChannel::Chat`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayChatPayload {
    pub sender: String,
    pub text: String,
    pub timestamp_ms: u64,
}

/// Pair présent dans un salon relais chiffré.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayPeerInfo {
    pub peer_id: u8,
    pub name: String,
    pub is_host: bool,
}

/// Invitation chiffrée `zaimule://room/<CODE>?crc=<CRC>&console=<CONSOLE>#<KEY>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayInvite {
    pub room_code: String,
    pub rom_crc32_hex: Option<String>,
    pub console: Option<String>,
    pub session_key: RelaySessionKey,
}

/// Formate un lien d'invitation E2EE dont la clé reste dans le fragment `#` (jamais envoyée au VPS).
pub fn format_relay_invite(
    room_code: &str,
    rom_crc32_hex: Option<&str>,
    console: Option<&str>,
    session_key: &RelaySessionKey,
) -> String {
    let code = room_code.trim().to_ascii_uppercase();
    let mut query = Vec::new();
    if let Some(crc) = rom_crc32_hex.map(str::trim).filter(|s| !s.is_empty()) {
        query.push(format!("crc={}", crc.to_ascii_uppercase()));
    }
    if let Some(con) = console.map(str::trim).filter(|s| !s.is_empty()) {
        query.push(format!("console={con}"));
    }
    let key_b64 = session_key.to_b64url();
    if query.is_empty() {
        format!("zaimule://room/{code}#{key_b64}")
    } else {
        format!("zaimule://room/{code}?{}#{key_b64}", query.join("&"))
    }
}

/// Analyse un lien `zaimule://room/<CODE>?...#<KEY>` ou un raccourci `<CODE>#<KEY>`.
pub fn parse_relay_invite(input: &str) -> Result<RelayInvite, String> {
    let trimmed = input.trim();
    let Some((head, fragment)) = trimmed.split_once('#') else {
        return Err("clé E2EE manquante après '#' dans le lien d'invitation".into());
    };
    let session_key = RelaySessionKey::from_b64url(fragment)?;

    let body = head.strip_prefix("zaimule://room/").unwrap_or(head);
    let (code_part, query_part) = match body.split_once('?') {
        Some((c, q)) => (c, q),
        None => (body, ""),
    };
    let room_code: String = code_part
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    if room_code.len() < 4 || room_code.len() > 12 {
        return Err("code de salon invalide (attendu : 6 caractères alphanumériques)".into());
    }

    let mut rom_crc32_hex = None;
    let mut console = None;
    for kv in query_part.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            let val = v.trim();
            if val.is_empty() {
                continue;
            }
            match k.trim() {
                "crc" => rom_crc32_hex = Some(val.to_ascii_uppercase()),
                "console" => console = Some(val.to_string()),
                _ => {}
            }
        }
    }

    Ok(RelayInvite {
        room_code,
        rom_crc32_hex,
        console,
        session_key,
    })
}

/// Hache un mot de passe de salon côté client (SHA-256 hex) pour ne jamais l'envoyer en clair.
pub fn hash_room_password(password: &str) -> Option<String> {
    let trimmed = password.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut ctx = digest::Context::new(&digest::SHA256);
    ctx.update(b"zaimule-room-pw-v1:");
    ctx.update(trimmed.as_bytes());
    let d = ctx.finish();
    let mut hex = String::with_capacity(64);
    for b in d.as_ref() {
        use fmt::Write as _;
        let _ = write!(&mut hex, "{b:02x}");
    }
    Some(hex)
}

/// Valide l'URL du serveur relais (`wss://` obligatoire sur Internet ; `ws://127.0.0.1` ou
/// `ws://localhost` autorisé uniquement pour les tests locaux).
pub fn validate_relay_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("URL du serveur relais vide".into());
    }
    let normalized = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        trimmed.to_string()
    };

    if let Some(rest) = normalized.strip_prefix("wss://") {
        if rest.is_empty() || rest.starts_with('/') {
            return Err("nom d'hôte manquant dans l'URL wss://".into());
        }
        return Ok(ensure_ws_path(normalized));
    }

    if let Some(rest) = normalized.strip_prefix("ws://") {
        let is_local = rest.starts_with("127.0.0.1")
            || rest.starts_with("localhost")
            || rest.starts_with("[::1]");
        if !is_local {
            return Err(
                "protocole non chiffré ws:// refusé hors localhost (utilisez wss://)".into(),
            );
        }
        return Ok(ensure_ws_path(normalized));
    }

    Err("l'URL du relais doit commencer par wss:// (ex: wss://relay.votre-domaine/ws)".into())
}

fn ensure_ws_path(mut url: String) -> String {
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    if !url[scheme_end..].contains('/') {
        url.push_str("/ws");
    } else if url.ends_with('/') {
        url.push_str("ws");
    }
    url
}

// =============================================================================
// PHASE 2 — TUNNEL SWITCH LDN / ROOM (Torzu · Fritchii · 0 port ouvert)
// =============================================================================

/// Port UDP local standard des salles multijoueur Yuzu / Torzu / Fritchii (`ENet`).
pub const DEFAULT_SWITCH_LDN_PORT: u16 = 24_872;
pub const SWITCH_LDN_PROTO_UDP: u8 = 1;
pub const SWITCH_LDN_DIR_CLIENT_TO_HOST: u8 = 1;
pub const SWITCH_LDN_DIR_HOST_TO_CLIENT: u8 = 2;
/// Taille maximale d'un datagramme UDP Switch encapsulé dans une trame `ZMR1` (< 64 Kio).
pub const SWITCH_LDN_MAX_PAYLOAD_BYTES: usize = 60_000;
const SWITCH_LDN_HEADER_LEN: usize = 6;

/// Datagramme Switch LDN / Room encapsulé dans `ZMR1` (`RelayChannel::SwitchLdn`).
/// Transporte de manière transparente les paquets UDP ENet de `Torzu` / `Fritchii`
/// entre la boucle locale `127.0.0.1` des joueurs sans ouvrir aucun port réseau.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchLdnPacket {
    pub proto: u8,
    pub direction: u8,
    pub client_port: u16,
    pub target_port: u16,
    pub payload: Vec<u8>,
}

impl SwitchLdnPacket {
    pub fn udp_client_to_host(client_port: u16, target_port: u16, payload: Vec<u8>) -> Self {
        Self {
            proto: SWITCH_LDN_PROTO_UDP,
            direction: SWITCH_LDN_DIR_CLIENT_TO_HOST,
            client_port,
            target_port,
            payload,
        }
    }

    pub fn udp_host_to_client(client_port: u16, target_port: u16, payload: Vec<u8>) -> Self {
        Self {
            proto: SWITCH_LDN_PROTO_UDP,
            direction: SWITCH_LDN_DIR_HOST_TO_CLIENT,
            client_port,
            target_port,
            payload,
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        if self.payload.is_empty() {
            return Err("datagramme Switch LDN vide".into());
        }
        if self.payload.len() > SWITCH_LDN_MAX_PAYLOAD_BYTES {
            return Err(format!(
                "datagramme Switch LDN trop volumineux ({} > {} octets)",
                self.payload.len(),
                SWITCH_LDN_MAX_PAYLOAD_BYTES
            ));
        }
        if self.client_port == 0 {
            return Err("port client Switch LDN invalide (0)".into());
        }
        let mut out = Vec::with_capacity(SWITCH_LDN_HEADER_LEN + self.payload.len());
        out.push(self.proto);
        out.push(self.direction);
        out.extend_from_slice(&self.client_port.to_le_bytes());
        out.extend_from_slice(&self.target_port.to_le_bytes());
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() <= SWITCH_LDN_HEADER_LEN {
            return Err("trame Switch LDN trop courte".into());
        }
        let proto = bytes[0];
        let direction = bytes[1];
        if proto != SWITCH_LDN_PROTO_UDP {
            return Err(format!("protocole Switch LDN non pris en charge ({proto})"));
        }
        if direction != SWITCH_LDN_DIR_CLIENT_TO_HOST && direction != SWITCH_LDN_DIR_HOST_TO_CLIENT
        {
            return Err(format!("direction Switch LDN invalide ({direction})"));
        }
        let client_port = u16::from_le_bytes([bytes[2], bytes[3]]);
        let target_port = u16::from_le_bytes([bytes[4], bytes[5]]);
        if client_port == 0 {
            return Err("port client Switch LDN invalide (0)".into());
        }
        let payload = bytes[SWITCH_LDN_HEADER_LEN..].to_vec();
        if payload.len() > SWITCH_LDN_MAX_PAYLOAD_BYTES {
            return Err("charge utile Switch LDN dépasse la taille maximale".into());
        }
        Ok(Self {
            proto,
            direction,
            client_port,
            target_port,
            payload,
        })
    }
}

// =============================================================================
// PHASE 3 — CÂBLE LINK GBA TEMPS RÉEL (SIOMULTI 2–4 joueurs · ZMR1)
// =============================================================================

/// Drapeaux d'échange Multi-Player GBA (`SIOMULTI0..3` / `SIOMLT_SEND`) dans `GbaLinkPayload`.
pub const GBA_LINK_FLAG_MASTER_TX: u8 = 0x01;
pub const GBA_LINK_FLAG_SLAVE_REPLY: u8 = 0x02;

/// Trame binaire compacte (14 octets en clair, chiffrée dans `ZMR1` sur `RelayChannel::GbaLink`)
/// pour le Câble Link GBA `SIOMULTI` temps réel (2 à 4 consoles).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GbaLinkPayload {
    pub frame: u32,
    pub buttons_mask: u16,
    pub siomlt_send: u16,
    pub siocnt: u16,
    pub flags: u8,
    pub player_id: u8,
    pub seq: u16,
}

impl GbaLinkPayload {
    pub const WIRE_LEN: usize = 14;

    pub fn to_bytes(self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..4].copy_from_slice(&self.frame.to_le_bytes());
        out[4..6].copy_from_slice(&self.buttons_mask.to_le_bytes());
        out[6..8].copy_from_slice(&self.siomlt_send.to_le_bytes());
        out[8..10].copy_from_slice(&self.siocnt.to_le_bytes());
        out[10] = self.flags;
        out[11] = self.player_id & 0x03;
        out[12..14].copy_from_slice(&self.seq.to_le_bytes());
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < Self::WIRE_LEN {
            return Err("charge utile GBA Link SIOMULTI trop courte".into());
        }
        Ok(Self {
            frame: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            buttons_mask: u16::from_le_bytes([bytes[4], bytes[5]]),
            siomlt_send: u16::from_le_bytes([bytes[6], bytes[7]]),
            siocnt: u16::from_le_bytes([bytes[8], bytes[9]]),
            flags: bytes[10],
            player_id: bytes[11] & 0x03,
            seq: u16::from_le_bytes([bytes[12], bytes[13]]),
        })
    }

    pub fn is_master_tx(self) -> bool {
        (self.flags & GBA_LINK_FLAG_MASTER_TX) != 0
    }

    pub fn is_slave_reply(self) -> bool {
        (self.flags & GBA_LINK_FLAG_SLAVE_REPLY) != 0
    }
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

    #[test]
    fn stun_candidate_and_empty_defaults_initialize_cleanly() {
        let inactive = discover_stun_candidate(0);
        assert!(!inactive.hole_punch_ready);
        assert_eq!(inactive.public_endpoint, "Indisponible");

        let candidate = discover_stun_candidate(54920);
        assert!(!candidate.public_endpoint.starts_with("198.51.100."));
        assert!(candidate.public_endpoint.ends_with(":54920"));
        assert!(default_netplay_rooms().is_empty());
        assert!(default_netplay_chat().is_empty());
    }

    #[test]
    fn udp_node_validates_and_disconnects_peers() {
        let mut node = UdpNetplayNode::bind_ephemeral();
        assert!(node.connect_peer_validated("").is_err());
        assert!(node.connect_peer_validated("not-an-ip").is_err());
        assert!(node.connect_peer_validated("127.0.0.1:0").is_err());

        let normalized = node.connect_peer_validated(" 127.0.0.1:54920 ").unwrap();
        assert_eq!(normalized, "127.0.0.1:54920");
        assert_eq!(node.peer_addr.as_deref(), Some("127.0.0.1:54920"));

        node.disconnect_peer();
        assert!(node.peer_addr.is_none());
    }

    #[test]
    fn tournament_bracket_advances_semifinals_and_crowns_champion() {
        let empty = default_tournament_bracket();
        assert!(empty.matches.is_empty());

        let mut bracket = TournamentBracket {
            id: "cup".into(),
            title: "Cup".into(),
            console: "GBC".into(),
            game_title: "Link".into(),
            matches: vec![
                TournamentMatch {
                    id: "semi-1".into(),
                    round_label: "Demi 1".into(),
                    player_a: "J1".into(),
                    player_b: "J2".into(),
                    score_a: 0,
                    score_b: 0,
                    winner: None,
                    spectators_count: 0,
                },
                TournamentMatch {
                    id: "semi-2".into(),
                    round_label: "Demi 2".into(),
                    player_a: "J3".into(),
                    player_b: "J4".into(),
                    score_a: 0,
                    score_b: 0,
                    winner: None,
                    spectators_count: 0,
                },
                TournamentMatch {
                    id: "final".into(),
                    round_label: "Finale".into(),
                    player_a: "TBD".into(),
                    player_b: "TBD".into(),
                    score_a: 0,
                    score_b: 0,
                    winner: None,
                    spectators_count: 0,
                },
            ],
            champion: None,
            spectator_mode_active: false,
        };
        bracket.advance_match("semi-1", 10, 4).unwrap();
        bracket.advance_match("semi-2", 2, 9).unwrap();
        bracket.advance_match("final", 260, 215).unwrap();
        assert_eq!(bracket.champion.as_deref(), Some("J1"));
    }

    #[test]
    fn zmr1_chacha20_poly1305_seals_opens_and_rejects_tampering_or_wrong_context() {
        let key_a = RelaySessionKey::generate().unwrap();
        let key_other = RelaySessionKey::generate().unwrap();
        assert!(!format!("{key_a:?}").contains(&key_a.to_b64url()));

        let cipher_a = Zmr1Cipher::new(&key_a).unwrap();
        let cipher_b = Zmr1Cipher::new(&key_a).unwrap();
        let cipher_wrong = Zmr1Cipher::new(&key_other).unwrap();

        let link = GbLinkPayload {
            frame: 1234,
            buttons_mask: 0x00A5,
            sio_sb: 0x5A,
            sio_sc: 0x81,
            flags: GB_LINK_FLAG_MASTER_TX,
            seq: 7,
        };
        let sealed = cipher_a
            .seal(RelayChannel::GbLink, "K9P4XQ", &link.to_bytes())
            .unwrap();
        assert!(sealed.starts_with(b"ZMR1"));
        assert_ne!(&sealed[ZMR1_HEADER_LEN..], &link.to_bytes());

        // Déchiffrement légitime par un autre pair possédant la même clé de session
        let opened = cipher_b
            .open(RelayChannel::GbLink, "k9p4xq", &sealed)
            .unwrap();
        assert_eq!(GbLinkPayload::from_bytes(&opened).unwrap(), link);

        // Rejet sur mauvaise clé, mauvais salon (AAD), mauvais canal ou octet altéré
        assert!(cipher_wrong
            .open(RelayChannel::GbLink, "K9P4XQ", &sealed)
            .is_err());
        assert!(cipher_b
            .open(RelayChannel::GbLink, "OTHER1", &sealed)
            .is_err());
        assert!(cipher_b
            .open(RelayChannel::Chat, "K9P4XQ", &sealed)
            .is_err());
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xFF;
        assert!(cipher_b
            .open(RelayChannel::GbLink, "K9P4XQ", &tampered)
            .is_err());
    }

    #[test]
    fn relay_invite_and_url_validation_enforce_e2ee_and_tls() {
        let key = RelaySessionKey::generate().unwrap();
        let uri = format_relay_invite("ab23cd", Some("deadbeef"), Some("GBC"), &key);
        assert!(uri.starts_with("zaimule://room/AB23CD?crc=DEADBEEF&console=GBC#"));

        let parsed = parse_relay_invite(&uri).unwrap();
        assert_eq!(parsed.room_code, "AB23CD");
        assert_eq!(parsed.rom_crc32_hex.as_deref(), Some("DEADBEEF"));
        assert_eq!(parsed.console.as_deref(), Some("GBC"));
        assert_eq!(parsed.session_key, key);

        // Raccourci CODE#KEY également accepté
        let short = format!("AB23CD#{}", key.to_b64url());
        assert_eq!(parse_relay_invite(&short).unwrap().room_code, "AB23CD");
        assert!(parse_relay_invite("zaimule://room/AB23CD").is_err());

        // URL relais : TLS obligatoire hors localhost
        assert_eq!(
            validate_relay_url("wss://relay.example.org").unwrap(),
            "wss://relay.example.org/ws"
        );
        assert_eq!(
            validate_relay_url("https://relay.example.org/ws").unwrap(),
            "wss://relay.example.org/ws"
        );
        assert_eq!(
            validate_relay_url("ws://127.0.0.1:3000/ws").unwrap(),
            "ws://127.0.0.1:3000/ws"
        );
        assert!(validate_relay_url("ws://relay.example.org/ws").is_err());
        assert!(validate_relay_url("http://relay.example.org").is_err());

        // Mot de passe haché de manière déterministe
        assert_eq!(hash_room_password("   "), None);
        let h = hash_room_password("pikachu").unwrap();
        assert_eq!(h.len(), 64);
        assert_ne!(h, "pikachu");
    }

    #[test]
    fn switch_ldn_packet_roundtrips_through_zmr1_e2ee_and_rejects_oversize() {
        let key = RelaySessionKey::generate().unwrap();
        let host_cipher = Zmr1Cipher::new(&key).unwrap();
        let guest_cipher = Zmr1Cipher::new(&key).unwrap();

        let pkt = SwitchLdnPacket::udp_client_to_host(
            51_234,
            DEFAULT_SWITCH_LDN_PORT,
            b"ENET-MK8D-JOIN".to_vec(),
        );
        let raw = pkt.to_bytes().unwrap();
        let sealed = guest_cipher
            .seal(RelayChannel::SwitchLdn, "MK8D42", &raw)
            .unwrap();
        let opened = host_cipher
            .open(RelayChannel::SwitchLdn, "MK8D42", &sealed)
            .unwrap();
        assert_eq!(SwitchLdnPacket::from_bytes(&opened).unwrap(), pkt);

        // Rejet des paquets vides, port 0 ou > SWITCH_LDN_MAX_PAYLOAD_BYTES
        assert!(SwitchLdnPacket::udp_client_to_host(51_234, 24872, vec![])
            .to_bytes()
            .is_err());
        assert!(SwitchLdnPacket::udp_client_to_host(0, 24872, vec![1])
            .to_bytes()
            .is_err());
        assert!(SwitchLdnPacket::udp_client_to_host(
            51_234,
            24872,
            vec![0u8; SWITCH_LDN_MAX_PAYLOAD_BYTES + 1]
        )
        .to_bytes()
        .is_err());
    }

    #[test]
    fn gba_link_payload_roundtrips_through_zmr1_e2ee() {
        let key = RelaySessionKey::generate().unwrap();
        let p0 = Zmr1Cipher::new(&key).unwrap();
        let p1 = Zmr1Cipher::new(&key).unwrap();

        let pkt = GbaLinkPayload {
            frame: 420,
            buttons_mask: 0x00A5,
            siomlt_send: 0xCAFE,
            siocnt: 0x6088,
            flags: GBA_LINK_FLAG_MASTER_TX,
            player_id: 0,
            seq: 17,
        };
        let sealed = p0
            .seal(RelayChannel::GbaLink, "EMERLD", &pkt.to_bytes())
            .unwrap();
        let opened = p1.open(RelayChannel::GbaLink, "EMERLD", &sealed).unwrap();
        let decoded = GbaLinkPayload::from_bytes(&opened).unwrap();
        assert_eq!(decoded, pkt);
        assert!(decoded.is_master_tx());
        assert!(!decoded.is_slave_reply());
    }
}
