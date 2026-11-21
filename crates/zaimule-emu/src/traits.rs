//! Unified `EmulatorBackend` trait, CPU hardware debug snapshots, button translation,
//! and real PCM + RGBA frame output structures.

use serde::{Deserialize, Serialize};
use termboy_core::Buttons;
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{
    BTN_A, BTN_B, BTN_DOWN, BTN_L, BTN_LEFT, BTN_R, BTN_RIGHT, BTN_SELECT, BTN_START, BTN_UP,
};

pub const DEFAULT_SAMPLE_RATE_HZ: u32 = 48_000;

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
    pub score: u32,
}

/// Output produced by stepping an emulator core for one video frame (~60 Hz).
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
    /// Dominant APU tone frequency in Hz (`0` = silence) for telemetry display.
    pub audio_freq_hz: u16,
    /// Interleaved stereo PCM samples (`[L0, R0, L1, R1, ...]`, `-1.0..=1.0`) at `sample_rate_hz`.
    pub audio_samples: Vec<f32>,
    /// Nominal sample rate in Hz (`48_000`).
    pub sample_rate_hz: u32,
    /// Outgoing byte placed on the Serial Link Bus during this frame, if any.
    pub serial_out_byte: Option<u8>,
    pub cpu: CpuDebugSnapshot,
}

/// Converts Zaimule's internal `keymap` bitmask into `termboy_core::Buttons`.
/// Note: Zaimule (`UP=0, DOWN=1, LEFT=2, RIGHT=3`) differs in bit order from
/// `termboy_core::Buttons` (`RIGHT=0, LEFT=1, UP=2, DOWN=3`, plus `START`/`SELECT` swap).
pub fn buttons_to_termboy(mask: u16) -> Buttons {
    let mut b = Buttons::default();
    if (mask & BTN_RIGHT) != 0 {
        b = b.with(Buttons::RIGHT);
    }
    if (mask & BTN_LEFT) != 0 {
        b = b.with(Buttons::LEFT);
    }
    if (mask & BTN_UP) != 0 {
        b = b.with(Buttons::UP);
    }
    if (mask & BTN_DOWN) != 0 {
        b = b.with(Buttons::DOWN);
    }
    if (mask & BTN_A) != 0 {
        b = b.with(Buttons::A);
    }
    if (mask & BTN_B) != 0 {
        b = b.with(Buttons::B);
    }
    if (mask & BTN_SELECT) != 0 {
        b = b.with(Buttons::SELECT);
    }
    if (mask & BTN_START) != 0 {
        b = b.with(Buttons::START);
    }
    if (mask & BTN_L) != 0 {
        b = b.with(Buttons::L);
    }
    if (mask & BTN_R) != 0 {
        b = b.with(Buttons::R);
    }
    b
}

/// Computes normalized RMS level (`0.0..=1.0`) from interleaved `f32` PCM samples.
pub fn pcm_rms_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt().clamp(0.0, 1.0)
}

/// Builds a solid obsidian RGBA8888 standby framebuffer (`width * height * 4`).
pub fn solid_obsidian_rgba(width: usize, height: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; width * height * 4];
    for px in rgba.as_chunks_mut::<4>().0 {
        px[0] = 9;
        px[1] = 12;
        px[2] = 16;
        px[3] = 255;
    }
    rgba
}

/// Unified interface implemented by Zaimule's pure-Rust emulator cores (`Gb`/`Gbc`, `Gba`, `Nds`).
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
    fn apply_ram_cheat(&mut self, _addr: u16, _val: u8) {}
    fn read_ram_slice(&self) -> &[u8] {
        &[]
    }
    fn read_vram_slice(&self) -> &[u8] {
        &[]
    }
    fn write_vram_byte(&mut self, _offset: usize, _val: u8) {}
    fn set_touch_stylus(&mut self, _x: u8, _y: u8, _pressed: bool) {}
    fn set_color_correction(&mut self, _enabled: bool) {}
    /// Refreshes internal SRAM cache on demand (avoids copying SRAM on every video frame).
    fn sync_sram_cache(&mut self) {}
    /// Returns `false` when the core tracks SRAM writes precisely and knows no write occurred since last sync.
    fn sram_maybe_dirty(&self) -> bool {
        true
    }
    /// Refreshes RAM/VRAM debug mirror slices on demand (avoids per-frame copies when debugger/watchpoints are idle).
    fn sync_debug_caches(&mut self) {}
    /// Applies CPU instruction budget scaling (`50..=400`, default `100`%) on cores that support overclocking.
    fn set_cpu_overclock_pct(&mut self, _pct: u16) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_bitmask_maps_accurately_to_termboy_buttons() {
        let mask = BTN_UP | BTN_RIGHT | BTN_SELECT | BTN_START | BTN_L;
        let mapped = buttons_to_termboy(mask);
        assert!(mapped.contains(Buttons::UP));
        assert!(mapped.contains(Buttons::RIGHT));
        assert!(mapped.contains(Buttons::SELECT));
        assert!(mapped.contains(Buttons::START));
        assert!(mapped.contains(Buttons::L));
        assert!(!mapped.contains(Buttons::DOWN));
        assert!(!mapped.contains(Buttons::LEFT));
        assert!(!mapped.contains(Buttons::A));
        assert!(!mapped.contains(Buttons::R));
    }
}
