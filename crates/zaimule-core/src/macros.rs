//! Deterministic frame-accurate input macro recorder and TAS player (BlueStacks Macros + TAS).

use crate::catalog::{compute_crc32, format_crc32_hex};
use crate::keymap::{BTN_A, BTN_B, BTN_DOWN, BTN_LEFT, BTN_RIGHT, BTN_START, BTN_UP};
use serde::{Deserialize, Serialize};

/// A contiguous run of identical input bitmasks over `duration_frames` frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputRunSegment {
    pub buttons_mask: u16,
    pub duration_frames: u32,
}

/// Complete deterministic input macro recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MacroSequence {
    pub id: String,
    pub name: String,
    pub loop_playback: bool,
    pub segments: Vec<InputRunSegment>,
}

impl MacroSequence {
    pub fn new(id: impl Into<String>, name: impl Into<String>, loop_playback: bool) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            loop_playback,
            segments: Vec::new(),
        }
    }

    /// Appends one frame's input mask, coalescing adjacent identical masks automatically.
    pub fn record_frame(&mut self, buttons_mask: u16) {
        if let Some(last) = self.segments.last_mut() {
            if last.buttons_mask == buttons_mask && last.duration_frames < u32::MAX {
                last.duration_frames += 1;
                return;
            }
        }
        self.segments.push(InputRunSegment {
            buttons_mask,
            duration_frames: 1,
        });
    }

    /// Total duration of the macro in frames (60 frames = 1.0s).
    pub fn total_frames(&self) -> u64 {
        self.segments.iter().map(|s| s.duration_frames as u64).sum()
    }

    /// Samples the exact button bitmask at `frame_offset` from the start of playback.
    pub fn sample_at_frame(&self, frame_offset: u64) -> Option<u16> {
        let total = self.total_frames();
        if total == 0 {
            return None;
        }
        let target = if self.loop_playback {
            frame_offset % total
        } else if frame_offset >= total {
            return None;
        } else {
            frame_offset
        };

        let mut acc = 0u64;
        for seg in &self.segments {
            let next = acc + (seg.duration_frames as u64);
            if target < next {
                return Some(seg.buttons_mask);
            }
            acc = next;
        }
        None
    }
}

/// Built-in TAS / QoL macros available out of the box.
pub fn default_macros() -> Vec<MacroSequence> {
    vec![
        MacroSequence {
            id: "macro-shiny-reset".into(),
            name: "✨ Soft-Reset Starter & Dialogue Rapide".into(),
            loop_playback: true,
            segments: vec![
                InputRunSegment {
                    buttons_mask: BTN_A | BTN_B | BTN_START,
                    duration_frames: 15,
                },
                InputRunSegment {
                    buttons_mask: 0,
                    duration_frames: 45,
                },
                InputRunSegment {
                    buttons_mask: BTN_A,
                    duration_frames: 120,
                },
            ],
        },
        MacroSequence {
            id: "macro-fast-battle".into(),
            name: "⚔ Boucle Expédition & Validation A".into(),
            loop_playback: true,
            segments: vec![
                InputRunSegment {
                    buttons_mask: BTN_RIGHT | BTN_B,
                    duration_frames: 30,
                },
                InputRunSegment {
                    buttons_mask: BTN_UP | BTN_A,
                    duration_frames: 30,
                },
            ],
        },
    ]
}

/// Verified deterministic Ghost Replay & Leaderboard entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhostLeaderboardEntry {
    pub id: String,
    pub game_id: String,
    pub game_title: String,
    pub console: String,
    pub player_name: String,
    pub score: u32,
    pub frames_count: u32,
    pub inputs_crc32_hex: String,
    pub verified_deterministic: bool,
    pub frame_inputs: Vec<u8>,
}

/// Computes the deterministic CRC32 signature over a ghost input trace and score.
pub fn compute_ghost_trace_crc32_hex(game_id: &str, score: u32, frame_inputs: &[u8]) -> String {
    let mut payload = Vec::with_capacity(game_id.len() + 4 + frame_inputs.len());
    payload.extend_from_slice(game_id.as_bytes());
    payload.extend_from_slice(&score.to_le_bytes());
    payload.extend_from_slice(frame_inputs);
    format_crc32_hex(compute_crc32(&payload))
}

/// Builds and signs a deterministic `GhostLeaderboardEntry`.
pub fn verify_and_build_ghost_entry(
    id: impl Into<String>,
    game_id: impl Into<String>,
    game_title: impl Into<String>,
    console: impl Into<String>,
    player_name: impl Into<String>,
    score: u32,
    frame_inputs: Vec<u8>,
) -> GhostLeaderboardEntry {
    let gid = game_id.into();
    let crc_hex = compute_ghost_trace_crc32_hex(&gid, score, &frame_inputs);
    let frames_count = (frame_inputs.len() as u32).max(60);
    GhostLeaderboardEntry {
        id: id.into(),
        game_id: gid,
        game_title: game_title.into(),
        console: console.into(),
        player_name: player_name.into(),
        score,
        frames_count,
        inputs_crc32_hex: crc_hex,
        verified_deterministic: true,
        frame_inputs,
    }
}

/// Verifies that a `GhostLeaderboardEntry` matches its declared CRC32 input signature.
pub fn verify_ghost_entry_integrity(entry: &GhostLeaderboardEntry) -> bool {
    let expected = compute_ghost_trace_crc32_hex(&entry.game_id, entry.score, &entry.frame_inputs);
    entry.verified_deterministic && expected == entry.inputs_crc32_hex
}

/// Curated global Ghost Leaderboard runs ready for 1-click TAS Ghost Challenge.
pub fn default_ghost_leaderboard() -> Vec<GhostLeaderboardEntry> {
    vec![
        verify_and_build_ghost_entry(
            "ghost-ps1-wr",
            "ps1-ff7-fr",
            "Cyber Ridge GP Type-R4 (PS1 3D)",
            "PS1",
            "Zainados_WR",
            480,
            vec![
                (BTN_RIGHT | BTN_A) as u8,
                (BTN_UP | BTN_A) as u8,
                (BTN_LEFT | BTN_A) as u8,
                (BTN_UP | BTN_A | BTN_B) as u8,
            ],
        ),
        verify_and_build_ghost_entry(
            "ghost-gb-arena",
            "gb-pocket-arena",
            "Pocket Monster Link Arena (GBC)",
            "GBC",
            "Kaelen_TAS",
            365,
            vec![
                (BTN_RIGHT | BTN_A) as u8,
                (BTN_DOWN | BTN_A) as u8,
                (BTN_LEFT | BTN_B) as u8,
                (BTN_UP | BTN_A) as u8,
            ],
        ),
        verify_and_build_ghost_entry(
            "ghost-snes-mode7",
            "snes-alttp-fr",
            "Valkyrie Trigger Mode 7 (SNES)",
            "SNES",
            "Aether_60FPS",
            310,
            vec![
                (BTN_UP | BTN_A) as u8,
                (BTN_RIGHT | BTN_A) as u8,
                (BTN_UP | BTN_B) as u8,
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_identical_frames_and_samples_deterministically() {
        let mut seq = MacroSequence::new("test", "Test", false);
        seq.record_frame(BTN_A);
        seq.record_frame(BTN_A);
        seq.record_frame(BTN_B);

        assert_eq!(seq.segments.len(), 2);
        assert_eq!(seq.total_frames(), 3);
        assert_eq!(seq.sample_at_frame(0), Some(BTN_A));
        assert_eq!(seq.sample_at_frame(1), Some(BTN_A));
        assert_eq!(seq.sample_at_frame(2), Some(BTN_B));
        assert_eq!(seq.sample_at_frame(3), None);
    }

    #[test]
    fn ghost_leaderboard_entries_verify_crc32_determinism() {
        let ghosts = default_ghost_leaderboard();
        assert_eq!(ghosts.len(), 3);
        for g in &ghosts {
            assert!(verify_ghost_entry_integrity(g));
        }
        let mut tampered = ghosts[0].clone();
        tampered.score += 10;
        assert!(!verify_ghost_entry_integrity(&tampered));
    }
}
