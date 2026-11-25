//! Coordinateur d'échange atomique Pokémon E2EE (**Zaimule Trade Hub**, Phase 3).
//!
//! Fonctionnement en 2 phases sur `RelayChannel::TradeHub` (`ChaCha20-Poly1305` `ZMR1`) :
//! 1. **Offre** : chaque joueur sélectionne un Pokémon de ses Boîtes PC (`.srm` validé).
//!    L'offre transporte le résumé + le Pokémon sérialisé + son empreinte `SHA-256`.
//! 2. **Double validation cryptographique** : chaque joueur confirme explicitement l'empreinte
//!    `SHA-256` exacte de l'offre adverse (empêchant toute substitution de dernière seconde).
//!    Dès que les deux confirmations concordent :
//!    - vérification que le Pokémon local n'a pas changé entre-temps,
//!    - écriture + évolution par échange automatique + recalcul complet des sommes de contrôle,
//!    - rotation de sauvegarde `.srm.bak1..3` avant écriture atomique sur disque,
//!    - rechargement à chaud de la SRAM dans `EmulatorSession` si le jeu est en cours d'exécution.

use crate::relay_client::RelayClient;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use zaimule_core::pokemon_trade::{
    apply_received_pokemon_trade, extract_boxed_pokemon, inspect_pokemon_save, ExtractedPokemon,
    PokemonBoxSlotSummary, TradeHubWireMessage,
};
use zaimule_core::storage::{load_sram_bytes, save_sram_with_rotation};
use zaimule_emu::session::EmulatorSession;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TradePhase {
    Idle,
    Negotiating,
    Committed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeHubStatus {
    pub phase: TradePhase,
    pub room_connected: bool,
    pub local_game_id: Option<String>,
    pub local_trainer_name: Option<String>,
    pub local_offer: Option<PokemonBoxSlotSummary>,
    pub local_format_label: Option<String>,
    pub local_accepted: bool,
    pub peer_trainer_name: Option<String>,
    pub peer_offer: Option<PokemonBoxSlotSummary>,
    pub peer_format_label: Option<String>,
    pub peer_accepted: bool,
    pub peer_committed: bool,
    pub compatible: bool,
    pub can_accept: bool,
    pub allow_evolution: bool,
    pub status_message: String,
    pub last_error: Option<String>,
}

impl Default for TradeHubStatus {
    fn default() -> Self {
        Self {
            phase: TradePhase::Idle,
            room_connected: false,
            local_game_id: None,
            local_trainer_name: None,
            local_offer: None,
            local_format_label: None,
            local_accepted: false,
            peer_trainer_name: None,
            peer_offer: None,
            peer_format_label: None,
            peer_accepted: false,
            peer_committed: false,
            compatible: false,
            can_accept: false,
            allow_evolution: true,
            status_message: "Chargez vos Boîtes PC et proposez un Pokémon.".into(),
            last_error: None,
        }
    }
}

struct TradeHubInner {
    phase: TradePhase,
    trade_id: String,
    local_game_id: Option<String>,
    local_sram_path: Option<PathBuf>,
    local_trainer_name: Option<String>,
    local_offer: Option<ExtractedPokemon>,
    local_box_index: Option<u8>,
    local_slot_index: Option<u8>,
    local_accepted_peer_sha: Option<String>,
    local_allow_evolution: bool,
    peer_trainer_name: Option<String>,
    peer_offer: Option<ExtractedPokemon>,
    peer_accepted_local_sha: Option<String>,
    peer_committed: bool,
    status_message: String,
    last_error: Option<String>,
}

impl Default for TradeHubInner {
    fn default() -> Self {
        Self {
            phase: TradePhase::Idle,
            trade_id: String::new(),
            local_game_id: None,
            local_sram_path: None,
            local_trainer_name: None,
            local_offer: None,
            local_box_index: None,
            local_slot_index: None,
            local_accepted_peer_sha: None,
            local_allow_evolution: true,
            peer_trainer_name: None,
            peer_offer: None,
            peer_accepted_local_sha: None,
            peer_committed: false,
            status_message: "Chargez vos Boîtes PC et proposez un Pokémon.".into(),
            last_error: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct TradeHubManager {
    inner: Arc<Mutex<TradeHubInner>>,
}

impl TradeHubManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Réinitialise complètement la session Trade Hub (appelé lors de `relay_leave_room`).
    pub fn reset(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = TradeHubInner::default();
        }
    }

    /// Propose un Pokémon extrait de la sauvegarde `.srm` locale au partenaire dans le salon E2EE.
    #[allow(clippy::too_many_arguments)]
    pub fn post_offer(
        &self,
        relay: &RelayClient,
        session: Option<&Arc<Mutex<EmulatorSession>>>,
        game_id: String,
        sram_path: PathBuf,
        sram_bytes: &[u8],
        box_index: u8,
        slot_index: u8,
    ) -> Result<TradeHubStatus, String> {
        let relay_status = relay.status_snapshot();
        if !relay_status.connected {
            return Err("connectez-vous d'abord à un salon relais chiffré".into());
        }

        let inv = inspect_pokemon_save(sram_bytes)?;
        let extracted = extract_boxed_pokemon(sram_bytes, box_index, slot_index)?;
        let trade_id = relay_status
            .room_code
            .clone()
            .unwrap_or_else(|| "TRADE".into());

        // Absorber d'abord tout message partenaire déjà arrivé
        self.drain_incoming(relay, session);

        let trainer_name = inv.trainer_name.clone();
        relay.send_trade_hub_message(TradeHubWireMessage::Offer {
            trade_id: trade_id.clone(),
            trainer_name: trainer_name.clone(),
            pokemon: extracted.clone(),
        })?;

        {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| "verrou Trade Hub indisponible".to_string())?;
            guard.phase = TradePhase::Negotiating;
            guard.trade_id = trade_id;
            guard.local_game_id = Some(game_id);
            guard.local_sram_path = Some(sram_path);
            guard.local_trainer_name = Some(trainer_name);
            guard.local_offer = Some(extracted);
            guard.local_box_index = Some(box_index);
            guard.local_slot_index = Some(slot_index);
            // Toute modification d'offre réinitialise les validations (anti bait-and-switch)
            guard.local_accepted_peer_sha = None;
            guard.peer_accepted_local_sha = None;
            guard.peer_committed = false;
            guard.last_error = None;
            guard.status_message = if guard.peer_offer.is_some() {
                "Les deux offres sont prêtes : vérifiez l'aperçu et confirmez l'échange.".into()
            } else {
                "Votre offre chiffrée est envoyée — en attente de l'offre du partenaire…".into()
            };
        }

        Ok(self.poll_and_snapshot(relay, session))
    }

    /// Confirme l'échange en verrouillant cryptographiquement l'empreinte SHA-256 de l'offre adverse.
    /// Si le partenaire a déjà confirmé notre offre, l'écriture atomique + backup `.bak1` s'exécute immédiatement.
    pub fn accept_offer(
        &self,
        relay: &RelayClient,
        session: Option<&Arc<Mutex<EmulatorSession>>>,
        allow_evolution: bool,
    ) -> Result<TradeHubStatus, String> {
        if !relay.is_in_room() {
            return Err("connectez-vous d'abord à un salon relais chiffré".into());
        }

        // Traiter d'abord d'éventuelles offres/acceptations reçues juste avant le clic
        self.drain_incoming(relay, session);

        let (trade_id, peer_sha) = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| "verrou Trade Hub indisponible".to_string())?;
            let Some(ref local) = guard.local_offer else {
                return Err("proposez d'abord un de vos Pokémon".into());
            };
            let Some(ref peer) = guard.peer_offer else {
                return Err("en attente de l'offre du partenaire".into());
            };
            if !local.format.is_compatible_with(peer.format) {
                return Err(format!(
                    "générations incompatibles : vous êtes en Gen {} et le partenaire est en Gen {}",
                    local.generation, peer.generation
                ));
            }
            let peer_sha = peer.offer_sha256.clone();
            guard.local_accepted_peer_sha = Some(peer_sha.clone());
            guard.local_allow_evolution = allow_evolution;
            guard.last_error = None;
            guard.status_message =
                "Validation enregistrée — en attente de la confirmation du partenaire…".into();
            (guard.trade_id.clone(), peer_sha)
        };

        relay.send_trade_hub_message(TradeHubWireMessage::Accept {
            trade_id,
            expected_peer_offer_sha256: peer_sha,
            allow_evolution,
        })?;

        self.try_commit_if_mutually_locked(relay, session);
        Ok(self.snapshot_with_room_flag(relay.is_in_room()))
    }

    /// Annule la négociation en cours et prévient le partenaire sur le canal chiffré.
    pub fn cancel_trade(&self, relay: &RelayClient, reason: Option<String>) -> TradeHubStatus {
        let reason_text = reason
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Échange annulé par le joueur")
            .to_string();
        let trade_id = self
            .inner
            .lock()
            .map(|g| g.trade_id.clone())
            .unwrap_or_default();
        if relay.is_in_room() {
            let _ = relay.send_trade_hub_message(TradeHubWireMessage::Cancel {
                trade_id,
                reason: reason_text.clone(),
            });
        }
        if let Ok(mut guard) = self.inner.lock() {
            *guard = TradeHubInner {
                phase: TradePhase::Cancelled,
                status_message: reason_text,
                ..TradeHubInner::default()
            };
        }
        self.snapshot_with_room_flag(relay.is_in_room())
    }

    /// Dépile les messages `RelayChannel::TradeHub`, déclenche l'écriture atomique si la double
    /// validation est atteinte, puis retourne l'instantané UI.
    pub fn poll_and_snapshot(
        &self,
        relay: &RelayClient,
        session: Option<&Arc<Mutex<EmulatorSession>>>,
    ) -> TradeHubStatus {
        if !relay.is_in_room() {
            return self.snapshot_with_room_flag(false);
        }
        self.drain_incoming(relay, session);
        self.try_commit_if_mutually_locked(relay, session);
        self.snapshot_with_room_flag(true)
    }

    fn drain_incoming(&self, relay: &RelayClient, _session: Option<&Arc<Mutex<EmulatorSession>>>) {
        let incoming = relay.drain_incoming_trade_hub();
        if incoming.is_empty() {
            return;
        }
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        for (_sender_id, msg) in incoming {
            match msg {
                TradeHubWireMessage::Offer {
                    trade_id,
                    trainer_name,
                    pokemon,
                } => {
                    if guard.trade_id.is_empty() {
                        guard.trade_id = trade_id;
                    }
                    guard.phase = TradePhase::Negotiating;
                    guard.peer_trainer_name = Some(trainer_name);
                    guard.peer_offer = Some(pokemon);
                    // Anti bait-and-switch : nouvelle offre adverse invalide les anciennes validations
                    guard.local_accepted_peer_sha = None;
                    guard.peer_accepted_local_sha = None;
                    guard.peer_committed = false;
                    guard.last_error = None;
                    guard.status_message = if guard.local_offer.is_some() {
                        "Offre du partenaire reçue ! Vérifiez le Pokémon et confirmez l'échange."
                            .into()
                    } else {
                        "Offre du partenaire reçue — sélectionnez un Pokémon à proposer en retour."
                            .into()
                    };
                }
                TradeHubWireMessage::Accept {
                    expected_peer_offer_sha256,
                    ..
                } => {
                    if guard.phase != TradePhase::Committed {
                        guard.peer_accepted_local_sha = Some(expected_peer_offer_sha256);
                        if guard.local_accepted_peer_sha.is_none() {
                            guard.status_message =
                                "Le partenaire a validé l'échange ! Confirmez pour finaliser."
                                    .into();
                        }
                    }
                }
                TradeHubWireMessage::Cancel { reason, .. } => {
                    if guard.phase != TradePhase::Committed {
                        *guard = TradeHubInner {
                            phase: TradePhase::Cancelled,
                            status_message: format!("Partenaire : {reason}"),
                            ..TradeHubInner::default()
                        };
                    }
                }
                TradeHubWireMessage::Committed { .. } => {
                    guard.peer_committed = true;
                }
            }
        }
    }

    fn try_commit_if_mutually_locked(
        &self,
        relay: &RelayClient,
        session: Option<&Arc<Mutex<EmulatorSession>>>,
    ) {
        let commit_plan = {
            let Ok(guard) = self.inner.lock() else {
                return;
            };
            if guard.phase == TradePhase::Committed {
                return;
            }
            let (Some(ref local), Some(ref peer)) = (&guard.local_offer, &guard.peer_offer) else {
                return;
            };
            let local_ok =
                guard.local_accepted_peer_sha.as_deref() == Some(peer.offer_sha256.as_str());
            let peer_ok =
                guard.peer_accepted_local_sha.as_deref() == Some(local.offer_sha256.as_str());
            if !local_ok || !peer_ok {
                return;
            }
            let (Some(ref game_id), Some(ref sram_path), Some(box_idx), Some(slot_idx)) = (
                &guard.local_game_id,
                &guard.local_sram_path,
                guard.local_box_index,
                guard.local_slot_index,
            ) else {
                return;
            };
            (
                guard.trade_id.clone(),
                game_id.clone(),
                sram_path.clone(),
                box_idx,
                slot_idx,
                local.offer_sha256.clone(),
                peer.clone(),
                guard.local_allow_evolution,
            )
        };

        let (
            trade_id,
            game_id,
            sram_path,
            box_idx,
            slot_idx,
            expected_local_sha,
            peer_offer,
            allow_evo,
        ) = commit_plan;

        // 1. Lire la SRAM la plus fraîche (en mémoire si jeu actif, sinon sur disque)
        let current_sram_res: Result<Vec<u8>, String> = (|| {
            if let Some(sess_arc) = session {
                if let Ok(mut sess) = sess_arc.lock() {
                    if sess.active_game_id == game_id {
                        sess.sync_active_sram();
                        let live = sess.active_sram_bytes();
                        if !live.is_empty() {
                            return Ok(live.to_vec());
                        }
                    }
                }
            }
            load_sram_bytes(&sram_path)?
                .ok_or_else(|| format!("sauvegarde introuvable : {}", sram_path.display()))
        })();

        let write_result = current_sram_res.and_then(|current_sram| {
            // Vérifier que le Pokémon cédé est toujours présent et identique à notre offre
            let still_there = extract_boxed_pokemon(&current_sram, box_idx, slot_idx)?;
            if still_there.offer_sha256 != expected_local_sha {
                return Err(
                    "le Pokémon proposé a été modifié ou déplacé pendant la négociation — échange annulé par sécurité"
                        .into(),
                );
            }
            let outcome = apply_received_pokemon_trade(
                &current_sram,
                box_idx,
                slot_idx,
                &peer_offer,
                allow_evo,
            )?;
            // Rotation .srm.bak1..3 avant écriture atomique sur disque
            save_sram_with_rotation(&sram_path, &outcome.updated_save_bytes)?;

            // Rechargement à chaud dans l'émulateur si ce jeu est actuellement ouvert
            if let Some(sess_arc) = session {
                if let Ok(mut sess) = sess_arc.lock() {
                    let _ = sess.reload_active_sram_from_bytes(&game_id, &outcome.updated_save_bytes);
                }
            }
            Ok(outcome)
        });

        match write_result {
            Ok(outcome) => {
                let _ = relay.send_trade_hub_message(TradeHubWireMessage::Committed { trade_id });
                if let Ok(mut guard) = self.inner.lock() {
                    guard.phase = TradePhase::Committed;
                    guard.last_error = None;
                    guard.status_message = if outcome.evolved {
                        if let Some(ref item) = outcome.consumed_item_name_fr {
                            format!(
                                "Échange atomique terminé ! {} a évolué en {} ({} consommé). Sauvegarde .srm.bak1 créée.",
                                outcome.received_species_before_fr,
                                outcome.received_species_after_fr,
                                item
                            )
                        } else {
                            format!(
                                "Échange atomique terminé ! {} a évolué en {} ! Sauvegarde .srm.bak1 créée.",
                                outcome.received_species_before_fr,
                                outcome.received_species_after_fr
                            )
                        }
                    } else {
                        format!(
                            "Échange atomique terminé ! {} a rejoint votre Boîte PC {}. Sauvegarde .srm.bak1 créée.",
                            outcome.received_species_after_fr,
                            box_idx + 1
                        )
                    };
                }
            }
            Err(err) => {
                let _ = relay.send_trade_hub_message(TradeHubWireMessage::Cancel {
                    trade_id,
                    reason: err.clone(),
                });
                if let Ok(mut guard) = self.inner.lock() {
                    guard.phase = TradePhase::Cancelled;
                    guard.last_error = Some(err.clone());
                    guard.status_message = format!("Échange interrompu en sécurité : {err}");
                }
            }
        }
    }

    fn snapshot_with_room_flag(&self, room_connected: bool) -> TradeHubStatus {
        let Ok(guard) = self.inner.lock() else {
            return TradeHubStatus::default();
        };
        let compatible = match (&guard.local_offer, &guard.peer_offer) {
            (Some(l), Some(p)) => l.format.is_compatible_with(p.format),
            _ => false,
        };
        let local_accepted = match (&guard.peer_offer, &guard.local_accepted_peer_sha) {
            (Some(p), Some(sha)) => sha == &p.offer_sha256,
            _ => false,
        };
        let peer_accepted = match (&guard.local_offer, &guard.peer_accepted_local_sha) {
            (Some(l), Some(sha)) => sha == &l.offer_sha256,
            _ => false,
        };
        let can_accept = room_connected
            && guard.phase == TradePhase::Negotiating
            && compatible
            && !local_accepted;

        TradeHubStatus {
            phase: guard.phase,
            room_connected,
            local_game_id: guard.local_game_id.clone(),
            local_trainer_name: guard.local_trainer_name.clone(),
            local_offer: guard.local_offer.as_ref().map(|o| o.summary.clone()),
            local_format_label: guard
                .local_offer
                .as_ref()
                .map(|o| o.format.label_fr().to_string()),
            local_accepted,
            peer_trainer_name: guard.peer_trainer_name.clone(),
            peer_offer: guard.peer_offer.as_ref().map(|o| o.summary.clone()),
            peer_format_label: guard
                .peer_offer
                .as_ref()
                .map(|o| o.format.label_fr().to_string()),
            peer_accepted,
            peer_committed: guard.peer_committed,
            compatible,
            can_accept,
            allow_evolution: guard.local_allow_evolution,
            status_message: guard.status_message.clone(),
            last_error: guard.last_error.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_client::{RelayCreateParams, RelayJoinParams};
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::http::StatusCode;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use zaimule_core::pokemon_trade::{crc16_ccitt, GEN4_SAVE_LEN};

    fn make_hgss_save_with_species(trainer: &str, species: u16, pid: u32) -> Vec<u8> {
        let mut buf = vec![0u8; GEN4_SAVE_LEN];
        let mut pk4 = [0u8; 136];
        pk4[0..4].copy_from_slice(&pid.to_le_bytes());

        // Block A..D unshuffled
        let mut unshuffled = [0u8; 128];
        unshuffled[0..2].copy_from_slice(&species.to_le_bytes());
        unshuffled[8..12].copy_from_slice(&64_000u32.to_le_bytes());

        // Encode trainer/OT in Gen 4 ASCII
        let enc_gen4 = |s: &str, n: usize| -> Vec<u8> {
            let mut out = vec![0u8; n * 2];
            let mut idx = 0usize;
            for ch in s.chars().take(n - 1) {
                let c: u16 = match ch {
                    'A'..='Z' => 0x012B + u16::from(ch as u8 - b'A'),
                    'a'..='z' => 0x0145 + u16::from(ch as u8 - b'a'),
                    _ => 0x01DE,
                };
                out[idx * 2..idx * 2 + 2].copy_from_slice(&c.to_le_bytes());
                idx += 1;
            }
            out[idx * 2..idx * 2 + 2].copy_from_slice(&0xFFFFu16.to_le_bytes());
            out
        };
        unshuffled[96..112].copy_from_slice(&enc_gen4(trainer, 8));

        // Shuffle + checksum + PRNG encrypt (pid = 0 -> identity perm [0,1,2,3])
        let mut chk = 0u16;
        for &chunk in unshuffled.as_chunks::<2>().0 {
            chk = chk.wrapping_add(u16::from_le_bytes(chunk));
        }
        pk4[6..8].copy_from_slice(&chk.to_le_bytes());
        let mut seed = u32::from(chk);
        let mut body = unshuffled;
        for chunk in body.as_chunks_mut::<2>().0 {
            seed = seed.wrapping_mul(0x41C6_4E6D).wrapping_add(0x6073);
            let k = (seed >> 16) as u16;
            let w = u16::from_le_bytes(*chunk) ^ k;
            *chunk = w.to_le_bytes();
        }
        pk4[8..136].copy_from_slice(&body);

        // Écrire partition 0 (count = 5)
        buf[0x64..0x74].copy_from_slice(&enc_gen4(trainer, 8));
        buf[0x0F700..0x0F700 + 136].copy_from_slice(&pk4);
        for (start, size, slot_id) in [(0x00000usize, 0x0F628usize, 0u16), (0x0F700, 0x12310, 1)] {
            let data_len = size - 16;
            let crc = crc16_ccitt(&buf[start..start + data_len]);
            let f = &mut buf[start + data_len..start + size];
            f[0..4].copy_from_slice(&5u32.to_le_bytes());
            f[4..8].copy_from_slice(&(size as u32).to_le_bytes());
            f[8..12].copy_from_slice(&0x2006_0623u32.to_le_bytes());
            f[12..14].copy_from_slice(&slot_id.to_le_bytes());
            f[14..16].copy_from_slice(&crc.to_le_bytes());
        }
        buf
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn atomic_two_player_e2ee_heartgold_trade_evolves_both_saves_and_rotates_backups() {
        const SECRET: &str = "zaimule-trade-hub-e2ee-secret-0123456789";

        // Mini relais WebSocket local aveugle (ne déchiffre jamais ZMR1)
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (bcast_tx, _) = broadcast::channel::<(u8, Vec<u8>)>(64);
        let server_bcast = bcast_tx.clone();

        let server_task = tokio::spawn(async move {
            let mut next_peer_id = 1u8;
            while let Ok((stream, _)) = listener.accept().await {
                let peer_id = next_peer_id;
                next_peer_id = next_peer_id.saturating_add(1);
                let tx_bus = server_bcast.clone();
                let mut rx_bus = server_bcast.subscribe();

                tokio::spawn(async move {
                    let ws = tokio_tungstenite::accept_hdr_async(
                        stream,
                        |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                            let ok = req
                                .headers()
                                .get("x-zaimule-key")
                                .and_then(|v| v.to_str().ok())
                                == Some(SECRET);
                            if ok {
                                Ok(resp)
                            } else {
                                Err(Response::builder()
                                    .status(StatusCode::UNAUTHORIZED)
                                    .body(None)
                                    .unwrap())
                            }
                        },
                    )
                    .await
                    .unwrap();

                    let (mut wtx, mut wrx) = ws.split();
                    let _ = wtx
                        .send(WsMessage::Text(
                            r#"{"type":"welcome","protocol_version":1,"max_frame_bytes":65536}"#
                                .into(),
                        ))
                        .await;
                    if let Some(Ok(WsMessage::Text(txt))) = wrx.next().await {
                        let reply = if txt.contains("create_room") {
                            format!(
                                r#"{{"type":"room_created","room_code":"HGSS42","peer_id":{peer_id},"peers":[{{"peerId":{peer_id},"name":"Luth","isHost":true}}],"console":"NDS","game_title":"HeartGold","rom_crc32":null,"max_players":2}}"#
                            )
                        } else {
                            format!(
                                r#"{{"type":"room_joined","room_code":"HGSS42","peer_id":{peer_id},"peers":[{{"peerId":1,"name":"Luth","isHost":true}},{{"peerId":{peer_id},"name":"Celesta","isHost":false}}],"console":"NDS","game_title":"HeartGold","rom_crc32":null,"max_players":2}}"#
                            )
                        };
                        let _ = wtx.send(WsMessage::Text(reply.into())).await;
                    }

                    loop {
                        tokio::select! {
                            incoming = wrx.next() => {
                                match incoming {
                                    Some(Ok(WsMessage::Binary(bin))) => {
                                        if bin.len() > 2 {
                                            let mut fwd = Vec::with_capacity(bin.len());
                                            fwd.push(bin[0]);
                                            fwd.push(peer_id);
                                            fwd.extend_from_slice(&bin[2..]);
                                            let _ = tx_bus.send((peer_id, fwd));
                                        }
                                    }
                                    Some(Ok(WsMessage::Close(_))) | None => break,
                                    _ => {}
                                }
                            }
                            bus_msg = rx_bus.recv() => {
                                if let Ok((sender, payload)) = bus_msg {
                                    if sender != peer_id {
                                        let _ = wtx.send(WsMessage::Binary(payload.into())).await;
                                    }
                                }
                            }
                        }
                    }
                });
            }
        });

        let relay_url = format!("ws://{addr}/ws");
        let client_a = RelayClient::new();
        let client_b = RelayClient::new();

        let snap_a = client_a
            .create_room(RelayCreateParams {
                relay_url: relay_url.clone(),
                access_key: SECRET.into(),
                console: "NDS".into(),
                game_title: "Pokémon HeartGold".into(),
                rom_crc32: None,
                player_name: "Luth".into(),
                password: None,
                max_players: 2,
            })
            .await
            .unwrap();
        let invite = snap_a.invite_uri.clone().unwrap();

        let _snap_b = client_b
            .join_room(RelayJoinParams {
                relay_url,
                access_key: SECRET.into(),
                invite_or_code: invite,
                explicit_session_key: None,
                local_rom_crc32: None,
                player_name: "Celesta".into(),
                password: None,
            })
            .await
            .unwrap();

        // Préparer deux vraies sauvegardes HeartGold sur disque temporaire
        let tmp_dir =
            std::env::temp_dir().join(format!("zaimule-trade-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let path_a = tmp_dir.join("heartgold_a.srm");
        let path_b = tmp_dir.join("soulsilver_b.srm");

        let save_a = make_hgss_save_with_species("LUTH", 93, 0); // Spectrum
        let save_b = make_hgss_save_with_species("CELESTA", 64, 0); // Kadabra
        std::fs::write(&path_a, &save_a).unwrap();
        std::fs::write(&path_b, &save_b).unwrap();

        let hub_a = TradeHubManager::new();
        let hub_b = TradeHubManager::new();

        // Phase 1 : les deux joueurs proposent leurs Pokémon
        hub_a
            .post_offer(
                &client_a,
                None,
                "hg-a".into(),
                path_a.clone(),
                &save_a,
                0,
                0,
            )
            .unwrap();
        hub_b
            .post_offer(
                &client_b,
                None,
                "ss-b".into(),
                path_b.clone(),
                &save_b,
                0,
                0,
            )
            .unwrap();

        // Attendre propagation chiffrée des offres
        let mut offers_ready = false;
        for _ in 0..40 {
            let sa = hub_a.poll_and_snapshot(&client_a, None);
            let sb = hub_b.poll_and_snapshot(&client_b, None);
            if sa.can_accept && sb.can_accept {
                assert_eq!(sa.peer_offer.as_ref().unwrap().species_name_fr, "Kadabra");
                assert_eq!(sb.peer_offer.as_ref().unwrap().species_name_fr, "Spectrum");
                offers_ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            offers_ready,
            "les offres chiffrées doivent parvenir aux deux pairs"
        );

        // Phase 2 : double validation atomique
        hub_a.accept_offer(&client_a, None, true).unwrap();
        hub_b.accept_offer(&client_b, None, true).unwrap();

        let mut both_committed = false;
        for _ in 0..40 {
            let sa = hub_a.poll_and_snapshot(&client_a, None);
            let sb = hub_b.poll_and_snapshot(&client_b, None);
            if sa.phase == TradePhase::Committed && sb.phase == TradePhase::Committed {
                assert!(sa.status_message.contains("Alakazam"));
                assert!(sb.status_message.contains("Ectoplasma"));
                both_committed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            both_committed,
            "les deux sauvegardes doivent être commitées atomiquement"
        );

        // Vérification sur disque : CRC-16 valide, évolutions appliquées, backups .bak1 créés !
        let disk_a = std::fs::read(&path_a).unwrap();
        let disk_b = std::fs::read(&path_b).unwrap();
        let inv_a = inspect_pokemon_save(&disk_a).unwrap();
        let inv_b = inspect_pokemon_save(&disk_b).unwrap();
        assert_eq!(inv_a.pokemon[0].species_id, 65); // Alakazam
        assert_eq!(inv_b.pokemon[0].species_id, 94); // Ectoplasma
        assert!(tmp_dir.join("heartgold_a.srm.bak1").exists());
        assert!(tmp_dir.join("soulsilver_b.srm.bak1").exists());

        client_a.leave_room();
        client_b.leave_room();
        server_task.abort();
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
