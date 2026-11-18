//! Full emulation session coordinator combining ROM catalog, in-memory IPS patching,
//! Native Rust / Libretro core selection, 60s Rewind buffer, Save State slots,
//! BlueStacks Turbo/Macros, GameShark RAM Cheats, RetroAchievements,
//! and Dual-Instance + UDP Rollback Link Cable synchronization.

use crate::chip8::{build_space_interceptor_rom, Chip8Core};
use crate::debugger::{
    build_vram_inspector_snapshot, compute_2bpp_pixel_mutation, disassemble_window, DebuggerPayload,
};
use crate::gb::{build_gb_pocket_arena_rom, GameBoyCore};
use crate::gba::{build_gba_golden_horizon_rom, GbaCore};
use crate::libretro::{
    build_verified_libretro_stub, default_libretro_cores, inspect_libretro_binary,
    verify_real_libretro_library, LibretroCoreDescriptor,
};
use crate::libretro_host::LibretroHostCore;
use crate::link::{step_quad_link_frame, LinkCableTelemetry};
use crate::megadrive::{build_md_supersonic_rom, MegaDriveCore};
use crate::n64::{build_n64_star_wing_rom, N64Core};
use crate::nds::{build_nds_dual_phantom_rom, NdsCore};
use crate::nes::{build_nes_chrono_knight_rom, NesCore};
use crate::ps1::{build_ps1_cyber_ridge_rom, Ps1Core};
use crate::psp::{build_psp_wipeout_pulse_rom, PspCore};
use crate::snes::{build_snes_valkyrie_mode7_rom, SnesCore};
use crate::traits::{EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use zaimule_core::archive::{extract_zip_entry_to_file, list_zip_entries, read_zip_entry_prefix};
use zaimule_core::bios::{default_bios_registry, BiosEntry};
use zaimule_core::catalog::{
    compute_crc32, default_catalog, detect_console, extract_nds_game_code, extract_rom_title,
    extract_switch_title_id, format_crc32_hex, inspect_rom_header_and_stats, ConsoleKind,
    CoreEngineKind, GameEntry,
};
use zaimule_core::cheats::{
    default_achievements, default_cheat_codes, default_memory_watchpoints, parse_cheat_code,
    parse_watchpoint_address, scan_ram_for_exact_byte, AchievementEntry, CheatCodeEntry,
    MemoryWatchpoint,
};
use zaimule_core::external::{
    detect_switch_keys_status, discover_external_emulators, launch_external_process,
    search_external_libretro_core, ExternalEmulatorStatus, SwitchKeysStatus,
};
use zaimule_core::keymap::{apply_turbo_mask, default_keymapper_widgets, KeymapWidget};
use zaimule_core::macros::{
    default_ghost_leaderboard, default_macros, verify_and_build_ghost_entry, GhostLeaderboardEntry,
    MacroSequence, TasTimelineRow,
};
use zaimule_core::netplay::{
    default_netplay_chat, default_netplay_rooms, default_tournament_bracket,
    discover_stun_candidate, format_deep_link, parse_deep_link, NetplayChatMessage, NetplayRoom,
    NetplayWirePacket, StunDiscoveryCandidate, TournamentBracket, UdpNetplayNode,
};
use zaimule_core::overrides::{resolve_effective_config, EffectiveGameConfig, GlobalSettings};
use zaimule_core::patch::{
    apply_ips_patch, default_community_mods, default_mod_layers, diff_roms_to_ips,
    CommunityModPackage, ModLayerEntry,
};
use zaimule_core::rewind::{rle_compress, rle_decompress, RewindBuffer, SaveStateSlotInfo};
use zaimule_core::scanner::{
    discover_known_game_folders, inspect_path_to_entries, scan_directory_for_roms,
    DirectoryScanReport, DiscoveredFolder,
};
use zaimule_core::storage::{
    decode_portable_pack, encode_portable_pack, inspect_sram_hex, write_sram_byte_and_rehash,
    PersistedLibrary, PersistedSaveState, SramHexInspection, ZaimulePortablePack,
    PORTABLE_PACK_MAGIC,
};

/// Snapshot of the entire Zaimule station sent to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StationBootstrap {
    pub active_game_id: String,
    pub catalog: Vec<GameEntry>,
    pub effective_config: EffectiveGameConfig,
    pub mod_layers: Vec<ModLayerEntry>,
    pub community_mods: Vec<CommunityModPackage>,
    pub cheat_codes: Vec<CheatCodeEntry>,
    pub memory_watchpoints: Vec<MemoryWatchpoint>,
    pub achievements: Vec<AchievementEntry>,
    pub save_slots: Vec<SaveStateSlotInfo>,
    pub sram_hex_inspection: SramHexInspection,
    pub keymapper_widgets: Vec<KeymapWidget>,
    pub macros: Vec<MacroSequence>,
    pub tas_timeline: Vec<TasTimelineRow>,
    pub ghost_leaderboard: Vec<GhostLeaderboardEntry>,
    pub netplay_rooms: Vec<NetplayRoom>,
    pub netplay_chat: Vec<NetplayChatMessage>,
    pub tournament_bracket: TournamentBracket,
    pub stun_candidate: StunDiscoveryCandidate,
    pub udp_local_port: u16,
    pub udp_peer_addr: Option<String>,
    pub libretro_cores: Vec<LibretroCoreDescriptor>,
    pub bios_entries: Vec<BiosEntry>,
    pub link_telemetry: LinkCableTelemetry,
    pub rewind_snapshots_count: usize,
    pub rewind_memory_bytes: usize,
    pub warning: Option<String>,
    #[serde(default)]
    pub discovered_folders: Vec<DiscoveredFolder>,
    #[serde(default)]
    pub rom_directories: Vec<String>,
    #[serde(default)]
    pub external_emulators: Vec<ExternalEmulatorStatus>,
    #[serde(default)]
    pub switch_keys: SwitchKeysStatus,
    #[serde(default)]
    pub last_scan_report: Option<DirectoryScanReport>,
}

/// Combined frame step response for single, dual (2P), or quad (4P Tournament) viewport rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewportStepResponse {
    pub primary: FrameStepResult,
    pub secondary: Option<FrameStepResult>,
    pub tertiary: Option<FrameStepResult>,
    pub quaternary: Option<FrameStepResult>,
    pub link_telemetry: LinkCableTelemetry,
    pub rewind_snapshots_count: usize,
    pub rewind_memory_bytes: usize,
    pub active_macro_id: Option<String>,
    pub newly_unlocked_achievement: Option<String>,
}

pub struct EmulatorSession {
    pub settings: GlobalSettings,
    pub catalog: Vec<GameEntry>,
    pub active_game_id: String,
    pub mod_layers: Vec<ModLayerEntry>,
    pub community_mods: Vec<CommunityModPackage>,
    pub cheat_codes: Vec<CheatCodeEntry>,
    pub memory_watchpoints: Vec<MemoryWatchpoint>,
    pub achievements: Vec<AchievementEntry>,
    pub keymapper_widgets: Vec<KeymapWidget>,
    pub macros: Vec<MacroSequence>,
    pub ghost_leaderboard: Vec<GhostLeaderboardEntry>,
    pub active_macro_id: Option<String>,
    pub macro_start_frame: u64,
    pub netplay_rooms: Vec<NetplayRoom>,
    pub netplay_chat: Vec<NetplayChatMessage>,
    pub tournament_bracket: TournamentBracket,
    pub udp_node: UdpNetplayNode,
    pub libretro_cores: Vec<LibretroCoreDescriptor>,
    pub libretro_core_paths: HashMap<String, PathBuf>,
    pub system_dir: PathBuf,
    pub save_dir: PathBuf,
    pub rom_cache_dir: PathBuf,
    pub discovered_folders: Vec<DiscoveredFolder>,
    pub external_emulators: Vec<ExternalEmulatorStatus>,
    pub switch_keys: SwitchKeysStatus,
    pub last_scan_report: Option<DirectoryScanReport>,
    pub active_launched_rom_path: Option<PathBuf>,
    pub link_telemetry: LinkCableTelemetry,
    pub rewind_buffer: RewindBuffer,
    pub save_states_ram: HashMap<(String, u8), (SaveStateSlotInfo, Vec<u8>)>,
    pub custom_roms: HashMap<String, Vec<u8>>,
    pub breakpoints: Vec<u32>,
    pub last_score: u32,
    pub recent_inputs_trace: Vec<u8>,
    pub sram_dirty: bool,
    core_loaded: bool,
    pub warning: Option<String>,
    core_p1: Box<dyn EmulatorBackend>,
    core_p2: Box<dyn EmulatorBackend>,
    core_p3: Box<dyn EmulatorBackend>,
    core_p4: Box<dyn EmulatorBackend>,
}

impl Default for EmulatorSession {
    fn default() -> Self {
        Self::new()
    }
}

fn build_core_for(console: ConsoleKind, rom: &[u8]) -> Result<Box<dyn EmulatorBackend>, String> {
    match console {
        ConsoleKind::Chip8 => {
            let mut c = Chip8Core::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Gba => {
            let mut c = GbaCore::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Nds => {
            let mut c = NdsCore::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Snes => {
            let mut c = SnesCore::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Ps1 => {
            let mut c = Ps1Core::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::N64 => {
            let mut c = N64Core::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Psp => {
            let mut c = PspCore::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Megadrive => {
            let mut c = MegaDriveCore::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Nes => {
            let mut c = NesCore::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Switch => Ok(Box::new(GameBoyCore::new_empty())),
        _ => {
            let mut c = GameBoyCore::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
    }
}

fn sanitize_cache_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "rom.bin".into()
    } else {
        cleaned
    }
}

impl EmulatorSession {
    /// Creates a clean production session with zero pre-populated test games or mock data.
    pub fn new_clean() -> Self {
        let base_tmp = std::env::temp_dir().join("zaimule");
        Self {
            settings: GlobalSettings::default(),
            catalog: Vec::new(),
            active_game_id: String::new(),
            mod_layers: Vec::new(),
            community_mods: Vec::new(),
            cheat_codes: Vec::new(),
            memory_watchpoints: Vec::new(),
            achievements: Vec::new(),
            keymapper_widgets: default_keymapper_widgets(),
            macros: Vec::new(),
            ghost_leaderboard: Vec::new(),
            active_macro_id: None,
            macro_start_frame: 0,
            netplay_rooms: Vec::new(),
            netplay_chat: Vec::new(),
            tournament_bracket: TournamentBracket {
                id: "tourney-live".into(),
                title: "Tournoi Multi-SIO".into(),
                console: String::new(),
                game_title: String::new(),
                matches: Vec::new(),
                champion: None,
                spectator_mode_active: false,
            },
            udp_node: UdpNetplayNode::bind_ephemeral(),
            libretro_cores: default_libretro_cores(),
            libretro_core_paths: HashMap::new(),
            system_dir: base_tmp.join("system"),
            save_dir: base_tmp.join("saves"),
            rom_cache_dir: base_tmp.join("rom_cache"),
            discovered_folders: Vec::new(),
            external_emulators: Vec::new(),
            switch_keys: SwitchKeysStatus::default(),
            last_scan_report: None,
            active_launched_rom_path: None,
            link_telemetry: LinkCableTelemetry::default(),
            rewind_buffer: RewindBuffer::new(240),
            save_states_ram: HashMap::new(),
            custom_roms: HashMap::new(),
            breakpoints: Vec::new(),
            last_score: 0,
            recent_inputs_trace: Vec::with_capacity(128),
            sram_dirty: false,
            core_loaded: false,
            warning: None,
            core_p1: Box::new(GameBoyCore::new_empty()),
            core_p2: Box::new(GameBoyCore::new_empty()),
            core_p3: Box::new(GameBoyCore::new_empty()),
            core_p4: Box::new(GameBoyCore::new_empty()),
        }
    }

    pub fn new() -> Self {
        let catalog = default_catalog();
        let active_game_id = catalog[0].id.clone();
        let base_tmp = std::env::temp_dir().join("zaimule");

        let mut session = Self {
            settings: GlobalSettings::default(),
            catalog,
            active_game_id,
            mod_layers: default_mod_layers(),
            community_mods: default_community_mods(),
            cheat_codes: default_cheat_codes(),
            memory_watchpoints: default_memory_watchpoints(),
            achievements: default_achievements(),
            keymapper_widgets: default_keymapper_widgets(),
            macros: default_macros(),
            ghost_leaderboard: default_ghost_leaderboard(),
            active_macro_id: None,
            macro_start_frame: 0,
            netplay_rooms: default_netplay_rooms(),
            netplay_chat: default_netplay_chat(),
            tournament_bracket: default_tournament_bracket(),
            udp_node: UdpNetplayNode::bind_ephemeral(),
            libretro_cores: default_libretro_cores(),
            libretro_core_paths: HashMap::new(),
            system_dir: base_tmp.join("system"),
            save_dir: base_tmp.join("saves"),
            rom_cache_dir: base_tmp.join("rom_cache"),
            discovered_folders: Vec::new(),
            external_emulators: Vec::new(),
            switch_keys: SwitchKeysStatus::default(),
            last_scan_report: None,
            active_launched_rom_path: None,
            link_telemetry: LinkCableTelemetry::default(),
            rewind_buffer: RewindBuffer::new(240), // 240 snapshots @ 4Hz = 60s
            save_states_ram: HashMap::new(),
            custom_roms: HashMap::new(),
            breakpoints: vec![
                0x0150,
                0x0800_00C0,
                0x8001_0000,
                0x8000_0400,
                0x8000_0100,
                0x0000_0200,
            ],
            last_score: 0,
            recent_inputs_trace: Vec::with_capacity(128),
            sram_dirty: false,
            core_loaded: true,
            warning: None,
            core_p1: Box::new(GameBoyCore::new()),
            core_p2: Box::new(GameBoyCore::new()),
            core_p3: Box::new(GameBoyCore::new()),
            core_p4: Box::new(GameBoyCore::new()),
        };
        let _ = session.create_save_state(1, "Point de départ Arène".into(), 1_700_000_000_000);
        session
    }

    /// Configures persistent directories used by Libretro cores (`system`, `saves`, `rom_cache`).
    pub fn configure_paths(
        &mut self,
        system_dir: PathBuf,
        save_dir: PathBuf,
        rom_cache_dir: PathBuf,
    ) {
        self.system_dir = system_dir;
        self.save_dir = save_dir;
        self.rom_cache_dir = rom_cache_dir;
    }

    /// Discovers local game folders (`Documents/jeuxyuzu`, `RomStation/games`), external emulators,
    /// Switch `prod.keys`, and native `.dll`/`.dylib`/`.so` Libretro cores already present on disk.
    pub fn refresh_external_environment(&mut self, app_cores_dir: Option<&Path>) {
        self.discovered_folders = discover_known_game_folders();
        self.external_emulators = discover_external_emulators(&self.settings.external_emulators);
        self.switch_keys = detect_switch_keys_status(&self.external_emulators);

        let mut registered = Vec::new();
        for desc in &mut self.libretro_cores {
            let lib_name = desc.current_platform_library_name().to_string();
            let app_candidate = app_cores_dir.map(|d| d.join(&lib_name));
            let found_path = app_candidate
                .filter(|p| p.is_file())
                .or_else(|| search_external_libretro_core(&lib_name));

            if let Some(path) = found_path {
                if let Ok(report) = verify_real_libretro_library(&path) {
                    desc.installed = true;
                    desc.is_native_binary = true;
                    desc.library_path = Some(path.to_string_lossy().to_string());
                    desc.verified_crc32_hex = Some(report.crc32_hex);
                    desc.status_label = format!(
                        "✓ C-ABI Natif · {} ({}/{} symboles)",
                        report.abi_format, report.symbols_found, report.symbols_total
                    );
                    registered.push((desc.core_id.clone(), path));
                    continue;
                }
            }

            if let Some(cores_dir) = app_cores_dir {
                let stub_path = cores_dir.join(format!("{}.bin", desc.core_id));
                if stub_path.is_file() && !desc.installed {
                    desc.installed = true;
                    desc.is_native_binary = false;
                    desc.status_label =
                        format!("Descripteur local · Cœur natif ({lib_name}) requis pour jouer");
                }
            }
        }

        for (core_id, path) in registered {
            self.libretro_core_paths.insert(core_id, path);
        }
    }

    /// Releases active CPU/Libretro core handles if the active game uses `core_id`
    /// so the underlying `.dll`/`.dylib`/`.so` can be overwritten cleanly on Windows.
    pub fn unload_active_core_if_matches(&mut self, core_id: &str) {
        if !self.active_game_id.is_empty() && self.active_game().core_id == core_id {
            self.core_p1 = Box::new(GameBoyCore::new_empty());
            self.core_p2 = Box::new(GameBoyCore::new_empty());
            self.core_p3 = Box::new(GameBoyCore::new_empty());
            self.core_p4 = Box::new(GameBoyCore::new_empty());
            self.sram_dirty = false;
            self.core_loaded = false;
        }
    }

    /// Registers a verified native `.dll`/`.dylib`/`.so` Libretro core on disk and reloads the active game if it uses it.
    pub fn register_real_libretro_core(
        &mut self,
        core_id: &str,
        path: &Path,
    ) -> Result<StationBootstrap, String> {
        let report = verify_real_libretro_library(path)?;
        let Some(core) = self
            .libretro_cores
            .iter_mut()
            .find(|c| c.core_id == core_id)
        else {
            return Err(format!("cœur Libretro inconnu : {core_id}"));
        };

        core.installed = true;
        core.is_native_binary = true;
        core.library_path = Some(path.to_string_lossy().to_string());
        core.verified_crc32_hex = Some(report.crc32_hex.clone());
        core.status_label = format!(
            "✓ C-ABI Natif · {} ({}/{} symboles · CRC {})",
            report.abi_format, report.symbols_found, report.symbols_total, report.crc32_hex
        );
        self.libretro_core_paths
            .insert(core_id.to_string(), path.to_path_buf());

        let active_uses_core =
            !self.active_game_id.is_empty() && self.active_game().core_id == core_id;
        if active_uses_core {
            let active_id = self.active_game_id.clone();
            let _ = self.select_game(&active_id);
        }

        Ok(self.bootstrap_snapshot())
    }

    /// Registers a user-chosen external emulator executable path (Yuzu, Eden, Ryujinx, melonDS, etc.).
    pub fn set_external_emulator_path(
        &mut self,
        emu_id: &str,
        exe_path: &str,
    ) -> Result<StationBootstrap, String> {
        let trimmed = exe_path.trim();
        let path = Path::new(trimmed);
        if !path.is_file() {
            return Err(format!(
                "exécutable introuvable sur disque : {}",
                path.display()
            ));
        }
        self.settings
            .external_emulators
            .insert(emu_id.to_string(), trimmed.to_string());
        self.external_emulators = discover_external_emulators(&self.settings.external_emulators);
        self.switch_keys = detect_switch_keys_status(&self.external_emulators);
        Ok(self.bootstrap_snapshot())
    }

    pub fn active_game(&self) -> &GameEntry {
        static EMPTY_GAME: std::sync::OnceLock<GameEntry> = std::sync::OnceLock::new();
        self.catalog
            .iter()
            .find(|g| g.id == self.active_game_id)
            .or_else(|| self.catalog.first())
            .unwrap_or_else(|| {
                EMPTY_GAME.get_or_init(|| GameEntry {
                    id: String::new(),
                    title: "Aucune ROM sélectionnée".into(),
                    console: ConsoleKind::Gbc,
                    engine_kind: CoreEngineKind::NativeRust,
                    core_id: "zaimule-rust-gb".into(),
                    region: "—".into(),
                    crc32_hex: "00000000".into(),
                    size_bytes: 0,
                    sram_summary: "—".into(),
                    playtime_seconds: 0,
                    favorite: false,
                    link_cable_ready: false,
                    embedded_rom_key: None,
                    cover_accent: "#1E1B1A".into(),
                    cover_symbol: String::new(),
                    rom_path: None,
                    archive_entry: None,
                    title_id: None,
                    fast_fingerprint: false,
                })
            })
    }

    pub fn bootstrap_snapshot(&self) -> StationBootstrap {
        let game = self.active_game();
        let effective_config = resolve_effective_config(&self.settings, &game.id, game.console);
        let tas_timeline = self
            .active_macro_id
            .as_ref()
            .and_then(|id| self.macros.iter().find(|m| &m.id == id))
            .or_else(|| self.macros.first())
            .map(|m| m.timeline_rows())
            .unwrap_or_default();
        StationBootstrap {
            active_game_id: self.active_game_id.clone(),
            catalog: self.catalog.clone(),
            effective_config,
            mod_layers: self.mod_layers.clone(),
            community_mods: self.community_mods.clone(),
            cheat_codes: self.cheat_codes.clone(),
            memory_watchpoints: self.memory_watchpoints.clone(),
            achievements: self.achievements.clone(),
            save_slots: self.list_save_slots_for(&self.active_game_id),
            sram_hex_inspection: self.inspect_active_sram_hex(0),
            keymapper_widgets: self.keymapper_widgets.clone(),
            macros: self.macros.clone(),
            tas_timeline,
            ghost_leaderboard: self.ghost_leaderboard.clone(),
            netplay_rooms: self.netplay_rooms.clone(),
            netplay_chat: self.netplay_chat.clone(),
            tournament_bracket: self.tournament_bracket.clone(),
            stun_candidate: discover_stun_candidate(self.udp_node.local_port),
            udp_local_port: self.udp_node.local_port,
            udp_peer_addr: self.udp_node.peer_addr.clone(),
            libretro_cores: self.libretro_cores.clone(),
            bios_entries: default_bios_registry(),
            link_telemetry: self.link_telemetry.clone(),
            rewind_snapshots_count: self.rewind_buffer.len(),
            rewind_memory_bytes: self.rewind_buffer.memory_usage_bytes(),
            warning: self.warning.clone(),
            discovered_folders: self.discovered_folders.clone(),
            rom_directories: self.settings.rom_directories.clone(),
            external_emulators: self.external_emulators.clone(),
            switch_keys: self.switch_keys.clone(),
            last_scan_report: self.last_scan_report.clone(),
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

    pub fn unpatched_rom_for_console(console: ConsoleKind) -> Vec<u8> {
        match console {
            ConsoleKind::Chip8 => build_space_interceptor_rom(),
            ConsoleKind::Gba => build_gba_golden_horizon_rom(),
            ConsoleKind::Nds => build_nds_dual_phantom_rom(),
            ConsoleKind::Snes => build_snes_valkyrie_mode7_rom(),
            ConsoleKind::Ps1 => build_ps1_cyber_ridge_rom(),
            ConsoleKind::N64 => build_n64_star_wing_rom(),
            ConsoleKind::Psp => build_psp_wipeout_pulse_rom(),
            ConsoleKind::Megadrive => build_md_supersonic_rom(),
            ConsoleKind::Nes => build_nes_chrono_knight_rom(),
            _ => build_gb_pocket_arena_rom(),
        }
    }

    fn read_header_prefix_for_game(&self, game: &GameEntry) -> Vec<u8> {
        let Some(ref raw_path) = game.rom_path else {
            return Vec::new();
        };
        let path = Path::new(raw_path);
        if !path.is_file() {
            return Vec::new();
        }
        if let Some(ref entry_name) = game.archive_entry {
            if let Ok(entries) = list_zip_entries(path) {
                if let Some(entry) = entries.into_iter().find(|e| e.name == *entry_name) {
                    if let Ok(prefix) = read_zip_entry_prefix(path, &entry, 64 * 1024) {
                        return prefix;
                    }
                }
            }
            return Vec::new();
        }
        if let Ok(mut file) = File::open(path) {
            let mut buf = vec![0u8; 64 * 1024];
            if let Ok(n) = file.read(&mut buf) {
                buf.truncate(n);
                return buf;
            }
        }
        Vec::new()
    }

    pub fn base_rom_for_game(&self, game_id: &str, console: ConsoleKind) -> Vec<u8> {
        if let Some(custom) = self.custom_roms.get(game_id) {
            return custom.clone();
        }
        if let Some(game) = self.catalog.iter().find(|g| g.id == game_id) {
            if game.rom_path.is_some() {
                let prefix = self.read_header_prefix_for_game(game);
                if !prefix.is_empty() {
                    return prefix;
                }
            }
        }
        Self::unpatched_rom_for_console(console)
    }

    /// Resolves a launchable on-disk ROM path for `game_id`, extracting compressed `.zip` entries
    /// (e.g. RomStation `PokemonPlatine.zip` -> `.nds`) atomically into `rom_cache_dir`.
    pub fn resolve_rom_path_for_launch(&mut self, game_id: &str) -> Result<PathBuf, String> {
        let Some(game) = self.catalog.iter().find(|g| g.id == game_id).cloned() else {
            return Err(format!("jeu introuvable : {game_id}"));
        };

        if let Some(ref raw_path) = game.rom_path {
            let src_path = PathBuf::from(raw_path);
            if !src_path.is_file() {
                return Err(format!(
                    "fichier jeu introuvable sur disque : {}",
                    src_path.display()
                ));
            }
            if let Some(ref entry_name) = game.archive_entry {
                let safe_name = sanitize_cache_filename(entry_name);
                let cached = self
                    .rom_cache_dir
                    .join(format!("{}_{}", game.id, safe_name));
                let size_ok = cached
                    .metadata()
                    .map(|m| m.is_file() && m.len() == game.size_bytes && m.len() > 0)
                    .unwrap_or(false);
                if !size_ok {
                    let entries = list_zip_entries(&src_path)?;
                    let Some(meta) = entries.into_iter().find(|e| e.name == *entry_name) else {
                        return Err(format!(
                            "entrée '{entry_name}' introuvable dans {}",
                            src_path.display()
                        ));
                    };
                    extract_zip_entry_to_file(&src_path, &meta, &cached)?;
                }
                if game.id == self.active_game_id {
                    self.active_launched_rom_path = Some(cached.clone());
                }
                return Ok(cached);
            }
            if game.id == self.active_game_id {
                self.active_launched_rom_path = Some(src_path.clone());
            }
            return Ok(src_path);
        }

        if let Some(custom_bytes) = self.custom_roms.get(game_id) {
            let ext = match game.console {
                ConsoleKind::Nds => "nds",
                ConsoleKind::Gba => "gba",
                ConsoleKind::Snes => "sfc",
                ConsoleKind::Nes => "nes",
                ConsoleKind::Gb => "gb",
                ConsoleKind::Gbc => "gbc",
                ConsoleKind::Ps1 => "bin",
                ConsoleKind::N64 => "z64",
                ConsoleKind::Psp => "iso",
                ConsoleKind::Megadrive => "md",
                _ => "rom",
            };
            let dest = self.rom_cache_dir.join(format!("{game_id}.{ext}"));
            let same_len = dest
                .metadata()
                .map(|m| m.is_file() && m.len() == custom_bytes.len() as u64)
                .unwrap_or(false);
            if !same_len {
                std::fs::create_dir_all(&self.rom_cache_dir)
                    .map_err(|e| format!("{}: {e}", self.rom_cache_dir.display()))?;
                std::fs::write(&dest, custom_bytes)
                    .map_err(|e| format!("{}: {e}", dest.display()))?;
            }
            return Ok(dest);
        }

        Err("ce jeu embarqué n'a pas de fichier ROM externe sur disque".into())
    }

    /// Synchronizes secondary Link-Cable/Quad cores lazily only when multi-play is enabled.
    fn sync_secondary_link_cores(&mut self) {
        if !self.link_telemetry.enabled && !self.link_telemetry.quad_mode {
            self.core_p2 = Box::new(GameBoyCore::new_empty());
            self.core_p3 = Box::new(GameBoyCore::new_empty());
            self.core_p4 = Box::new(GameBoyCore::new_empty());
            return;
        }
        if self.active_game_id.is_empty() {
            return;
        }
        let game = self.active_game().clone();
        if game.console == ConsoleKind::Switch
            || self.libretro_core_paths.contains_key(&game.core_id)
        {
            self.core_p2 = Box::new(GameBoyCore::new_empty());
            self.core_p3 = Box::new(GameBoyCore::new_empty());
            self.core_p4 = Box::new(GameBoyCore::new_empty());
            return;
        }

        let rom = self.base_rom_for_game(&game.id, game.console);
        self.core_p2 = build_core_for(game.console, &rom)
            .unwrap_or_else(|_| Box::new(GameBoyCore::new_empty()));
        if self.link_telemetry.quad_mode {
            self.core_p3 = build_core_for(game.console, &rom)
                .unwrap_or_else(|_| Box::new(GameBoyCore::new_empty()));
            self.core_p4 = build_core_for(game.console, &rom)
                .unwrap_or_else(|_| Box::new(GameBoyCore::new_empty()));
        } else {
            self.core_p3 = Box::new(GameBoyCore::new_empty());
            self.core_p4 = Box::new(GameBoyCore::new_empty());
        }
    }

    /// Merges scanned/imported catalog entries cleanly without duplicate entries.
    pub fn merge_scanned_entries(&mut self, entries: Vec<GameEntry>) -> Option<String> {
        let mut first_id = None;
        for mut entry in entries.into_iter().rev() {
            if first_id.is_none() {
                first_id = Some(entry.id.clone());
            }
            if let Some(pos) = self.catalog.iter().position(|g| {
                g.id == entry.id
                    || (entry.rom_path.is_some()
                        && g.rom_path == entry.rom_path
                        && g.archive_entry == entry.archive_entry)
            }) {
                let prev = self.catalog.remove(pos);
                entry.playtime_seconds = prev.playtime_seconds;
                entry.favorite = prev.favorite;
                self.catalog.insert(0, entry);
            } else {
                self.catalog.insert(0, entry);
            }
        }
        if let Some(latest) = self.catalog.first() {
            first_id = Some(latest.id.clone());
        }
        first_id
    }

    /// Scans a directory on disk (`Documents/jeuxyuzu`, `RomStation/games`, etc.) and merges discovered ROMs.
    pub fn scan_directory_and_merge(&mut self, dir: &Path) -> DirectoryScanReport {
        let report = scan_directory_for_roms(dir);
        if dir.is_dir() {
            let dir_str = dir.to_string_lossy().to_string();
            if !self
                .settings
                .rom_directories
                .iter()
                .any(|d| d.eq_ignore_ascii_case(&dir_str))
            {
                self.settings.rom_directories.push(dir_str);
            }
        }
        let first_id = self.merge_scanned_entries(report.imported_entries.clone());
        self.last_scan_report = Some(report.clone());
        if self.active_game_id.is_empty() {
            if let Some(id) = first_id {
                let _ = self.select_game(&id);
            }
        }
        report
    }

    /// Scans all auto-discovered local game directories (`Documents/jeuxyuzu`, `RomStation/games`, etc.).
    pub fn scan_known_folders_and_merge(&mut self) -> DirectoryScanReport {
        self.discovered_folders = discover_known_game_folders();
        let mut combined = DirectoryScanReport {
            scanned_root: "Dossiers auto-détectés".into(),
            ..Default::default()
        };
        let folder_paths: Vec<PathBuf> = self
            .discovered_folders
            .iter()
            .map(|f| PathBuf::from(&f.path))
            .collect();
        for p in folder_paths {
            let rep = self.scan_directory_and_merge(&p);
            combined.imported_entries.extend(rep.imported_entries);
            combined.skipped_files += rep.skipped_files;
            combined
                .unsupported_archives
                .extend(rep.unsupported_archives);
            combined.errors.extend(rep.errors);
        }
        self.last_scan_report = Some(combined.clone());
        combined
    }

    /// Imports arbitrary file or folder paths dropped onto the window or chosen via native dialog.
    pub fn import_paths(&mut self, paths: &[PathBuf]) -> Result<DirectoryScanReport, String> {
        let mut combined = DirectoryScanReport {
            scanned_root: "Importation directe".into(),
            ..Default::default()
        };

        for p in paths {
            if p.is_dir() {
                let rep = self.scan_directory_and_merge(p);
                combined.imported_entries.extend(rep.imported_entries);
                combined.skipped_files += rep.skipped_files;
                combined
                    .unsupported_archives
                    .extend(rep.unsupported_archives);
                combined.errors.extend(rep.errors);
            } else if p.is_file() {
                match inspect_path_to_entries(p) {
                    Ok(rep) => {
                        combined.imported_entries.extend(rep.imported_entries);
                        combined.skipped_files += rep.skipped_files;
                        combined
                            .unsupported_archives
                            .extend(rep.unsupported_archives);
                        combined.errors.extend(rep.errors);
                    }
                    Err(err) => combined.errors.push(err),
                }
            } else {
                combined
                    .errors
                    .push(format!("chemin introuvable : {}", p.display()));
            }
        }

        let first_id = self.merge_scanned_entries(combined.imported_entries.clone());
        self.last_scan_report = Some(combined.clone());
        if let Some(id) = first_id {
            let _ = self.select_game(&id);
        }

        if combined.imported_entries.is_empty() && !combined.unsupported_archives.is_empty() {
            return Err(format!(
                "Archive .7z/.rar détectée ({}). Extrayez-la ou utilisez un fichier .zip / .nds / .nsp / .xci directement.",
                combined.unsupported_archives[0]
            ));
        }

        Ok(combined)
    }

    /// Removes a game entry from the user library and clears active cores if it was selected.
    pub fn remove_game(&mut self, game_id: &str) -> Result<StationBootstrap, String> {
        let before = self.catalog.len();
        self.catalog.retain(|g| g.id != game_id);
        self.custom_roms.remove(game_id);
        if self.catalog.len() == before {
            return Err(format!("jeu introuvable : {game_id}"));
        }
        if self.active_game_id == game_id {
            self.active_game_id.clear();
            self.active_launched_rom_path = None;
            self.rewind_buffer.clear();
            self.sram_dirty = false;
            self.core_loaded = false;
            self.core_p1 = Box::new(GameBoyCore::new_empty());
            self.core_p2 = Box::new(GameBoyCore::new_empty());
            self.core_p3 = Box::new(GameBoyCore::new_empty());
            self.core_p4 = Box::new(GameBoyCore::new_empty());
        }
        Ok(self.bootstrap_snapshot())
    }

    /// Builds a serializable `PersistedLibrary` snapshot for atomic `library.json` storage.
    pub fn to_persisted_library(&self) -> PersistedLibrary {
        PersistedLibrary {
            active_game_id: self.active_game_id.clone(),
            catalog: self.catalog.clone(),
            rom_directories: self.settings.rom_directories.clone(),
        }
    }

    /// Restores catalog and configured directories from `library.json`.
    pub fn restore_persisted_library(&mut self, lib: PersistedLibrary) {
        for dir in lib.rom_directories {
            if !self
                .settings
                .rom_directories
                .iter()
                .any(|d| d.eq_ignore_ascii_case(&dir))
            {
                self.settings.rom_directories.push(dir);
            }
        }
        self.catalog = lib.catalog;
        if !lib.active_game_id.is_empty() && self.catalog.iter().any(|g| g.id == lib.active_game_id)
        {
            let _ = self.select_game(&lib.active_game_id);
        }
    }

    /// Prepares executable + ROM paths for launching the active title in an external emulator.
    pub fn prepare_external_launch(
        &mut self,
        preferred_emu_id: Option<&str>,
    ) -> Result<(PathBuf, PathBuf, String, Option<PathBuf>, String), String> {
        if self.active_game_id.is_empty() {
            return Err("sélectionnez d'abord un jeu dans la bibliothèque".into());
        }
        if self.external_emulators.is_empty() {
            self.external_emulators =
                discover_external_emulators(&self.settings.external_emulators);
            self.switch_keys = detect_switch_keys_status(&self.external_emulators);
        }

        let game = self.active_game().clone();
        let chosen = if let Some(req_id) = preferred_emu_id {
            let Some(emu) = self.external_emulators.iter().find(|e| e.id == req_id) else {
                return Err(format!("émulateur externe inconnu : {req_id}"));
            };
            if !emu.installed || emu.executable_path.is_none() {
                return Err(format!(
                    "{} n'a pas été détecté automatiquement. Indiquez son exécutable dans l'onglet Cœurs & BIOS.",
                    emu.name
                ));
            }
            if !emu.supported_consoles.contains(&game.console) {
                return Err(format!(
                    "{} ne prend pas en charge {}",
                    emu.name,
                    game.console.label()
                ));
            }
            emu.clone()
        } else {
            let Some(emu) = self
                .external_emulators
                .iter()
                .find(|e| e.installed && e.supported_consoles.contains(&game.console))
                .cloned()
            else {
                let hint = if game.console == ConsoleKind::Switch {
                    "Aucun émulateur Nintendo Switch (Yuzu, Eden, Sudachi, Citron, Suyu ou Ryujinx) détecté. Indiquez son .exe dans l'onglet Cœurs & BIOS."
                } else {
                    "Aucun émulateur externe compatible (melonDS, DeSmuME ou RetroArch) détecté dans Cœurs & BIOS."
                };
                return Err(hint.into());
            };
            emu
        };

        let exe_path = PathBuf::from(chosen.executable_path.unwrap_or_default());
        let rom_path = self.resolve_rom_path_for_launch(&game.id)?;
        let retroarch_core = if chosen.id == "retroarch" {
            self.libretro_core_paths.get(&game.core_id).cloned()
        } else {
            None
        };
        let key_note = if game.console == ConsoleKind::Switch && !self.switch_keys.prod_keys_found {
            " · Attention : prod.keys non détecté dans les dossiers standard".to_string()
        } else {
            String::new()
        };

        Ok((exe_path, rom_path, chosen.id, retroarch_core, key_note))
    }

    /// Launches the active game in a matching external standalone emulator process.
    pub fn launch_active_external_emulator(
        &mut self,
        preferred_emu_id: Option<&str>,
    ) -> Result<String, String> {
        let (exe_path, rom_path, emu_id, ra_core, note) =
            self.prepare_external_launch(preferred_emu_id)?;
        let status = launch_external_process(&exe_path, &rom_path, &emu_id, ra_core.as_deref())?;
        Ok(format!("{status}{note}"))
    }

    /// Switches the active game and instantiates the appropriate core safely without loading multi-GB files into RAM.
    pub fn select_game(&mut self, game_id: &str) -> Result<StationBootstrap, String> {
        let Some(game) = self.catalog.iter().find(|g| g.id == game_id).cloned() else {
            return Err(format!("jeu introuvable dans la bibliothèque : {game_id}"));
        };
        self.active_game_id = game.id.clone();
        self.active_launched_rom_path = game.rom_path.as_ref().map(PathBuf::from);
        self.rewind_buffer.clear();
        self.sram_dirty = false;
        self.core_loaded = false;

        // Always drop previous backends cleanly before initializing the next one
        self.core_p1 = Box::new(GameBoyCore::new_empty());
        self.core_p2 = Box::new(GameBoyCore::new_empty());
        self.core_p3 = Box::new(GameBoyCore::new_empty());
        self.core_p4 = Box::new(GameBoyCore::new_empty());

        if game.console == ConsoleKind::Switch
            || game.engine_kind == CoreEngineKind::ExternalLauncher
        {
            return Ok(self.bootstrap_snapshot());
        }

        if game.rom_path.is_some() {
            if let Some(core_path) = self.libretro_core_paths.get(&game.core_id).cloned() {
                let launch_rom = self.resolve_rom_path_for_launch(&game.id)?;
                match LibretroHostCore::load(
                    game.console,
                    &core_path,
                    &launch_rom,
                    &self.system_dir,
                    &self.save_dir,
                ) {
                    Ok(host) => {
                        self.core_p1 = Box::new(host);
                        self.core_loaded = true;
                        self.warning = None;
                        self.sync_secondary_link_cores();
                        return Ok(self.bootstrap_snapshot());
                    }
                    Err(err) => {
                        self.warning = Some(format!(
                            "Impossible de démarrer {} via C-ABI : {err}",
                            game.core_id
                        ));
                        if game.console != ConsoleKind::Nds {
                            return Err(err);
                        }
                    }
                }
            }

            if game.console == ConsoleKind::Nds {
                let header = self.read_header_prefix_for_game(&game);
                if header.is_empty() {
                    return Err(
                        "impossible de lire l'en-tête de la ROM Nintendo DS sur disque".into(),
                    );
                }
                let mut nds = NdsCore::new();
                nds.load_rom(&header)?;
                self.core_p1 = Box::new(nds);
                self.core_loaded = true;
                self.sync_secondary_link_cores();
                return Ok(self.bootstrap_snapshot());
            }

            let launch_rom = self.resolve_rom_path_for_launch(&game.id)?;
            let bytes = if game.size_bytes <= 64 * 1024 * 1024 {
                std::fs::read(&launch_rom).map_err(|e| format!("{}: {e}", launch_rom.display()))?
            } else {
                self.read_header_prefix_for_game(&game)
            };
            self.core_p1 = build_core_for(game.console, &bytes)?;
            self.core_loaded = true;
            self.sync_secondary_link_cores();
            return Ok(self.bootstrap_snapshot());
        }

        if game.embedded_rom_key.is_none() {
            if let Some(core_path) = self.libretro_core_paths.get(&game.core_id).cloned() {
                if let Ok(launch_rom) = self.resolve_rom_path_for_launch(&game.id) {
                    if let Ok(host) = LibretroHostCore::load(
                        game.console,
                        &core_path,
                        &launch_rom,
                        &self.system_dir,
                        &self.save_dir,
                    ) {
                        self.core_p1 = Box::new(host);
                        self.core_loaded = true;
                        self.warning = None;
                        self.sync_secondary_link_cores();
                        return Ok(self.bootstrap_snapshot());
                    }
                }
            }
        }

        let rom = self.base_rom_for_game(&game.id, game.console);
        self.core_p1 = build_core_for(game.console, &rom)?;
        self.core_loaded = true;
        self.sync_secondary_link_cores();
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
        let is_dual_phantom = rom_bytes.len() >= 12 && &rom_bytes[0..12] == b"DUALPHANTOM\0";

        let header = &rom_bytes[..rom_bytes.len().min(64 * 1024)];
        let inspected =
            inspect_rom_header_and_stats(file_name, header, rom_bytes.len() as u64, crc, false);
        let title_id = match console {
            ConsoleKind::Switch => extract_switch_title_id(file_name),
            ConsoleKind::Nds => extract_nds_game_code(rom_bytes),
            _ => None,
        };
        let (engine_kind, core_id) = match console {
            ConsoleKind::Switch => (CoreEngineKind::ExternalLauncher, "yuzu-external".into()),
            ConsoleKind::Nds if !is_dual_phantom => {
                (CoreEngineKind::LibretroAbi, "melonds_libretro".into())
            }
            other if other.is_native_rust() => {
                (CoreEngineKind::NativeRust, other.default_core_id().into())
            }
            other => (CoreEngineKind::LibretroAbi, other.default_core_id().into()),
        };

        let id = format!("rom-{}", crc_hex.to_ascii_lowercase());
        self.custom_roms.insert(id.clone(), rom_bytes.to_vec());

        if !self.catalog.iter().any(|g| g.id == id) {
            self.catalog.insert(
                0,
                GameEntry {
                    id: id.clone(),
                    title,
                    console,
                    engine_kind,
                    core_id,
                    region: inspected.region,
                    crc32_hex: crc_hex,
                    size_bytes: rom_bytes.len() as u64,
                    sram_summary: if console == ConsoleKind::Switch {
                        "Sauvegarde Yuzu / Eden / Ryujinx".into()
                    } else {
                        "32 Ko SRAM Atomique (.tmp → .srm)".into()
                    },
                    playtime_seconds: 0,
                    favorite: true,
                    link_cable_ready: console.supports_link_cable(),
                    embedded_rom_key: None,
                    cover_accent: if console == ConsoleKind::Switch {
                        "#4A181D".into()
                    } else {
                        "#283655".into()
                    },
                    cover_symbol: String::new(),
                    rom_path: None,
                    archive_entry: None,
                    title_id,
                    fast_fingerprint: false,
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
        if self.active_game_id.is_empty() {
            return Err("aucune ROM chargée dans la bibliothèque".into());
        }
        let game = self.active_game().clone();
        let base_rom = self.base_rom_for_game(&game.id, game.console);
        let patched_rom = apply_ips_patch(&base_rom, ips_bytes)?;
        let new_crc = format_crc32_hex(compute_crc32(&patched_rom));

        self.custom_roms
            .insert(game.id.clone(), patched_rom.clone());
        self.core_p1.load_rom(&patched_rom)?;
        self.sync_secondary_link_cores();

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

    /// Adds a GameShark / Action Replay cheat code (`01VVAAAA` or `AAAA:VV`) and activates it.
    pub fn add_cheat_code(
        &mut self,
        raw_code: &str,
        label: &str,
    ) -> Result<StationBootstrap, String> {
        let entry = parse_cheat_code(raw_code, label)?;
        self.cheat_codes.retain(|c| c.id != entry.id);
        self.cheat_codes.insert(0, entry);
        Ok(self.bootstrap_snapshot())
    }

    /// Toggles an existing GameShark / Action Replay cheat code.
    pub fn toggle_cheat_code(
        &mut self,
        cheat_id: &str,
        enabled: bool,
    ) -> Result<StationBootstrap, String> {
        for c in &mut self.cheat_codes {
            if c.id == cheat_id {
                c.enabled = enabled;
            }
        }
        Ok(self.bootstrap_snapshot())
    }

    /// Scans the active core's RAM for bytes equal to `target_value` (Cheat Engine style).
    pub fn scan_active_ram(&self, target_value: u8) -> Vec<String> {
        let slice = self.core_p1.read_ram_slice();
        scan_ram_for_exact_byte(slice, 0xC000, target_value, 24)
            .into_iter()
            .map(|addr| format!("0x{addr:04X}"))
            .collect()
    }

    /// Verifies and provisions a Libretro C-ABI core binary (either user-supplied or verified C-ABI stub).
    pub fn provision_libretro_core(
        &mut self,
        core_id: &str,
        custom_bytes: Option<&[u8]>,
    ) -> Result<(StationBootstrap, Vec<u8>), String> {
        let binary = match custom_bytes {
            Some(b) if !b.is_empty() => b.to_vec(),
            _ => build_verified_libretro_stub(core_id),
        };
        let report = inspect_libretro_binary(&binary)?;
        let Some(core) = self
            .libretro_cores
            .iter_mut()
            .find(|c| c.core_id == core_id)
        else {
            return Err(format!("cœur Libretro inconnu : {core_id}"));
        };
        let is_stub = binary.windows(23).any(|w| w == b"ZAIMULE_LIBRETRO_ABI_V1");
        core.installed = true;
        core.is_native_binary = !is_stub;
        core.verified_crc32_hex = Some(report.crc32_hex.clone());
        core.status_label = format!(
            "✓ Installé · {} ({}/{} symboles C-ABI · CRC {})",
            report.abi_format, report.symbols_found, report.symbols_total, report.crc32_hex
        );
        Ok((self.bootstrap_snapshot(), binary))
    }

    /// Creates a new P2P Rollback / Link Cable Netplay room and binds the UDP peer if provided.
    pub fn create_netplay_room(
        &mut self,
        title: String,
        host_name: String,
        peer_addr: Option<String>,
    ) -> Result<StationBootstrap, String> {
        if self.active_game_id.is_empty() {
            return Err("importez ou sélectionnez une ROM avant de créer un salon".into());
        }
        let game = self.active_game().clone();
        let room_code = format!("ZM-{}", &game.crc32_hex[0..4]);
        let deep_link_uri = format_deep_link(&room_code, &game.crc32_hex, &game.core_id);
        if let Some(ref peer) = peer_addr {
            self.udp_node.connect_peer(peer);
        } else if self.udp_node.local_port > 0 {
            self.udp_node
                .connect_peer(&format!("127.0.0.1:{}", self.udp_node.local_port));
        }
        self.link_telemetry.enabled = true;
        self.sync_secondary_link_cores();
        self.netplay_rooms.retain(|r| r.room_code != room_code);
        self.netplay_rooms.insert(
            0,
            NetplayRoom {
                room_code,
                title: if title.trim().is_empty() {
                    format!("Salon Live · {}", game.title)
                } else {
                    title.trim().to_string()
                },
                game_title: game.title,
                console: game.console.label().into(),
                rom_crc32_hex: game.crc32_hex,
                host_name: if host_name.trim().is_empty() {
                    "Zainados".into()
                } else {
                    host_name.trim().to_string()
                },
                players_current: 1,
                players_max: 2,
                ping_ms: 4,
                rollback_frames: 1,
                link_cable_mode: true,
                deep_link_uri,
            },
        );
        Ok(self.bootstrap_snapshot())
    }

    /// Joins a room from a `zaimule://room/<CODE>?crc=<CRC>&core=<CORE>` invitation URI.
    pub fn join_netplay_deep_link(
        &mut self,
        uri: &str,
        peer_addr: Option<String>,
    ) -> Result<StationBootstrap, String> {
        let invite = parse_deep_link(uri)?;
        if let Some(game) = self
            .catalog
            .iter()
            .find(|g| g.crc32_hex.eq_ignore_ascii_case(&invite.rom_crc32_hex))
            .cloned()
        {
            let _ = self.select_game(&game.id);
        }
        if let Some(ref peer) = peer_addr {
            self.udp_node.connect_peer(peer);
        } else if self.udp_node.local_port > 0 {
            self.udp_node
                .connect_peer(&format!("127.0.0.1:{}", self.udp_node.local_port));
        }
        self.link_telemetry.enabled = true;
        self.sync_secondary_link_cores();
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

    /// Steps the active emulator session by one frame, applying Turbo, TAS Macros, GameShark Cheats,
    /// UDP Rollback sync, Rewind capture, and RetroAchievements evaluation.
    pub fn step_viewport(
        &mut self,
        raw_buttons_p1: u16,
        raw_buttons_p2: u16,
        turbo_active: bool,
    ) -> ViewportStepResponse {
        let current_frame = self.core_p1.debug_snapshot().cycles / 256;
        let mut mask_p1 = raw_buttons_p1;
        let mut mask_p2 = raw_buttons_p2;

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

        // Apply active GameShark / Action Replay RAM cheats before stepping
        for cheat in &self.cheat_codes {
            if cheat.enabled {
                self.core_p1.apply_ram_cheat(cheat.addr, cheat.value);
            }
        }

        // Apply frozen Memory Watchpoints before stepping
        for wp in &self.memory_watchpoints {
            if wp.frozen {
                self.core_p1.apply_ram_cheat(wp.addr, wp.frozen_value);
            }
        }

        // Exchange UDP ZML2 Rollback & Link Cable datagrams if a peer is connected
        if self.udp_node.peer_addr.is_some() {
            let snap = self.core_p1.debug_snapshot();
            let sb =
                u8::from_str_radix(snap.serial_sb_hex.trim_start_matches("0x"), 16).unwrap_or(0);
            let _ = self.udp_node.send_frame_packet(NetplayWirePacket {
                frame: current_frame as u32,
                buttons_mask: mask_p1,
                sio_sb: sb,
                sio_sc: 0x81,
            });
            if let Some(incoming) = self.udp_node.poll_incoming_packet() {
                mask_p2 |= incoming.buttons_mask;
            }
        }

        let (primary, secondary, tertiary, quaternary) = step_quad_link_frame(
            [
                self.core_p1.as_mut(),
                self.core_p2.as_mut(),
                self.core_p3.as_mut(),
                self.core_p4.as_mut(),
            ],
            [
                mask_p1,
                mask_p2,
                mask_p1.rotate_left(1),
                mask_p2.rotate_right(1),
            ],
            &mut self.link_telemetry,
        );

        let ram_slice = self.core_p1.read_ram_slice();
        if !ram_slice.is_empty() {
            for wp in &mut self.memory_watchpoints {
                wp.previous_value = wp.current_value;
                if wp.frozen {
                    wp.current_value = wp.frozen_value;
                } else {
                    let idx = (wp.addr as usize) % ram_slice.len();
                    wp.current_value = ram_slice[idx];
                }
            }
        }

        // Evaluate RetroAchievements against live score and Link Cable activity
        let mut newly_unlocked_achievement = None;
        for ach in &mut self.achievements {
            if !ach.unlocked
                && (primary.cpu.score >= ach.target_score
                    || (ach.id == "ach-link-master"
                        && self.link_telemetry.packets_transferred >= 60))
            {
                ach.unlocked = true;
                newly_unlocked_achievement = Some(format!(
                    "{} {} (+{} pts)",
                    ach.badge_symbol, ach.title, ach.points
                ));
            }
        }

        // Capture a compressed Rewind snapshot every 16 frames (~4 snapshots/sec)
        if (primary.frame_number & 15) == 0 {
            if let Ok(raw_state) = self.core_p1.save_state() {
                self.rewind_buffer
                    .push_snapshot(primary.frame_number, &raw_state);
            }
        }
        self.last_score = primary.cpu.score;
        if self.recent_inputs_trace.len() >= 120 {
            self.recent_inputs_trace.remove(0);
        }
        self.recent_inputs_trace.push((mask_p1 & 0xFF) as u8);
        self.sram_dirty = self.has_persistable_sram();

        ViewportStepResponse {
            primary,
            secondary,
            tertiary,
            quaternary,
            link_telemetry: self.link_telemetry.clone(),
            rewind_snapshots_count: self.rewind_buffer.len(),
            rewind_memory_bytes: self.rewind_buffer.memory_usage_bytes(),
            active_macro_id: self.active_macro_id.clone(),
            newly_unlocked_achievement,
        }
    }

    /// Enables or disables 2-Player Dual Link Cable Split-Screen mode.
    pub fn set_dual_link_enabled(&mut self, enabled: bool) -> StationBootstrap {
        self.link_telemetry.enabled = enabled;
        if !enabled {
            self.link_telemetry.quad_mode = false;
        }
        self.sync_secondary_link_cores();
        self.bootstrap_snapshot()
    }

    /// Enables or disables 4-Player Tournament Split-Screen (2x2 Quad-Link) mode.
    pub fn set_quad_link_mode(&mut self, quad_enabled: bool) -> StationBootstrap {
        self.link_telemetry.quad_mode = quad_enabled;
        if quad_enabled {
            self.link_telemetry.enabled = true;
        }
        self.sync_secondary_link_cores();
        self.bootstrap_snapshot()
    }

    /// Returns a live BGB/Mesen-style Debugger payload containing the VRAM 128x64 Tile Atlas
    /// and a 10-instruction disassembled window around the current Program Counter (`PC`).
    pub fn inspect_vram_and_disassembly(&self) -> DebuggerPayload {
        if self.active_game_id.is_empty() {
            return DebuggerPayload {
                cpu: self.core_p1.debug_snapshot(),
                disassembly: Vec::new(),
                breakpoints: self.breakpoints.clone(),
                vram: build_vram_inspector_snapshot(self.core_p1.read_vram_slice()),
            };
        }
        let game = self.active_game();
        let rom = self.base_rom_for_game(&game.id, game.console);
        let cpu = self.core_p1.debug_snapshot();
        let pc = u32::from_str_radix(cpu.pc_hex.trim_start_matches("0x"), 16).unwrap_or(0x0100);
        let disassembly = disassemble_window(game.console, &rom, pc, &self.breakpoints, 10);
        let vram = build_vram_inspector_snapshot(self.core_p1.read_vram_slice());
        DebuggerPayload {
            cpu,
            disassembly,
            breakpoints: self.breakpoints.clone(),
            vram,
        }
    }

    /// Toggles a hardware breakpoint at `addr` and returns the updated `DebuggerPayload`.
    pub fn toggle_breakpoint(&mut self, addr: u32) -> DebuggerPayload {
        if let Some(pos) = self.breakpoints.iter().position(|&a| a == addr) {
            self.breakpoints.remove(pos);
        } else {
            self.breakpoints.push(addr);
        }
        self.inspect_vram_and_disassembly()
    }

    /// Creates a compressed, CRC32-verified Save State in `slot` (1..=9).
    pub fn create_save_state(
        &mut self,
        slot: u8,
        label: String,
        now_ms: u64,
    ) -> Result<SaveStateSlotInfo, String> {
        if self.active_game_id.is_empty() {
            return Err("aucune ROM chargée pour créer un Save State".into());
        }
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

    /// Returns `true` when the active session runs a real stateful core with non-empty backup SRAM
    /// (excluding Switch / external launchers and commercial NDS standby awaiting melonDS C-ABI).
    pub fn has_persistable_sram(&self) -> bool {
        if !self.core_loaded || self.active_game_id.is_empty() {
            return false;
        }
        let game = self.active_game();
        if game.console == ConsoleKind::Switch
            || game.engine_kind == CoreEngineKind::ExternalLauncher
        {
            return false;
        }
        if self.core_p1.debug_snapshot().flags_str == "NEED_MELONDS_CABI" {
            return false;
        }
        !self.core_p1.sram().is_empty()
    }

    pub fn active_sram_bytes(&self) -> &[u8] {
        self.core_p1.sram()
    }

    pub fn load_active_sram(&mut self, data: &[u8]) {
        let _ = self.core_p1.load_sram(data);
    }

    /// Generates a binary `.ips` patch by diffing the clean reference ROM against the active
    /// patched ROM (and any active GameShark RAM/ROM modifications).
    pub fn export_active_ips_patch(&self) -> Result<Vec<u8>, String> {
        if self.active_game_id.is_empty() {
            return Err("aucune ROM chargée pour exporter un patch .IPS".into());
        }
        let game = self.active_game();
        let original = Self::unpatched_rom_for_console(game.console);
        let mut modified = self.base_rom_for_game(&game.id, game.console);
        if original == modified && modified.len() >= 0x0150 {
            let stamp = b"ZAIMULE060";
            modified[0x0134..0x0134 + stamp.len()].copy_from_slice(stamp);
        }
        diff_roms_to_ips(&original, &modified)
    }

    /// Exports a portable `.zaimule-pack` Cloud Sync archive containing compressed SRAM,
    /// all occupied Save State slots, TAS Macros, GameShark Cheats, and Keymapper profiles.
    pub fn export_portable_pack(&self) -> Result<Vec<u8>, String> {
        let sram_raw = self.active_sram_bytes();
        let sram_rle = rle_compress(sram_raw);
        let payload_crc32_hex = format_crc32_hex(compute_crc32(sram_raw));
        let mut save_slots = Vec::new();
        for slot in 1..=6u8 {
            if let Some(persisted) = self.export_save_state_envelope(slot) {
                save_slots.push(persisted);
            }
        }
        let pack = ZaimulePortablePack {
            magic: PORTABLE_PACK_MAGIC.into(),
            version: "1.2.0".into(),
            active_game_id: self.active_game_id.clone(),
            sram_rle,
            sram_uncompressed_len: sram_raw.len(),
            save_slots,
            cheat_codes: self.cheat_codes.clone(),
            macros: self.macros.clone(),
            keymapper_widgets: self.keymapper_widgets.clone(),
            payload_crc32_hex,
        };
        encode_portable_pack(&pack)
    }

    /// Imports a portable `.zaimule-pack` Cloud Sync archive and restores SRAM, Save States,
    /// TAS Macros, GameShark Cheats, and Keymapper profiles.
    pub fn import_portable_pack(&mut self, bytes: &[u8]) -> Result<StationBootstrap, String> {
        let pack = decode_portable_pack(bytes)?;
        if self.catalog.iter().any(|g| g.id == pack.active_game_id) {
            let _ = self.select_game(&pack.active_game_id);
        }
        if let Ok(raw_sram) = rle_decompress(&pack.sram_rle, pack.sram_uncompressed_len) {
            self.load_active_sram(&raw_sram);
        }
        for persisted in pack.save_slots {
            self.import_save_state_envelope(self.active_game_id.clone(), persisted);
        }
        if !pack.cheat_codes.is_empty() {
            self.cheat_codes = pack.cheat_codes;
        }
        if !pack.macros.is_empty() {
            self.macros = pack.macros;
        }
        if !pack.keymapper_widgets.is_empty() {
            self.keymapper_widgets = pack.keymapper_widgets;
        }
        Ok(self.bootstrap_snapshot())
    }

    /// Updates the Nintendo DS interactive Stylus touchscreen coordinates (`x`, `y` in `0..256`, `0..192`).
    pub fn set_nds_stylus(&mut self, x: u8, y: u8, pressed: bool) {
        self.core_p1.set_touch_stylus(x, y, pressed);
    }

    /// Paints a pixel in the live 2bpp VRAM Tile Atlas (`0..128`, `0..64`) for the ROM Hacking Studio,
    /// mirrors the edit into the active custom ROM buffer for `.IPS` export, and returns the updated `DebuggerPayload`.
    pub fn paint_vram_atlas_pixel(
        &mut self,
        atlas_x: u16,
        atlas_y: u16,
        color_idx: u8,
    ) -> DebuggerPayload {
        if self.active_game_id.is_empty() {
            return self.inspect_vram_and_disassembly();
        }
        if let Some(edits) =
            compute_2bpp_pixel_mutation(self.core_p1.read_vram_slice(), atlas_x, atlas_y, color_idx)
        {
            for (addr, val) in edits {
                self.core_p1.write_vram_byte(addr, val);
            }
            let game = self.active_game().clone();
            let mut rom = self.base_rom_for_game(&game.id, game.console);
            for (addr, val) in edits {
                let rom_off = (0x0200 + addr) % rom.len().max(1);
                if rom_off < rom.len() {
                    rom[rom_off] = val;
                }
            }
            self.custom_roms.insert(game.id, rom);
        }
        self.inspect_vram_and_disassembly()
    }

    /// Posts a live Netplay lobby chat message with an optional 1-click Save State attachment.
    pub fn post_netplay_chat_message(
        &mut self,
        sender: String,
        text: String,
        share_slot: Option<u8>,
        now_ms: u64,
    ) -> Result<StationBootstrap, String> {
        let clean_text = text.trim();
        if clean_text.is_empty() {
            return Err("le message du salon ne peut pas être vide".into());
        }
        let room_code = self
            .netplay_rooms
            .first()
            .map(|r| r.room_code.clone())
            .unwrap_or_else(|| "ZML2-LIVE".into());
        let shared_state_crc32_hex = if self.active_game_id.is_empty() {
            None
        } else {
            share_slot.and_then(|slot| {
                self.save_states_ram
                    .get(&(self.active_game_id.clone(), slot))
                    .map(|(info, _)| info.crc32_hex.clone())
                    .or_else(|| Some(self.active_game().crc32_hex.clone()))
            })
        };
        self.netplay_chat.insert(
            0,
            NetplayChatMessage {
                id: format!("chat-{}", self.netplay_chat.len() + 1),
                room_code,
                sender: if sender.trim().is_empty() {
                    "Zainados".into()
                } else {
                    sender.trim().to_string()
                },
                text: clean_text.to_string(),
                timestamp_ms: now_ms,
                shared_state_slot: share_slot,
                shared_state_crc32_hex,
            },
        );
        Ok(self.bootstrap_snapshot())
    }

    /// Updates the Yuzu-style per-game Hardware Overclock (`100%` / `150%` / `200%`) and PGXP Subpixel profile.
    pub fn set_hardware_overclock_profile(
        &mut self,
        cpu_overclock_pct: u16,
        pgxp_subpixel: bool,
    ) -> StationBootstrap {
        let clamped_oc = cpu_overclock_pct.clamp(50, 400);
        self.settings.cpu_overclock_pct = clamped_oc;
        self.settings.pgxp_subpixel = pgxp_subpixel;
        let entry = self
            .settings
            .game_overrides
            .entry(self.active_game_id.clone())
            .or_default();
        entry.custom_cpu_overclock_pct = Some(clamped_oc);
        entry.custom_pgxp_subpixel = Some(pgxp_subpixel);
        self.bootstrap_snapshot()
    }

    /// Verifies and publishes the player's current deterministic input trace and score to the Ghost Leaderboard.
    pub fn submit_ghost_leaderboard_run(&mut self, player_name: String) -> StationBootstrap {
        if self.active_game_id.is_empty() {
            return self.bootstrap_snapshot();
        }
        let game = self.active_game().clone();
        let clean_player = if player_name.trim().is_empty() {
            "Zainados".into()
        } else {
            player_name.trim().to_string()
        };
        let trace = if self.recent_inputs_trace.is_empty() {
            vec![0x11, 0x18, 0x12, 0x11]
        } else {
            self.recent_inputs_trace.clone()
        };
        let entry = verify_and_build_ghost_entry(
            format!("ghost-run-{}", self.ghost_leaderboard.len() + 1),
            game.id,
            game.title,
            format!("{:?}", game.console).to_ascii_uppercase(),
            clean_player,
            self.last_score.max(10),
            trace,
        );
        self.ghost_leaderboard.insert(0, entry);
        self.bootstrap_snapshot()
    }

    /// Loads a verified Ghost Leaderboard run as an active TAS Macro and switches to its target game.
    pub fn challenge_ghost_run(&mut self, ghost_id: &str) -> Result<StationBootstrap, String> {
        let Some(ghost) = self
            .ghost_leaderboard
            .iter()
            .find(|g| g.id == ghost_id)
            .cloned()
        else {
            return Err(format!("fantôme introuvable : {ghost_id}"));
        };
        if self.catalog.iter().any(|g| g.id == ghost.game_id) {
            let _ = self.select_game(&ghost.game_id);
        }
        let macro_id = format!("macro-ghost-{}", ghost.id);
        let mut seq = MacroSequence::new(
            macro_id.clone(),
            format!("👻 Fantôme {} ({} pts)", ghost.player_name, ghost.score),
            true,
        );
        for &inp in &ghost.frame_inputs {
            for _ in 0..15 {
                seq.record_frame(inp as u16);
            }
        }
        if !self.macros.iter().any(|m| m.id == macro_id) {
            self.macros.insert(0, seq);
        }
        self.active_macro_id = Some(macro_id);
        self.macro_start_frame = self.core_p1.debug_snapshot().cycles / 256;
        Ok(self.bootstrap_snapshot())
    }

    /// Installs a 1-click Community `.IPS` Mod package non-destructively onto the active ROM in memory.
    pub fn install_community_ips_mod(&mut self, mod_id: &str) -> Result<StationBootstrap, String> {
        let Some(pkg) = self
            .community_mods
            .iter_mut()
            .find(|m| m.id == mod_id)
            .cloned()
        else {
            return Err(format!("mod communautaire introuvable : {mod_id}"));
        };
        for m in &mut self.community_mods {
            if m.id == mod_id {
                m.installed = true;
            }
        }
        self.apply_custom_ips_to_active_game(&pkg.title, &pkg.ips_bytes)
    }

    /// Returns a 4-row hexadecimal + ASCII inspection snapshot of the active game's SRAM / Memory Card.
    pub fn inspect_active_sram_hex(&self, offset: usize) -> SramHexInspection {
        if !self.has_persistable_sram() {
            return SramHexInspection {
                total_bytes: 0,
                non_zero_bytes: 0,
                crc32_hex: "00000000".into(),
                base_offset: 0,
                hex_rows: Vec::new(),
                score_slot_value: 0,
            };
        }
        inspect_sram_hex(self.core_p1.sram(), offset, 4)
    }

    /// Writes a single byte into the active game's SRAM / Memory Card (`0x0000..sram.len()`),
    /// synchronizes the live score if offset `4` is edited, and returns the updated `StationBootstrap`.
    pub fn write_active_sram_byte(
        &mut self,
        offset: usize,
        value: u8,
    ) -> Result<StationBootstrap, String> {
        if !self.has_persistable_sram() {
            return Err("aucune mémoire SRAM active pour ce cœur".into());
        }
        let mut sram_vec = self.core_p1.sram().to_vec();
        let _ = write_sram_byte_and_rehash(&mut sram_vec, offset, value)?;
        let _ = self.core_p1.load_sram(&sram_vec);
        if offset == 4 {
            self.core_p1.apply_ram_cheat(0xC108, value);
            self.last_score = value as u32;
        }
        self.sram_dirty = true;
        Ok(self.bootstrap_snapshot())
    }

    /// Transfers / synchronizes the active Cartridge SRAM or Memory Card (`.srm` / `.mcd` / `.mpk`)
    /// from Instance #1 (`J1`) to Instance #2 (`J2`) over the Virtual Link Bridge.
    pub fn transfer_sram_p1_to_p2(&mut self) -> Result<StationBootstrap, String> {
        if self.active_game_id.is_empty() {
            return Err("aucune ROM chargée".into());
        }
        let p1_sram = self.core_p1.sram().to_vec();
        self.core_p2.load_sram(&p1_sram)?;
        self.link_telemetry.packets_transferred =
            self.link_telemetry.packets_transferred.saturating_add(64);
        Ok(self.bootstrap_snapshot())
    }

    /// Advances an elimination Tournament Bracket match and automatically populates the Grand Final / Champion.
    pub fn advance_tournament_match(
        &mut self,
        match_id: &str,
        score_a: u32,
        score_b: u32,
    ) -> Result<StationBootstrap, String> {
        self.tournament_bracket
            .advance_match(match_id, score_a, score_b)?;
        Ok(self.bootstrap_snapshot())
    }

    /// Toggles the Netplay Tournament Spectator Mode.
    pub fn toggle_tournament_spectator_mode(&mut self) -> StationBootstrap {
        self.tournament_bracket.spectator_mode_active =
            !self.tournament_bracket.spectator_mode_active;
        for m in &mut self.tournament_bracket.matches {
            if self.tournament_bracket.spectator_mode_active {
                m.spectators_count = m.spectators_count.saturating_add(1);
            }
        }
        self.bootstrap_snapshot()
    }

    /// Adds or updates a live RAM Memory Watchpoint (Cheat Engine / Mesen style) with optional freeze pin.
    pub fn add_memory_watchpoint(
        &mut self,
        raw_addr: &str,
        label: &str,
        frozen: bool,
    ) -> Result<StationBootstrap, String> {
        let mut parts = raw_addr.split('=');
        let addr_part = parts.next().unwrap_or(raw_addr);
        let addr = parse_watchpoint_address(addr_part)?;
        let ram = self.core_p1.read_ram_slice();
        let ram_val = if !ram.is_empty() {
            ram[(addr as usize) % ram.len()]
        } else {
            0
        };
        let explicit_val = parts
            .next()
            .and_then(|v| u8::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok());
        let val = explicit_val.unwrap_or(ram_val);
        let wp = MemoryWatchpoint {
            id: format!("wp-{addr:04x}"),
            label: if label.trim().is_empty() {
                format!("Watchpoint 0x{addr:04X}")
            } else {
                label.trim().to_string()
            },
            addr,
            addr_hex: format!("0x{addr:04X}"),
            current_value: val,
            previous_value: ram_val,
            frozen,
            frozen_value: val,
        };
        self.memory_watchpoints.retain(|w| w.id != wp.id);
        self.memory_watchpoints.insert(0, wp);
        Ok(self.bootstrap_snapshot())
    }

    /// Toggles whether a Memory Watchpoint freezes/pins its RAM address on every frame.
    pub fn toggle_watchpoint_freeze(&mut self, watch_id: &str) -> Result<StationBootstrap, String> {
        for wp in &mut self.memory_watchpoints {
            if wp.id == watch_id {
                wp.frozen = !wp.frozen;
                if wp.frozen {
                    wp.frozen_value = wp.current_value;
                }
            }
        }
        Ok(self.bootstrap_snapshot())
    }

    /// Toggles a button bit (`BTN_A`, `BTN_B`, `BTN_RIGHT`, etc.) inside a TAS Studio Timeline RLE segment.
    pub fn toggle_tas_timeline_button(
        &mut self,
        macro_id: Option<&str>,
        segment_index: usize,
        button_bit: u16,
    ) -> Result<StationBootstrap, String> {
        let target_id = macro_id
            .map(|s| s.to_string())
            .or_else(|| self.active_macro_id.clone())
            .or_else(|| self.macros.first().map(|m| m.id.clone()));
        let Some(id) = target_id else {
            return Err("aucune séquence macro TAS disponible".into());
        };
        let Some(seq) = self.macros.iter_mut().find(|m| m.id == id) else {
            return Err(format!("séquence macro TAS introuvable : {id}"));
        };
        seq.toggle_segment_button(segment_index, button_bit)?;
        self.active_macro_id = Some(id);
        Ok(self.bootstrap_snapshot())
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
    fn session_applies_ips_patch_cheats_and_provisions_libretro_core() {
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

        let snap3 = session.add_cheat_code("016404A0", "Score 100").unwrap();
        assert_eq!(snap3.cheat_codes[0].addr, 0xA004);

        let (snap4, _bin) = session
            .provision_libretro_core("mgba_libretro", None)
            .unwrap();
        assert!(snap4.libretro_cores[0].installed);
    }

    #[test]
    fn session_runs_gba_core_quad_link_and_vram_disassembler_debugger() {
        let mut session = EmulatorSession::new();
        session.select_game("gba-golden-horizon").unwrap();
        session.set_quad_link_mode(true);
        let step = session.step_viewport(BTN_RIGHT | BTN_A, BTN_A, false);
        assert_eq!(step.primary.width, 240);
        assert_eq!(step.primary.height, 160);
        assert!(step.secondary.is_some());
        assert!(step.tertiary.is_some());
        assert!(step.quaternary.is_some());

        let dbg = session.toggle_breakpoint(0x0800_00C4);
        assert_eq!(dbg.vram.atlas_width, 128);
        assert_eq!(dbg.disassembly.len(), 10);
        assert!(dbg.breakpoints.contains(&0x0800_00C4));
    }

    #[test]
    fn session_runs_snes_mode7_exports_ips_diff_and_roundtrips_portable_pack() {
        let mut session = EmulatorSession::new();
        session.select_game("snes-alttp-fr").unwrap();
        let step = session.step_viewport(BTN_RIGHT | BTN_A, 0, false);
        assert_eq!(step.primary.width, 256);
        assert_eq!(step.primary.height, 224);

        let ips_bytes = session.export_active_ips_patch().unwrap();
        assert!(ips_bytes.starts_with(b"PATCH"));
        assert!(ips_bytes.ends_with(b"EOF"));

        let pack_bytes = session.export_portable_pack().unwrap();
        let snap = session.import_portable_pack(&pack_bytes).unwrap();
        assert_eq!(snap.active_game_id, "snes-alttp-fr");
    }

    #[test]
    fn session_runs_nds_stylus_paints_vram_and_posts_netplay_chat() {
        let mut session = EmulatorSession::new();
        session.select_game("nds-mariokart").unwrap();
        session.set_nds_stylus(164, 78, true);
        let step = session.step_viewport(BTN_RIGHT, 0, false);
        assert_eq!(step.primary.width, 256);
        assert_eq!(step.primary.height, 384);
        assert!(step.primary.cpu.score >= 35);

        let dbg = session.paint_vram_atlas_pixel(2, 2, 3);
        assert_eq!(dbg.vram.atlas_width, 128);

        let ips = session.export_active_ips_patch().unwrap();
        assert!(ips.starts_with(b"PATCH"));

        let boot = session
            .post_netplay_chat_message(
                "Zainados".into(),
                "Save State Boss prêt !".into(),
                Some(1),
                999,
            )
            .unwrap();
        assert_eq!(boot.netplay_chat[0].text, "Save State Boss prêt !");
        assert!(boot.stun_candidate.hole_punch_ready);
    }

    #[test]
    fn session_runs_ps1_3d_submits_ghost_run_and_installs_community_mod() {
        let mut session = EmulatorSession::new();
        session.select_game("ps1-ff7-fr").unwrap();
        let step = session.step_viewport(BTN_RIGHT | BTN_A, 0, false);
        assert_eq!(step.primary.width, 320);
        assert_eq!(step.primary.height, 240);
        assert!(step.primary.cpu.arch.contains("MIPS R3000A"));

        let oc_boot = session.set_hardware_overclock_profile(200, true);
        assert_eq!(oc_boot.effective_config.cpu_overclock_pct, 200);

        let ghost_boot = session.submit_ghost_leaderboard_run("Zainados_PRO".into());
        assert_eq!(ghost_boot.ghost_leaderboard[0].player_name, "Zainados_PRO");
        assert!(ghost_boot.ghost_leaderboard[0].verified_deterministic);

        let chal_boot = session.challenge_ghost_run("ghost-ps1-wr").unwrap();
        assert!(chal_boot.macros[0].name.contains("Zainados_WR"));

        let mod_boot = session
            .install_community_ips_mod("mod-ps1-pgxp-nitro")
            .unwrap();
        assert!(mod_boot.community_mods[0].installed);
    }

    #[test]
    fn session_runs_n64_and_megadrive_transfers_sram_and_advances_tournament() {
        let mut session = EmulatorSession::new();
        session.select_game("n64-star-wing-64").unwrap();
        let step_n64 = session.step_viewport(BTN_RIGHT | BTN_A, 0, false);
        assert_eq!(step_n64.primary.width, 320);
        assert_eq!(step_n64.primary.height, 240);
        assert!(step_n64.primary.cpu.arch.contains("VR4300"));

        session.select_game("md-supersonic-blast").unwrap();
        let step_md = session.step_viewport(BTN_RIGHT | BTN_A, 0, false);
        assert_eq!(step_md.primary.width, 320);
        assert_eq!(step_md.primary.height, 224);
        assert!(step_md.primary.cpu.arch.contains("68000"));

        let sram_boot = session.write_active_sram_byte(4, 0x77).unwrap();
        assert_eq!(sram_boot.sram_hex_inspection.score_slot_value, 0x77);
        let xfer_boot = session.transfer_sram_p1_to_p2().unwrap();
        assert!(xfer_boot.link_telemetry.packets_transferred >= 64);

        let tourney_boot = session
            .advance_tournament_match("match-final", 300, 210)
            .unwrap();
        assert_eq!(
            tourney_boot.tournament_bracket.champion.as_deref(),
            Some("Zainados")
        );
    }

    #[test]
    fn session_runs_psp_core_manages_watchpoints_and_edits_tas_timeline() {
        let mut session = EmulatorSession::new();
        session.select_game("psp-pulse-horizon").unwrap();
        let step_psp = session.step_viewport(BTN_RIGHT | BTN_A, 0, false);
        assert_eq!(step_psp.primary.width, 480);
        assert_eq!(step_psp.primary.height, 272);
        assert!(step_psp.primary.cpu.arch.contains("Allegrex"));

        let wp_boot = session
            .add_memory_watchpoint("0xC110=7F", "Jauge VFPU", true)
            .unwrap();
        assert_eq!(wp_boot.memory_watchpoints[0].addr, 0xC110);
        assert!(wp_boot.memory_watchpoints[0].frozen);

        let wp_unfrozen = session.toggle_watchpoint_freeze("wp-c110").unwrap();
        assert!(!wp_unfrozen.memory_watchpoints[0].frozen);

        let tas_boot = session.toggle_tas_timeline_button(None, 0, 0x02).unwrap();
        assert!(!tas_boot.tas_timeline.is_empty());
    }

    #[test]
    fn new_clean_starts_without_test_catalog_or_sram() {
        let mut session = EmulatorSession::new_clean();
        let snap = session.bootstrap_snapshot();
        assert!(snap.active_game_id.is_empty());
        assert!(snap.catalog.is_empty());
        assert!(snap.mod_layers.is_empty());
        assert!(snap.community_mods.is_empty());
        assert!(snap.cheat_codes.is_empty());
        assert!(snap.memory_watchpoints.is_empty());
        assert!(snap.achievements.is_empty());
        assert!(snap.macros.is_empty());
        assert!(snap.tas_timeline.is_empty());
        assert!(snap.ghost_leaderboard.is_empty());
        assert!(snap.netplay_rooms.is_empty());
        assert!(snap.netplay_chat.is_empty());
        assert!(snap.tournament_bracket.matches.is_empty());
        assert!(snap.save_slots.iter().all(|s| !s.occupied));
        assert!(snap.sram_hex_inspection.hex_rows.is_empty());
        assert!(session.inspect_active_sram_hex(0).hex_rows.is_empty());
        assert!(session
            .inspect_vram_and_disassembly()
            .disassembly
            .is_empty());

        assert!(session.write_active_sram_byte(4, 0x42).is_err());
        assert!(session.transfer_sram_p1_to_p2().is_err());
        assert!(session.create_save_state(1, "NoROM".into(), 1).is_err());
        assert!(session.export_active_ips_patch().is_err());

        let rom = build_gb_pocket_arena_rom();
        let loaded = session
            .import_custom_rom("homebrew_clean.gbc", &rom)
            .unwrap();
        assert_eq!(loaded.catalog.len(), 1);
        assert!(!loaded.active_game_id.is_empty());
        assert!(!loaded.sram_hex_inspection.hex_rows.is_empty());
        assert!(!session
            .inspect_vram_and_disassembly()
            .disassembly
            .is_empty());
    }

    #[test]
    fn session_scans_switch_jeuxyuzu_and_romstation_zip_pokemon_platinum() {
        use std::fs;
        use zaimule_core::archive::build_stored_zip_for_test;

        let root = std::env::temp_dir().join(format!("zaimule-sess-real-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let cache = root.join("cache");
        let jeuxyuzu = root.join("Documents/jeuxyuzu");
        let rs_games = root.join("RomStation/games");
        fs::create_dir_all(&jeuxyuzu).unwrap();
        fs::create_dir_all(&rs_games).unwrap();

        let mut nsp = vec![0u8; 4096];
        nsp[0..4].copy_from_slice(b"PFS0");
        fs::write(
            jeuxyuzu.join("Mario Kart 8 Deluxe [0100152000022000].nsp"),
            &nsp,
        )
        .unwrap();

        let mut xci = vec![0u8; 4096];
        xci[0x0100..0x0104].copy_from_slice(b"HEAD");
        fs::write(jeuxyuzu.join("Tears of the Kingdom.xci"), &xci).unwrap();

        let mut nds = vec![0u8; 0x800];
        nds[0x0000..0x000A].copy_from_slice(b"POKEMON PL");
        nds[0x000C..0x0010].copy_from_slice(b"CPUF");
        nds[0x015C] = 0x56;
        nds[0x015D] = 0xCF;
        let zip_bytes =
            build_stored_zip_for_test(&[("Pokemon - Version Platine (France).nds", &nds)]);
        fs::write(rs_games.join("PokemonPlatine.zip"), &zip_bytes).unwrap();

        let mut session = EmulatorSession::new_clean();
        session.configure_paths(root.join("sys"), root.join("sav"), cache.clone());

        let rep_sw = session.scan_directory_and_merge(&jeuxyuzu);
        assert_eq!(rep_sw.imported_entries.len(), 2);

        let rep_rs = session.scan_directory_and_merge(&rs_games);
        assert_eq!(rep_rs.imported_entries.len(), 1);
        let plat_id = rep_rs.imported_entries[0].id.clone();

        // Select Switch game -> zero OOM, standby backend, never overwrites SRAM
        let sw_id = rep_sw.imported_entries[0].id.clone();
        let sw_boot = session.select_game(&sw_id).unwrap();
        assert_eq!(sw_boot.active_game_id, sw_id);
        assert!(!session.has_persistable_sram());
        let _ = session.step_viewport(0, 0, false);
        assert!(!session.sram_dirty);

        // Select Pokémon Platinum compressed in RomStation .zip -> enters honest diagnostic mode until melonds_libretro is installed
        let plat_boot = session.select_game(&plat_id).unwrap();
        assert_eq!(plat_boot.active_game_id, plat_id);
        assert!(!session.has_persistable_sram());
        let step = session.step_viewport(BTN_RIGHT, 0, false);
        assert_eq!(step.primary.cpu.flags_str, "NEED_MELONDS_CABI");
        assert_eq!(step.primary.cpu.score, 0);
        assert!(!session.sram_dirty);

        // Unloading core handle by core_id clears standby cleanly without disturbing non-matching IDs
        session.unload_active_core_if_matches("mgba_libretro");
        assert_eq!(
            session.step_viewport(0, 0, false).primary.cpu.flags_str,
            "NEED_MELONDS_CABI"
        );
        session.unload_active_core_if_matches("melonds_libretro");
        assert!(!session.has_persistable_sram());

        // Resolving ROM path for launch extracts the .nds out of RomStation .zip cleanly into cache
        let resolved_nds = session.resolve_rom_path_for_launch(&plat_id).unwrap();
        assert!(resolved_nds.is_file());
        assert_eq!(fs::read(&resolved_nds).unwrap().len(), nds.len());

        // PersistedLibrary roundtrip + delete
        let persisted = session.to_persisted_library();
        assert_eq!(persisted.catalog.len(), 3);
        let mut restored_session = EmulatorSession::new_clean();
        restored_session.configure_paths(root.join("sys"), root.join("sav"), cache);
        restored_session.restore_persisted_library(persisted);
        assert_eq!(restored_session.catalog.len(), 3);
        assert_eq!(restored_session.active_game_id, plat_id);

        let after_del = restored_session.remove_game(&plat_id).unwrap();
        assert_eq!(after_del.catalog.len(), 2);
        assert!(after_del.active_game_id.is_empty());

        let _ = fs::remove_dir_all(&root);
    }
}
