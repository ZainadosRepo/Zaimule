//! Native Rust emulation engines (`Chip8Core`, `GameBoyCore`, `GbaCore`, `NdsCore`, `NesCore`, `SnesCore`),
//! Multi-Instance Virtual Link Cable (`step_dual_link_frame`, `step_quad_link_frame`),
//! Libretro C-ABI host bridge, BGB/Mesen VRAM/Disassembler debugger, and `EmulatorSession`.

pub mod chip8;
pub mod debugger;
pub mod gb;
pub mod gba;
pub mod libretro;
pub mod link;
pub mod megadrive;
pub mod n64;
pub mod nds;
pub mod nes;
pub mod ps1;
pub mod session;
pub mod snes;
pub mod traits;
