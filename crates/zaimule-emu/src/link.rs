//! Multi-Instance Split-Screen & Virtual Link Cable Bus (`SerialLinkBus`).
//! Runs two emulator instances in lockstep and transfers `SB`/`SC` (`0xFF01`/`0xFF02`) bytes.

use crate::traits::{EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};

/// Telemetry snapshot for the Virtual Link Cable between Instance #1 and Instance #2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LinkCableTelemetry {
    pub enabled: bool,
    pub packets_transferred: u64,
    pub last_p1_to_p2_hex: String,
    pub last_p2_to_p1_hex: String,
    pub baud_rate_bps: u32,
}

/// Steps two emulator cores in lockstep and crosses their Serial I/O (`SB`/`SC`) lines when `link_enabled` is true.
pub fn step_dual_link_frame(
    core_p1: &mut dyn EmulatorBackend,
    core_p2: &mut dyn EmulatorBackend,
    buttons_p1: u16,
    buttons_p2: u16,
    telemetry: &mut LinkCableTelemetry,
) -> (FrameStepResult, Option<FrameStepResult>) {
    let frame_p1 = core_p1.step_frame(buttons_p1);
    if !telemetry.enabled {
        return (frame_p1, None);
    }

    let frame_p2 = core_p2.step_frame(buttons_p2);
    if let (Some(tx1), Some(tx2)) = (frame_p1.serial_out_byte, frame_p2.serial_out_byte) {
        let _ = core_p1.exchange_serial_byte(tx2);
        let _ = core_p2.exchange_serial_byte(tx1);
        telemetry.packets_transferred = telemetry.packets_transferred.saturating_add(1);
        telemetry.last_p1_to_p2_hex = format!("0x{tx1:02X}");
        telemetry.last_p2_to_p1_hex = format!("0x{tx2:02X}");
        telemetry.baud_rate_bps = 262_144;
    }

    (frame_p1, Some(frame_p2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gb::GameBoyCore;
    use zaimule_core::keymap::{BTN_LEFT, BTN_RIGHT};

    #[test]
    fn dual_link_transfers_serial_bytes_between_two_gameboy_instances() {
        let mut p1 = GameBoyCore::new();
        let mut p2 = GameBoyCore::new();
        let mut tel = LinkCableTelemetry {
            enabled: true,
            ..Default::default()
        };

        let (f1, f2) = step_dual_link_frame(&mut p1, &mut p2, BTN_RIGHT, BTN_LEFT, &mut tel);
        assert!(f2.is_some());
        assert_eq!(f1.frame_number, 1);
        assert_eq!(tel.packets_transferred, 1);
        assert!(tel.last_p1_to_p2_hex.starts_with("0x"));
    }
}
