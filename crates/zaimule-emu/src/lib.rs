//! Pure-Rust native emulation cores (`GameBoyCore` via `termboy-gb`, `GbaCore` via `termboy-gba`,
//! and `NdsCore` via `nds_core`), Multi-Instance Virtual Link Cable (`step_dual_link_frame`,
//! `step_quad_link_frame`), debugger/VRAM inspector, binary frame/PCM packet encoder, and
//! Option-A Switch Vault session coordinator (`Torzu` Switch 1 & `Fritchii` Switch 2).

pub mod debugger;
pub mod gb;
pub mod gba;
pub mod link;
pub mod nds;
pub mod nds_core;
pub mod session;
pub mod traits;
