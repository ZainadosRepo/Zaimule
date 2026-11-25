use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::outbound::PeerSender;
use crate::protocol::{
    encode_server_binary_frame, normalize_crc32, normalize_room_code, sanitize_label,
    OutboundFrame, PeerInfo, RelayChannel, ServerMessage,
};
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use subtle::ConstantTimeEq;

/// Alphabet sans caractères ambigus (`0/O`, `1/I`) pour des codes faciles à dicter.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const ROOM_CODE_LEN: usize = 6;

#[derive(Debug, Clone)]
pub struct CreateRoomRequest {
    pub console: String,
    pub game_title: String,
    pub rom_crc32: Option<String>,
    pub player_name: String,
    pub password_hash: Option<String>,
    pub max_players: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct JoinRoomRequest {
    pub room_code: String,
    pub rom_crc32: Option<String>,
    pub player_name: String,
    pub password_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RoomMembership {
    pub room_code: String,
    pub peer_id: u8,
}

#[derive(Debug)]
pub struct RoomError {
    pub code: &'static str,
    pub message: String,
}

impl RoomError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

struct PeerSlot {
    name: String,
    tx: PeerSender,
}

struct Room {
    console: String,
    game_title: String,
    rom_crc32: Option<String>,
    password_digest: Option<[u8; 32]>,
    max_players: u8,
    host_peer_id: u8,
    next_peer_id: u8,
    last_activity: Instant,
    peers: BTreeMap<u8, PeerSlot>,
}

impl Room {
    fn peer_list(&self) -> Vec<PeerInfo> {
        self.peers
            .iter()
            .map(|(&peer_id, slot)| PeerInfo {
                peer_id,
                name: slot.name.clone(),
                is_host: peer_id == self.host_peer_id,
            })
            .collect()
    }

    fn verify_password(&self, candidate: Option<&str>) -> bool {
        match (
            self.password_digest,
            candidate.map(str::trim).filter(|s| !s.is_empty()),
        ) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(expected), Some(actual)) => {
                let actual_digest = sha256_bytes(actual.as_bytes());
                bool::from(expected.ct_eq(&actual_digest))
            }
        }
    }
}

pub struct RoomManager {
    max_rooms: usize,
    max_peers_per_room: usize,
    idle_ttl: Duration,
    rng: SystemRandom,
    rooms: Mutex<HashMap<String, Room>>,
}

impl RoomManager {
    pub fn new(max_rooms: usize, max_peers_per_room: usize, idle_ttl: Duration) -> Self {
        Self {
            max_rooms,
            max_peers_per_room,
            idle_ttl,
            rng: SystemRandom::new(),
            rooms: Mutex::new(HashMap::new()),
        }
    }

    pub fn create_room(
        &self,
        req: CreateRoomRequest,
        tx: PeerSender,
    ) -> Result<(RoomMembership, ServerMessage), RoomError> {
        let mut rooms = self
            .rooms
            .lock()
            .map_err(|_| RoomError::new("internal_error", "verrou interne indisponible"))?;

        if rooms.len() >= self.max_rooms {
            return Err(RoomError::new(
                "relay_full",
                "nombre maximal de salons atteint sur ce relais",
            ));
        }

        let room_code = self.generate_unique_code(&rooms)?;
        let console = sanitize_label(&req.console, 16, "GB");
        let game_title = sanitize_label(&req.game_title, 96, "Partie Zaimule");
        let player_name = sanitize_label(&req.player_name, 32, "Joueur 1");
        let rom_crc32 = normalize_crc32(req.rom_crc32);
        let password_digest = req
            .password_hash
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| sha256_bytes(s.as_bytes()));
        let max_players = req
            .max_players
            .unwrap_or(2)
            .clamp(2, self.max_peers_per_room.min(u8::MAX as usize) as u8);

        let peer_id = 1u8;
        let mut peers = BTreeMap::new();
        peers.insert(
            peer_id,
            PeerSlot {
                name: player_name,
                tx,
            },
        );

        let room = Room {
            console: console.clone(),
            game_title: game_title.clone(),
            rom_crc32: rom_crc32.clone(),
            password_digest,
            max_players,
            host_peer_id: peer_id,
            next_peer_id: 2,
            last_activity: Instant::now(),
            peers,
        };
        let peer_list = room.peer_list();
        rooms.insert(room_code.clone(), room);

        Ok((
            RoomMembership {
                room_code: room_code.clone(),
                peer_id,
            },
            ServerMessage::RoomCreated {
                room_code,
                peer_id,
                peers: peer_list,
                console,
                game_title,
                rom_crc32,
                max_players,
            },
        ))
    }

    pub fn join_room(
        &self,
        req: JoinRoomRequest,
        tx: PeerSender,
    ) -> Result<(RoomMembership, ServerMessage), RoomError> {
        let code = normalize_room_code(&req.room_code);
        if code.len() != ROOM_CODE_LEN {
            return Err(RoomError::new(
                "invalid_room_code",
                "code de salon invalide",
            ));
        }

        let mut rooms = self
            .rooms
            .lock()
            .map_err(|_| RoomError::new("internal_error", "verrou interne indisponible"))?;

        let Some(room) = rooms.get_mut(&code) else {
            return Err(RoomError::new(
                "room_not_found",
                "salon introuvable ou expiré",
            ));
        };

        if !room.verify_password(req.password_hash.as_deref()) {
            return Err(RoomError::new(
                "invalid_password",
                "mot de passe du salon incorrect",
            ));
        }

        if let Some(expected_crc) = &room.rom_crc32 {
            let provided = normalize_crc32(req.rom_crc32);
            if provided.as_deref() != Some(expected_crc.as_str()) {
                return Err(RoomError::new(
                    "rom_mismatch",
                    format!("ROM différente de l'hôte (CRC32 attendu: {expected_crc})"),
                ));
            }
        }

        if room.peers.len() >= room.max_players as usize {
            return Err(RoomError::new("room_full", "salon complet"));
        }

        let peer_id = allocate_peer_id(room)?;
        let player_name = sanitize_label(&req.player_name, 32, &format!("Joueur {peer_id}"));
        let joined_info = PeerInfo {
            peer_id,
            name: player_name.clone(),
            is_host: false,
        };

        // Prévenir les pairs déjà présents avant d'insérer le nouveau pair
        let notify = OutboundFrame::Control(ServerMessage::PeerJoined {
            peer: joined_info.clone(),
        });
        for slot in room.peers.values() {
            slot.tx.push(notify.clone());
        }

        room.peers.insert(
            peer_id,
            PeerSlot {
                name: player_name,
                tx,
            },
        );
        room.last_activity = Instant::now();

        let response = ServerMessage::RoomJoined {
            room_code: code.clone(),
            peer_id,
            peers: room.peer_list(),
            console: room.console.clone(),
            game_title: room.game_title.clone(),
            rom_crc32: room.rom_crc32.clone(),
            max_players: room.max_players,
        };

        Ok((
            RoomMembership {
                room_code: code,
                peer_id,
            },
            response,
        ))
    }

    pub fn leave_room(&self, membership: &RoomMembership, reason: &str) {
        let Ok(mut rooms) = self.rooms.lock() else {
            return;
        };
        let Some(room) = rooms.get_mut(&membership.room_code) else {
            return;
        };

        if room.peers.remove(&membership.peer_id).is_none() {
            return;
        }
        room.last_activity = Instant::now();

        if room.peers.is_empty() {
            rooms.remove(&membership.room_code);
            return;
        }

        let left_notice = OutboundFrame::Control(ServerMessage::PeerLeft {
            peer_id: membership.peer_id,
            reason: reason.to_string(),
        });
        for slot in room.peers.values() {
            slot.tx.push(left_notice.clone());
        }

        if room.host_peer_id == membership.peer_id {
            if let Some(&new_host) = room.peers.keys().next() {
                room.host_peer_id = new_host;
                let migrated = OutboundFrame::Control(ServerMessage::HostMigrated {
                    new_host_peer_id: new_host,
                });
                for slot in room.peers.values() {
                    slot.tx.push(migrated.clone());
                }
            }
        }
    }

    /// Relaie une trame binaire E2EE opaque strictement au sein du même salon.
    pub fn relay_binary(
        &self,
        membership: &RoomMembership,
        channel: RelayChannel,
        target_peer_id: u8,
        opaque_payload: &[u8],
    ) -> Result<usize, RoomError> {
        let Ok(mut rooms) = self.rooms.lock() else {
            return Err(RoomError::new(
                "internal_error",
                "verrou interne indisponible",
            ));
        };
        let Some(room) = rooms.get_mut(&membership.room_code) else {
            return Err(RoomError::new("not_in_room", "salon introuvable"));
        };
        if !room.peers.contains_key(&membership.peer_id) {
            return Err(RoomError::new("not_in_room", "pair absent du salon"));
        }

        room.last_activity = Instant::now();
        let frame = OutboundFrame::Binary(encode_server_binary_frame(
            channel,
            membership.peer_id,
            opaque_payload,
        ));

        let mut delivered = 0usize;
        if target_peer_id == 0 {
            for (&peer_id, slot) in &room.peers {
                if peer_id != membership.peer_id && slot.tx.push(frame.clone()) {
                    delivered += 1;
                }
            }
        } else if target_peer_id != membership.peer_id {
            if let Some(slot) = room.peers.get(&target_peer_id) {
                if slot.tx.push(frame) {
                    delivered = 1;
                }
            }
        }
        Ok(delivered)
    }

    /// Supprime les salons inactifs dépassant `idle_ttl`.
    pub fn purge_idle_rooms(&self, now: Instant) -> usize {
        let Ok(mut rooms) = self.rooms.lock() else {
            return 0;
        };
        let expired: Vec<String> = rooms
            .iter()
            .filter(|(_, room)| now.duration_since(room.last_activity) >= self.idle_ttl)
            .map(|(code, _)| code.clone())
            .collect();

        for code in &expired {
            if let Some(room) = rooms.remove(code) {
                let msg = OutboundFrame::Control(ServerMessage::Error {
                    code: "room_expired".to_string(),
                    message: "salon expiré pour inactivité".to_string(),
                });
                for slot in room.peers.values() {
                    slot.tx.push(msg.clone());
                }
            }
        }
        expired.len()
    }

    #[cfg(test)]
    pub fn room_count(&self) -> usize {
        self.rooms.lock().map(|r| r.len()).unwrap_or(0)
    }

    fn generate_unique_code(&self, existing: &HashMap<String, Room>) -> Result<String, RoomError> {
        let mut buf = [0u8; ROOM_CODE_LEN];
        for _ in 0..32 {
            self.rng
                .fill(&mut buf)
                .map_err(|_| RoomError::new("rng_error", "générateur aléatoire indisponible"))?;
            let code: String = buf
                .iter()
                .map(|b| CODE_ALPHABET[(*b as usize) % CODE_ALPHABET.len()] as char)
                .collect();
            if !existing.contains_key(&code) {
                return Ok(code);
            }
        }
        Err(RoomError::new(
            "code_exhausted",
            "impossible d'allouer un code de salon",
        ))
    }
}

fn allocate_peer_id(room: &mut Room) -> Result<u8, RoomError> {
    for _ in 0..254 {
        let candidate = room.next_peer_id;
        room.next_peer_id = if room.next_peer_id == u8::MAX {
            1
        } else {
            room.next_peer_id + 1
        };
        if candidate != 0 && !room.peers.contains_key(&candidate) {
            return Ok(candidate);
        }
    }
    Err(RoomError::new(
        "room_full",
        "aucun identifiant de pair libre",
    ))
}

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let digest = digest::digest(&digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgr() -> RoomManager {
        RoomManager::new(4, 4, Duration::from_secs(60))
    }

    #[test]
    fn create_join_crc_password_and_strict_room_isolation() {
        let mgr = mgr();
        let (tx1, mut rx1) = PeerSender::channel();
        let (m1, msg1) = mgr
            .create_room(
                CreateRoomRequest {
                    console: "GBC".into(),
                    game_title: "Pokemon Crystal".into(),
                    rom_crc32: Some("deadbeef".into()),
                    player_name: "Alice".into(),
                    password_hash: Some("secret-pass".into()),
                    max_players: Some(2),
                },
                tx1,
            )
            .expect("room 1");
        assert_eq!(m1.peer_id, 1);
        assert!(matches!(msg1, ServerMessage::RoomCreated { .. }));

        // Mauvais mot de passe refusé
        let (tx_bad, _) = PeerSender::channel();
        let err = mgr
            .join_room(
                JoinRoomRequest {
                    room_code: m1.room_code.clone(),
                    rom_crc32: Some("DEADBEEF".into()),
                    player_name: "Mallory".into(),
                    password_hash: Some("wrong".into()),
                },
                tx_bad,
            )
            .unwrap_err();
        assert_eq!(err.code, "invalid_password");

        // Mauvais CRC32 refusé
        let (tx_bad_crc, _) = PeerSender::channel();
        let err = mgr
            .join_room(
                JoinRoomRequest {
                    room_code: m1.room_code.clone(),
                    rom_crc32: Some("CAFEBABE".into()),
                    player_name: "Bob".into(),
                    password_hash: Some("secret-pass".into()),
                },
                tx_bad_crc,
            )
            .unwrap_err();
        assert_eq!(err.code, "rom_mismatch");

        // Connexion légitime acceptée
        let (tx2, mut rx2) = PeerSender::channel();
        let (m2, _) = mgr
            .join_room(
                JoinRoomRequest {
                    room_code: m1.room_code.clone(),
                    rom_crc32: Some("DEADBEEF".into()),
                    player_name: "Bob".into(),
                    password_hash: Some("secret-pass".into()),
                },
                tx2,
            )
            .expect("join ok");
        assert_eq!(m2.peer_id, 2);

        // Alice reçoit PeerJoined
        match rx1.try_recv().expect("peer joined").frame {
            OutboundFrame::Control(ServerMessage::PeerJoined { peer }) => {
                assert_eq!(peer.peer_id, 2);
                assert_eq!(peer.name, "Bob");
            }
            other => panic!("unexpected frame: {other:?}"),
        }

        // Salon complet (max 2)
        let (tx3, _) = PeerSender::channel();
        let err = mgr
            .join_room(
                JoinRoomRequest {
                    room_code: m1.room_code.clone(),
                    rom_crc32: Some("DEADBEEF".into()),
                    player_name: "Charlie".into(),
                    password_hash: Some("secret-pass".into()),
                },
                tx3,
            )
            .unwrap_err();
        assert_eq!(err.code, "room_full");

        // Deuxième salon totalement isolé
        let (tx_other, mut rx_other) = PeerSender::channel();
        let (_m_other, _) = mgr
            .create_room(
                CreateRoomRequest {
                    console: "Switch".into(),
                    game_title: "Mario Kart 8".into(),
                    rom_crc32: None,
                    player_name: "Other".into(),
                    password_hash: None,
                    max_players: Some(4),
                },
                tx_other,
            )
            .expect("room 2");

        // Envoi binaire d'Alice -> Bob uniquement, jamais dans le salon 2
        let delivered = mgr
            .relay_binary(&m1, RelayChannel::GbLink, 0, b"ZMR1-cipher")
            .expect("relayed");
        assert_eq!(delivered, 1);

        match rx2.try_recv().expect("bob receives").frame {
            OutboundFrame::Binary(bytes) => {
                assert_eq!(bytes[0], RelayChannel::GbLink as u8);
                assert_eq!(bytes[1], 1); // expéditeur estampillé par le relais
                assert_eq!(&bytes[2..], b"ZMR1-cipher");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        assert!(rx1.try_recv().is_err(), "sender does not receive own echo");
        assert!(rx_other.try_recv().is_err(), "other room stays isolated");

        // Départ de l'hôte -> migration vers Bob
        mgr.leave_room(&m1, "disconnected");
        assert!(matches!(
            rx2.try_recv().expect("peer left").frame,
            OutboundFrame::Control(ServerMessage::PeerLeft { peer_id: 1, .. })
        ));
        assert!(matches!(
            rx2.try_recv().expect("host migrated").frame,
            OutboundFrame::Control(ServerMessage::HostMigrated {
                new_host_peer_id: 2
            })
        ));

        // Départ du dernier joueur -> suppression du salon
        mgr.leave_room(&m2, "left");
        assert_eq!(mgr.room_count(), 1);
    }

    #[test]
    fn idle_rooms_are_purged() {
        let mgr = RoomManager::new(4, 4, Duration::from_secs(10));
        let (tx, mut rx) = PeerSender::channel();
        let _ = mgr
            .create_room(
                CreateRoomRequest {
                    console: "GB".into(),
                    game_title: "Tetris".into(),
                    rom_crc32: None,
                    player_name: "Solo".into(),
                    password_hash: None,
                    max_players: Some(2),
                },
                tx,
            )
            .expect("created");
        assert_eq!(mgr.room_count(), 1);

        let purged = mgr.purge_idle_rooms(Instant::now() + Duration::from_secs(15));
        assert_eq!(purged, 1);
        assert_eq!(mgr.room_count(), 0);
        assert!(matches!(
            rx.try_recv().expect("expired notice").frame,
            OutboundFrame::Control(ServerMessage::Error { ref code, .. }) if code == "room_expired"
        ));
    }

    #[test]
    fn long_password_is_accepted_by_both_create_and_join() {
        let mgr = mgr();
        let password = "p".repeat(200);
        let (tx1, _rx1) = PeerSender::channel();
        let (host, _) = mgr
            .create_room(
                CreateRoomRequest {
                    console: "GB".into(),
                    game_title: "Tetris".into(),
                    rom_crc32: None,
                    player_name: "Host".into(),
                    password_hash: Some(password.clone()),
                    max_players: Some(2),
                },
                tx1,
            )
            .expect("created");
        let (tx2, _rx2) = PeerSender::channel();
        mgr.join_room(
            JoinRoomRequest {
                room_code: host.room_code,
                rom_crc32: None,
                player_name: "Guest".into(),
                password_hash: Some(password),
            },
            tx2,
        )
        .expect("joined");
    }
}
