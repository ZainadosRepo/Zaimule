//! Core domain library for Zaimule: multi-console ROM catalog, IEEE 802.3 CRC32 hasher,
//! non-destructive in-memory IPS patcher, 60s RLE rewind ring-buffer, BlueStacks-style
//! visual keymapper & TAS macro engine, Yuzu-style per-game overrides, atomic persistence,
//! and P2P netplay protocol.

pub mod archive;
pub mod bios;
pub mod catalog;
pub mod cheats;
pub mod external;
pub mod keymap;
pub mod macros;
pub mod metadata;
pub mod netplay;
pub mod overrides;
pub mod patch;
pub mod rewind;
pub mod scanner;
pub mod storage;
