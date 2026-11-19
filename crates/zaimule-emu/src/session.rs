//! Full emulation session coordinator combining clean user-curated ROM catalog,
//! pure-Rust native cores (`GameBoyCore`, `GbaCore`, `NdsCore`), `"ZMFR"` v1 binary
//! frame + stereo PCM packet streaming, 60s Rewind buffer, Save State slots,
//! BlueStacks Keymapper/Turbo/Macros, GameShark RAM Cheats, Link Cable sync,
//! and Option-A Switch Vault (`Torzu` Switch 1 & `Fritchii` Switch 2).

use crate::debugger::{
    build_vram_inspector_snapshot, compute_2bpp_pixel_mutation, disassemble_window, DebuggerPayload,
};
use crate::gb::GameBoyCore;
use crate::gba::GbaCore;
use crate::link::{step_quad_link_frame, LinkCableTelemetry};
use crate::nds::NdsCore;
use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use zaimule_core::archive::{
    extract_7z_entry_to_file, extract_zip_entry_to_file, list_zip_entries,
};
use zaimule_core::bios::{default_bios_registry, BiosEntry};
use zaimule_core::catalog::{
    compute_crc32, detect_console, extract_nds_game_code, extract_rom_title,
    extract_switch_title_id, format_crc32_hex, inspect_rom_header_and_stats, ConsoleKind,
    CoreEngineKind, GameEntry, SwitchEngineOverride,
};
use zaimule_core::cheats::{
    parse_cheat_code, parse_watchpoint_address, scan_ram_for_exact_byte, AchievementEntry,
    CheatCodeEntry, MemoryWatchpoint,
};
use zaimule_core::external::{
    clear_switch_shader_cache as core_clear_switch_shader_cache,
    import_switch_engine as core_import_switch_engine,
    import_switch_keys as core_import_switch_keys, inspect_switch_vault,
    prepare_switch_launch as core_prepare_switch_launch, resolve_switch_engine_for_game,
    SwitchEngineTarget, SwitchLaunchPlan, SwitchVaultStatus,
};
use zaimule_core::keymap::{apply_turbo_mask, default_keymapper_widgets, KeymapWidget};
use zaimule_core::macros::{
    verify_and_build_ghost_entry, GhostLeaderboardEntry, MacroSequence, TasTimelineRow,
};
use zaimule_core::netplay::{
    discover_stun_candidate, format_deep_link, parse_deep_link, NetplayChatMessage, NetplayRoom,
    NetplayWirePacket, StunDiscoveryCandidate, TournamentBracket, UdpNetplayNode,
};
use zaimule_core::overrides::{resolve_effective_config, EffectiveGameConfig, GlobalSettings};
use zaimule_core::patch::{apply_ips_patch, diff_roms_to_ips, CommunityModPackage, ModLayerEntry};
use zaimule_core::rewind::{rle_compress, rle_decompress, RewindBuffer, SaveStateSlotInfo};
use zaimule_core::scanner::{
    inspect_path_to_entries, scan_directory_for_roms, DirectoryScanReport,
};
use zaimule_core::storage::{
    decode_portable_pack, encode_portable_pack, inspect_sram_hex, write_sram_byte_and_rehash,
    PersistedLibrary, PersistedSaveState, SramHexInspection, ZaimulePortablePack,
    PORTABLE_PACK_MAGIC,
};

pub const VIEWPORT_BINARY_MAGIC: [u8; 4] = *b"ZMFR";
pub const VIEWPORT_BINARY_VERSION: u8 = 1;

/// Read-only hardware engine metadata exposed to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeCoreInfo {
    pub core_id: String,
    pub name: String,
    pub consoles: String,
    pub engine: String,
    pub audio_summary: String,
    pub limitations: Option<String>,
}

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
    pub native_cores: Vec<NativeCoreInfo>,
    pub bios_entries: Vec<BiosEntry>,
    pub link_telemetry: LinkCableTelemetry,
    pub rewind_snapshots_count: usize,
    pub rewind_memory_bytes: usize,
    pub warning: Option<String>,
    #[serde(default)]
    pub rom_directories: Vec<String>,
    #[serde(default)]
    pub switch_vault: SwitchVaultStatus,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ViewportPacketMetadata<'a> {
    console: ConsoleKind,
    cpu: &'a CpuDebugSnapshot,
    link_telemetry: &'a LinkCableTelemetry,
    active_macro_id: Option<&'a str>,
    newly_unlocked_achievement: Option<&'a str>,
}

pub fn built_in_native_cores_info() -> Vec<NativeCoreInfo> {
    vec![
        NativeCoreInfo {
            core_id: "zaimule-rust-gb".into(),
            name: "Game Boy / Game Boy Color Natif".into(),
            consoles: "GB · GBC".into(),
            engine: "termboy-gb (Rust MIT)".into(),
            audio_summary: "APU 4 canaux 48 kHz PCM stéréo + Câble Link".into(),
            limitations: None,
        },
        NativeCoreInfo {
            core_id: "zaimule-rust-gba".into(),
            name: "Game Boy Advance Natif".into(),
            consoles: "GBA".into(),
            engine: "termboy-gba (ARM7TDMI Rust MIT)".into(),
            audio_summary: "PSG + Direct Sound DMA FIFO 48 kHz stéréo".into(),
            limitations: Some("Correction LCD active ; BIOS HLE intégré.".into()),
        },
        NativeCoreInfo {
            core_id: "zaimule-rust-nds".into(),
            name: "Nintendo DS Natif".into(),
            consoles: "NDS".into(),
            engine: "nds_core (x0cero/nds Rust MIT + extensions SPU/3D/2D)".into(),
            audio_summary: "SPU 16 canaux (PCM8/PCM16/ADPCM/PSG/Bruit) 48 kHz stéréo".into(),
            limitations: Some(
                "Stylet tactile inférieur, textures 3D 4×4, Decal/Toon & PPU 2D complet.".into(),
            ),
        },
    ]
}

fn build_core_for(console: ConsoleKind, rom: &[u8]) -> Result<Box<dyn EmulatorBackend>, String> {
    match console {
        ConsoleKind::Gb | ConsoleKind::Gbc => {
            let mut c = GameBoyCore::new_empty();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Gba => {
            let mut c = GbaCore::new_empty();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Nds => {
            let mut c = NdsCore::new_empty();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
        ConsoleKind::Switch | ConsoleKind::Switch2 => Ok(Box::new(GameBoyCore::new_empty())),
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

/// Encodes a [`ViewportStepResponse`] into the zero-JSON-array `"ZMFR"` v1 binary wire format.
pub fn encode_viewport_binary_packet(resp: &ViewportStepResponse, sram_dirty: bool) -> Vec<u8> {
    let mut views: Vec<&FrameStepResult> = Vec::with_capacity(4);
    views.push(&resp.primary);
    if let Some(ref s) = resp.secondary {
        views.push(s);
    }
    if let Some(ref t) = resp.tertiary {
        views.push(t);
    }
    if let Some(ref q) = resp.quaternary {
        views.push(q);
    }

    let meta_json = serde_json::to_vec(&ViewportPacketMetadata {
        console: resp.primary.console,
        cpu: &resp.primary.cpu,
        link_telemetry: &resp.link_telemetry,
        active_macro_id: resp.active_macro_id.as_deref(),
        newly_unlocked_achievement: resp.newly_unlocked_achievement.as_deref(),
    })
    .unwrap_or_else(|_| b"{}".to_vec());

    let rgba_total: usize = views.iter().map(|v| 8 + v.rgba.len()).sum();
    let pcm_bytes_len = resp.primary.audio_samples.len() * 4;
    let total_cap = 35 + rgba_total + 4 + pcm_bytes_len + 4 + meta_json.len();
    let mut out = Vec::with_capacity(total_cap);

    let mut flags = 0u8;
    if resp.secondary.is_some() {
        flags |= 0x01;
    }
    if resp.tertiary.is_some() || resp.quaternary.is_some() {
        flags |= 0x02;
    }
    if sram_dirty {
        flags |= 0x04;
    }
    if resp.active_macro_id.is_some() {
        flags |= 0x08;
    }

    out.extend_from_slice(&VIEWPORT_BINARY_MAGIC);
    out.push(VIEWPORT_BINARY_VERSION);
    out.push(flags);
    out.extend_from_slice(&resp.primary.sample_rate_hz.to_le_bytes());
    out.extend_from_slice(&resp.primary.frame_number.to_le_bytes());
    out.extend_from_slice(&resp.primary.audio_level.to_le_bytes());
    out.extend_from_slice(&resp.primary.audio_freq_hz.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&(resp.rewind_snapshots_count as u32).to_le_bytes());
    out.extend_from_slice(&(resp.rewind_memory_bytes as u32).to_le_bytes());
    out.push(views.len() as u8);

    for v in &views {
        out.extend_from_slice(&v.width.to_le_bytes());
        out.extend_from_slice(&v.height.to_le_bytes());
        out.extend_from_slice(&(v.rgba.len() as u32).to_le_bytes());
        out.extend_from_slice(&v.rgba);
    }

    out.extend_from_slice(&(resp.primary.audio_samples.len() as u32).to_le_bytes());
    for &sample in &resp.primary.audio_samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }

    out.extend_from_slice(&(meta_json.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta_json);
    out
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
    pub system_dir: PathBuf,
    pub save_dir: PathBuf,
    pub rom_cache_dir: PathBuf,
    pub switch_vault_root: PathBuf,
    pub switch_vault: SwitchVaultStatus,
    pub last_scan_report: Option<DirectoryScanReport>,
    pub active_launched_rom_path: Option<PathBuf>,
    pub active_disasm_rom: Vec<u8>,
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

impl EmulatorSession {
    /// Starts a clean Zaimule session with zero demo ROMs, zero fake data, and zero disk auto-scans.
    pub fn new() -> Self {
        let base_tmp = std::env::temp_dir().join("zaimule");
        let switch_vault_root = base_tmp.join("switch");
        let switch_vault = inspect_switch_vault(&switch_vault_root);
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
            system_dir: base_tmp.join("system"),
            save_dir: base_tmp.join("saves"),
            rom_cache_dir: base_tmp.join("rom_cache"),
            switch_vault_root,
            switch_vault,
            last_scan_report: None,
            active_launched_rom_path: None,
            active_disasm_rom: Vec::new(),
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

    pub fn new_clean() -> Self {
        Self::new()
    }

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

    pub fn set_switch_vault_root(&mut self, vault_root: PathBuf) {
        self.switch_vault_root = vault_root;
        self.switch_vault = inspect_switch_vault(&self.switch_vault_root);
    }

    pub fn refresh_switch_vault(&mut self) -> StationBootstrap {
        self.switch_vault = inspect_switch_vault(&self.switch_vault_root);
        self.bootstrap_snapshot()
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
                    switch_engine: SwitchEngineOverride::Auto,
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
            native_cores: built_in_native_cores_info(),
            bios_entries: default_bios_registry(),
            link_telemetry: self.link_telemetry.clone(),
            rewind_snapshots_count: self.rewind_buffer.len(),
            rewind_memory_bytes: self.rewind_buffer.memory_usage_bytes(),
            warning: self.warning.clone(),
            rom_directories: self.settings.rom_directories.clone(),
            switch_vault: self.switch_vault.clone(),
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

    fn read_full_rom_bytes_for_active_ops(&mut self, game_id: &str) -> Result<Vec<u8>, String> {
        if let Some(custom) = self.custom_roms.get(game_id) {
            return Ok(custom.clone());
        }
        let path = self.resolve_rom_path_for_launch(game_id)?;
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.len() > 512 * 1024 * 1024 {
            return Err("ce fichier (> 512 Mo) est trop volumineux pour un patch en RAM".into());
        }
        std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Resolves a launchable on-disk ROM path for `game_id`, extracting compressed `.zip` or `.7z`
    /// entries atomically into `rom_cache_dir`.
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
                    let lower = src_path.to_string_lossy().to_ascii_lowercase();
                    if lower.ends_with(".7z") {
                        extract_7z_entry_to_file(&src_path, entry_name, &cached)?;
                    } else {
                        let entries = list_zip_entries(&src_path)?;
                        let Some(meta) = entries.into_iter().find(|e| e.name == *entry_name) else {
                            return Err(format!(
                                "entrée '{entry_name}' introuvable dans {}",
                                src_path.display()
                            ));
                        };
                        extract_zip_entry_to_file(&src_path, &meta, &cached)?;
                    }
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
                ConsoleKind::Gb => "gb",
                ConsoleKind::Gbc => "gbc",
                ConsoleKind::Switch => "nsp",
                ConsoleKind::Switch2 => "nx",
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

        Err("aucun fichier jeu sur disque trouvé pour cet identifiant".into())
    }

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
        if game.console.is_external_only() {
            self.core_p2 = Box::new(GameBoyCore::new_empty());
            self.core_p3 = Box::new(GameBoyCore::new_empty());
            self.core_p4 = Box::new(GameBoyCore::new_empty());
            return;
        }
        let rom_opt = self.custom_roms.get(&game.id).cloned().or_else(|| {
            if self.active_disasm_rom.len() >= 0x200 {
                self.active_launched_rom_path
                    .as_ref()
                    .and_then(|p| std::fs::read(p).ok())
            } else {
                None
            }
        });
        let Some(rom) = rom_opt else {
            return;
        };

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
                entry.switch_engine = prev.switch_engine;
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
                "Format d'archive non supporté ({}). Utilisez .zip, .7z ou le fichier ROM direct.",
                combined.unsupported_archives[0]
            ));
        }

        Ok(combined)
    }

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
            self.active_disasm_rom.clear();
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

    pub fn to_persisted_library(&self) -> PersistedLibrary {
        PersistedLibrary {
            active_game_id: self.active_game_id.clone(),
            catalog: self.catalog.clone(),
            rom_directories: self.settings.rom_directories.clone(),
        }
    }

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

    /// Imports user-supplied **Torzu** (Switch 1) or **Fritchii / Lichii** (Switch 2) into the Option-A Vault.
    pub fn import_switch_engine(
        &mut self,
        source: &Path,
        target: SwitchEngineTarget,
    ) -> Result<StationBootstrap, String> {
        self.switch_vault = core_import_switch_engine(source, &self.switch_vault_root, target)?;
        Ok(self.bootstrap_snapshot())
    }

    /// Imports user-supplied `prod.keys` / `title.keys` into the Option-A Vault and mirrors to installed engines.
    pub fn import_switch_keys(&mut self, files: &[PathBuf]) -> Result<StationBootstrap, String> {
        self.switch_vault = core_import_switch_keys(files, &self.switch_vault_root)?;
        Ok(self.bootstrap_snapshot())
    }

    /// Sets per-game Switch engine routing (`Auto` -> Torzu for Switch 1 / Fritchii for Switch 2, or explicit override).
    pub fn set_game_switch_engine(
        &mut self,
        game_id: &str,
        mode: SwitchEngineOverride,
    ) -> Result<StationBootstrap, String> {
        let Some(game) = self.catalog.iter_mut().find(|g| g.id == game_id) else {
            return Err(format!("jeu introuvable : {game_id}"));
        };
        if !game.console.is_external_only() {
            return Err("seuls les jeux Switch 1 / Switch 2 utilisent Torzu ou Fritchii".into());
        }
        let target = resolve_switch_engine_for_game(game.console, mode)?;
        game.switch_engine = mode;
        game.core_id = target.id().into();
        Ok(self.bootstrap_snapshot())
    }

    /// Prepares a validated Switch launch plan (`Torzu` or `Fritchii` + synced keys + resolved ROM path).
    pub fn prepare_switch_launch(
        &mut self,
        game_id: Option<&str>,
    ) -> Result<(String, SwitchLaunchPlan), String> {
        let target_id = game_id
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.active_game_id.clone());
        if target_id.is_empty() {
            return Err("sélectionnez d'abord un jeu Switch dans la bibliothèque".into());
        }
        let Some(game) = self.catalog.iter().find(|g| g.id == target_id).cloned() else {
            return Err(format!("jeu introuvable : {target_id}"));
        };
        let target =
            resolve_switch_engine_for_game(game.console, game.switch_engine).map_err(|_| {
                format!(
                    "{} s'exécute directement dans Zaimule (cœur natif Rust)",
                    game.title
                )
            })?;
        let rom_path = self.resolve_rom_path_for_launch(&game.id)?;
        let plan = core_prepare_switch_launch(&self.switch_vault_root, target, &rom_path)?;
        self.switch_vault = inspect_switch_vault(&self.switch_vault_root);
        Ok((game.id, plan))
    }

    /// Clears shader caches for `Torzu` or `Fritchii` inside the Switch Vault and returns freed bytes.
    pub fn clear_switch_shader_cache(
        &mut self,
        target: SwitchEngineTarget,
    ) -> Result<(u64, StationBootstrap), String> {
        let cleared = core_clear_switch_shader_cache(&self.switch_vault_root, target)?;
        self.switch_vault = inspect_switch_vault(&self.switch_vault_root);
        Ok((cleared, self.bootstrap_snapshot()))
    }

    /// Increments playtime for `game_id` after an external Switch session exits.
    pub fn record_switch_playtime(&mut self, game_id: &str, elapsed_seconds: u64) {
        if let Some(game) = self.catalog.iter_mut().find(|g| g.id == game_id) {
            game.playtime_seconds = game.playtime_seconds.saturating_add(elapsed_seconds);
        }
    }

    /// Switches the active game and loads the appropriate pure-Rust backend (`Gb`, `Gbc`, `Gba`, `Nds`)
    /// or enters Switch launcher standby for `Switch` / `Switch2`.
    pub fn select_game(&mut self, game_id: &str) -> Result<StationBootstrap, String> {
        let Some(game) = self.catalog.iter().find(|g| g.id == game_id).cloned() else {
            return Err(format!("jeu introuvable dans la bibliothèque : {game_id}"));
        };
        self.active_game_id = game.id.clone();
        self.active_launched_rom_path = game.rom_path.as_ref().map(PathBuf::from);
        self.active_disasm_rom.clear();
        self.rewind_buffer.clear();
        self.sram_dirty = false;
        self.core_loaded = false;

        self.core_p1 = Box::new(GameBoyCore::new_empty());
        self.core_p2 = Box::new(GameBoyCore::new_empty());
        self.core_p3 = Box::new(GameBoyCore::new_empty());
        self.core_p4 = Box::new(GameBoyCore::new_empty());

        if game.console.is_external_only() || game.engine_kind == CoreEngineKind::ExternalLauncher {
            return Ok(self.bootstrap_snapshot());
        }

        let rom_bytes = if let Some(custom) = self.custom_roms.get(&game.id) {
            custom.clone()
        } else if game.rom_path.is_some() {
            let launch_rom = self.resolve_rom_path_for_launch(&game.id)?;
            let meta = std::fs::metadata(&launch_rom)
                .map_err(|e| format!("{}: {e}", launch_rom.display()))?;
            if meta.len() > 512 * 1024 * 1024 {
                return Err("ROM trop volumineuse (> 512 Mo) pour le cœur natif".into());
            }
            std::fs::read(&launch_rom).map_err(|e| format!("{}: {e}", launch_rom.display()))?
        } else {
            return Err("aucune donnée ROM disponible pour ce jeu".into());
        };

        self.core_p1 = build_core_for(game.console, &rom_bytes)?;
        let cap = rom_bytes.len().min(4 * 1024 * 1024);
        self.active_disasm_rom = rom_bytes[..cap].to_vec();
        self.core_loaded = true;
        self.warning = None;
        self.sync_secondary_link_cores();
        Ok(self.bootstrap_snapshot())
    }

    /// Imports raw ROM bytes, identifies the console, and selects/loads the game.
    pub fn import_custom_rom(
        &mut self,
        file_name: &str,
        rom_bytes: &[u8],
    ) -> Result<StationBootstrap, String> {
        if rom_bytes.is_empty() {
            return Err("le fichier ROM est vide".into());
        }
        let header = &rom_bytes[..rom_bytes.len().min(64 * 1024)];
        let console = detect_console(file_name, header).ok_or_else(|| {
            format!("Format non supporté pour '{file_name}'. Formats acceptés : .gb, .gbc, .gba, .nds, .nsp, .xci, .nsz, .xcz, .nro, .nso, .nx, .nup, .zip, .7z")
        })?;
        let title = extract_rom_title(file_name, console, header);
        let crc = compute_crc32(rom_bytes);
        let crc_hex = format_crc32_hex(crc);

        let inspected =
            inspect_rom_header_and_stats(file_name, header, rom_bytes.len() as u64, crc, false)?;
        let title_id = match console {
            ConsoleKind::Switch | ConsoleKind::Switch2 => extract_switch_title_id(file_name),
            ConsoleKind::Nds => extract_nds_game_code(header),
            _ => None,
        };
        let (engine_kind, core_id) = match console {
            ConsoleKind::Switch => (CoreEngineKind::ExternalLauncher, "torzu".into()),
            ConsoleKind::Switch2 => (CoreEngineKind::ExternalLauncher, "fritchii".into()),
            other => (CoreEngineKind::NativeRust, other.default_core_id().into()),
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
                    switch_engine: SwitchEngineOverride::Auto,
                    core_id,
                    region: inspected.region,
                    crc32_hex: crc_hex,
                    size_bytes: rom_bytes.len() as u64,
                    sram_summary: match console {
                        ConsoleKind::Switch => "Sauvegarde Torzu (Switch 1)".into(),
                        ConsoleKind::Switch2 => "Sauvegarde Fritchii (Switch 2)".into(),
                        ConsoleKind::Nds => "512 Ko Flash Backup DS".into(),
                        ConsoleKind::Gba => "SRAM / Flash / EEPROM GBA".into(),
                        ConsoleKind::Gb | ConsoleKind::Gbc => "SRAM Batterie MBC (.sav)".into(),
                    },
                    playtime_seconds: 0,
                    favorite: true,
                    link_cable_ready: console.supports_link_cable(),
                    embedded_rom_key: None,
                    cover_accent: match console {
                        ConsoleKind::Switch => "#4A181D".into(),
                        ConsoleKind::Switch2 => "#0D9488".into(),
                        ConsoleKind::Nds => "#0F766E".into(),
                        ConsoleKind::Gba => "#283655".into(),
                        _ => "#3B5998".into(),
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

    pub fn apply_custom_ips_to_active_game(
        &mut self,
        patch_name: &str,
        ips_bytes: &[u8],
    ) -> Result<StationBootstrap, String> {
        if self.active_game_id.is_empty() {
            return Err("aucune ROM chargée dans la bibliothèque".into());
        }
        let game = self.active_game().clone();
        if game.console.is_external_only() {
            return Err("les patchs IPS ne s'appliquent pas aux images Switch".into());
        }
        let base_rom = self.read_full_rom_bytes_for_active_ops(&game.id)?;
        let patched_rom = apply_ips_patch(&base_rom, ips_bytes)?;
        let new_crc = format_crc32_hex(compute_crc32(&patched_rom));

        self.custom_roms
            .insert(game.id.clone(), patched_rom.clone());
        self.core_p1.load_rom(&patched_rom)?;
        self.active_disasm_rom = patched_rom[..patched_rom.len().min(4 * 1024 * 1024)].to_vec();
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
                    "Patch IPS appliqué en mémoire sur {} ({} octets).",
                    game.title,
                    ips_bytes.len()
                ),
            },
        );
        Ok(self.bootstrap_snapshot())
    }

    pub fn add_cheat_code(
        &mut self,
        raw_code: &str,
        label: String,
    ) -> Result<StationBootstrap, String> {
        let entry = parse_cheat_code(raw_code, &label)?;
        self.cheat_codes.retain(|c| c.id != entry.id);
        self.cheat_codes.insert(0, entry);
        Ok(self.bootstrap_snapshot())
    }

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

    pub fn scan_active_ram(&self, target_value: u8) -> Vec<String> {
        let slice = self.core_p1.read_ram_slice();
        scan_ram_for_exact_byte(slice, 0xC000, target_value, 24)
            .into_iter()
            .map(|addr| format!("0x{addr:04X}"))
            .collect()
    }

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

    pub fn step_viewport(
        &mut self,
        raw_buttons_p1: u16,
        raw_buttons_p2: u16,
        turbo_active: bool,
    ) -> ViewportStepResponse {
        self.step_viewport_with_speed(raw_buttons_p1, raw_buttons_p2, turbo_active, 1)
    }

    /// Steps the active session `speed_mult` (`1..=8`) frames and returns the final visual frame
    /// plus rate-matched stereo PCM samples.
    pub fn step_viewport_with_speed(
        &mut self,
        raw_buttons_p1: u16,
        raw_buttons_p2: u16,
        turbo_active: bool,
        speed_mult: u8,
    ) -> ViewportStepResponse {
        let steps = usize::from(speed_mult.clamp(1, 8));
        let mut last_resp: Option<ViewportStepResponse> = None;
        let mut merged_pcm: Vec<f32> = Vec::new();

        for s in 0..steps {
            let mut single =
                self.step_single_viewport_tick(raw_buttons_p1, raw_buttons_p2, turbo_active);
            if steps > 1 {
                let pcm = std::mem::take(&mut single.primary.audio_samples);
                for (pair_idx, pair) in pcm.as_chunks::<2>().0.iter().enumerate() {
                    if pair_idx % steps == 0 {
                        merged_pcm.push(pair[0]);
                        merged_pcm.push(pair[1]);
                    }
                }
            } else {
                merged_pcm = std::mem::take(&mut single.primary.audio_samples);
            }
            if s + 1 == steps {
                single.primary.audio_samples = std::mem::take(&mut merged_pcm);
                last_resp = Some(single);
            }
        }

        last_resp.unwrap_or_else(|| {
            self.step_single_viewport_tick(raw_buttons_p1, raw_buttons_p2, turbo_active)
        })
    }

    /// Executes `step_viewport_with_speed` and serializes directly to `"ZMFR"` v1 binary bytes.
    pub fn step_viewport_binary_bytes(
        &mut self,
        raw_buttons_p1: u16,
        raw_buttons_p2: u16,
        turbo_active: bool,
        speed_mult: u8,
    ) -> Vec<u8> {
        let resp =
            self.step_viewport_with_speed(raw_buttons_p1, raw_buttons_p2, turbo_active, speed_mult);
        encode_viewport_binary_packet(&resp, self.sram_dirty)
    }

    fn step_single_viewport_tick(
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

        for cheat in &self.cheat_codes {
            if cheat.enabled {
                self.core_p1.apply_ram_cheat(cheat.addr, cheat.value);
            }
        }

        for wp in &self.memory_watchpoints {
            if wp.frozen {
                self.core_p1.apply_ram_cheat(wp.addr, wp.frozen_value);
            }
        }

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
                    let idx = (wp.addr as usize).wrapping_sub(0xC000) % ram_slice.len();
                    wp.current_value = ram_slice[idx];
                }
            }
        }

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

        if self.core_loaded && (primary.frame_number & 15) == 0 {
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

    pub fn set_dual_link_enabled(&mut self, enabled: bool) -> StationBootstrap {
        self.link_telemetry.enabled = enabled;
        if !enabled {
            self.link_telemetry.quad_mode = false;
        }
        self.sync_secondary_link_cores();
        self.bootstrap_snapshot()
    }

    pub fn set_quad_link_mode(&mut self, quad_enabled: bool) -> StationBootstrap {
        self.link_telemetry.quad_mode = quad_enabled;
        if quad_enabled {
            self.link_telemetry.enabled = true;
        }
        self.sync_secondary_link_cores();
        self.bootstrap_snapshot()
    }

    pub fn inspect_vram_and_disassembly(&self) -> DebuggerPayload {
        if self.active_game_id.is_empty() || !self.core_loaded {
            return DebuggerPayload {
                cpu: self.core_p1.debug_snapshot(),
                disassembly: Vec::new(),
                breakpoints: self.breakpoints.clone(),
                vram: build_vram_inspector_snapshot(self.core_p1.read_vram_slice()),
            };
        }
        let game = self.active_game();
        let cpu = self.core_p1.debug_snapshot();
        let pc = u32::from_str_radix(cpu.pc_hex.trim_start_matches("0x"), 16).unwrap_or(0x0100);
        let disassembly = disassemble_window(
            game.console,
            &self.active_disasm_rom,
            pc,
            &self.breakpoints,
            10,
        );
        let vram = build_vram_inspector_snapshot(self.core_p1.read_vram_slice());
        DebuggerPayload {
            cpu,
            disassembly,
            breakpoints: self.breakpoints.clone(),
            vram,
        }
    }

    pub fn toggle_breakpoint(&mut self, addr: u32) -> DebuggerPayload {
        if let Some(pos) = self.breakpoints.iter().position(|&a| a == addr) {
            self.breakpoints.remove(pos);
        } else {
            self.breakpoints.push(addr);
        }
        self.inspect_vram_and_disassembly()
    }

    pub fn create_save_state(
        &mut self,
        slot: u8,
        label: String,
        now_ms: u64,
    ) -> Result<SaveStateSlotInfo, String> {
        if self.active_game_id.is_empty() || !self.core_loaded {
            return Err("aucun cœur natif actif pour créer un Save State".into());
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

    pub fn rewind(&mut self, steps_back: usize) -> Result<u64, String> {
        let Some((frame, raw_state)) = self.rewind_buffer.rewind_steps(steps_back)? else {
            return Err("tampon de rembobinage vide".into());
        };
        self.core_p1.load_state(&raw_state)?;
        Ok(frame)
    }

    pub fn has_persistable_sram(&self) -> bool {
        if !self.core_loaded || self.active_game_id.is_empty() {
            return false;
        }
        let game = self.active_game();
        if game.console.is_external_only() || game.engine_kind == CoreEngineKind::ExternalLauncher {
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

    pub fn export_active_ips_patch(&mut self) -> Result<Vec<u8>, String> {
        if self.active_game_id.is_empty() {
            return Err("aucune ROM chargée pour exporter un patch .IPS".into());
        }
        let game = self.active_game().clone();
        if game.console.is_external_only() {
            return Err("export IPS non disponible pour les images Switch".into());
        }
        let modified = self.read_full_rom_bytes_for_active_ops(&game.id)?;
        let original = if let Some(ref raw_path) = game.rom_path {
            let p = Path::new(raw_path);
            if game.archive_entry.is_none() && p.is_file() {
                std::fs::read(p).unwrap_or_else(|_| modified.clone())
            } else {
                modified.clone()
            }
        } else {
            modified.clone()
        };
        diff_roms_to_ips(&original, &modified)
    }

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
            version: "1.3.0".into(),
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

    pub fn set_nds_stylus(&mut self, x: u8, y: u8, pressed: bool) {
        self.core_p1.set_touch_stylus(x, y, pressed);
    }

    pub fn paint_vram_atlas_pixel(
        &mut self,
        atlas_x: u16,
        atlas_y: u16,
        color_idx: u8,
    ) -> DebuggerPayload {
        if self.active_game_id.is_empty() || !self.core_loaded {
            return self.inspect_vram_and_disassembly();
        }
        if let Some(edits) =
            compute_2bpp_pixel_mutation(self.core_p1.read_vram_slice(), atlas_x, atlas_y, color_idx)
        {
            for (addr, val) in edits {
                self.core_p1.write_vram_byte(addr, val);
            }
        }
        self.inspect_vram_and_disassembly()
    }

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
        self.core_p1.set_color_correction(pgxp_subpixel);
        self.bootstrap_snapshot()
    }

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
        self.sram_dirty = true;
        Ok(self.bootstrap_snapshot())
    }

    pub fn transfer_sram_p1_to_p2(&mut self) -> Result<StationBootstrap, String> {
        if !self.has_persistable_sram() {
            return Err("aucune sauvegarde SRAM active à transférer".into());
        }
        let p1_sram = self.core_p1.sram().to_vec();
        self.core_p2.load_sram(&p1_sram)?;
        self.link_telemetry.packets_transferred =
            self.link_telemetry.packets_transferred.saturating_add(64);
        Ok(self.bootstrap_snapshot())
    }

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
            ram[(addr as usize).wrapping_sub(0xC000) % ram.len()]
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
    use zaimule_core::archive::build_stored_zip_for_test;
    use zaimule_core::keymap::{BTN_A, BTN_RIGHT};
    use zaimule_core::patch::build_ips_patch;

    fn test_gbc_battery_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x0134..0x0140].copy_from_slice(b"ZAIMULE_TEST");
        rom[0x0143] = 0x80;
        rom[0x0147] = 0x03; // MBC1 + RAM + BATTERY
        rom[0x0149] = 0x03; // 32 KiB SRAM
        rom[0x0100] = 0xC3;
        rom[0x0101] = 0x50;
        rom[0x0102] = 0x01;
        let code: &[u8] = &[
            0x3E, 0x80, 0xE0, 0x26, // APU ON
            0x3E, 0x77, 0xE0, 0x24, // NR50
            0x3E, 0x22, 0xE0, 0x25, // NR51
            0x3E, 0xF0, 0xE0, 0x17, // NR22
            0x3E, 0x87, 0xE0, 0x19, // NR24 trigger
            0x18, 0xFE, // spin
        ];
        rom[0x0150..0x0150 + code.len()].copy_from_slice(code);
        rom
    }

    fn test_gba_sram_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x200];
        rom[0..4].copy_from_slice(&0xEA00_002Eu32.to_le_bytes());
        rom[0xB2] = 0x96;
        rom[0x140..0x149].copy_from_slice(b"SRAM_V110");
        let prog: &[u32] = &[0xE3A0_0405, 0xE3A0_101F, 0xE1C0_10B0, 0xEAFF_FFFE];
        for (i, w) in prog.iter().enumerate() {
            let p = 0xC0 + i * 4;
            rom[p..p + 4].copy_from_slice(&w.to_le_bytes());
        }
        rom
    }

    fn test_nds_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x1000];
        rom[0x0000..0x000C].copy_from_slice(b"ZAIMULENDS\0\0");
        rom[0x000C..0x0010].copy_from_slice(b"ZMLN");
        rom[0x20..0x24].copy_from_slice(&0x200u32.to_le_bytes());
        rom[0x24..0x28].copy_from_slice(&0x0200_0800u32.to_le_bytes());
        rom[0x28..0x2C].copy_from_slice(&0x0200_0800u32.to_le_bytes());
        rom[0x2C..0x30].copy_from_slice(&0x10u32.to_le_bytes());
        rom[0x30..0x34].copy_from_slice(&0x400u32.to_le_bytes());
        rom[0x34..0x38].copy_from_slice(&0x0380_0000u32.to_le_bytes());
        rom[0x38..0x3C].copy_from_slice(&0x0380_0000u32.to_le_bytes());
        rom[0x3C..0x40].copy_from_slice(&0x10u32.to_le_bytes());
        rom[0x200..0x204].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        rom[0x400..0x404].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        rom
    }

    #[test]
    fn clean_session_starts_empty_and_runs_real_gbc_savestate_rewind_and_binary_packet() {
        let mut session = EmulatorSession::new();
        let snap = session.bootstrap_snapshot();
        assert!(snap.active_game_id.is_empty());
        assert!(snap.catalog.is_empty());
        assert_eq!(snap.native_cores.len(), 3);
        assert!(session.create_save_state(1, "Empty".into(), 1).is_err());

        let rom = test_gbc_battery_rom();
        let loaded = session.import_custom_rom("arena.gbc", &rom).unwrap();
        assert_eq!(loaded.catalog.len(), 1);
        assert_eq!(loaded.catalog[0].console, ConsoleKind::Gbc);

        for _ in 0..32 {
            session.step_viewport(BTN_RIGHT, 0, true);
        }
        assert!(session.rewind_buffer.len() >= 2);
        let saved = session
            .create_save_state(1, "Boss Room".into(), 123_456)
            .unwrap();
        assert!(saved.occupied);
        assert!(session.restore_save_state(1).is_ok());
        assert!(session.rewind(1).is_ok());

        // Verify `"ZMFR"` v1 binary frame + PCM packet framing
        let pkt = session.step_viewport_binary_bytes(BTN_A, 0, false, 1);
        assert_eq!(&pkt[0..4], b"ZMFR");
        assert_eq!(pkt[4], 1);
        let sample_rate = u32::from_le_bytes([pkt[6], pkt[7], pkt[8], pkt[9]]);
        assert_eq!(sample_rate, 48_000);
        let view_count = pkt[34];
        assert_eq!(view_count, 1);
        let w = u16::from_le_bytes([pkt[35], pkt[36]]);
        let h = u16::from_le_bytes([pkt[37], pkt[38]]);
        let rgba_len = u32::from_le_bytes([pkt[39], pkt[40], pkt[41], pkt[42]]) as usize;
        assert_eq!((w, h), (160, 144));
        assert_eq!(rgba_len, 160 * 144 * 4);
        let pcm_off = 43 + rgba_len;
        let float_count = u32::from_le_bytes([
            pkt[pcm_off],
            pkt[pcm_off + 1],
            pkt[pcm_off + 2],
            pkt[pcm_off + 3],
        ]) as usize;
        assert!(float_count > 0);
        assert_eq!(float_count % 2, 0);
        let meta_off = pcm_off + 4 + float_count * 4;
        let meta_len = u32::from_le_bytes([
            pkt[meta_off],
            pkt[meta_off + 1],
            pkt[meta_off + 2],
            pkt[meta_off + 3],
        ]) as usize;
        let meta_json: serde_json::Value =
            serde_json::from_slice(&pkt[meta_off + 4..meta_off + 4 + meta_len]).unwrap();
        assert_eq!(meta_json["console"], "GBC");
    }

    #[test]
    fn session_handles_gba_nds_switch_option_a_vault_zip_and_7z() {
        use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
        use std::fs;

        let root = std::env::temp_dir().join(format!("zaimule-sess-v2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let games = root.join("games");
        let cache = root.join("cache");
        let vault = root.join("vault");
        fs::create_dir_all(&games).unwrap();

        // 1. Write GBA, zipped NDS, 7z GBC, Switch 1 NSP, Switch 2 NX
        let gba_path = games.join("GoldenHorizon.gba");
        fs::write(&gba_path, test_gba_sram_rom()).unwrap();

        let nds_zip = games.join("Platine.zip");
        let nds_rom = test_nds_rom();
        fs::write(
            &nds_zip,
            build_stored_zip_for_test(&[("Platine (FR).nds", &nds_rom)]),
        )
        .unwrap();

        let gbc_7z = games.join("Arena.7z");
        let gbc_rom = test_gbc_battery_rom();
        {
            let mut sz = ArchiveWriter::create(&gbc_7z).unwrap();
            sz.push_archive_entry(
                ArchiveEntry::new_file("Arena.gbc"),
                Some(gbc_rom.as_slice()),
            )
            .unwrap();
            sz.finish().unwrap();
        }

        let mut nsp = vec![0u8; 4096];
        nsp[0..4].copy_from_slice(b"PFS0");
        let sw1_path = games.join("Wonder [0100152000022000].nsp");
        fs::write(&sw1_path, &nsp).unwrap();

        let mut nx = vec![0u8; 4096];
        nx[0..4].copy_from_slice(b"NX2\0");
        let sw2_path = games.join("MarioKart9 [0200999000011000].nx");
        fs::write(&sw2_path, &nx).unwrap();

        let mut session = EmulatorSession::new();
        session.configure_paths(root.join("sys"), root.join("sav"), cache);
        session.set_switch_vault_root(vault);

        let report = session.scan_directory_and_merge(&games);
        assert_eq!(report.imported_entries.len(), 5);

        // GBA native step + IPS + portable pack + debugger
        let gba_id = session
            .catalog
            .iter()
            .find(|g| g.console == ConsoleKind::Gba)
            .unwrap()
            .id
            .clone();
        session.select_game(&gba_id).unwrap();
        let gba_step = session.step_viewport(BTN_RIGHT, 0, false);
        assert_eq!(
            (gba_step.primary.width, gba_step.primary.height),
            (240, 160)
        );
        assert!(!gba_step.primary.audio_samples.is_empty());
        let patch = build_ips_patch(0x134, b"PATCHED").unwrap();
        assert!(session
            .apply_custom_ips_to_active_game("test.ips", &patch)
            .is_ok());
        assert_eq!(session.inspect_vram_and_disassembly().disassembly.len(), 10);

        // Zipped NDS loads into real nds_core cleanly!
        let nds_id = session
            .catalog
            .iter()
            .find(|g| g.console == ConsoleKind::Nds)
            .unwrap()
            .id
            .clone();
        session.select_game(&nds_id).unwrap();
        session.set_nds_stylus(128, 96, true);
        let nds_step = session.step_viewport(BTN_A, 0, false);
        assert_eq!(
            (nds_step.primary.width, nds_step.primary.height),
            (256, 384)
        );
        assert!(!nds_step.primary.audio_samples.is_empty());
        assert_eq!(nds_step.primary.audio_samples.len() % 2, 0);
        assert!(nds_step.primary.cpu.arch.contains("SPU 16-ch"));

        // 7z GBC extracts atomically on select and runs termboy-gb!
        let gbc_id = session
            .catalog
            .iter()
            .find(|g| g.console == ConsoleKind::Gbc)
            .unwrap()
            .id
            .clone();
        session.select_game(&gbc_id).unwrap();
        let gbc_step = session.step_viewport_with_speed(0, 0, false, 2);
        assert_eq!(
            (gbc_step.primary.width, gbc_step.primary.height),
            (160, 144)
        );
        assert!(!gbc_step.primary.audio_samples.is_empty());

        // Switch 1 (Torzu) & Switch 2 (Fritchii) Option-A Vault workflow
        let sw1_id = session
            .catalog
            .iter()
            .find(|g| g.console == ConsoleKind::Switch)
            .unwrap()
            .id
            .clone();
        let sw2_id = session
            .catalog
            .iter()
            .find(|g| g.console == ConsoleKind::Switch2)
            .unwrap()
            .id
            .clone();

        let torzu_src = root.join("Torzu");
        let fritchii_src = root.join("Fritchii");
        let keys_src = root.join("keys");
        fs::create_dir_all(&torzu_src).unwrap();
        fs::create_dir_all(&fritchii_src).unwrap();
        fs::create_dir_all(&keys_src).unwrap();
        fs::write(torzu_src.join("torzu.exe"), b"MZ_TORZU").unwrap();
        fs::write(fritchii_src.join("fritchii.exe"), b"MZ_FRITCHII").unwrap();
        fs::write(keys_src.join("prod.keys"), b"key = 00").unwrap();

        session
            .import_switch_engine(&torzu_src, SwitchEngineTarget::Torzu)
            .unwrap();
        session
            .import_switch_engine(&fritchii_src, SwitchEngineTarget::Fritchii)
            .unwrap();
        session
            .import_switch_keys(&[keys_src.join("prod.keys")])
            .unwrap();

        let (_, plan1) = session.prepare_switch_launch(Some(&sw1_id)).unwrap();
        assert_eq!(plan1.target, SwitchEngineTarget::Torzu);

        let (_, plan2) = session.prepare_switch_launch(Some(&sw2_id)).unwrap();
        assert_eq!(plan2.target, SwitchEngineTarget::Fritchii);

        session
            .set_game_switch_engine(&sw1_id, SwitchEngineOverride::Fritchii)
            .unwrap();
        let (_, plan1_override) = session.prepare_switch_launch(Some(&sw1_id)).unwrap();
        assert_eq!(plan1_override.target, SwitchEngineTarget::Fritchii);

        let _ = fs::remove_dir_all(&root);
    }
}
