use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::response::IntoResponse;
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::error::RelayError;
use crate::limits::{extract_client_ip, ConnectionGuard, ConnectionLimiter, FrameRateLimiter};
use crate::protocol::{
    parse_client_binary_frame, ClientMessage, OutboundFrame, ServerMessage, MAX_CONTROL_TEXT_BYTES,
    PROTOCOL_VERSION,
};
use crate::rooms::{
    CreateRoomRequest, JoinRoomRequest, RoomManager, RoomMembership, PEER_OUTBOUND_QUEUE,
};

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
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundFrame>(PEER_OUTBOUND_QUEUE);

    // Accueil immédiat avec version du protocole et taille maximale de trame
    let _ = out_tx.try_send(OutboundFrame::Control(ServerMessage::Welcome {
        protocol_version: PROTOCOL_VERSION,
        max_frame_bytes: state.config.max_frame_bytes,
    }));

    let mut shutdown_rx = state.shutdown.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                maybe_frame = out_rx.recv() => {
                    let Some(frame) = maybe_frame else { break };
                    let msg = match frame {
                        OutboundFrame::Control(ctrl) => match serde_json::to_string(&ctrl) {
                            Ok(text) => Message::Text(text.into()),
                            Err(_) => continue,
                        },
                        OutboundFrame::Binary(bytes) => Message::Binary(bytes.into()),
                    };
                    if ws_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        let _ = ws_tx.send(Message::Close(Some(CloseFrame {
                            code: 1001,
                            reason: "relay shutting down".into(),
                        }))).await;
                        break;
                    }
                }
            }
        }
    });

    let mut membership: Option<RoomMembership> = None;
    let mut rate_limiter = FrameRateLimiter::new(state.config.max_frames_per_sec);
    let mut reader_shutdown = state.shutdown.clone();
    let idle_timeout = state.config.ws_idle_timeout;

    loop {
        let next_msg = tokio::select! {
            _ = reader_shutdown.changed() => {
                if *reader_shutdown.borrow() {
                    break;
                }
                continue;
            }
            res = tokio::time::timeout(idle_timeout, ws_rx.next()) => {
                match res {
                    Ok(Some(Ok(msg))) => msg,
                    Ok(Some(Err(_))) | Ok(None) => break,
                    Err(_) => {
                        let _ = out_tx.try_send(OutboundFrame::Control(ServerMessage::Error {
                            code: "idle_timeout".to_string(),
                            message: "connexion expirée pour inactivité".to_string(),
                        }));
                        break;
                    }
                }
            }
        };

        if !rate_limiter.allow() {
            let _ = out_tx.try_send(OutboundFrame::Control(ServerMessage::Error {
                code: "rate_limited".to_string(),
                message: "débit maximal dépassé".to_string(),
            }));
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
                        if let Some(prev) = membership.take() {
                            state.rooms.leave_room(&prev, "switched_room");
                        }
                        match state.rooms.create_room(
                            CreateRoomRequest {
                                console,
                                game_title,
                                rom_crc32,
                                player_name,
                                password_hash,
                                max_players,
                            },
                            out_tx.clone(),
                        ) {
                            Ok((new_membership, reply)) => {
                                membership = Some(new_membership);
                                let _ = out_tx.try_send(OutboundFrame::Control(reply));
                            }
                            Err(err) => send_error(&out_tx, err.code, &err.message),
                        }
                    }
                    Ok(ClientMessage::JoinRoom {
                        room_code,
                        rom_crc32,
                        player_name,
                        password_hash,
                    }) => {
                        if let Some(prev) = membership.take() {
                            state.rooms.leave_room(&prev, "switched_room");
                        }
                        match state.rooms.join_room(
                            JoinRoomRequest {
                                room_code,
                                rom_crc32,
                                player_name,
                                password_hash,
                            },
                            out_tx.clone(),
                        ) {
                            Ok((new_membership, reply)) => {
                                membership = Some(new_membership);
                                let _ = out_tx.try_send(OutboundFrame::Control(reply));
                            }
                            Err(err) => send_error(&out_tx, err.code, &err.message),
                        }
                    }
                    Ok(ClientMessage::LeaveRoom) => {
                        if let Some(prev) = membership.take() {
                            state.rooms.leave_room(&prev, "left");
                        }
                    }
                    Ok(ClientMessage::Ping { ts_ms }) => {
                        let _ = out_tx.try_send(OutboundFrame::Control(ServerMessage::Pong {
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
    drop(out_tx);
    let _ = writer.await;
}

fn send_error(tx: &mpsc::Sender<OutboundFrame>, code: &str, message: &str) {
    let _ = tx.try_send(OutboundFrame::Control(ServerMessage::Error {
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
