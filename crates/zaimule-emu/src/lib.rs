//! Native Rust emulation engines (`Chip8Core`, `GameBoyCore`, `NesCore`),
//! Multi-Instance Virtual Link Cable (`step_dual_link_frame`), Libretro C-ABI host bridge,
//! and unified `EmulatorSession` coordinator.

pub mod chip8;
pub mod gb;
pub mod libretro;
pub mod link;
pub mod nes;
pub mod session;
pub mod traits;
