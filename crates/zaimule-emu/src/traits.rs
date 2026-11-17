//! Unified `EmulatorBackend` trait, CPU hardware debug snapshots, and frame output structures.

use serde::{Deserialize, Serialize};
use zaimule_core::catalog::ConsoleKind;

/// Hardware register snapshot exposed to the real-time debugger and telemetry HUD.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CpuDebugSnapshot {
    pub arch: String,
    pub pc_hex: String,
    pub sp_hex: String,
    pub reg_a_hex: String,
    pub reg_bc_hex: String,
    pub reg_de_hex: String,
    pub reg_hl_hex: String,
    pub flags_str: String,
    pub ly_scanline: u8,
    pub serial_sb_hex: String,
    pub serial_sc_hex: String,
    pub cycles: u64,
}

/// Output produced by stepping an emulator core for one 60 Hz frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameStepResult {
    pub console: ConsoleKind,
    pub frame_number: u64,
    pub width: u16,
    pub height: u16,
    /// Packed RGBA8888 pixels (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
    /// Root-mean-square audio level (`0.0..=1.0`) for telemetry meters.
    pub audio_level: f32,
    /// Outgoing byte placed on the Serial Link Bus (`0xFF01`/`0xFF02`) during this frame, if any.
    pub serial_out_byte: Option<u8>,
    pub cpu: CpuDebugSnapshot,
}

/// Unified interface implemented by all native Rust cores and the Libretro C-ABI adapter.
pub trait EmulatorBackend: Send {
    fn console(&self) -> ConsoleKind;
    fn load_rom(&mut self, rom: &[u8]) -> Result<(), String>;
    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult;
    fn save_state(&self) -> Result<Vec<u8>, String>;
    fn load_state(&mut self, data: &[u8]) -> Result<(), String>;
    fn sram(&self) -> &[u8];
    fn load_sram(&mut self, data: &[u8]) -> Result<(), String>;
    fn exchange_serial_byte(&mut self, incoming: u8) -> u8;
    fn debug_snapshot(&self) -> CpuDebugSnapshot;
}
