//! File de sortie d'un pair, bornée en trames et en octets.

use std::sync::Arc;

use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};

use crate::protocol::OutboundFrame;

/// Nombre maximal de trames en attente par pair.
pub const PEER_OUTBOUND_QUEUE: usize = 128;

/// Octets de jeu en attente par pair : avec 128 connexions au plus (voir `config`),
/// le total reste sous 32 Mio, dans la limite de 64 Mo du conteneur.
pub const PEER_OUTBOUND_BYTES: usize = 256 * 1024;

pub struct Outbound {
    pub frame: OutboundFrame,
    /// Rend les octets au budget une fois la trame écrite (ou abandonnée).
    pub budget: Option<OwnedSemaphorePermit>,
}

#[derive(Clone)]
pub struct PeerSender {
    tx: mpsc::Sender<Outbound>,
    bytes: Arc<Semaphore>,
    kick: Arc<watch::Sender<bool>>,
}

impl PeerSender {
    pub fn channel() -> (Self, mpsc::Receiver<Outbound>) {
        let (tx, rx) = mpsc::channel(PEER_OUTBOUND_QUEUE);
        let sender = Self {
            tx,
            bytes: Arc::new(Semaphore::new(PEER_OUTBOUND_BYTES)),
            kick: Arc::new(watch::Sender::new(false)),
        };
        (sender, rx)
    }

    /// Met une trame en file. Un pair qui ne peut plus la recevoir est déconnecté :
    /// un trou dans le flux désynchroniserait la partie sans que personne le sache.
    pub fn push(&self, frame: OutboundFrame) -> bool {
        if *self.kick.borrow() {
            return false;
        }
        let queued = self.try_queue(frame);
        if !queued {
            self.kick.send_replace(true);
        }
        queued
    }

    fn try_queue(&self, frame: OutboundFrame) -> bool {
        let budget = match &frame {
            OutboundFrame::Binary(bytes) => {
                let Ok(permit) = Arc::clone(&self.bytes).try_acquire_many_owned(bytes.len() as u32)
                else {
                    return false;
                };
                Some(permit)
            }
            OutboundFrame::Control(_) => None,
        };
        self.tx.try_send(Outbound { frame, budget }).is_ok()
    }

    /// Change quand le pair est trop en retard et doit être déconnecté.
    pub fn kicked(&self) -> watch::Receiver<bool> {
        self.kick.subscribe()
    }

    /// Se résout quand la tâche d'écriture s'est arrêtée.
    pub async fn writer_gone(&self) {
        self.tx.closed().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lagging_peer_is_kicked_instead_of_losing_frames_silently() {
        let (sender, mut rx) = PeerSender::channel();
        let kicked = sender.kicked();
        let frame = || OutboundFrame::Binary(vec![0; 65_536]);

        for _ in 0..PEER_OUTBOUND_BYTES / 65_536 {
            assert!(sender.push(frame()));
        }
        assert!(!*kicked.borrow());
        assert!(!sender.push(frame()), "byte budget exhausted");
        assert!(*kicked.borrow());

        // Une fois condamné, plus rien n'est accepté, même après libération du budget
        drop(rx.try_recv().expect("queued frame"));
        assert!(!sender.push(frame()));
    }
}
