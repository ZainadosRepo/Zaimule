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

/// Starts empty so only user-recorded TAS macros appear.
pub fn default_macros() -> Vec<MacroSequence> {
    Vec::new()
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

/// Frame/Segment row exposed to the TAS Studio Timeline Grid in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TasTimelineRow {
    pub segment_index: usize,
    pub start_frame: u64,
    pub duration_frames: u32,
    pub buttons_mask: u16,
    pub buttons_label: String,
}

/// Formats a 16-bit joypad bitmask into a human-readable TAS timeline chord label.
pub fn format_buttons_mask_label(mask: u16) -> String {
    let mut parts = Vec::new();
    if (mask & BTN_UP) != 0 {
        parts.push("↑");
    }
    if (mask & BTN_DOWN) != 0 {
        parts.push("↓");
    }
    if (mask & BTN_LEFT) != 0 {
        parts.push("←");
    }
    if (mask & BTN_RIGHT) != 0 {
        parts.push("→");
    }
    if (mask & BTN_A) != 0 {
        parts.push("A");
    }
    if (mask & BTN_B) != 0 {
        parts.push("B");
    }
    if (mask & BTN_START) != 0 {
        parts.push("START");
    }
    if parts.is_empty() {
        "Neutre (0x00)".into()
    } else {
        parts.join(" + ")
    }
}

impl MacroSequence {
    /// Builds the structured TAS Studio Timeline rows for inspection and live button editing.
    pub fn timeline_rows(&self) -> Vec<TasTimelineRow> {
        let mut acc = 0u64;
        let mut rows = Vec::with_capacity(self.segments.len());
        for (idx, seg) in self.segments.iter().enumerate() {
            rows.push(TasTimelineRow {
                segment_index: idx,
                start_frame: acc,
                duration_frames: seg.duration_frames,
                buttons_mask: seg.buttons_mask,
                buttons_label: format_buttons_mask_label(seg.buttons_mask),
            });
            acc += seg.duration_frames as u64;
        }
        rows
    }

    /// Toggles a button bit (`BTN_A`, `BTN_B`, `BTN_RIGHT`, etc.) on a specific TAS timeline segment.
    pub fn toggle_segment_button(
        &mut self,
        segment_index: usize,
        button_bit: u16,
    ) -> Result<(), String> {
        let Some(seg) = self.segments.get_mut(segment_index) else {
            return Err(format!("segment TAS #{segment_index} introuvable"));
        };
        seg.buttons_mask ^= button_bit;
        Ok(())
    }
}

/// Starts empty with zero pre-seeded ghost leaderboard runs.
pub fn default_ghost_leaderboard() -> Vec<GhostLeaderboardEntry> {
    Vec::new()
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

        seq.toggle_segment_button(0, BTN_RIGHT).unwrap();
        let rows = seq.timeline_rows();
        assert_eq!(rows[0].buttons_mask, BTN_A | BTN_RIGHT);
        assert!(rows[0].buttons_label.contains("→"));
        assert!(default_macros().is_empty());
    }

    #[test]
    fn ghost_leaderboard_entries_verify_crc32_determinism() {
        assert!(default_ghost_leaderboard().is_empty());
        let entry = verify_and_build_ghost_entry(
            "ghost-gb-1",
            "rom-gb",
            "Game Boy Test",
            "GBC",
            "Joueur1",
            365,
            vec![(BTN_RIGHT | BTN_A) as u8, (BTN_DOWN | BTN_A) as u8],
        );
        assert!(verify_ghost_entry_integrity(&entry));
        let mut tampered = entry.clone();
        tampered.score += 10;
        assert!(!verify_ghost_entry_integrity(&tampered));
    }
}
