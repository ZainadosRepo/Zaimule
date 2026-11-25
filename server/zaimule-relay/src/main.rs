use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use tokio::sync::watch;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod error;
mod healthcheck;
mod limits;
mod protocol;
mod rooms;
mod routes;

use config::Config;
use limits::ConnectionLimiter;
use rooms::RoomManager;
use routes::AppState;

fn main() -> std::process::ExitCode {
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("PORT doit être un nombre valide");

    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck::run(port);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Échec d'initialisation du runtime Tokio");

    match runtime.block_on(serve()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Erreur fatale : {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zaimule_relay=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().compact())
        .init();

    let config = Arc::new(Config::from_env()?);
    let port = config.port;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let state = AppState {
        limiter: Arc::new(ConnectionLimiter::new(
            config.max_connections_total,
            config.max_connections_per_ip,
        )),
        rooms: Arc::new(RoomManager::new(
            config.max_rooms,
            config.max_peers_per_room,
            config.room_idle_ttl,
        )),
        config,
        shutdown: shutdown_rx,
    };

    tokio::spawn(janitor_loop(state.clone()));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(address = %addr, config = ?state.config, "🚀 Démarrage de Zaimule Relay");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = shutdown_tx.send(true);
        })
        .await?;

    tracing::info!("Arrêt propre de Zaimule Relay terminé");
    Ok(())
}

async fn janitor_loop(state: AppState) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    let mut shutdown = state.shutdown.clone();
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let purged = state.rooms.purge_idle_rooms(Instant::now());
                if purged > 0 {
                    tracing::info!(purged, "Salons inactifs purgés");
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

fn router(state: AppState) -> Router {
    let health = Router::new()
        .route("/healthz", get(routes::health_check))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(5),
        ));

    // /ws est une connexion persistante : pas de TimeoutLayer HTTP global dessus,
    // mais l'authentification par clé s'exécute avant l'extraction WebSocketUpgrade.
    let ws_route = Router::new()
        .route("/ws", get(routes::ws_upgrade))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            routes::require_access_key,
        ));

    Router::new()
        .merge(health)
        .merge(ws_route)
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(middleware::map_response(security_headers))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
    ] {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    response
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Échec d'écoute de l'événement Ctrl+C");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Échec d'enregistrement du gestionnaire SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Signal SIGINT (Ctrl+C) reçu, début du graceful shutdown...");
        },
        _ = terminate => {
            tracing::info!("Signal SIGTERM reçu (arrêt conteneur Docker), début du graceful shutdown...");
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use futures_util::{SinkExt, StreamExt};
    use protocol::{RelayChannel, ServerMessage};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as TungsteniteMsg;
    use tower::ServiceExt;

    const TEST_KEY: &str = "zaimule-test-secret-key-0123456789abcdef";

    fn test_state(key: Option<&str>) -> (AppState, watch::Sender<bool>) {
        let cfg = Arc::new(Config::for_tests(key));
        let (tx, rx) = watch::channel(false);
        let state = AppState {
            limiter: Arc::new(ConnectionLimiter::new(
                cfg.max_connections_total,
                cfg.max_connections_per_ip,
            )),
            rooms: Arc::new(RoomManager::new(
                cfg.max_rooms,
                cfg.max_peers_per_room,
                cfg.room_idle_ttl,
            )),
            config: cfg,
            shutdown: rx,
        };
        (state, tx)
    }

    #[tokio::test]
    async fn healthz_and_auth_guards_fail_closed() {
        // Sans clé configurée -> /healthz 200, /ws 503
        let (unconfigured, _tx) = test_state(None);
        let app = router(unconfigured);
        let health = app
            .clone()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(health.headers()[header::REFERRER_POLICY], "no-referrer");

        let ws_unconf = app
            .clone()
            .oneshot(Request::get("/ws").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ws_unconf.status(), StatusCode::SERVICE_UNAVAILABLE);

        // Avec clé configurée -> sans clé ou mauvaise clé -> 401
        let (configured, _tx2) = test_state(Some(TEST_KEY));
        let app = router(configured);

        let no_key = app
            .clone()
            .oneshot(Request::get("/ws").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(no_key.status(), StatusCode::UNAUTHORIZED);

        let bad_key = app
            .clone()
            .oneshot(
                Request::get("/ws")
                    .header("x-zaimule-key", "wrong-key-value-0000000000000000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_key.status(), StatusCode::UNAUTHORIZED);

        let root_404 = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(root_404.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn end_to_end_ws_relay_authenticates_and_forwards_opaque_e2ee_frames() {
        let (state, shutdown_tx) = test_state(Some(TEST_KEY));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let mut rx = state.shutdown.clone();
            axum::serve(listener, router(state))
                .with_graceful_shutdown(async move {
                    let _ = rx.changed().await;
                })
                .await
                .unwrap();
        });

        let connect_client = |key: Option<&'static str>| async move {
            let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
            if let Some(k) = key {
                req.headers_mut()
                    .insert("x-zaimule-key", HeaderValue::from_str(k).unwrap());
            }
            tokio_tungstenite::connect_async(req).await
        };

        // Un inconnu sans clé est rejeté dès le handshake HTTP (401)
        assert!(connect_client(None).await.is_err());
        assert!(connect_client(Some("bad-key-0123456789abcdef"))
            .await
            .is_err());

        // Joueur 1 crée un salon
        let (mut p1, _) = connect_client(Some(TEST_KEY)).await.expect("p1 connected");
        let welcome1: ServerMessage =
            serde_json::from_str(p1.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert!(matches!(
            welcome1,
            ServerMessage::Welcome {
                protocol_version: 1,
                ..
            }
        ));

        p1.send(TungsteniteMsg::Text(
            serde_json::json!({
                "type": "create_room",
                "console": "GBC",
                "game_title": "Pokemon Crystal",
                "rom_crc32": "ABCDEF01",
                "player_name": "Red",
                "max_players": 2
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        let room_code = match serde_json::from_str::<ServerMessage>(
            p1.next().await.unwrap().unwrap().to_text().unwrap(),
        )
        .unwrap()
        {
            ServerMessage::RoomCreated {
                room_code,
                peer_id: 1,
                ..
            } => room_code,
            other => panic!("unexpected p1 msg: {other:?}"),
        };

        // Joueur 2 rejoint le salon
        let (mut p2, _) = connect_client(Some(TEST_KEY)).await.expect("p2 connected");
        let _welcome2 = p2.next().await.unwrap().unwrap();
        p2.send(TungsteniteMsg::Text(
            serde_json::json!({
                "type": "join_room",
                "room_code": room_code,
                "rom_crc32": "abcdef01",
                "player_name": "Blue"
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        let joined2: ServerMessage =
            serde_json::from_str(p2.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert!(matches!(
            joined2,
            ServerMessage::RoomJoined { peer_id: 2, .. }
        ));

        // P1 reçoit PeerJoined
        let peer_joined: ServerMessage =
            serde_json::from_str(p1.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert!(matches!(peer_joined, ServerMessage::PeerJoined { .. }));

        // P1 envoie une trame binaire chiffrée opaque ZMR1 -> P2 la reçoit avec sender=1
        let mut frame = vec![RelayChannel::GbLink as u8, 0u8];
        frame.extend_from_slice(b"ZMR1\x01opaque-encrypted-bytes");
        p1.send(TungsteniteMsg::Binary(frame.into())).await.unwrap();

        match p2.next().await.unwrap().unwrap() {
            TungsteniteMsg::Binary(received) => {
                assert_eq!(received[0], RelayChannel::GbLink as u8);
                assert_eq!(received[1], 1);
                assert_eq!(&received[2..], b"ZMR1\x01opaque-encrypted-bytes");
            }
            other => panic!("expected binary frame, got {other:?}"),
        }

        // Ping/Pong RTT
        p1.send(TungsteniteMsg::Text(
            serde_json::json!({ "type": "ping", "ts_ms": 123456 })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let pong: ServerMessage =
            serde_json::from_str(p1.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert!(matches!(pong, ServerMessage::Pong { ts_ms: 123456, .. }));

        let _ = shutdown_tx.send(true);
        let _ = server.await;
    }
}
