//! Native Rust emulation engines (`Chip8Core`, `GameBoyCore`, `GbaCore`, `NesCore`),
//! Multi-Instance Virtual Link Cable (`step_dual_link_frame`, `step_quad_link_frame`),
//! Libretro C-ABI host bridge, BGB/Mesen VRAM/Disassembler debugger, and `EmulatorSession`.

pub mod chip8;
pub mod debugger;
pub mod gb;
pub mod gba;
pub mod libretro;
pub mod link;
pub mod nes;
pub mod session;
pub mod traits;
