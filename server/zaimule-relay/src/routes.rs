use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::response::IntoResponse;
use axum::Json;
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::error::RelayError;
use crate::limits::{extract_client_ip, ConnectionGuard, ConnectionLimiter, FrameRateLimiter};
use crate::outbound::{Outbound, PeerSender};
use crate::protocol::{
    parse_client_binary_frame, ClientMessage, OutboundFrame, ServerMessage, MAX_CONTROL_TEXT_BYTES,
    PROTOCOL_VERSION,
};
use crate::rooms::{CreateRoomRequest, JoinRoomRequest, RoomError, RoomManager, RoomMembership};

/// Au-delà, un client qui ne lit plus est abandonné et son slot libéré.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub limiter: Arc<ConnectionLimiter>,
    pub rooms: Arc<RoomManager>,
    pub shutdown: watch::Receiver<bool>,
}

pub async fn health_check() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "service": "zaimule-relay"
    }))
}

pub async fn require_access_key(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, RelayError> {
    if !state.config.is_configured() {
        return Err(RelayError::ServiceUnavailable);
    }

    let provided_key = extract_access_key(req.headers()).ok_or(RelayError::Unauthorized)?;
    if !state.config.verify_access_key(provided_key) {
        return Err(RelayError::Unauthorized);
    }

    Ok(next.run(req).await)
}

pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, RelayError> {
    let client_ip = extract_client_ip(&headers);
    let Some(conn_guard) = state.limiter.try_acquire(&client_ip) else {
        tracing::warn!(client_ip = %client_ip, "Limite de connexions atteinte");
        return Err(RelayError::TooManyConnections);
    };

    let max_frame = state.config.max_frame_bytes;
    Ok(ws
        .max_frame_size(max_frame)
        .max_message_size(max_frame)
        // Les 128 Kio par défaut pèseraient à eux seuls 16 Mio à 128 connexions
        .read_buffer_size(16 * 1024)
        .write_buffer_size(16 * 1024)
        .on_upgrade(move |socket| handle_socket(socket, state, conn_guard)))
}

fn extract_access_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(val) = headers
        .get("x-zaimule-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(val);
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| {
            raw.strip_prefix("Bearer ")
                .or_else(|| raw.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

async fn handle_socket(socket: WebSocket, state: AppState, _conn_guard: ConnectionGuard) {
    let (ws_tx, mut ws_rx) = socket.split();
    let (out_tx, out_rx) = PeerSender::channel();

    // Accueil immédiat avec version du protocole et taille maximale de trame
    out_tx.push(OutboundFrame::Control(ServerMessage::Welcome {
        protocol_version: PROTOCOL_VERSION,
        max_frame_bytes: state.config.max_frame_bytes,
    }));

    let idle_timeout = state.config.ws_idle_timeout;
    // Les pings du serveur gardent en vie un joueur qui attend ou ne fait que recevoir
    let mut writer = tokio::spawn(write_loop(
        ws_tx,
        out_rx,
        state.shutdown.clone(),
        idle_timeout / 3,
        WRITE_TIMEOUT,
    ));

    let mut membership: Option<RoomMembership> = None;
    let mut rate_limiter = FrameRateLimiter::new(state.config.max_frames_per_sec);
    let mut reader_shutdown = state.shutdown.clone();
    let mut kicked = out_tx.kicked();

    loop {
        let next_msg = tokio::select! {
            _ = reader_shutdown.changed() => {
                if *reader_shutdown.borrow() {
                    break;
                }
                continue;
            }
            _ = kicked.changed() => break,
            _ = out_tx.writer_gone() => break,
            res = tokio::time::timeout(idle_timeout, ws_rx.next()) => {
                match res {
                    Ok(Some(Ok(msg))) => msg,
                    Ok(Some(Err(_))) | Ok(None) => break,
                    Err(_) => {
                        send_error(&out_tx, "idle_timeout", "connexion expirée pour inactivité");
                        break;
                    }
                }
            }
        };

        if !rate_limiter.allow() {
            send_error(&out_tx, "rate_limited", "débit maximal dépassé");
            break;
        }

        match next_msg {
            Message::Text(text) => {
                if text.len() > MAX_CONTROL_TEXT_BYTES {
                    send_error(
                        &out_tx,
                        "frame_too_large",
                        "message de contrôle trop volumineux",
                    );
                    break;
                }
                match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(ClientMessage::CreateRoom {
                        console,
                        game_title,
                        rom_crc32,
                        player_name,
                        password_hash,
                        max_players,
                    }) => {
                        let result = state.rooms.create_room(
                            CreateRoomRequest {
                                console,
                                game_title,
                                rom_crc32,
                                player_name,
                                password_hash,
                                max_players,
                            },
                            out_tx.clone(),
                        );
                        enter_room(&state.rooms, &mut membership, &out_tx, result);
                    }
                    Ok(ClientMessage::JoinRoom {
                        room_code,
                        rom_crc32,
                        player_name,
                        password_hash,
                    }) => {
                        let result = state.rooms.join_room(
                            JoinRoomRequest {
                                room_code,
                                rom_crc32,
                                player_name,
                                password_hash,
                            },
                            out_tx.clone(),
                        );
                        enter_room(&state.rooms, &mut membership, &out_tx, result);
                    }
                    Ok(ClientMessage::LeaveRoom) => {
                        if let Some(prev) = membership.take() {
                            state.rooms.leave_room(&prev, "left");
                        }
                    }
                    Ok(ClientMessage::Ping { ts_ms }) => {
                        out_tx.push(OutboundFrame::Control(ServerMessage::Pong {
                            ts_ms,
                            server_ts_ms: unix_now_ms(),
                        }));
                    }
                    Err(_) => {
                        send_error(&out_tx, "invalid_message", "commande JSON invalide");
                    }
                }
            }
            Message::Binary(bytes) => {
                if bytes.len() > state.config.max_frame_bytes {
                    send_error(&out_tx, "frame_too_large", "trame binaire trop volumineuse");
                    break;
                }
                let Some(ref member) = membership else {
                    send_error(
                        &out_tx,
                        "not_in_room",
                        "rejoignez un salon avant d'envoyer des données",
                    );
                    continue;
                };
                let Some((channel, target_peer_id, payload)) = parse_client_binary_frame(&bytes)
                else {
                    send_error(
                        &out_tx,
                        "invalid_binary_frame",
                        "en-tête binaire ZMR1 invalide",
                    );
                    continue;
                };
                if let Err(err) = state
                    .rooms
                    .relay_binary(member, channel, target_peer_id, payload)
                {
                    send_error(&out_tx, err.code, &err.message);
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => break,
        }
    }

    if let Some(member) = membership.take() {
        state.rooms.leave_room(&member, "disconnected");
    }
    let lagging = *kicked.borrow();
    drop(out_tx);
    if lagging
        || tokio::time::timeout(WRITE_TIMEOUT, &mut writer)
            .await
            .is_err()
    {
        writer.abort();
    }
}

async fn write_loop<S>(
    mut ws_tx: S,
    mut out_rx: mpsc::Receiver<Outbound>,
    mut shutdown_rx: watch::Receiver<bool>,
    ping_every: Duration,
    write_timeout: Duration,
) where
    S: Sink<Message> + Unpin,
{
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + ping_every, ping_every);
    loop {
        // `_budget` vit jusqu'à la fin de l'écriture
        let (msg, _budget) = tokio::select! {
            queued = out_rx.recv() => {
                let Some(Outbound { frame, budget }) = queued else { break };
                let msg = match frame {
                    OutboundFrame::Control(ctrl) => match serde_json::to_string(&ctrl) {
                        Ok(text) => Message::Text(text.into()),
                        Err(_) => continue,
                    },
                    OutboundFrame::Binary(bytes) => Message::Binary(bytes.into()),
                };
                (msg, budget)
            }
            _ = ping.tick() => (Message::Ping(Default::default()), None),
            _ = shutdown_rx.changed() => {
                if !*shutdown_rx.borrow() {
                    continue;
                }
                let close = CloseFrame {
                    code: 1001,
                    reason: "relay shutting down".into(),
                };
                (Message::Close(Some(close)), None)
            }
        };
        let closing = matches!(msg, Message::Close(_));
        let sent = tokio::time::timeout(write_timeout, ws_tx.send(msg)).await;
        if closing || !matches!(sent, Ok(Ok(()))) {
            break;
        }
    }
}

/// L'ancien salon n'est quitté qu'une fois le nouveau obtenu.
fn enter_room(
    rooms: &RoomManager,
    membership: &mut Option<RoomMembership>,
    out_tx: &PeerSender,
    result: Result<(RoomMembership, ServerMessage), RoomError>,
) {
    match result {
        Ok((new_membership, reply)) => {
            if let Some(prev) = membership.replace(new_membership) {
                rooms.leave_room(&prev, "switched_room");
            }
            out_tx.push(OutboundFrame::Control(reply));
        }
        Err(err) => send_error(out_tx, err.code, &err.message),
    }
}

fn send_error(tx: &PeerSender, code: &str, message: &str) {
    tx.push(OutboundFrame::Control(ServerMessage::Error {
        code: code.to_string(),
        message: message.to_string(),
    }));
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

    #[tokio::test]
    async fn writer_gives_up_on_a_client_that_stops_reading() {
        let stuck = Box::pin(futures_util::sink::unfold((), |(), _: Message| {
            std::future::pending::<Result<(), ()>>()
        }));
        let (sender, out_rx) = PeerSender::channel();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        sender.push(OutboundFrame::Binary(vec![1]));

        let writer = write_loop(
            stuck,
            out_rx,
            shutdown_rx,
            Duration::from_secs(60),
            Duration::from_millis(50),
        );
        tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .expect("writer returns despite the blocked socket");
        tokio::time::timeout(Duration::from_secs(1), sender.writer_gone())
            .await
            .expect("reader is told the writer stopped");
    }
}
