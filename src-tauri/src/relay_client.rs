//! Client WebSocket TLS sortant (`wss://`) vers le relais privé `zaimule-relay`.
//!
//! - **0 port ouvert sur le PC du joueur** (connexion 100 % sortante vers Caddy `443`).
//! - **Authentification serveur** par en-tête `X-Zaimule-Key` avant upgrade WebSocket.
//! - **Chiffrement de bout en bout (`ZMR1`, `ChaCha20-Poly1305`)** sur toutes les trames
//!   binaires (Câble Link GB/GBC, chat chiffré, et futurs canaux Switch LDN / Trade Hub / GBA / NDS).

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use zaimule_core::netplay::{
    format_relay_invite, hash_room_password, parse_relay_invite, validate_relay_url, GbLinkPayload,
    RelayChannel, RelayChatPayload, RelayPeerInfo, RelaySessionKey, SwitchLdnPacket, Zmr1Cipher,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(5);
const MAX_CHAT_HISTORY: usize = 64;
const MAX_INBOUND_LINK_QUEUE: usize = 64;
const OUTBOUND_CMD_CAPACITY: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RelayChatEntry {
    pub sender: String,
    pub text: String,
    pub timestamp_ms: u64,
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayStatusSnapshot {
    pub connected: bool,
    pub relay_url: Option<String>,
    pub room_code: Option<String>,
    pub invite_uri: Option<String>,
    pub peer_id: u8,
    pub is_host: bool,
    pub player_name: String,
    pub console: String,
    pub game_title: String,
    pub rom_crc32: Option<String>,
    pub max_players: u8,
    pub peers: Vec<RelayPeerInfo>,
    pub rtt_ms: Option<u32>,
    pub e2ee_cipher: String,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub chat_messages: Vec<RelayChatEntry>,
    pub last_error: Option<String>,
}

impl Default for RelayStatusSnapshot {
    fn default() -> Self {
        Self {
            connected: false,
            relay_url: None,
            room_code: None,
            invite_uri: None,
            peer_id: 0,
            is_host: false,
            player_name: String::new(),
            console: String::new(),
            game_title: String::new(),
            rom_crc32: None,
            max_players: 2,
            peers: Vec::new(),
            rtt_ms: None,
            e2ee_cipher: "ChaCha20-Poly1305 (ZMR1)".to_string(),
            packets_sent: 0,
            packets_received: 0,
            chat_messages: Vec::new(),
            last_error: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RelayCreateParams {
    pub relay_url: String,
    pub access_key: String,
    pub console: String,
    pub game_title: String,
    pub rom_crc32: Option<String>,
    pub player_name: String,
    pub password: Option<String>,
    pub max_players: u8,
}

#[derive(Debug, Clone)]
pub struct RelayJoinParams {
    pub relay_url: String,
    pub access_key: String,
    pub invite_or_code: String,
    pub explicit_session_key: Option<String>,
    pub local_rom_crc32: Option<String>,
    pub player_name: String,
    pub password: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireClientControl {
    CreateRoom {
        console: String,
        game_title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        rom_crc32: Option<String>,
        player_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        password_hash: Option<String>,
        max_players: u8,
    },
    JoinRoom {
        room_code: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        rom_crc32: Option<String>,
        player_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        password_hash: Option<String>,
    },
    LeaveRoom,
    Ping {
        ts_ms: u64,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireServerControl {
    Welcome {
        #[allow(dead_code)]
        protocol_version: u8,
        #[allow(dead_code)]
        max_frame_bytes: usize,
    },
    RoomCreated {
        room_code: String,
        peer_id: u8,
        peers: Vec<RelayPeerInfo>,
        console: String,
        game_title: String,
        rom_crc32: Option<String>,
        max_players: u8,
    },
    RoomJoined {
        room_code: String,
        peer_id: u8,
        peers: Vec<RelayPeerInfo>,
        console: String,
        game_title: String,
        rom_crc32: Option<String>,
        max_players: u8,
    },
    PeerJoined {
        peer: RelayPeerInfo,
    },
    PeerLeft {
        peer_id: u8,
        #[allow(dead_code)]
        reason: String,
    },
    HostMigrated {
        new_host_peer_id: u8,
    },
    Pong {
        ts_ms: u64,
        #[allow(dead_code)]
        server_ts_ms: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

enum OutboundCmd {
    GbLink(GbLinkPayload),
    Chat(RelayChatPayload),
    SwitchLdn {
        target_peer_id: u8,
        packet: SwitchLdnPacket,
    },
    Leave,
}

struct RelayInner {
    status: RelayStatusSnapshot,
    cmd_tx: Option<mpsc::Sender<OutboundCmd>>,
    inbound_gb_link: VecDeque<GbLinkPayload>,
    switch_ldn_tx: Option<mpsc::Sender<(u8, SwitchLdnPacket)>>,
    session_generation: u64,
}

impl Default for RelayInner {
    fn default() -> Self {
        Self {
            status: RelayStatusSnapshot::default(),
            cmd_tx: None,
            inbound_gb_link: VecDeque::with_capacity(32),
            switch_ldn_tx: None,
            session_generation: 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct RelayClient {
    in_room: Arc<AtomicBool>,
    inner: Arc<Mutex<RelayInner>>,
}

impl RelayClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Vérification sans verrou O(1) utilisée dans la boucle vidéo 60 Hz.
    pub fn is_in_room(&self) -> bool {
        self.in_room.load(Ordering::Relaxed)
    }

    pub fn status_snapshot(&self) -> RelayStatusSnapshot {
        self.inner
            .lock()
            .map(|g| g.status.clone())
            .unwrap_or_default()
    }

    /// Retourne l'identifiant du pair Hôte actif dans le salon (pour le routage Switch LDN).
    pub fn host_peer_id(&self) -> Option<u8> {
        let guard = self.inner.lock().ok()?;
        if !guard.status.connected {
            return None;
        }
        guard
            .status
            .peers
            .iter()
            .find(|p| p.is_host)
            .map(|p| p.peer_id)
            .or(Some(1))
    }

    /// Envoie un datagramme Switch LDN chiffré `ZMR1` en unicast (`target_peer_id`).
    pub fn send_switch_ldn_packet(&self, target_peer_id: u8, packet: SwitchLdnPacket) -> bool {
        if !self.is_in_room() {
            return false;
        }
        let tx = self.inner.lock().ok().and_then(|g| g.cmd_tx.clone());
        match tx {
            Some(tx) => tx
                .try_send(OutboundCmd::SwitchLdn {
                    target_peer_id,
                    packet,
                })
                .is_ok(),
            None => false,
        }
    }

    /// Attache un récepteur asynchrone pour le canal `RelayChannel::SwitchLdn`.
    pub fn attach_switch_ldn_receiver(&self) -> mpsc::Receiver<(u8, SwitchLdnPacket)> {
        let (tx, rx) = mpsc::channel(256);
        if let Ok(mut guard) = self.inner.lock() {
            guard.switch_ldn_tx = Some(tx);
        }
        rx
    }

    pub fn detach_switch_ldn_receiver(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.switch_ldn_tx = None;
        }
    }

    /// Empile un paquet Câble Link GB/GBC pour chiffrement `ZMR1` et envoi asynchrone.
    pub fn send_gb_link_payload(&self, payload: GbLinkPayload) {
        if !self.is_in_room() {
            return;
        }
        let tx = self.inner.lock().ok().and_then(|g| g.cmd_tx.clone());
        if let Some(tx) = tx {
            let _ = tx.try_send(OutboundCmd::GbLink(payload));
        }
    }

    /// Récupère tous les paquets Câble Link GB/GBC déchiffrés et authentifiés depuis le relais.
    pub fn drain_incoming_gb_link(&self) -> Vec<GbLinkPayload> {
        if !self.is_in_room() {
            return Vec::new();
        }
        let Ok(mut guard) = self.inner.lock() else {
            return Vec::new();
        };
        guard.inbound_gb_link.drain(..).collect()
    }

    /// Envoie un message de chat chiffré `ZMR1` (`ChaCha20-Poly1305`) dans le salon actif.
    pub fn send_chat(&self, text: &str) -> Result<RelayStatusSnapshot, String> {
        let cleaned: String = text
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(400)
            .collect();
        if cleaned.is_empty() {
            return Err("message vide".into());
        }
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "verrou relais indisponible".to_string())?;
        if !guard.status.connected || guard.cmd_tx.is_none() {
            return Err("vous n'êtes connecté à aucun salon relais".into());
        }
        let sender = if guard.status.player_name.is_empty() {
            format!("Joueur {}", guard.status.peer_id)
        } else {
            guard.status.player_name.clone()
        };
        let ts = unix_now_ms();
        let msg = RelayChatPayload {
            sender: sender.clone(),
            text: cleaned.clone(),
            timestamp_ms: ts,
        };
        if let Some(ref tx) = guard.cmd_tx {
            tx.try_send(OutboundCmd::Chat(msg))
                .map_err(|_| "file d'envoi relais saturée".to_string())?;
        }
        push_chat_bounded(
            &mut guard.status.chat_messages,
            RelayChatEntry {
                sender,
                text: cleaned,
                timestamp_ms: ts,
                is_self: true,
            },
        );
        Ok(guard.status.clone())
    }

    /// Quitte proprement le salon actif et ferme la connexion WebSocket sortante.
    pub fn leave_room(&self) -> RelayStatusSnapshot {
        self.in_room.store(false, Ordering::Relaxed);
        let Ok(mut guard) = self.inner.lock() else {
            return RelayStatusSnapshot::default();
        };
        guard.session_generation = guard.session_generation.wrapping_add(1);
        if let Some(tx) = guard.cmd_tx.take() {
            let _ = tx.try_send(OutboundCmd::Leave);
        }
        guard.inbound_gb_link.clear();
        guard.switch_ldn_tx = None;
        guard.status = RelayStatusSnapshot::default();
        guard.status.clone()
    }

    /// Crée un nouveau salon privé sur le relais VPS et génère sa clé E2EE 256 bits locale.
    pub async fn create_room(
        &self,
        params: RelayCreateParams,
    ) -> Result<RelayStatusSnapshot, String> {
        let url = validate_relay_url(&params.relay_url)?;
        let key = params.access_key.trim().to_string();
        if key.len() < 16 {
            return Err(
                "clé d'accès relais manquante ou trop courte (configurez-la dans Paramètres → Multijoueur)"
                    .into(),
            );
        }
        let session_key = RelaySessionKey::generate()?;
        let player_name = sanitize_name(&params.player_name, "Hôte");
        let control = WireClientControl::CreateRoom {
            console: params.console.clone(),
            game_title: params.game_title.clone(),
            rom_crc32: params.rom_crc32.clone(),
            player_name: player_name.clone(),
            password_hash: params.password.as_deref().and_then(hash_room_password),
            max_players: params.max_players.clamp(2, 8),
        };
        self.connect_and_enter(&url, &key, session_key, player_name, control)
            .await
    }

    /// Rejoint un salon existant à partir d'un lien `zaimule://room/CODE?...#KEY` (ou `CODE` + clé).
    pub async fn join_room(&self, params: RelayJoinParams) -> Result<RelayStatusSnapshot, String> {
        let url = validate_relay_url(&params.relay_url)?;
        let key = params.access_key.trim().to_string();
        if key.len() < 16 {
            return Err(
                "clé d'accès relais manquante ou trop courte (configurez-la dans Paramètres → Multijoueur)"
                    .into(),
            );
        }

        let (room_code, session_key, invite_crc) = if let Ok(invite) =
            parse_relay_invite(&params.invite_or_code)
        {
            (invite.room_code, invite.session_key, invite.rom_crc32_hex)
        } else {
            let code: String = params
                .invite_or_code
                .trim()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_uppercase();
            if code.len() < 4 || code.len() > 12 {
                return Err(
                        "lien d'invitation ou code de salon invalide (attendu : zaimule://room/CODE?...#CLE)"
                            .into(),
                    );
            }
            let raw_key = params
                .explicit_session_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    "clé E2EE manquante : collez le lien complet zaimule://room/CODE#CLE"
                        .to_string()
                })?;
            (code, RelaySessionKey::from_b64url(raw_key)?, None)
        };

        let player_name = sanitize_name(&params.player_name, "Joueur");
        let effective_crc = params.local_rom_crc32.or(invite_crc);
        let control = WireClientControl::JoinRoom {
            room_code,
            rom_crc32: effective_crc,
            player_name: player_name.clone(),
            password_hash: params.password.as_deref().and_then(hash_room_password),
        };
        self.connect_and_enter(&url, &key, session_key, player_name, control)
            .await
    }

    async fn connect_and_enter(
        &self,
        url: &str,
        access_key: &str,
        session_key: RelaySessionKey,
        player_name: String,
        join_or_create: WireClientControl,
    ) -> Result<RelayStatusSnapshot, String> {
        ensure_rustls_ring_provider();
        // Ferme toute session précédente proprement
        let _ = self.leave_room();

        let mut req = url
            .into_client_request()
            .map_err(|e| format!("URL relais invalide : {e}"))?;
        let header_val = HeaderValue::from_str(access_key)
            .map_err(|_| "clé d'accès relais contient des caractères invalides".to_string())?;
        req.headers_mut().insert("x-zaimule-key", header_val);

        let (ws_stream, _) =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::connect_async(req))
                .await
                .map_err(|_| "délai dépassé lors de la connexion au serveur relais".to_string())?
                .map_err(format_ws_connect_error)?;

        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        // Attendre le message `welcome` initial
        let welcome_raw = tokio::time::timeout(HANDSHAKE_TIMEOUT, ws_rx.next())
            .await
            .map_err(|_| "le serveur relais n'a pas répondu (welcome)".to_string())?
            .ok_or_else(|| "connexion fermée par le relais".to_string())?
            .map_err(|e| format!("erreur WebSocket : {e}"))?;
        if let WsMessage::Text(text) = welcome_raw {
            if let Ok(WireServerControl::Error { message, .. }) = serde_json::from_str(&text) {
                return Err(format!("relais : {message}"));
            }
        }

        // Envoyer CreateRoom ou JoinRoom
        let payload = serde_json::to_string(&join_or_create)
            .map_err(|e| format!("sérialisation commande relais : {e}"))?;
        ws_tx
            .send(WsMessage::Text(payload.into()))
            .await
            .map_err(|e| format!("échec envoi commande salon : {e}"))?;

        // Attendre RoomCreated / RoomJoined (ou Error)
        let entered = loop {
            let next = tokio::time::timeout(HANDSHAKE_TIMEOUT, ws_rx.next())
                .await
                .map_err(|_| "délai dépassé en attente de confirmation du salon".to_string())?
                .ok_or_else(|| "le relais a fermé la connexion".to_string())?
                .map_err(|e| format!("erreur WebSocket : {e}"))?;

            if let WsMessage::Text(text) = next {
                match serde_json::from_str::<WireServerControl>(&text) {
                    Ok(
                        WireServerControl::RoomCreated {
                            room_code,
                            peer_id,
                            peers,
                            console,
                            game_title,
                            rom_crc32,
                            max_players,
                        }
                        | WireServerControl::RoomJoined {
                            room_code,
                            peer_id,
                            peers,
                            console,
                            game_title,
                            rom_crc32,
                            max_players,
                        },
                    ) => {
                        break (
                            room_code,
                            peer_id,
                            peers,
                            console,
                            game_title,
                            rom_crc32,
                            max_players,
                        );
                    }
                    Ok(WireServerControl::Error { code, message }) => {
                        return Err(format!("refusé par le relais ({code}) : {message}"));
                    }
                    Ok(WireServerControl::Welcome { .. }) => continue,
                    _ => continue,
                }
            }
        };

        let (room_code, peer_id, peers, console, game_title, rom_crc32, max_players) = entered;
        let cipher = Arc::new(Zmr1Cipher::new(&session_key)?);
        let invite_uri = format_relay_invite(
            &room_code,
            rom_crc32.as_deref(),
            Some(&console),
            &session_key,
        );
        let is_host = peers
            .iter()
            .find(|p| p.peer_id == peer_id)
            .map(|p| p.is_host)
            .unwrap_or(peer_id == 1);

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<OutboundCmd>(OUTBOUND_CMD_CAPACITY);
        let snapshot = RelayStatusSnapshot {
            connected: true,
            relay_url: Some(url.to_string()),
            room_code: Some(room_code.clone()),
            invite_uri: Some(invite_uri),
            peer_id,
            is_host,
            player_name,
            console,
            game_title,
            rom_crc32,
            max_players,
            peers,
            rtt_ms: None,
            e2ee_cipher: "ChaCha20-Poly1305 (ZMR1)".to_string(),
            packets_sent: 0,
            packets_received: 0,
            chat_messages: Vec::new(),
            last_error: None,
        };

        let generation = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| "verrou relais indisponible".to_string())?;
            guard.session_generation = guard.session_generation.wrapping_add(1);
            guard.status = snapshot.clone();
            guard.cmd_tx = Some(cmd_tx);
            guard.inbound_gb_link.clear();
            guard.session_generation
        };
        self.in_room.store(true, Ordering::Relaxed);

        let shared_inner = Arc::clone(&self.inner);
        let shared_flag = Arc::clone(&self.in_room);

        tokio::spawn(async move {
            let mut ping_ticker = tokio::time::interval(PING_INTERVAL);
            // Premier ping immédiat pour mesurer le RTT sans attendre 5 s
            let initial_ping = serde_json::to_string(&WireClientControl::Ping {
                ts_ms: unix_now_ms(),
            })
            .unwrap_or_default();
            let _ = ws_tx.send(WsMessage::Text(initial_ping.into())).await;

            loop {
                tokio::select! {
                    _ = ping_ticker.tick() => {
                        let ping_json = serde_json::to_string(&WireClientControl::Ping {
                            ts_ms: unix_now_ms(),
                        }).unwrap_or_default();
                        if ws_tx.send(WsMessage::Text(ping_json.into())).await.is_err() {
                            break;
                        }
                    }
                    maybe_cmd = cmd_rx.recv() => {
                        let Some(cmd) = maybe_cmd else { break };
                        match cmd {
                            OutboundCmd::Leave => {
                                let leave = serde_json::to_string(&WireClientControl::LeaveRoom)
                                    .unwrap_or_default();
                                let _ = ws_tx.send(WsMessage::Text(leave.into())).await;
                                let _ = ws_tx.send(WsMessage::Close(None)).await;
                                break;
                            }
                            OutboundCmd::GbLink(link_payload) => {
                                if let Ok(sealed) = cipher.seal(
                                    RelayChannel::GbLink,
                                    &room_code,
                                    &link_payload.to_bytes(),
                                ) {
                                    let frame = encode_client_binary(RelayChannel::GbLink, 0, &sealed);
                                    if ws_tx.send(WsMessage::Binary(frame.into())).await.is_err() {
                                        break;
                                    }
                                    if let Ok(mut g) = shared_inner.lock() {
                                        if g.session_generation == generation {
                                            g.status.packets_sent = g.status.packets_sent.saturating_add(1);
                                        }
                                    }
                                }
                            }
                            OutboundCmd::Chat(chat_payload) => {
                                if let Ok(plain) = serde_json::to_vec(&chat_payload) {
                                    if let Ok(sealed) = cipher.seal(
                                        RelayChannel::Chat,
                                        &room_code,
                                        &plain,
                                    ) {
                                        let frame = encode_client_binary(RelayChannel::Chat, 0, &sealed);
                                        if ws_tx.send(WsMessage::Binary(frame.into())).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                            OutboundCmd::SwitchLdn {
                                target_peer_id,
                                packet,
                            } => {
                                if let Ok(plain) = packet.to_bytes() {
                                    if let Ok(sealed) = cipher.seal(
                                        RelayChannel::SwitchLdn,
                                        &room_code,
                                        &plain,
                                    ) {
                                        let frame = encode_client_binary(
                                            RelayChannel::SwitchLdn,
                                            target_peer_id,
                                            &sealed,
                                        );
                                        if ws_tx.send(WsMessage::Binary(frame.into())).await.is_err() {
                                            break;
                                        }
                                        if let Ok(mut g) = shared_inner.lock() {
                                            if g.session_generation == generation {
                                                g.status.packets_sent =
                                                    g.status.packets_sent.saturating_add(1);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    maybe_msg = ws_rx.next() => {
                        let Some(Ok(msg)) = maybe_msg else { break };
                        match msg {
                            WsMessage::Text(text) => {
                                if let Ok(ctrl) = serde_json::from_str::<WireServerControl>(&text) {
                                    apply_server_control(&shared_inner, generation, ctrl);
                                }
                            }
                            WsMessage::Binary(bytes) => {
                                handle_inbound_binary(
                                    &shared_inner,
                                    generation,
                                    &cipher,
                                    &room_code,
                                    &bytes,
                                );
                            }
                            WsMessage::Close(_) => break,
                            _ => {}
                        }
                    }
                }
            }

            if let Ok(mut g) = shared_inner.lock() {
                if g.session_generation == generation {
                    shared_flag.store(false, Ordering::Relaxed);
                    g.cmd_tx = None;
                    g.switch_ldn_tx = None;
                    g.status.connected = false;
                }
            }
        });

        Ok(snapshot)
    }
}

fn ensure_rustls_ring_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

fn format_ws_connect_error(err: tokio_tungstenite::tungstenite::Error) -> String {
    let text = err.to_string();
    if text.contains("401") {
        return "accès refusé (401) : clé privée ZAIMULE_RELAY_ACCESS_KEY invalide".into();
    }
    if text.contains("503") {
        return "serveur relais non configuré (503) : vérifiez ZAIMULE_RELAY_ACCESS_KEY sur le VPS"
            .into();
    }
    if text.contains("429") {
        return "trop de connexions simultanées sur ce relais (429)".into();
    }
    format!("impossible de joindre le relais : {text}")
}

fn encode_client_binary(
    channel: RelayChannel,
    target_peer_id: u8,
    opaque_payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + opaque_payload.len());
    out.push(channel as u8);
    out.push(target_peer_id);
    out.extend_from_slice(opaque_payload);
    out
}

fn apply_server_control(inner: &Arc<Mutex<RelayInner>>, generation: u64, ctrl: WireServerControl) {
    let Ok(mut guard) = inner.lock() else {
        return;
    };
    if guard.session_generation != generation {
        return;
    }
    match ctrl {
        WireServerControl::PeerJoined { peer } => {
            if !guard.status.peers.iter().any(|p| p.peer_id == peer.peer_id) {
                guard.status.peers.push(peer);
            }
        }
        WireServerControl::PeerLeft { peer_id, .. } => {
            guard.status.peers.retain(|p| p.peer_id != peer_id);
        }
        WireServerControl::HostMigrated { new_host_peer_id } => {
            for p in &mut guard.status.peers {
                p.is_host = p.peer_id == new_host_peer_id;
            }
            guard.status.is_host = guard.status.peer_id == new_host_peer_id;
        }
        WireServerControl::Pong { ts_ms, .. } => {
            let now = unix_now_ms();
            let rtt = now.saturating_sub(ts_ms).min(u32::MAX as u64) as u32;
            guard.status.rtt_ms = Some(rtt);
        }
        WireServerControl::Error { code, message } => {
            guard.status.last_error = Some(format!("{code}: {message}"));
        }
        _ => {}
    }
}

fn handle_inbound_binary(
    inner: &Arc<Mutex<RelayInner>>,
    generation: u64,
    cipher: &Zmr1Cipher,
    room_code: &str,
    raw: &[u8],
) {
    if raw.len() <= 2 {
        return;
    }
    let Some(channel) = RelayChannel::from_byte(raw[0]) else {
        return;
    };
    let sender_peer_id = raw[1];
    let opaque = &raw[2..];

    let Ok(plain) = cipher.open(channel, room_code, opaque) else {
        // Trame corrompue ou mauvaise clé E2EE -> ignorée silencieusement
        return;
    };

    let Ok(mut guard) = inner.lock() else {
        return;
    };
    if guard.session_generation != generation {
        return;
    }

    match channel {
        RelayChannel::GbLink => {
            if let Ok(pkt) = GbLinkPayload::from_bytes(&plain) {
                if guard.inbound_gb_link.len() >= MAX_INBOUND_LINK_QUEUE {
                    let _ = guard.inbound_gb_link.pop_front();
                }
                guard.inbound_gb_link.push_back(pkt);
                guard.status.packets_received = guard.status.packets_received.saturating_add(1);
            }
        }
        RelayChannel::Chat => {
            if let Ok(chat) = serde_json::from_slice::<RelayChatPayload>(&plain) {
                push_chat_bounded(
                    &mut guard.status.chat_messages,
                    RelayChatEntry {
                        sender: chat.sender,
                        text: chat.text,
                        timestamp_ms: chat.timestamp_ms,
                        is_self: false,
                    },
                );
            }
        }
        RelayChannel::SwitchLdn => {
            if let Ok(pkt) = SwitchLdnPacket::from_bytes(&plain) {
                guard.status.packets_received = guard.status.packets_received.saturating_add(1);
                if let Some(ref tx) = guard.switch_ldn_tx {
                    let _ = tx.try_send((sender_peer_id, pkt));
                }
            }
        }
        _ => {}
    }
}

fn push_chat_bounded(list: &mut Vec<RelayChatEntry>, msg: RelayChatEntry) {
    list.push(msg);
    if list.len() > MAX_CHAT_HISTORY {
        let excess = list.len() - MAX_CHAT_HISTORY;
        list.drain(0..excess);
    }
}

fn sanitize_name(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(32)
        .collect();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::http::StatusCode;
    use zaimule_core::netplay::GB_LINK_FLAG_MASTER_TX;

    #[tokio::test]
    async fn relay_client_authenticates_and_exchanges_e2ee_gb_link_and_chat() {
        const SECRET: &str = "zaimule-local-test-secret-key-0123456789";

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (bcast_tx, _) = broadcast::channel::<(u8, Vec<u8>)>(32);
        let server_bcast = bcast_tx.clone();
        let seen_ciphertext = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let seen_ct_srv = Arc::clone(&seen_ciphertext);

        // Mini relais local simulant exactement le protocole de apps/public/zaimule-relay
        let server = tokio::spawn(async move {
            let mut next_peer: u8 = 0;
            while let Ok((stream, _)) = listener.accept().await {
                let btx = server_bcast.clone();
                let mut brx = server_bcast.subscribe();
                let seen_ct = Arc::clone(&seen_ct_srv);
                let assigned_peer = next_peer.saturating_add(1);

                #[allow(clippy::result_large_err)]
                let callback = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                    let ok = req
                        .headers()
                        .get("x-zaimule-key")
                        .and_then(|v| v.to_str().ok())
                        == Some(SECRET);
                    if ok {
                        Ok(resp)
                    } else {
                        let mut err = ErrorResponse::new(Some("unauthorized".into()));
                        *err.status_mut() = StatusCode::UNAUTHORIZED;
                        Err(err)
                    }
                };

                let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                    continue;
                };
                next_peer = assigned_peer;

                tokio::spawn(async move {
                    let (mut tx, mut rx) = ws.split();
                    let _ = tx
                        .send(WsMessage::Text(
                            r#"{"type":"welcome","protocol_version":1,"max_frame_bytes":65536}"#
                                .into(),
                        ))
                        .await;
                    loop {
                        tokio::select! {
                            incoming = rx.next() => {
                                let Some(Ok(msg)) = incoming else { break };
                                match msg {
                                    WsMessage::Text(t) => {
                                        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                                        match v["type"].as_str() {
                                            Some("create_room") => {
                                                let reply = serde_json::json!({
                                                    "type": "room_created",
                                                    "room_code": "PKMN42",
                                                    "peer_id": assigned_peer,
                                                    "peers": [{"peerId": 1, "name": "Alice", "isHost": true}],
                                                    "console": "GBC",
                                                    "game_title": "Pokemon Crystal",
                                                    "rom_crc32": "DEADBEEF",
                                                    "max_players": 2
                                                });
                                                let _ = tx.send(WsMessage::Text(reply.to_string().into())).await;
                                            }
                                            Some("join_room") => {
                                                let reply = serde_json::json!({
                                                    "type": "room_joined",
                                                    "room_code": "PKMN42",
                                                    "peer_id": assigned_peer,
                                                    "peers": [
                                                        {"peerId": 1, "name": "Alice", "isHost": true},
                                                        {"peerId": 2, "name": "Bob", "isHost": false}
                                                    ],
                                                    "console": "GBC",
                                                    "game_title": "Pokemon Crystal",
                                                    "rom_crc32": "DEADBEEF",
                                                    "max_players": 2
                                                });
                                                let _ = tx.send(WsMessage::Text(reply.to_string().into())).await;
                                            }
                                            Some("ping") => {
                                                let ts = v["ts_ms"].as_u64().unwrap_or(0);
                                                let reply = serde_json::json!({
                                                    "type": "pong",
                                                    "ts_ms": ts,
                                                    "server_ts_ms": ts
                                                });
                                                let _ = tx.send(WsMessage::Text(reply.to_string().into())).await;
                                            }
                                            _ => {}
                                        }
                                    }
                                    WsMessage::Binary(bin) => {
                                        seen_ct.lock().unwrap().push(bin.to_vec());
                                        let mut out = bin.to_vec();
                                        out[1] = assigned_peer;
                                        let _ = btx.send((assigned_peer, out));
                                    }
                                    WsMessage::Close(_) => break,
                                    _ => {}
                                }
                            }
                            Ok((sender, bytes)) = brx.recv() => {
                                if sender != assigned_peer {
                                    let _ = tx.send(WsMessage::Binary(bytes.into())).await;
                                }
                            }
                        }
                    }
                });
            }
        });

        let url = format!("ws://{addr}/ws");

        // 1. Mauvaise clé rejetée immédiatement en 401
        let bad_client = RelayClient::new();
        let err = bad_client
            .create_room(RelayCreateParams {
                relay_url: url.clone(),
                access_key: "mauvaise-cle-0123456789abcdef".into(),
                console: "GBC".into(),
                game_title: "Pokemon Crystal".into(),
                rom_crc32: Some("DEADBEEF".into()),
                player_name: "Mallory".into(),
                password: None,
                max_players: 2,
            })
            .await
            .unwrap_err();
        assert!(err.contains("401"));

        // 2. Alice crée un salon et récupère le lien d'invitation avec clé #KEY
        let alice = RelayClient::new();
        let alice_status = alice
            .create_room(RelayCreateParams {
                relay_url: url.clone(),
                access_key: SECRET.into(),
                console: "GBC".into(),
                game_title: "Pokemon Crystal".into(),
                rom_crc32: Some("DEADBEEF".into()),
                player_name: "Alice".into(),
                password: None,
                max_players: 2,
            })
            .await
            .unwrap();
        assert!(alice_status.connected);
        assert_eq!(alice_status.room_code.as_deref(), Some("PKMN42"));
        let invite = alice_status.invite_uri.clone().expect("invite URI");
        assert!(invite.contains("zaimule://room/PKMN42"));
        assert!(invite.contains('#'));

        // 3. Bob rejoint via le lien d'invitation E2EE
        let bob = RelayClient::new();
        let bob_status = bob
            .join_room(RelayJoinParams {
                relay_url: url,
                access_key: SECRET.into(),
                invite_or_code: invite,
                explicit_session_key: None,
                local_rom_crc32: Some("DEADBEEF".into()),
                player_name: "Bob".into(),
                password: None,
            })
            .await
            .unwrap();
        assert!(bob_status.connected);
        assert_eq!(bob_status.peer_id, 2);

        // 4. Alice envoie une trame Câble Link + un message de chat chiffré
        let link_pkt = GbLinkPayload {
            frame: 99,
            buttons_mask: 0x0010,
            sio_sb: 0x7E,
            sio_sc: 0x81,
            flags: GB_LINK_FLAG_MASTER_TX,
            seq: 1,
        };
        alice.send_gb_link_payload(link_pkt);
        alice.send_chat("Echange Ectoplasma pret !").unwrap();

        // Attendre la réception déchiffrée chez Bob
        let mut received_link = Vec::new();
        let mut received_chat = Vec::new();
        for _ in 0..40 {
            received_link.extend(bob.drain_incoming_gb_link());
            received_chat = bob.status_snapshot().chat_messages;
            if !received_link.is_empty() && !received_chat.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(received_link, vec![link_pkt]);
        assert_eq!(received_chat.len(), 1);
        assert_eq!(received_chat[0].sender, "Alice");
        assert_eq!(received_chat[0].text, "Echange Ectoplasma pret !");
        assert!(!received_chat[0].is_self);

        // 5. Le serveur relais n'a vu AUCUN texte en clair (chiffrement ZMR1 vérifié)
        let raw_frames = seen_ciphertext.lock().unwrap();
        assert!(raw_frames.len() >= 2);
        for frame in raw_frames.iter() {
            let as_lossy = String::from_utf8_lossy(frame);
            assert!(!as_lossy.contains("Ectoplasma"));
            assert!(&frame[2..6] == b"ZMR1");
        }

        alice.leave_room();
        bob.leave_room();
        assert!(!alice.is_in_room());
        server.abort();
    }
}
