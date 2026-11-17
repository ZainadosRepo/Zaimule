//! Full emulation session coordinator combining ROM catalog, in-memory IPS patching,
//! Native Rust / Libretro core selection, 60s Rewind buffer, Save State slots,
//! BlueStacks Turbo/Macros, and Dual-Instance Link Cable synchronization.

use crate::chip8::{build_space_interceptor_rom, Chip8Core};
use crate::gb::{build_gb_pocket_arena_rom, GameBoyCore};
use crate::libretro::{default_libretro_cores, LibretroCoreDescriptor};
use crate::link::{step_dual_link_frame, LinkCableTelemetry};
use crate::nes::{build_nes_chrono_knight_rom, NesCore};
use crate::traits::{EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use zaimule_core::bios::{default_bios_registry, BiosEntry};
use zaimule_core::catalog::{
    compute_crc32, default_catalog, detect_console, extract_rom_title, format_crc32_hex,
    ConsoleKind, CoreEngineKind, GameEntry,
};
use zaimule_core::keymap::{apply_turbo_mask, default_keymapper_widgets, KeymapWidget};
use zaimule_core::macros::{default_macros, MacroSequence};
use zaimule_core::netplay::{default_netplay_rooms, NetplayRoom};
use zaimule_core::overrides::{resolve_effective_config, EffectiveGameConfig, GlobalSettings};
use zaimule_core::patch::{apply_ips_patch, default_mod_layers, ModLayerEntry};
use zaimule_core::rewind::{rle_compress, rle_decompress, RewindBuffer, SaveStateSlotInfo};
use zaimule_core::storage::PersistedSaveState;

/// Snapshot of the entire Zaimule station sent to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StationBootstrap {
    pub active_game_id: String,
    pub catalog: Vec<GameEntry>,
    pub effective_config: EffectiveGameConfig,
    pub mod_layers: Vec<ModLayerEntry>,
    pub save_slots: Vec<SaveStateSlotInfo>,
    pub keymapper_widgets: Vec<KeymapWidget>,
    pub macros: Vec<MacroSequence>,
    pub netplay_rooms: Vec<NetplayRoom>,
    pub libretro_cores: Vec<LibretroCoreDescriptor>,
    pub bios_entries: Vec<BiosEntry>,
    pub link_telemetry: LinkCableTelemetry,
    pub rewind_snapshots_count: usize,
    pub rewind_memory_bytes: usize,
    pub warning: Option<String>,
}

/// Combined frame step response for single or dual-instance viewport rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewportStepResponse {
    pub primary: FrameStepResult,
    pub secondary: Option<FrameStepResult>,
    pub link_telemetry: LinkCableTelemetry,
    pub rewind_snapshots_count: usize,
    pub rewind_memory_bytes: usize,
    pub active_macro_id: Option<String>,
}

pub struct EmulatorSession {
    pub settings: GlobalSettings,
    pub catalog: Vec<GameEntry>,
    pub active_game_id: String,
    pub mod_layers: Vec<ModLayerEntry>,
    pub keymapper_widgets: Vec<KeymapWidget>,
    pub macros: Vec<MacroSequence>,
    pub active_macro_id: Option<String>,
    pub macro_start_frame: u64,
    pub netplay_rooms: Vec<NetplayRoom>,
    pub link_telemetry: LinkCableTelemetry,
    pub rewind_buffer: RewindBuffer,
    pub save_states_ram: HashMap<(String, u8), (SaveStateSlotInfo, Vec<u8>)>,
    pub custom_roms: HashMap<String, Vec<u8>>,
    pub sram_dirty: bool,
    pub warning: Option<String>,
    core_p1: Box<dyn EmulatorBackend>,
    core_p2: Box<dyn EmulatorBackend>,
}

impl Default for EmulatorSession {
    fn default() -> Self {
        Self::new()
    }
}

impl EmulatorSession {
    pub fn new() -> Self {
        let catalog = default_catalog();
        let active_game_id = catalog[0].id.clone();
        let core_p1: Box<dyn EmulatorBackend> = Box::new(GameBoyCore::new());
        let core_p2: Box<dyn EmulatorBackend> = Box::new(GameBoyCore::new());

        let mut session = Self {
            settings: GlobalSettings::default(),
            catalog,
            active_game_id,
            mod_layers: default_mod_layers(),
            keymapper_widgets: default_keymapper_widgets(),
            macros: default_macros(),
            active_macro_id: None,
            macro_start_frame: 0,
            netplay_rooms: default_netplay_rooms(),
            link_telemetry: LinkCableTelemetry::default(),
            rewind_buffer: RewindBuffer::new(240), // 240 snapshots @ 4Hz = 60s
            save_states_ram: HashMap::new(),
            custom_roms: HashMap::new(),
            sram_dirty: false,
            warning: None,
            core_p1,
            core_p2,
        };
        // Seed slot #1 so the user immediately has a working initial Save State
        let _ = session.create_save_state(1, "Point de départ Arène".into(), 1_700_000_000_000);
        session
    }

    pub fn active_game(&self) -> &GameEntry {
        self.catalog
            .iter()
            .find(|g| g.id == self.active_game_id)
            .unwrap_or(&self.catalog[0])
    }

    pub fn bootstrap_snapshot(&self) -> StationBootstrap {
        let game = self.active_game();
        let effective_config = resolve_effective_config(&self.settings, &game.id, game.console);
        StationBootstrap {
            active_game_id: self.active_game_id.clone(),
            catalog: self.catalog.clone(),
            effective_config,
            mod_layers: self.mod_layers.clone(),
            save_slots: self.list_save_slots_for(&self.active_game_id),
            keymapper_widgets: self.keymapper_widgets.clone(),
            macros: self.macros.clone(),
            netplay_rooms: self.netplay_rooms.clone(),
            libretro_cores: default_libretro_cores(),
            bios_entries: default_bios_registry(),
            link_telemetry: self.link_telemetry.clone(),
            rewind_snapshots_count: self.rewind_buffer.len(),
            rewind_memory_bytes: self.rewind_buffer.memory_usage_bytes(),
            warning: self.warning.clone(),
        }
    }

    pub fn list_save_slots_for(&self, game_id: &str) -> Vec<SaveStateSlotInfo> {
        (1..=6)
            .map(|slot| {
                if let Some((info, _)) = self.save_states_ram.get(&(game_id.to_string(), slot)) {
                    info.clone()
                } else {
                    SaveStateSlotInfo {
                        slot,
                        occupied: false,
                        frame: 0,
                        timestamp_ms: 0,
                        crc32_hex: "00000000".into(),
                        compressed_bytes: 0,
                        label: format!("Slot #{slot} Vide"),
                    }
                }
            })
            .collect()
    }

    pub fn base_rom_for_game(&self, game_id: &str, console: ConsoleKind) -> Vec<u8> {
        if let Some(custom) = self.custom_roms.get(game_id) {
            return custom.clone();
        }
        match console {
            ConsoleKind::Chip8 => build_space_interceptor_rom(),
            ConsoleKind::Nes | ConsoleKind::Snes | ConsoleKind::Ps1 | ConsoleKind::N64 => {
                build_nes_chrono_knight_rom()
            }
            _ => build_gb_pocket_arena_rom(),
        }
    }

    /// Switches the active game and instantiates the appropriate Rust core.
    pub fn select_game(&mut self, game_id: &str) -> Result<StationBootstrap, String> {
        let Some(game) = self.catalog.iter().find(|g| g.id == game_id).cloned() else {
            return Err(format!("jeu introuvable dans la bibliothèque : {game_id}"));
        };
        self.active_game_id = game.id.clone();
        self.rewind_buffer.clear();
        let rom = self.base_rom_for_game(&game.id, game.console);

        let (p1, p2): (Box<dyn EmulatorBackend>, Box<dyn EmulatorBackend>) = match game.console {
            ConsoleKind::Chip8 => {
                let mut c1 = Chip8Core::new();
                let mut c2 = Chip8Core::new();
                c1.load_rom(&rom)?;
                c2.load_rom(&rom)?;
                (Box::new(c1), Box::new(c2))
            }
            ConsoleKind::Nes | ConsoleKind::Snes | ConsoleKind::Ps1 | ConsoleKind::N64 => {
                let mut c1 = NesCore::new();
                let mut c2 = NesCore::new();
                c1.load_rom(&rom)?;
                c2.load_rom(&rom)?;
                (Box::new(c1), Box::new(c2))
            }
            _ => {
                let mut c1 = GameBoyCore::new();
                let mut c2 = GameBoyCore::new();
                c1.load_rom(&rom)?;
                c2.load_rom(&rom)?;
                (Box::new(c1), Box::new(c2))
            }
        };

        self.core_p1 = p1;
        self.core_p2 = p2;
        Ok(self.bootstrap_snapshot())
    }

    /// Imports raw ROM bytes dropped onto the window, hashes them with IEEE CRC32, and loads the core.
    pub fn import_custom_rom(
        &mut self,
        file_name: &str,
        rom_bytes: &[u8],
    ) -> Result<StationBootstrap, String> {
        if rom_bytes.is_empty() {
            return Err("le fichier ROM est vide".into());
        }
        let console = detect_console(file_name, rom_bytes);
        let title = extract_rom_title(file_name, console, rom_bytes);
        let crc = compute_crc32(rom_bytes);
        let crc_hex = format_crc32_hex(crc);
        let id = format!("rom-{}", crc_hex.to_ascii_lowercase());

        self.custom_roms.insert(id.clone(), rom_bytes.to_vec());

        if !self.catalog.iter().any(|g| g.id == id) {
            self.catalog.insert(
                0,
                GameEntry {
                    id: id.clone(),
                    title,
                    console,
                    engine_kind: if console.is_native_rust() {
                        CoreEngineKind::NativeRust
                    } else {
                        CoreEngineKind::LibretroAbi
                    },
                    core_id: console.default_core_id().into(),
                    region: "Import Local · CRC32 Vérifié".into(),
                    crc32_hex: crc_hex,
                    size_bytes: rom_bytes.len() as u64,
                    sram_summary: "32 Ko SRAM Atomique (.tmp → .srm)".into(),
                    playtime_seconds: 0,
                    favorite: true,
                    link_cable_ready: console.supports_link_cable(),
                    embedded_rom_key: None,
                    cover_accent: "#1e3a8a".into(),
                    cover_symbol: "🕹️".into(),
                },
            );
        }

        self.select_game(&id)
    }

    /// Applies an IPS binary patch in RAM to the currently active game without touching disk ROMs.
    pub fn apply_custom_ips_to_active_game(
        &mut self,
        patch_name: &str,
        ips_bytes: &[u8],
    ) -> Result<StationBootstrap, String> {
        let game = self.active_game().clone();
        let base_rom = self.base_rom_for_game(&game.id, game.console);
        let patched_rom = apply_ips_patch(&base_rom, ips_bytes)?;
        let new_crc = format_crc32_hex(compute_crc32(&patched_rom));

        self.custom_roms
            .insert(game.id.clone(), patched_rom.clone());
        self.core_p1.load_rom(&patched_rom)?;
        self.core_p2.load_rom(&patched_rom)?;

        let mod_id = format!("ips-{}", new_crc.to_ascii_lowercase());
        self.mod_layers.retain(|m| m.id != mod_id);
        self.mod_layers.insert(
            0,
            ModLayerEntry {
                id: mod_id,
                label: format!("{patch_name} (RAM CRC {new_crc})"),
                kind: "IPS Patch RAM".into(),
                enabled: true,
                description: format!(
                    "Patch IPS appliqué à la volée sur {} ({} octets).",
                    game.title,
                    ips_bytes.len()
                ),
            },
        );
        Ok(self.bootstrap_snapshot())
    }

    /// Saves a newly recorded frame-by-frame TAS macro into the session and activates it.
    pub fn save_custom_macro(
        &mut self,
        name: String,
        frames: Vec<u16>,
    ) -> Result<StationBootstrap, String> {
        if frames.is_empty() {
            return Err("la séquence macro enregistrée ne contient aucune frame".into());
        }
        let clean_name = if name.trim().is_empty() {
            format!("⚡ Macro Perso ({}f)", frames.len())
        } else {
            name.trim().to_string()
        };
        let id = format!("macro-custom-{}", self.macros.len() + 1);
        let mut seq = MacroSequence::new(id.clone(), clean_name, true);
        for mask in frames {
            seq.record_frame(mask);
        }
        self.macros.insert(0, seq);
        self.active_macro_id = Some(id);
        self.macro_start_frame = self.core_p1.debug_snapshot().cycles / 256;
        Ok(self.bootstrap_snapshot())
    }

    /// Updates the BlueStacks-style Visual Keymapper widget positions and turbo settings.
    pub fn update_keymapper_widgets(
        &mut self,
        widgets: Vec<KeymapWidget>,
    ) -> Result<StationBootstrap, String> {
        if widgets.is_empty() {
            return Err("la configuration Keymapper ne peut pas être vide".into());
        }
        self.keymapper_widgets = widgets;
        Ok(self.bootstrap_snapshot())
    }

    /// Steps the active emulator session by one frame, applying Turbo, TAS Macros, Rewind capture, and Link Cable sync.
    pub fn step_viewport(
        &mut self,
        raw_buttons_p1: u16,
        raw_buttons_p2: u16,
        turbo_active: bool,
    ) -> ViewportStepResponse {
        let current_frame = self.core_p1.debug_snapshot().cycles / 256;
        let mut mask_p1 = raw_buttons_p1;

        if turbo_active {
            let turbos: Vec<(u16, u8)> = self
                .keymapper_widgets
                .iter()
                .filter_map(|w| w.turbo_hz.map(|hz| (w.button_mask, hz)))
                .collect();
            mask_p1 = apply_turbo_mask(mask_p1, &turbos, current_frame);
        }

        if let Some(ref macro_id) = self.active_macro_id {
            if let Some(seq) = self.macros.iter().find(|m| &m.id == macro_id) {
                let offset = current_frame.saturating_sub(self.macro_start_frame);
                if let Some(macro_mask) = seq.sample_at_frame(offset) {
                    mask_p1 |= macro_mask;
                } else {
                    self.active_macro_id = None;
                }
            }
        }

        let (primary, secondary) = step_dual_link_frame(
            self.core_p1.as_mut(),
            self.core_p2.as_mut(),
            mask_p1,
            raw_buttons_p2,
            &mut self.link_telemetry,
        );

        // Capture a compressed Rewind snapshot every 16 frames (~4 snapshots/sec)
        if (primary.frame_number & 15) == 0 {
            if let Ok(raw_state) = self.core_p1.save_state() {
                self.rewind_buffer
                    .push_snapshot(primary.frame_number, &raw_state);
            }
        }
        self.sram_dirty = true;

        ViewportStepResponse {
            primary,
            secondary,
            link_telemetry: self.link_telemetry.clone(),
            rewind_snapshots_count: self.rewind_buffer.len(),
            rewind_memory_bytes: self.rewind_buffer.memory_usage_bytes(),
            active_macro_id: self.active_macro_id.clone(),
        }
    }

    /// Creates a compressed, CRC32-verified Save State in `slot` (1..=9).
    pub fn create_save_state(
        &mut self,
        slot: u8,
        label: String,
        now_ms: u64,
    ) -> Result<SaveStateSlotInfo, String> {
        let slot = slot.clamp(1, 9);
        let raw = self.core_p1.save_state()?;
        let crc_hex = format_crc32_hex(compute_crc32(&raw));
        let compressed = rle_compress(&raw);
        let frame = self.core_p1.debug_snapshot().cycles / 256;
        let info = SaveStateSlotInfo {
            slot,
            occupied: true,
            frame,
            timestamp_ms: now_ms,
            crc32_hex: crc_hex,
            compressed_bytes: compressed.len(),
            label,
        };
        let mut envelope = (raw.len() as u32).to_le_bytes().to_vec();
        envelope.extend_from_slice(&compressed);
        self.save_states_ram.insert(
            (self.active_game_id.clone(), slot),
            (info.clone(), envelope),
        );
        Ok(info)
    }

    /// Restores a compressed Save State from `slot` (1..=9).
    pub fn restore_save_state(&mut self, slot: u8) -> Result<SaveStateSlotInfo, String> {
        let slot = slot.clamp(1, 9);
        let Some((info, envelope)) = self
            .save_states_ram
            .get(&(self.active_game_id.clone(), slot))
            .cloned()
        else {
            return Err(format!("aucune sauvegarde présente dans le Slot #{slot}"));
        };
        if envelope.len() < 4 {
            return Err("Save State corrompu (en-tête manquant)".into());
        }
        let expected_len =
            u32::from_le_bytes([envelope[0], envelope[1], envelope[2], envelope[3]]) as usize;
        let raw = rle_decompress(&envelope[4..], expected_len)?;
        self.core_p1.load_state(&raw)?;
        Ok(info)
    }

    pub fn export_save_state_envelope(&self, slot: u8) -> Option<PersistedSaveState> {
        let (info, envelope) = self
            .save_states_ram
            .get(&(self.active_game_id.clone(), slot.clamp(1, 9)))?;
        if envelope.len() < 4 {
            return None;
        }
        let uncompressed_len =
            u32::from_le_bytes([envelope[0], envelope[1], envelope[2], envelope[3]]) as usize;
        Some(PersistedSaveState {
            info: info.clone(),
            uncompressed_len,
            compressed_bytes: envelope[4..].to_vec(),
        })
    }

    pub fn import_save_state_envelope(&mut self, game_id: String, persisted: PersistedSaveState) {
        let mut envelope = (persisted.uncompressed_len as u32).to_le_bytes().to_vec();
        envelope.extend_from_slice(&persisted.compressed_bytes);
        let slot = persisted.info.slot.clamp(1, 9);
        self.save_states_ram
            .insert((game_id, slot), (persisted.info, envelope));
    }

    /// Rewinds the active emulator by `steps_back` snapshots from the circular buffer.
    pub fn rewind(&mut self, steps_back: usize) -> Result<u64, String> {
        let Some((frame, raw_state)) = self.rewind_buffer.rewind_steps(steps_back)? else {
            return Err("tampon de rembobinage vide".into());
        };
        self.core_p1.load_state(&raw_state)?;
        Ok(frame)
    }

    pub fn active_sram_bytes(&self) -> &[u8] {
        self.core_p1.sram()
    }

    pub fn load_active_sram(&mut self, data: &[u8]) {
        let _ = self.core_p1.load_sram(data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaimule_core::keymap::{BTN_A, BTN_RIGHT};
    use zaimule_core::patch::build_ips_patch;

    #[test]
    fn session_steps_creates_savestate_and_rewinds_cleanly() {
        let mut session = EmulatorSession::new();
        for _ in 0..32 {
            session.step_viewport(BTN_RIGHT, 0, true);
        }
        assert!(session.rewind_buffer.len() >= 2);
        let saved = session
            .create_save_state(2, "Test Boss".into(), 123456)
            .unwrap();
        assert!(saved.occupied);
        let restored = session.restore_save_state(2).unwrap();
        assert_eq!(restored.label, "Test Boss");
        assert!(session.rewind(1).is_ok());
    }

    #[test]
    fn session_applies_ips_patch_and_records_custom_macro() {
        let mut session = EmulatorSession::new();
        let patch = build_ips_patch(0x0134, b"ZAIMULE_DX").unwrap();
        let snap = session
            .apply_custom_ips_to_active_game("DX Edition.ips", &patch)
            .unwrap();
        assert!(snap.mod_layers[0].label.contains("DX Edition.ips"));

        let snap2 = session
            .save_custom_macro("Combo Plasma".into(), vec![BTN_A, BTN_A, BTN_RIGHT])
            .unwrap();
        assert_eq!(snap2.macros[0].name, "Combo Plasma");
        assert_eq!(snap2.macros[0].total_frames(), 3);
    }
}
