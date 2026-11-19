//! Multi-Instance Split-Screen & Virtual Link Cable Bus (`SerialLinkBus`).
//! Runs two or four emulator instances in lockstep and transfers real `SB`/`SC` bytes
//! whenever games initiate serial transfers.

use crate::traits::{EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};

/// Telemetry snapshot for the Virtual Link Cable between 2 or 4 synchronized instances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LinkCableTelemetry {
    pub enabled: bool,
    pub quad_mode: bool,
    pub packets_transferred: u64,
    pub last_p1_to_p2_hex: String,
    pub last_p2_to_p1_hex: String,
    pub baud_rate_bps: u32,
}

/// Steps two emulator cores in lockstep and crosses their Serial I/O (`SB`/`SC`) lines when `enabled` is true.
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
    if frame_p1.serial_out_byte.is_some() || frame_p2.serial_out_byte.is_some() {
        let tx1 = frame_p1.serial_out_byte.unwrap_or(0xFF);
        let tx2 = frame_p2.serial_out_byte.unwrap_or(0xFF);
        let _ = core_p1.exchange_serial_byte(tx2);
        let _ = core_p2.exchange_serial_byte(tx1);
        telemetry.packets_transferred = telemetry.packets_transferred.saturating_add(1);
        telemetry.last_p1_to_p2_hex = format!("0x{tx1:02X}");
        telemetry.last_p2_to_p1_hex = format!("0x{tx2:02X}");
        telemetry.baud_rate_bps = 262_144;
    }

    (frame_p1, Some(frame_p2))
}

/// Steps up to 4 emulator cores in lockstep (2x2 Tournament Split-Screen mode) and routes
/// real outgoing serial bytes through a 4-port Serial Link ring (`P1 -> P2 -> P3 -> P4 -> P1`).
pub fn step_quad_link_frame(
    cores: [&mut dyn EmulatorBackend; 4],
    buttons: [u16; 4],
    telemetry: &mut LinkCableTelemetry,
) -> (
    FrameStepResult,
    Option<FrameStepResult>,
    Option<FrameStepResult>,
    Option<FrameStepResult>,
) {
    let [c1, c2, c3, c4] = cores;
    if !telemetry.quad_mode {
        let (f1, f2) = step_dual_link_frame(c1, c2, buttons[0], buttons[1], telemetry);
        return (f1, f2, None, None);
    }

    telemetry.enabled = true;
    let f1 = c1.step_frame(buttons[0]);
    let f2 = c2.step_frame(buttons[1]);
    let f3 = c3.step_frame(buttons[2]);
    let f4 = c4.step_frame(buttons[3]);

    if f1.serial_out_byte.is_some()
        || f2.serial_out_byte.is_some()
        || f3.serial_out_byte.is_some()
        || f4.serial_out_byte.is_some()
    {
        let tx1 = f1.serial_out_byte.unwrap_or(0xFF);
        let tx2 = f2.serial_out_byte.unwrap_or(0xFF);
        let tx3 = f3.serial_out_byte.unwrap_or(0xFF);
        let tx4 = f4.serial_out_byte.unwrap_or(0xFF);

        let _ = c1.exchange_serial_byte(tx4);
        let _ = c2.exchange_serial_byte(tx1);
        let _ = c3.exchange_serial_byte(tx2);
        let _ = c4.exchange_serial_byte(tx3);

        telemetry.packets_transferred = telemetry.packets_transferred.saturating_add(4);
        telemetry.last_p1_to_p2_hex = format!("0x{tx1:02X}→0x{tx2:02X}");
        telemetry.last_p2_to_p1_hex = format!("0x{tx3:02X}→0x{tx4:02X}");
        telemetry.baud_rate_bps = 115_200 * 4;
    }

    (f1, Some(f2), Some(f3), Some(f4))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gb::GameBoyCore;
    use zaimule_core::keymap::{BTN_A, BTN_B, BTN_LEFT, BTN_RIGHT};

    fn serial_tx_rom(byte_to_send: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x0100] = 0xC3;
        rom[0x0101] = 0x50;
        rom[0x0102] = 0x01;
        let code: &[u8] = &[
            0x3E,
            byte_to_send,
            0xE0,
            0x01, // LD A, byte; LDH (SB), A
            0x3E,
            0x81,
            0xE0,
            0x02, // LD A, 0x81; LDH (SC), A (Master TX)
            0x18,
            0xFE, // JR -2
        ];
        rom[0x0150..0x0150 + code.len()].copy_from_slice(code);
        rom
    }

    #[test]
    fn dual_link_transfers_serial_bytes_between_two_gameboy_instances() {
        let mut p1 = GameBoyCore::new_empty();
        let mut p2 = GameBoyCore::new_empty();
        p1.load_rom(&serial_tx_rom(0xA1)).unwrap();
        p2.load_rom(&serial_tx_rom(0xB2)).unwrap();

        let mut tel = LinkCableTelemetry {
            enabled: true,
            ..Default::default()
        };

        let (f1, f2) = step_dual_link_frame(&mut p1, &mut p2, BTN_RIGHT, BTN_LEFT, &mut tel);
        assert!(f2.is_some());
        assert_eq!(f1.frame_number, 1);
        assert_eq!(tel.packets_transferred, 1);
        assert_eq!(tel.last_p1_to_p2_hex, "0xA1");
        assert_eq!(tel.last_p2_to_p1_hex, "0xB2");
        assert_eq!(p1.debug_snapshot().serial_sb_hex, "0xB2");
        assert_eq!(p2.debug_snapshot().serial_sb_hex, "0xA1");
    }

    #[test]
    fn quad_link_synchronizes_four_instances_in_tournament_ring() {
        let mut p1 = GameBoyCore::new_empty();
        let mut p2 = GameBoyCore::new_empty();
        let mut p3 = GameBoyCore::new_empty();
        let mut p4 = GameBoyCore::new_empty();
        p1.load_rom(&serial_tx_rom(0x11)).unwrap();
        p2.load_rom(&serial_tx_rom(0x22)).unwrap();
        p3.load_rom(&serial_tx_rom(0x33)).unwrap();
        p4.load_rom(&serial_tx_rom(0x44)).unwrap();

        let mut tel = LinkCableTelemetry {
            enabled: true,
            quad_mode: true,
            ..Default::default()
        };

        let (f1, f2, f3, f4) = step_quad_link_frame(
            [&mut p1, &mut p2, &mut p3, &mut p4],
            [BTN_RIGHT, BTN_LEFT, BTN_A, BTN_B],
            &mut tel,
        );
        assert_eq!(f1.frame_number, 1);
        assert!(f2.is_some() && f3.is_some() && f4.is_some());
        assert_eq!(tel.packets_transferred, 4);
        assert_eq!(p1.debug_snapshot().serial_sb_hex, "0x44");
        assert_eq!(p2.debug_snapshot().serial_sb_hex, "0x11");
        assert_eq!(p3.debug_snapshot().serial_sb_hex, "0x22");
        assert_eq!(p4.debug_snapshot().serial_sb_hex, "0x33");
    }
}
