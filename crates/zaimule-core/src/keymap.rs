//! BlueStacks-inspired Visual Keymapper Overlay, NDS touch zones, and adjustable Turbo/Autofire engine.

use serde::{Deserialize, Serialize};

/// Standard joypad button bitmask flags shared across all Zaimule cores.
pub const BTN_UP: u16 = 1 << 0;
pub const BTN_DOWN: u16 = 1 << 1;
pub const BTN_LEFT: u16 = 1 << 2;
pub const BTN_RIGHT: u16 = 1 << 3;
pub const BTN_A: u16 = 1 << 4;
pub const BTN_B: u16 = 1 << 5;
pub const BTN_START: u16 = 1 << 6;
pub const BTN_SELECT: u16 = 1 << 7;
pub const BTN_L: u16 = 1 << 8;
pub const BTN_R: u16 = 1 << 9;

/// Visual overlay control widget category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeymapWidgetKind {
    Dpad,
    ActionButton,
    TurboButton,
    TouchTapZone,
}

/// Visual keymapper widget anchored by normalized screen coordinates (`0.0..=100.0`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeymapWidget {
    pub id: String,
    pub label: String,
    pub kind: KeymapWidgetKind,
    pub key_binding: String,
    pub button_mask: u16,
    pub x_pct: f32,
    pub y_pct: f32,
    pub turbo_hz: Option<u8>,
}

/// Computes whether a turbo button running at `turbo_hz` (1..=30 Hz) should assert its bitmask on `frame` at `fps` (default 60).
pub fn turbo_should_fire(frame: u64, turbo_hz: u8, fps: u8) -> bool {
    let hz = turbo_hz.clamp(1, 30) as u64;
    let fps = fps.max(30) as u64;
    let period = (fps / hz).max(2);
    let half = (period / 2).max(1);
    (frame % period) < half
}

/// Applies active turbo widgets onto a raw player input bitmask for the given `frame`.
pub fn apply_turbo_mask(raw_mask: u16, active_turbo_masks: &[(u16, u8)], frame: u64) -> u16 {
    let mut out = raw_mask;
    for &(mask, hz) in active_turbo_masks {
        if turbo_should_fire(frame, hz, 60) {
            out |= mask;
        } else {
            out &= !mask;
        }
    }
    out
}

/// Default BlueStacks-style visual keymapper layout.
pub fn default_keymapper_widgets() -> Vec<KeymapWidget> {
    vec![
        KeymapWidget {
            id: "km-dpad".into(),
            label: "✚ D-PAD [ZQSD / Flèches]".into(),
            kind: KeymapWidgetKind::Dpad,
            key_binding: "ZQSD".into(),
            button_mask: BTN_UP | BTN_DOWN | BTN_LEFT | BTN_RIGHT,
            x_pct: 14.0,
            y_pct: 78.0,
            turbo_hz: None,
        },
        KeymapWidget {
            id: "km-btn-a".into(),
            label: "🅱 Bouton A [J / Espace]".into(),
            kind: KeymapWidgetKind::ActionButton,
            key_binding: "KeyJ".into(),
            button_mask: BTN_A,
            x_pct: 82.0,
            y_pct: 74.0,
            turbo_hz: None,
        },
        KeymapWidget {
            id: "km-btn-b".into(),
            label: "🅰 Bouton B [K / Shift]".into(),
            kind: KeymapWidgetKind::ActionButton,
            key_binding: "KeyK".into(),
            button_mask: BTN_B,
            x_pct: 68.0,
            y_pct: 82.0,
            turbo_hz: None,
        },
        KeymapWidget {
            id: "km-turbo-a".into(),
            label: "⚡ TURBO A 20Hz [U]".into(),
            kind: KeymapWidgetKind::TurboButton,
            key_binding: "KeyU".into(),
            button_mask: BTN_A,
            x_pct: 78.0,
            y_pct: 16.0,
            turbo_hz: Some(20),
        },
        KeymapWidget {
            id: "km-touch-nds".into(),
            label: "🖱 Tactile NDS / Menu [E]".into(),
            kind: KeymapWidgetKind::TouchTapZone,
            key_binding: "KeyE".into(),
            button_mask: BTN_START,
            x_pct: 20.0,
            y_pct: 16.0,
            turbo_hz: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turbo_pulse_alternates_at_expected_duty_cycle() {
        // At 15 Hz on a 60 FPS clock, period is 4 frames (2 frames ON, 2 frames OFF).
        assert!(turbo_should_fire(0, 15, 60));
        assert!(turbo_should_fire(1, 15, 60));
        assert!(!turbo_should_fire(2, 15, 60));
        assert!(!turbo_should_fire(3, 15, 60));
        assert!(turbo_should_fire(4, 15, 60));
    }
}
