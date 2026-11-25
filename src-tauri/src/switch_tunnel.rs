//! Tunnel UDP Loopback `127.0.0.1` chiffré E2EE (`ZMR1`, `RelayChannel::SwitchLdn`)
//! pour le multijoueur Nintendo Switch 1 & 2 (`Torzu` / `Fritchii`).
//!
//! - **0 port ouvert sur les PC ou box des joueurs** : toutes les sockets locales sont
//!   strictement liées à `127.0.0.1` (boucle locale uniquement, inaccessible depuis le LAN/WAN).
//! - **Mode Invité (`is_host == false`)** : ouvre un proxy UDP éphémère sur `127.0.0.1:<port>` ;
//!   chaque datagramme ENet envoyé par `Torzu`/`Fritchii` est chiffré en `ChaCha20-Poly1305`
//!   et transmis en unicast vers l'hôte via `wss://relay.<domaine>/ws`.
//! - **Mode Hôte (`is_host == true`)** : reçoit les datagrammes chiffrés des invités, ouvre une
//!   socket loopback dédiée par `(peer_id, client_port)` et les relaie vers la salle locale
//!   `Torzu`/`Fritchii` sur `127.0.0.1:<host_room_port>` (par défaut `24872`).

use crate::relay_client::RelayClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use zaimule_core::netplay::{
    SwitchLdnPacket, DEFAULT_SWITCH_LDN_PORT, SWITCH_LDN_DIR_CLIENT_TO_HOST,
    SWITCH_LDN_DIR_HOST_TO_CLIENT, SWITCH_LDN_MAX_PAYLOAD_BYTES,
};

const MAX_HOST_NAT_ENTRIES: usize = 64;
const HOST_NAT_IDLE_TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SwitchTunnelStatus {
    pub active: bool,
    pub role: String,
    pub local_endpoint: Option<String>,
    pub local_port: u16,
    pub host_room_port: u16,
    pub packets_out: u64,
    pub packets_in: u64,
    pub active_clients: usize,
    pub last_error: Option<String>,
}

impl Default for SwitchTunnelStatus {
    fn default() -> Self {
        Self {
            active: false,
            role: "inactive".to_string(),
            local_endpoint: None,
            local_port: 0,
            host_room_port: DEFAULT_SWITCH_LDN_PORT,
            packets_out: 0,
            packets_in: 0,
            active_clients: 0,
            last_error: None,
        }
    }
}

#[derive(Default)]
struct TunnelInner {
    status: SwitchTunnelStatus,
    stop_tx: Option<watch::Sender<bool>>,
    generation: u64,
}

#[derive(Clone, Default)]
pub struct SwitchLdnTunnel {
    running: Arc<AtomicBool>,
    inner: Arc<Mutex<TunnelInner>>,
}

impl SwitchLdnTunnel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn status_snapshot(&self) -> SwitchTunnelStatus {
        self.inner
            .lock()
            .map(|g| g.status.clone())
            .unwrap_or_default()
    }

    /// Arrête immédiatement toutes les sockets loopback et réinitialise l'état.
    pub fn stop(&self, relay: &RelayClient) -> SwitchTunnelStatus {
        self.running.store(false, Ordering::Relaxed);
        relay.detach_switch_ldn_receiver();
        let Ok(mut guard) = self.inner.lock() else {
            return SwitchTunnelStatus::default();
        };
        guard.generation = guard.generation.wrapping_add(1);
        if let Some(tx) = guard.stop_tx.take() {
            let _ = tx.send(true);
        }
        guard.status = SwitchTunnelStatus::default();
        guard.status.clone()
    }

    /// Démarre le tunnel loopback `127.0.0.1` en mode Hôte ou Invité selon le rôle du salon actif.
    pub async fn start(
        &self,
        relay: Arc<RelayClient>,
        host_room_port: Option<u16>,
    ) -> Result<SwitchTunnelStatus, String> {
        let relay_status = relay.status_snapshot();
        if !relay_status.connected {
            return Err(
                "connectez-vous d'abord à un salon Multijoueur E2EE avant d'activer le tunnel Switch"
                    .into(),
            );
        }

        // Arrêter tout tunnel précédent proprement
        let _ = self.stop(&relay);

        let room_port = host_room_port
            .filter(|&p| p >= 1024)
            .unwrap_or(DEFAULT_SWITCH_LDN_PORT);
        let (stop_tx, stop_rx) = watch::channel(false);
        let inbound_rx = relay.attach_switch_ldn_receiver();

        if relay_status.is_host {
            let status = SwitchTunnelStatus {
                active: true,
                role: "host".to_string(),
                local_endpoint: Some(format!("127.0.0.1:{room_port}")),
                local_port: room_port,
                host_room_port: room_port,
                packets_out: 0,
                packets_in: 0,
                active_clients: 0,
                last_error: None,
            };
            let generation = self.install_active(status.clone(), stop_tx)?;
            let shared_inner = Arc::clone(&self.inner);
            let shared_running = Arc::clone(&self.running);
            tokio::spawn(run_host_loop(
                relay,
                room_port,
                inbound_rx,
                stop_rx,
                shared_inner,
                shared_running,
                generation,
            ));
            Ok(status)
        } else {
            // Mode Invité : socket UDP strictement liée à 127.0.0.1:0 (jamais exposée sur le réseau)
            let loopback = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
            let proxy_socket = UdpSocket::bind(loopback).await.map_err(|e| {
                format!("impossible de lier le proxy loopback Switch 127.0.0.1 : {e}")
            })?;
            let local_port = proxy_socket
                .local_addr()
                .map_err(|e| format!("adresse locale loopback introuvable : {e}"))?
                .port();

            let status = SwitchTunnelStatus {
                active: true,
                role: "guest".to_string(),
                local_endpoint: Some(format!("127.0.0.1:{local_port}")),
                local_port,
                host_room_port: room_port,
                packets_out: 0,
                packets_in: 0,
                active_clients: 1,
                last_error: None,
            };
            let generation = self.install_active(status.clone(), stop_tx)?;
            let shared_inner = Arc::clone(&self.inner);
            let shared_running = Arc::clone(&self.running);
            tokio::spawn(run_guest_loop(
                relay,
                proxy_socket,
                room_port,
                inbound_rx,
                stop_rx,
                shared_inner,
                shared_running,
                generation,
            ));
            Ok(status)
        }
    }

    fn install_active(
        &self,
        status: SwitchTunnelStatus,
        stop_tx: watch::Sender<bool>,
    ) -> Result<u64, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "verrou tunnel Switch indisponible".to_string())?;
        guard.generation = guard.generation.wrapping_add(1);
        guard.status = status;
        guard.stop_tx = Some(stop_tx);
        self.running.store(true, Ordering::Relaxed);
        Ok(guard.generation)
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_guest_loop(
    relay: Arc<RelayClient>,
    proxy_socket: UdpSocket,
    host_room_port: u16,
    mut inbound_rx: tokio::sync::mpsc::Receiver<(u8, SwitchLdnPacket)>,
    mut stop_rx: watch::Receiver<bool>,
    inner: Arc<Mutex<TunnelInner>>,
    running: Arc<AtomicBool>,
    generation: u64,
) {
    let mut recv_buf = vec![0u8; SWITCH_LDN_MAX_PAYLOAD_BYTES + 128];

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    break;
                }
            }
            udp_res = proxy_socket.recv_from(&mut recv_buf) => {
                let Ok((len, src)) = udp_res else { break };
                // Sécurité stricte : n'accepter que les paquets locaux 127.0.0.1
                if !src.ip().is_loopback() || len == 0 || len > SWITCH_LDN_MAX_PAYLOAD_BYTES {
                    continue;
                }
                let Some(host_peer) = relay.host_peer_id() else {
                    continue;
                };
                let pkt = SwitchLdnPacket::udp_client_to_host(
                    src.port(),
                    host_room_port,
                    recv_buf[..len].to_vec(),
                );
                if relay.send_switch_ldn_packet(host_peer, pkt) {
                    if let Ok(mut g) = inner.lock() {
                        if g.generation == generation {
                            g.status.packets_out = g.status.packets_out.saturating_add(1);
                        }
                    }
                }
            }
            maybe_in = inbound_rx.recv() => {
                let Some((_sender_peer, pkt)) = maybe_in else { break };
                if pkt.direction != SWITCH_LDN_DIR_HOST_TO_CLIENT {
                    continue;
                }
                let dst = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, pkt.client_port));
                if proxy_socket.send_to(&pkt.payload, dst).await.is_ok() {
                    if let Ok(mut g) = inner.lock() {
                        if g.generation == generation {
                            g.status.packets_in = g.status.packets_in.saturating_add(1);
                        }
                    }
                }
            }
        }
    }

    mark_stopped(&inner, &running, generation);
}

struct HostNatEntry {
    socket: Arc<UdpSocket>,
    last_seen: Instant,
    stop_tx: watch::Sender<bool>,
}

async fn run_host_loop(
    relay: Arc<RelayClient>,
    host_room_port: u16,
    mut inbound_rx: tokio::sync::mpsc::Receiver<(u8, SwitchLdnPacket)>,
    mut stop_rx: watch::Receiver<bool>,
    inner: Arc<Mutex<TunnelInner>>,
    running: Arc<AtomicBool>,
    generation: u64,
) {
    let target_room = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, host_room_port));
    let mut nat_table: HashMap<(u8, u16), HostNatEntry> = HashMap::new();
    let mut janitor = tokio::time::interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    break;
                }
            }
            _ = janitor.tick() => {
                let now = Instant::now();
                let expired: Vec<(u8, u16)> = nat_table
                    .iter()
                    .filter(|(_, e)| now.duration_since(e.last_seen) >= HOST_NAT_IDLE_TTL)
                    .map(|(&k, _)| k)
                    .collect();
                for k in expired {
                    if let Some(entry) = nat_table.remove(&k) {
                        let _ = entry.stop_tx.send(true);
                    }
                }
                if let Ok(mut g) = inner.lock() {
                    if g.generation == generation {
                        g.status.active_clients = nat_table.len();
                    }
                }
            }
            maybe_in = inbound_rx.recv() => {
                let Some((sender_peer, pkt)) = maybe_in else { break };
                if pkt.direction != SWITCH_LDN_DIR_CLIENT_TO_HOST {
                    continue;
                }
                let key = (sender_peer, pkt.client_port);
                if !nat_table.contains_key(&key) {
                    if nat_table.len() >= MAX_HOST_NAT_ENTRIES {
                        // Éviction de l'entrée la plus ancienne
                        if let Some(oldest_key) = nat_table
                            .iter()
                            .min_by_key(|(_, e)| e.last_seen)
                            .map(|(&k, _)| k)
                        {
                            if let Some(old) = nat_table.remove(&oldest_key) {
                                let _ = old.stop_tx.send(true);
                            }
                        }
                    }
                    let loopback = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
                    let Ok(sock) = UdpSocket::bind(loopback).await else {
                        continue;
                    };
                    let sock = Arc::new(sock);
                    let (entry_stop_tx, mut entry_stop_rx) = watch::channel(false);

                    // Tâche de lecture des réponses de la salle Torzu/Fritchii vers l'invité
                    let reader_sock = Arc::clone(&sock);
                    let reader_relay = Arc::clone(&relay);
                    let reader_inner = Arc::clone(&inner);
                    let client_port = pkt.client_port;
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; SWITCH_LDN_MAX_PAYLOAD_BYTES + 128];
                        loop {
                            tokio::select! {
                                _ = entry_stop_rx.changed() => {
                                    if *entry_stop_rx.borrow() {
                                        break;
                                    }
                                }
                                res = reader_sock.recv_from(&mut buf) => {
                                    let Ok((len, src)) = res else { break };
                                    if !src.ip().is_loopback() || len == 0 || len > SWITCH_LDN_MAX_PAYLOAD_BYTES {
                                        continue;
                                    }
                                    let reply = SwitchLdnPacket::udp_host_to_client(
                                        client_port,
                                        host_room_port,
                                        buf[..len].to_vec(),
                                    );
                                    if reader_relay.send_switch_ldn_packet(sender_peer, reply) {
                                        if let Ok(mut g) = reader_inner.lock() {
                                            if g.generation == generation {
                                                g.status.packets_out = g.status.packets_out.saturating_add(1);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    });

                    nat_table.insert(
                        key,
                        HostNatEntry {
                            socket: sock,
                            last_seen: Instant::now(),
                            stop_tx: entry_stop_tx,
                        },
                    );
                }

                if let Some(entry) = nat_table.get_mut(&key) {
                    entry.last_seen = Instant::now();
                    if entry.socket.send_to(&pkt.payload, target_room).await.is_ok() {
                        if let Ok(mut g) = inner.lock() {
                            if g.generation == generation {
                                g.status.packets_in = g.status.packets_in.saturating_add(1);
                                g.status.active_clients = nat_table.len();
                            }
                        }
                    }
                }
            }
        }
    }

    for (_, entry) in nat_table {
        let _ = entry.stop_tx.send(true);
    }
    mark_stopped(&inner, &running, generation);
}

fn mark_stopped(inner: &Arc<Mutex<TunnelInner>>, running: &Arc<AtomicBool>, generation: u64) {
    if let Ok(mut g) = inner.lock() {
        if g.generation == generation {
            running.store(false, Ordering::Relaxed);
            g.stop_tx = None;
            g.status.active = false;
            g.status.active_clients = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_client::{RelayCreateParams, RelayJoinParams};
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::http::StatusCode;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    #[tokio::test]
    async fn switch_ldn_tunnel_proxies_udp_datagrams_end_to_end_over_zmr1_e2ee_relay() {
        const SECRET: &str = "zaimule-switch-test-secret-0123456789ab";

        // 1. Salle Switch locale simulée (ex. Torzu / Fritchii sur 127.0.0.1:<port>)
        let room_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let room_port = room_sock.local_addr().unwrap().port();
        let room_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, src)) = room_sock.recv_from(&mut buf).await {
                let mut reply = b"ACK:".to_vec();
                reply.extend_from_slice(&buf[..n]);
                let _ = room_sock.send_to(&reply, src).await;
            }
        });

        // 2. Relais WS local simulant le routage unicast de apps/public/zaimule-relay
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = listener.local_addr().unwrap();
        let (bcast_tx, _) = broadcast::channel::<(u8, u8, Vec<u8>)>(64);
        let server_bcast = bcast_tx.clone();
        let captured_frames = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let captured_srv = Arc::clone(&captured_frames);

        let relay_task = tokio::spawn(async move {
            let mut next_peer: u8 = 0;
            while let Ok((stream, _)) = listener.accept().await {
                let btx = server_bcast.clone();
                let mut brx = server_bcast.subscribe();
                let cap = Arc::clone(&captured_srv);
                let assigned_peer = next_peer.saturating_add(1);

                #[allow(clippy::result_large_err)]
                let cb = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
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
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await else {
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
                                                    "room_code": "MK8D99",
                                                    "peer_id": assigned_peer,
                                                    "peers": [{"peerId": 1, "name": "Mario", "isHost": true}],
                                                    "console": "Switch",
                                                    "game_title": "Mario Kart 8 Deluxe",
                                                    "rom_crc32": null,
                                                    "max_players": 4
                                                });
                                                let _ = tx.send(WsMessage::Text(reply.to_string().into())).await;
                                            }
                                            Some("join_room") => {
                                                let reply = serde_json::json!({
                                                    "type": "room_joined",
                                                    "room_code": "MK8D99",
                                                    "peer_id": assigned_peer,
                                                    "peers": [
                                                        {"peerId": 1, "name": "Mario", "isHost": true},
                                                        {"peerId": 2, "name": "Luigi", "isHost": false}
                                                    ],
                                                    "console": "Switch",
                                                    "game_title": "Mario Kart 8 Deluxe",
                                                    "rom_crc32": null,
                                                    "max_players": 4
                                                });
                                                let _ = tx.send(WsMessage::Text(reply.to_string().into())).await;
                                            }
                                            _ => {}
                                        }
                                    }
                                    WsMessage::Binary(bin) => {
                                        let target = bin[1];
                                        cap.lock().unwrap().push(bin.to_vec());
                                        let mut out = bin.to_vec();
                                        out[1] = assigned_peer;
                                        let _ = btx.send((assigned_peer, target, out));
                                    }
                                    WsMessage::Close(_) => break,
                                    _ => {}
                                }
                            }
                            Ok((sender, target, bytes)) = brx.recv() => {
                                if sender != assigned_peer && (target == 0 || target == assigned_peer) {
                                    let _ = tx.send(WsMessage::Binary(bytes.into())).await;
                                }
                            }
                        }
                    }
                });
            }
        });

        let relay_url = format!("ws://{relay_addr}/ws");

        // 3. Hôte (Mario) crée le salon Switch et active son tunnel Hôte vers 127.0.0.1:<room_port>
        let host_relay = Arc::new(RelayClient::new());
        let host_snap = host_relay
            .create_room(RelayCreateParams {
                relay_url: relay_url.clone(),
                access_key: SECRET.into(),
                console: "Switch".into(),
                game_title: "Mario Kart 8 Deluxe".into(),
                rom_crc32: None,
                player_name: "Mario".into(),
                password: None,
                max_players: 4,
            })
            .await
            .unwrap();
        let invite = host_snap.invite_uri.clone().unwrap();

        let host_tunnel = SwitchLdnTunnel::new();
        let host_t_status = host_tunnel
            .start(Arc::clone(&host_relay), Some(room_port))
            .await
            .unwrap();
        assert!(host_t_status.active);
        assert_eq!(host_t_status.role, "host");

        // 4. Invité (Luigi) rejoint le salon et ouvre son proxy local 127.0.0.1:<proxy_port>
        let guest_relay = Arc::new(RelayClient::new());
        let _ = guest_relay
            .join_room(RelayJoinParams {
                relay_url,
                access_key: SECRET.into(),
                invite_or_code: invite,
                explicit_session_key: None,
                local_rom_crc32: None,
                player_name: "Luigi".into(),
                password: None,
            })
            .await
            .unwrap();

        let guest_tunnel = SwitchLdnTunnel::new();
        let guest_t_status = guest_tunnel
            .start(Arc::clone(&guest_relay), Some(room_port))
            .await
            .unwrap();
        assert!(guest_t_status.active);
        assert_eq!(guest_t_status.role, "guest");
        assert!(guest_t_status.local_port > 0);
        let guest_proxy_addr = guest_t_status.local_endpoint.unwrap();
        assert!(guest_proxy_addr.starts_with("127.0.0.1:"));

        // 5. L'émulateur Torzu de Luigi envoie un paquet ENet vers 127.0.0.1:<proxy_port>
        let luigi_emu = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        luigi_emu
            .send_to(b"MK8D-ENET-HELLO-LUIGI", &guest_proxy_addr)
            .await
            .unwrap();

        let mut reply_buf = [0u8; 256];
        let (len, _) =
            tokio::time::timeout(Duration::from_secs(3), luigi_emu.recv_from(&mut reply_buf))
                .await
                .expect("timeout waiting for Switch LDN reply")
                .unwrap();
        assert_eq!(&reply_buf[..len], b"ACK:MK8D-ENET-HELLO-LUIGI");

        // 6. Vérifier que le relais VPS n'a JAMAIS vu les paquets Switch en clair
        let raw = captured_frames.lock().unwrap();
        assert!(raw.len() >= 2);
        for frame in raw.iter() {
            assert_eq!(frame[0], 3); // RelayChannel::SwitchLdn
            assert_eq!(&frame[2..6], b"ZMR1");
            assert!(!String::from_utf8_lossy(frame).contains("MK8D-ENET-HELLO-LUIGI"));
        }

        // Arrêt propre
        let stopped = guest_tunnel.stop(&guest_relay);
        assert!(!stopped.active);
        let _ = host_tunnel.stop(&host_relay);
        guest_relay.leave_room();
        host_relay.leave_room();
        relay_task.abort();
        room_task.abort();
    }
}
