//! Deterministic frame-accurate input macro recorder and TAS player (BlueStacks Macros + TAS).

use crate::keymap::{BTN_A, BTN_B, BTN_RIGHT, BTN_START, BTN_UP};
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
}
