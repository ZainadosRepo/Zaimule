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
use crate::nds_core::NdsBiosAssets;
use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use zaimule_core::archive::{
    extract_7z_entry_to_file, extract_zip_entry_to_file, list_zip_entries,
};
use zaimule_core::bios::{import_bios_from_paths, scan_bios_registry, BiosEntry, BiosImportReport};
use zaimule_core::catalog::{
    compute_crc32, detect_console, extract_nds_game_code, extract_rom_title,
    extract_switch_title_id, format_crc32_hex, inspect_rom_header_and_stats, ConsoleKind,
    CoreEngineKind, GameEntry, SwitchEngineOverride,
};
use zaimule_core::cheats::{
    parse_cheat_code, parse_watchpoint_address, scan_ram_for_exact_byte, AchievementEntry,
    CheatCodeEntry, CheatKind, MemoryWatchpoint,
};
use zaimule_core::external::{
    clear_switch_shader_cache as core_clear_switch_shader_cache,
    import_switch_engine as core_import_switch_engine,
    import_switch_firmware as core_import_switch_firmware,
    import_switch_keys as core_import_switch_keys, inspect_switch_vault,
    prepare_switch_launch as core_prepare_switch_launch, resolve_switch_engine_for_game,
    SwitchEngineTarget, SwitchLaunchPlan, SwitchVaultStatus,
};
use zaimule_core::keymap::{apply_turbo_mask, default_keymapper_widgets, KeymapWidget};
use zaimule_core::macros::{
    verify_and_build_ghost_entry, GhostLeaderboardEntry, MacroSequence, TasTimelineRow,
};
use zaimule_core::metadata::{normalize_collection_name, GameMetadataUpdate};
use zaimule_core::netplay::{
    discover_stun_candidate, format_deep_link, parse_deep_link, validate_relay_url, GbLinkPayload,
    GbaLinkPayload, NdsWifiMode, NdsWifiPacket, NdsWifiTelemetry, NetplayChatMessage, NetplayRoom,
    NetplayWirePacket, StunDiscoveryCandidate, TournamentBracket, UdpNetplayNode,
    GBA_LINK_FLAG_MASTER_TX, GBA_LINK_FLAG_SLAVE_REPLY, GB_LINK_FLAG_MASTER_TX,
    GB_LINK_FLAG_SLAVE_REPLY,
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
    #[serde(default)]
    pub collections: Vec<String>,
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
    #[serde(default)]
    pub udp_packets_sent: u64,
    #[serde(default)]
    pub udp_packets_received: u64,
    pub native_cores: Vec<NativeCoreInfo>,
    pub bios_entries: Vec<BiosEntry>,
    pub link_telemetry: LinkCableTelemetry,
    pub rewind_snapshots_count: usize,
    pub rewind_memory_bytes: usize,
    pub warning: Option<String>,
    #[serde(default)]
    pub rom_directories: Vec<String>,
    #[serde(default)]
    pub auto_rescan_rom_dirs_on_startup: bool,
    #[serde(default)]
    pub cloud_sync_dir: Option<String>,
    #[serde(default)]
    pub switch_vault: SwitchVaultStatus,
    #[serde(default)]
    pub last_scan_report: Option<DirectoryScanReport>,
    #[serde(default)]
    pub active_shader_css: String,
    #[serde(default)]
    pub upscaler_mode: String,
    #[serde(default)]
    pub speed_multiplier: u8,
    #[serde(default)]
    pub nds_layout: String,
    #[serde(default)]
    pub nds_3d_scale: u8,
    #[serde(default)]
    pub viewport_integer_scale: bool,
    #[serde(default)]
    pub rewind_enabled: bool,
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub relay_key_configured: bool,
    #[serde(default)]
    pub relay_player_name: Option<String>,
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

fn build_core_for(
    console: ConsoleKind,
    rom: &[u8],
    system_dir: &Path,
) -> Result<Box<dyn EmulatorBackend>, String> {
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
            let bios = NdsBiosAssets::load_from_dir(system_dir);
            c.load_rom_with_bios(rom, bios)?;
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
    encode_viewport_binary_packet_with_meta(resp, sram_dirty, true)
}

/// Encodes a [`ViewportStepResponse`] into `"ZMFR"` v1 binary bytes, optionally omitting
/// the JSON debug metadata trailer when no telemetry/achievement/link update is needed.
pub fn encode_viewport_binary_packet_with_meta(
    resp: &ViewportStepResponse,
    sram_dirty: bool,
    include_debug_meta: bool,
) -> Vec<u8> {
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

    let emit_meta = include_debug_meta
        || resp.newly_unlocked_achievement.is_some()
        || resp.link_telemetry.enabled
        || resp.active_macro_id.is_some();

    let meta_json = if emit_meta {
        serde_json::to_vec(&ViewportPacketMetadata {
            console: resp.primary.console,
            cpu: &resp.primary.cpu,
            link_telemetry: &resp.link_telemetry,
            active_macro_id: resp.active_macro_id.as_deref(),
            newly_unlocked_achievement: resp.newly_unlocked_achievement.as_deref(),
        })
        .unwrap_or_else(|_| b"{}".to_vec())
    } else {
        Vec::new()
    };

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

fn native_fps_for_console(console: ConsoleKind) -> f64 {
    match console {
        ConsoleKind::Gb | ConsoleKind::Gbc | ConsoleKind::Gba => 59.7275,
        ConsoleKind::Nds => 59.8261,
        ConsoleKind::Switch | ConsoleKind::Switch2 => 60.0,
    }
}

fn normalize_nds_layout(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "horizontal" => "horizontal".into(),
        "hybrid" => "hybrid".into(),
        "vertical-swapped" => "vertical-swapped".into(),
        "horizontal-swapped" => "horizontal-swapped".into(),
        _ => "vertical".into(),
    }
}

/// Per-tick options controlling `"ZMFR"` binary viewport serialization and smart frameskip.
#[derive(Debug, Clone, Copy)]
pub struct BinaryStepOptions {
    pub include_debug_meta: bool,
    pub prefer_rgb565: bool,
    pub skip_render: bool,
}

impl Default for BinaryStepOptions {
    fn default() -> Self {
        Self {
            include_debug_meta: true,
            prefer_rgb565: false,
            skip_render: false,
        }
    }
}

struct RewindJob {
    generation: u64,
    frame_number: u64,
    raw: Vec<u8>,
}

struct RewindResult {
    generation: u64,
    frame_number: u64,
    encoded: Vec<u8>,
}

/// Persistent background thread compressing ~6 MiB NDS rewind snapshots (`RLE + CRC32`)
/// off the emulation frame loop so periodic rewind captures never stall audio or video.
struct BackgroundRewindWorker {
    tx: std::sync::mpsc::SyncSender<RewindJob>,
    rx: std::sync::mpsc::Receiver<RewindResult>,
}

impl BackgroundRewindWorker {
    fn spawn() -> Option<Self> {
        let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<RewindJob>(1);
        let (res_tx, res_rx) = std::sync::mpsc::channel::<RewindResult>();
        let spawned = std::thread::Builder::new()
            .name("zaimule-rewind".into())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    if let Ok(encoded) = crate::nds_core::state::compress_raw_zmnd(&job.raw) {
                        if res_tx
                            .send(RewindResult {
                                generation: job.generation,
                                frame_number: job.frame_number,
                                encoded,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            });
        spawned.ok().map(|_| Self {
            tx: job_tx,
            rx: res_rx,
        })
    }
}

pub struct EmulatorSession {
    pub settings: GlobalSettings,
    pub catalog: Vec<GameEntry>,
    pub collections: Vec<String>,
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
    bios_entries_cache: Vec<BiosEntry>,
    pcm_scratch: Vec<f32>,
    merged_pcm_scratch: Vec<f32>,
    rgb565_scratch: Vec<u8>,
    pub last_scan_report: Option<DirectoryScanReport>,
    pub active_launched_rom_path: Option<PathBuf>,
    pub active_disasm_rom: Vec<u8>,
    pub link_telemetry: LinkCableTelemetry,
    pub online_link_enabled: bool,
    pub online_gba_player_id: u8,
    online_gba_last_master_word: u16,
    online_link_seq: u16,
    online_link_rx_queue: VecDeque<GbLinkPayload>,
    online_link_tx_queue: VecDeque<GbLinkPayload>,
    online_gba_link_rx_queue: VecDeque<GbaLinkPayload>,
    online_gba_link_tx_queue: VecDeque<GbaLinkPayload>,
    pub nds_wifi_mode: NdsWifiMode,
    pub nds_wifi_peer_id: u8,
    pub nds_wifi_wfc_dns: Option<[u8; 4]>,
    online_nds_wifi_rx_queue: VecDeque<NdsWifiPacket>,
    online_nds_wifi_tx_queue: VecDeque<NdsWifiPacket>,
    nds_wfc_external_ip_rx: VecDeque<Vec<u8>>,
    nds_wfc_external_ip_tx: VecDeque<Vec<u8>>,
    pub rewind_buffer: RewindBuffer,
    rewind_worker: Option<BackgroundRewindWorker>,
    rewind_generation: u64,
    pub save_states_ram: HashMap<(String, u8), (SaveStateSlotInfo, Vec<u8>)>,
    pub custom_roms: HashMap<String, Vec<u8>>,
    pub breakpoints: Vec<u32>,
    pub last_score: u32,
    pub recent_inputs_trace: Vec<u8>,
    pub frame_counter: u64,
    playtime_frame_accum: f64,
    pub playtime_dirty: bool,
    last_sram_crc: Option<u32>,
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
        let system_dir = base_tmp.join("system");
        let switch_vault_root = base_tmp.join("switch");
        let switch_vault = inspect_switch_vault(&switch_vault_root);
        let bios_entries_cache = scan_bios_registry(&system_dir);
        Self {
            settings: GlobalSettings::default(),
            catalog: Vec::new(),
            collections: Vec::new(),
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
            system_dir,
            save_dir: base_tmp.join("saves"),
            rom_cache_dir: base_tmp.join("rom_cache"),
            switch_vault_root,
            switch_vault,
            bios_entries_cache,
            pcm_scratch: Vec::with_capacity(2048),
            merged_pcm_scratch: Vec::with_capacity(2048),
            rgb565_scratch: Vec::new(),
            last_scan_report: None,
            active_launched_rom_path: None,
            active_disasm_rom: Vec::new(),
            link_telemetry: LinkCableTelemetry::default(),
            online_link_enabled: false,
            online_gba_player_id: 0,
            online_gba_last_master_word: 0xFFFF,
            online_link_seq: 0,
            online_link_rx_queue: VecDeque::with_capacity(32),
            online_link_tx_queue: VecDeque::with_capacity(32),
            online_gba_link_rx_queue: VecDeque::with_capacity(32),
            online_gba_link_tx_queue: VecDeque::with_capacity(32),
            nds_wifi_mode: NdsWifiMode::Off,
            nds_wifi_peer_id: 1,
            nds_wifi_wfc_dns: None,
            online_nds_wifi_rx_queue: VecDeque::with_capacity(32),
            online_nds_wifi_tx_queue: VecDeque::with_capacity(32),
            nds_wfc_external_ip_rx: VecDeque::with_capacity(32),
            nds_wfc_external_ip_tx: VecDeque::with_capacity(32),
            rewind_buffer: RewindBuffer::new(240),
            rewind_worker: None,
            rewind_generation: 0,
            save_states_ram: HashMap::new(),
            custom_roms: HashMap::new(),
            breakpoints: Vec::new(),
            last_score: 0,
            recent_inputs_trace: Vec::with_capacity(128),
            frame_counter: 0,
            playtime_frame_accum: 0.0,
            playtime_dirty: false,
            last_sram_crc: None,
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
        self.refresh_bios_cache();
    }

    /// Re-scans `system_dir` for BIOS/firmware files and updates the cached registry.
    pub fn refresh_bios_cache(&mut self) {
        self.bios_entries_cache = scan_bios_registry(&self.system_dir);
    }

    pub fn set_switch_vault_root(&mut self, vault_root: PathBuf) {
        self.switch_vault_root = vault_root;
        self.switch_vault = inspect_switch_vault(&self.switch_vault_root);
    }

    pub fn refresh_switch_vault(&mut self) -> StationBootstrap {
        self.switch_vault = inspect_switch_vault(&self.switch_vault_root);
        self.bootstrap_snapshot()
    }

    /// Copies live runtime state (keymapper, cheats, watchpoints, macros) into `self.settings`
    /// before writing `settings.json` to disk.
    pub fn sync_runtime_to_settings(&mut self) {
        self.settings.keymapper_widgets = self.keymapper_widgets.clone();
        self.settings.cheat_codes = self.cheat_codes.clone();
        self.settings.memory_watchpoints = self.memory_watchpoints.clone();
        self.settings.macros = self.macros.clone();
    }

    /// Restores persisted `GlobalSettings` into live session state while preserving sensible defaults.
    pub fn apply_loaded_settings(&mut self, mut settings: GlobalSettings) {
        if settings.active_shader_css.trim().is_empty() {
            settings.active_shader_css = "mode-clean".into();
        }
        if settings.upscaler_mode.trim().is_empty() {
            settings.upscaler_mode = "XBRZ".into();
        }
        settings.nds_layout = normalize_nds_layout(&settings.nds_layout);
        settings.speed_multiplier = settings.speed_multiplier.clamp(1, 8);
        settings.nds_3d_scale = settings.nds_3d_scale.clamp(1, 2);
        if !settings.keymapper_widgets.is_empty() {
            self.keymapper_widgets = settings.keymapper_widgets.clone();
        }
        if !settings.cheat_codes.is_empty() {
            self.cheat_codes = settings.cheat_codes.clone();
        }
        if !settings.memory_watchpoints.is_empty() {
            self.memory_watchpoints = settings.memory_watchpoints.clone();
        }
        if !settings.macros.is_empty() {
            self.macros = settings.macros.clone();
        }
        self.settings = settings;
        self.apply_effective_hardware_profile();
    }

    fn apply_effective_hardware_profile(&mut self) {
        if !self.core_loaded || self.active_game_id.is_empty() {
            return;
        }
        let game = self.active_game().clone();
        let cfg = resolve_effective_config(&self.settings, &game.id, game.console);
        self.core_p1.set_cpu_overclock_pct(cfg.cpu_overclock_pct);
        self.core_p1.set_color_correction(cfg.pgxp_subpixel);
        self.core_p1
            .set_3d_scale(self.settings.nds_3d_scale.clamp(1, 2));
    }

    /// Refreshes the active core's cached SRAM slice before inspection or export.
    pub fn sync_active_sram(&mut self) {
        if self.core_loaded {
            self.core_p1.sync_sram_cache();
        }
    }

    /// Returns `true` if native gameplay playtime increased since the last persistence check and clears the flag.
    pub fn take_playtime_dirty(&mut self) -> bool {
        std::mem::take(&mut self.playtime_dirty)
    }

    pub fn update_ui_preferences(
        &mut self,
        shader_css: Option<String>,
        upscaler_mode: Option<String>,
        speed_multiplier: Option<u8>,
        nds_layout: Option<String>,
    ) -> StationBootstrap {
        self.update_ui_preferences_extended(
            shader_css,
            upscaler_mode,
            speed_multiplier,
            nds_layout,
            None,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_ui_preferences_extended(
        &mut self,
        shader_css: Option<String>,
        upscaler_mode: Option<String>,
        speed_multiplier: Option<u8>,
        nds_layout: Option<String>,
        nds_3d_scale: Option<u8>,
        viewport_integer_scale: Option<bool>,
        rewind_enabled: Option<bool>,
    ) -> StationBootstrap {
        if let Some(shader) = shader_css {
            let trimmed = shader.trim();
            if !trimmed.is_empty() {
                self.settings.active_shader_css = trimmed.to_string();
            }
        }
        if let Some(upscaler) = upscaler_mode {
            let trimmed = upscaler.trim();
            if !trimmed.is_empty() {
                self.settings.upscaler_mode = trimmed.to_string();
            }
        }
        if let Some(speed) = speed_multiplier {
            self.settings.speed_multiplier = speed.clamp(1, 8);
        }
        if let Some(layout) = nds_layout {
            if !layout.trim().is_empty() {
                self.settings.nds_layout = normalize_nds_layout(&layout);
            }
        }
        if let Some(scale) = nds_3d_scale {
            self.settings.nds_3d_scale = scale.clamp(1, 2);
            self.core_p1.set_3d_scale(self.settings.nds_3d_scale);
        }
        if let Some(integer_scale) = viewport_integer_scale {
            self.settings.viewport_integer_scale = integer_scale;
        }
        if let Some(rewind_on) = rewind_enabled {
            self.settings.rewind_enabled = rewind_on;
            if !rewind_on {
                self.clear_rewind_history();
            }
        }
        self.bootstrap_snapshot()
    }

    fn clear_rewind_history(&mut self) {
        self.rewind_generation = self.rewind_generation.wrapping_add(1);
        self.rewind_buffer.clear();
        if let Some(ref worker) = self.rewind_worker {
            while worker.rx.try_recv().is_ok() {}
        }
    }

    /// Moves any asynchronously compressed `"ZMND"` rewind snapshots into `rewind_buffer` (non-blocking).
    pub fn drain_completed_rewind_snapshots(&mut self) {
        let generation = self.rewind_generation;
        if let Some(ref worker) = self.rewind_worker {
            while let Ok(done) = worker.rx.try_recv() {
                if done.generation == generation && self.settings.rewind_enabled {
                    self.rewind_buffer
                        .push_snapshot_owned(done.frame_number, done.encoded);
                }
            }
        }
    }

    fn capture_rewind_snapshot(&mut self, frame_number: u64) {
        if let Ok(Some(raw)) = self.core_p1.save_state_raw_for_rewind() {
            if self.rewind_worker.is_none() {
                self.rewind_worker = BackgroundRewindWorker::spawn();
            }
            if let Some(ref worker) = self.rewind_worker {
                let _ = worker.tx.try_send(RewindJob {
                    generation: self.rewind_generation,
                    frame_number,
                    raw,
                });
                return;
            }
            if let Ok(encoded) = crate::nds_core::state::compress_raw_zmnd(&raw) {
                self.rewind_buffer
                    .push_snapshot_owned(frame_number, encoded);
            }
            return;
        }
        if let Ok(raw_state) = self.core_p1.save_state() {
            self.rewind_buffer
                .push_snapshot_owned(frame_number, raw_state);
        }
    }

    /// Captures the current active SRAM CRC as the clean on-disk baseline and clears `sram_dirty`.
    pub fn sync_sram_baseline(&mut self) {
        self.sync_active_sram();
        if self.has_persistable_sram() {
            self.last_sram_crc = Some(compute_crc32(self.core_p1.sram()));
        } else {
            self.last_sram_crc = None;
        }
        self.sram_dirty = false;
    }

    /// Compares active SRAM CRC against the last flushed baseline and updates `sram_dirty`.
    pub fn refresh_sram_dirty_flag(&mut self) -> bool {
        self.sync_active_sram();
        if !self.has_persistable_sram() {
            self.sram_dirty = false;
            return false;
        }
        let crc = compute_crc32(self.core_p1.sram());
        if self.last_sram_crc != Some(crc) {
            self.sram_dirty = true;
        }
        self.sram_dirty
    }

    /// Marks the currently active SRAM bytes as cleanly persisted on disk.
    pub fn mark_sram_flushed(&mut self) {
        self.sync_sram_baseline();
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
                    cover_path: None,
                    metadata_genre: None,
                    metadata_developer: None,
                    metadata_publisher: None,
                    metadata_release_year: None,
                    metadata_description: None,
                    metadata_canonical_name: None,
                    collections: Vec::new(),
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
            collections: self.collections.clone(),
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
            udp_packets_sent: self.udp_node.packets_sent,
            udp_packets_received: self.udp_node.packets_received,
            native_cores: built_in_native_cores_info(),
            bios_entries: self.bios_entries_cache.clone(),
            link_telemetry: self.link_telemetry.clone(),
            rewind_snapshots_count: self.rewind_buffer.len(),
            rewind_memory_bytes: self.rewind_buffer.memory_usage_bytes(),
            warning: self.warning.clone(),
            rom_directories: self.settings.rom_directories.clone(),
            auto_rescan_rom_dirs_on_startup: self.settings.auto_rescan_rom_dirs_on_startup,
            cloud_sync_dir: self.settings.cloud_sync_dir.clone(),
            switch_vault: self.switch_vault.clone(),
            last_scan_report: self.last_scan_report.clone(),
            active_shader_css: self.settings.active_shader_css.clone(),
            upscaler_mode: self.settings.upscaler_mode.clone(),
            speed_multiplier: self.settings.speed_multiplier.clamp(1, 8),
            nds_layout: normalize_nds_layout(&self.settings.nds_layout),
            nds_3d_scale: self.settings.nds_3d_scale.clamp(1, 2),
            viewport_integer_scale: self.settings.viewport_integer_scale,
            rewind_enabled: self.settings.rewind_enabled,
            relay_url: self.settings.relay_url.clone(),
            relay_key_configured: self
                .settings
                .relay_access_key
                .as_deref()
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false),
            relay_player_name: self.settings.relay_player_name.clone(),
        }
    }

    pub fn list_save_slots_for(&self, game_id: &str) -> Vec<SaveStateSlotInfo> {
        (1..=9)
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

        self.core_p2 = build_core_for(game.console, &rom, &self.system_dir)
            .unwrap_or_else(|_| Box::new(GameBoyCore::new_empty()));
        if self.link_telemetry.quad_mode {
            self.core_p3 = build_core_for(game.console, &rom, &self.system_dir)
                .unwrap_or_else(|_| Box::new(GameBoyCore::new_empty()));
            self.core_p4 = build_core_for(game.console, &rom, &self.system_dir)
                .unwrap_or_else(|_| Box::new(GameBoyCore::new_empty()));
        } else {
            self.core_p3 = Box::new(GameBoyCore::new_empty());
            self.core_p4 = Box::new(GameBoyCore::new_empty());
        }
    }

    pub fn merge_scanned_entries(&mut self, entries: Vec<GameEntry>) -> Option<String> {
        if entries.is_empty() {
            return None;
        }
        let mut newest_id = None;
        for mut entry in entries.into_iter().rev() {
            newest_id = Some(entry.id.clone());
            if let Some(pos) = self.catalog.iter().position(|g| {
                g.id == entry.id
                    || (entry.rom_path.is_some()
                        && g.rom_path == entry.rom_path
                        && g.archive_entry == entry.archive_entry)
            }) {
                let prev = self.catalog.remove(pos);
                if !prev.title.is_empty() {
                    entry.title = prev.title;
                }
                entry.playtime_seconds = prev.playtime_seconds;
                entry.favorite = prev.favorite;
                entry.switch_engine = prev.switch_engine;
                entry.cover_path = prev.cover_path;
                entry.metadata_genre = prev.metadata_genre;
                entry.metadata_developer = prev.metadata_developer;
                entry.metadata_publisher = prev.metadata_publisher;
                entry.metadata_release_year = prev.metadata_release_year;
                entry.metadata_description = prev.metadata_description;
                entry.metadata_canonical_name = prev.metadata_canonical_name;
                entry.collections = prev.collections;
                if entry.title_id.is_none() {
                    entry.title_id = prev.title_id;
                }
                self.catalog.insert(0, entry);
            } else {
                self.catalog.insert(0, entry);
            }
        }
        newest_id
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

    /// Removes a watched ROM folder from `settings.rom_directories` without deleting files on disk.
    pub fn remove_rom_directory(&mut self, dir_path: &str) -> Result<StationBootstrap, String> {
        let target = dir_path.trim();
        let before = self.settings.rom_directories.len();
        self.settings
            .rom_directories
            .retain(|d| !d.eq_ignore_ascii_case(target));
        if self.settings.rom_directories.len() == before {
            return Err(format!("dossier introuvable dans la liste : {target}"));
        }
        Ok(self.bootstrap_snapshot())
    }

    /// Manually rescans every registered ROM folder and merges newly discovered entries.
    pub fn rescan_all_rom_directories(&mut self) -> DirectoryScanReport {
        let dirs = self.settings.rom_directories.clone();
        let mut combined = DirectoryScanReport {
            scanned_root: if dirs.len() == 1 {
                dirs[0].clone()
            } else {
                format!("{} dossier(s) surveillé(s)", dirs.len())
            },
            ..Default::default()
        };
        for dir in dirs {
            let rep = self.scan_directory_and_merge(Path::new(&dir));
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

    pub fn import_paths(&mut self, paths: &[PathBuf]) -> Result<DirectoryScanReport, String> {
        let mut combined = DirectoryScanReport {
            scanned_root: "Importation directe".into(),
            ..Default::default()
        };
        let mut file_entries: Vec<GameEntry> = Vec::new();
        let mut first_dir_entry_id: Option<String> = None;

        for p in paths {
            if p.is_dir() {
                let rep = self.scan_directory_and_merge(p);
                if first_dir_entry_id.is_none() {
                    first_dir_entry_id = rep.imported_entries.first().map(|e| e.id.clone());
                }
                combined.imported_entries.extend(rep.imported_entries);
                combined.skipped_files += rep.skipped_files;
                combined
                    .unsupported_archives
                    .extend(rep.unsupported_archives);
                combined.errors.extend(rep.errors);
            } else if p.is_file() {
                match inspect_path_to_entries(p) {
                    Ok(rep) => {
                        file_entries.extend(rep.imported_entries.clone());
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

        let first_file_id = self.merge_scanned_entries(file_entries);
        self.last_scan_report = Some(combined.clone());
        if let Some(id) = first_file_id.or(first_dir_entry_id) {
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
            self.clear_rewind_history();
            self.frame_counter = 0;
            self.playtime_frame_accum = 0.0;
            self.last_sram_crc = None;
            self.sram_dirty = false;
            self.core_loaded = false;
            self.core_p1 = Box::new(GameBoyCore::new_empty());
            self.core_p2 = Box::new(GameBoyCore::new_empty());
            self.core_p3 = Box::new(GameBoyCore::new_empty());
            self.core_p4 = Box::new(GameBoyCore::new_empty());
        }
        Ok(self.bootstrap_snapshot())
    }

    pub fn toggle_game_favorite(&mut self, game_id: &str) -> Result<StationBootstrap, String> {
        let Some(game) = self.catalog.iter_mut().find(|g| g.id == game_id) else {
            return Err(format!("jeu introuvable : {game_id}"));
        };
        game.favorite = !game.favorite;
        Ok(self.bootstrap_snapshot())
    }

    /// Creates a new custom collection (case-insensitive deduplication).
    pub fn create_collection(&mut self, raw_name: &str) -> Result<StationBootstrap, String> {
        let name = normalize_collection_name(raw_name)?;
        if !self
            .collections
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&name))
        {
            self.collections.push(name);
            self.collections.sort_by_key(|a| a.to_ascii_lowercase());
        }
        Ok(self.bootstrap_snapshot())
    }

    /// Deletes a custom collection and removes its tag from every game in the catalog.
    pub fn delete_collection(&mut self, raw_name: &str) -> Result<StationBootstrap, String> {
        let target = raw_name.trim();
        let before = self.collections.len();
        self.collections.retain(|c| !c.eq_ignore_ascii_case(target));
        for game in &mut self.catalog {
            game.collections.retain(|c| !c.eq_ignore_ascii_case(target));
        }
        if self.collections.len() == before {
            return Err(format!("collection introuvable : {target}"));
        }
        Ok(self.bootstrap_snapshot())
    }

    /// Adds or removes a game from a collection (creating the collection if needed).
    pub fn toggle_game_collection(
        &mut self,
        game_id: &str,
        raw_collection: &str,
    ) -> Result<StationBootstrap, String> {
        let name = normalize_collection_name(raw_collection)?;
        let canonical = self
            .collections
            .iter()
            .find(|c| c.eq_ignore_ascii_case(&name))
            .cloned()
            .unwrap_or_else(|| {
                self.collections.push(name.clone());
                self.collections.sort_by_key(|a| a.to_ascii_lowercase());
                name
            });

        let Some(game) = self.catalog.iter_mut().find(|g| g.id == game_id) else {
            return Err(format!("jeu introuvable : {game_id}"));
        };
        if let Some(idx) = game
            .collections
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&canonical))
        {
            game.collections.remove(idx);
        } else {
            game.collections.push(canonical);
            game.collections.sort_by_key(|a| a.to_ascii_lowercase());
        }
        Ok(self.bootstrap_snapshot())
    }

    /// Applies user-edited or scraped metadata fields to a game entry.
    pub fn update_game_metadata(
        &mut self,
        game_id: &str,
        update: GameMetadataUpdate,
    ) -> Result<StationBootstrap, String> {
        let Some(game) = self.catalog.iter_mut().find(|g| g.id == game_id) else {
            return Err(format!("jeu introuvable : {game_id}"));
        };
        let clean_opt = |opt: Option<String>, max_len: usize| -> Option<Option<String>> {
            opt.map(|raw| {
                let t = raw.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.chars().take(max_len).collect())
                }
            })
        };
        if let Some(raw_title) = update.title {
            let trimmed = raw_title.trim();
            if !trimmed.is_empty() {
                game.title = trimmed.chars().take(120).collect();
            }
        }
        if let Some(val) = clean_opt(update.genre, 80) {
            game.metadata_genre = val;
        }
        if let Some(val) = clean_opt(update.developer, 80) {
            game.metadata_developer = val;
        }
        if let Some(val) = clean_opt(update.publisher, 80) {
            game.metadata_publisher = val;
        }
        if let Some(val) = clean_opt(update.release_year, 16) {
            game.metadata_release_year = val;
        }
        if let Some(val) = clean_opt(update.description, 600) {
            game.metadata_description = val;
        }
        if let Some(val) = clean_opt(update.canonical_name, 180) {
            game.metadata_canonical_name = val;
        }
        Ok(self.bootstrap_snapshot())
    }

    /// Sets or clears the cached local cover filename for a game entry.
    pub fn set_game_cover_path(
        &mut self,
        game_id: &str,
        cover_path: Option<String>,
    ) -> Result<StationBootstrap, String> {
        let Some(game) = self.catalog.iter_mut().find(|g| g.id == game_id) else {
            return Err(format!("jeu introuvable : {game_id}"));
        };
        game.cover_path = cover_path.and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        });
        Ok(self.bootstrap_snapshot())
    }

    /// Enables or disables automatic rescanning of user-registered ROM folders on startup.
    pub fn set_auto_rescan_on_startup(&mut self, enabled: bool) -> StationBootstrap {
        self.settings.auto_rescan_rom_dirs_on_startup = enabled;
        self.bootstrap_snapshot()
    }

    /// Configures or clears the user-selected Cloud Saves sync directory.
    pub fn set_cloud_sync_dir(&mut self, dir: Option<String>) -> StationBootstrap {
        self.settings.cloud_sync_dir = dir.and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        });
        self.bootstrap_snapshot()
    }

    /// Configures the private VPS E2EE multiplayer relay (`wss://relay.<domain>/ws` + private key).
    pub fn set_relay_config(
        &mut self,
        relay_url: Option<String>,
        access_key: Option<String>,
        clear_access_key: bool,
        player_name: Option<String>,
    ) -> Result<StationBootstrap, String> {
        if let Some(raw_url) = relay_url {
            let trimmed = raw_url.trim();
            if trimmed.is_empty() {
                self.settings.relay_url = None;
            } else {
                self.settings.relay_url = Some(validate_relay_url(trimmed)?);
            }
        }
        if clear_access_key {
            self.settings.relay_access_key = None;
        } else if let Some(raw_key) = access_key {
            let trimmed = raw_key.trim();
            if !trimmed.is_empty() {
                if trimmed.len() < 16 {
                    return Err(
                        "clé d'accès relais trop courte (minimum 16 caractères requis)".into(),
                    );
                }
                self.settings.relay_access_key = Some(trimmed.to_string());
            }
        }
        if let Some(raw_name) = player_name {
            let cleaned: String = raw_name
                .trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(32)
                .collect();
            self.settings.relay_player_name = if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            };
        }
        Ok(self.bootstrap_snapshot())
    }

    /// Enables or disables the online encrypted GB/GBC & GBA Link Cable tunnel on `core_p1`.
    pub fn set_online_link_enabled(&mut self, enabled: bool) {
        self.set_online_link_enabled_with_role(enabled, self.online_gba_player_id);
    }

    /// Enables or disables the online encrypted Link Cable tunnel and assigns the GBA Multi-Player
    /// console ID (`0` = Master/Parent, `1..=3` = Slaves/Children).
    pub fn set_online_link_enabled_with_role(&mut self, enabled: bool, gba_player_id: u8) {
        self.online_link_enabled = enabled;
        self.online_gba_player_id = gba_player_id & 0x03;
        self.core_p1
            .set_gba_multi_connected(enabled, self.online_gba_player_id);
        if !enabled {
            self.online_link_rx_queue.clear();
            self.online_link_tx_queue.clear();
            self.online_gba_link_rx_queue.clear();
            self.online_gba_link_tx_queue.clear();
            self.online_gba_last_master_word = 0xFFFF;
        }
    }

    /// Hot-reloads updated SRAM bytes (e.g. right after an atomic Trade Hub Pokémon swap) into
    /// `core_p1` if `game_id` is currently active, preventing the next autosave from overwriting it.
    pub fn reload_active_sram_from_bytes(
        &mut self,
        game_id: &str,
        sram_bytes: &[u8],
    ) -> Result<bool, String> {
        if !self.core_loaded || self.active_game_id != game_id {
            return Ok(false);
        }
        self.core_p1.load_sram(sram_bytes)?;
        self.core_p1.sync_sram_cache();
        let current = self.core_p1.sram();
        self.last_sram_crc = if current.is_empty() {
            None
        } else {
            Some(compute_crc32(current))
        };
        self.sram_dirty = false;
        Ok(true)
    }

    /// Queues an authenticated `GbLinkPayload` decrypted from the `ZMR1` relay tunnel.
    pub fn push_remote_link_payload(&mut self, payload: GbLinkPayload) {
        if !self.online_link_enabled {
            return;
        }
        if self.online_link_rx_queue.len() >= 64 {
            let _ = self.online_link_rx_queue.pop_front();
        }
        self.online_link_rx_queue.push_back(payload);
    }

    /// Drains pending outbound `GbLinkPayload`s produced by `core_p1` for E2EE relay transmission.
    pub fn drain_outgoing_link_payloads(&mut self) -> Vec<GbLinkPayload> {
        self.online_link_tx_queue.drain(..).collect()
    }

    /// Queues an authenticated `GbaLinkPayload` (`SIOMULTI`) decrypted from the `ZMR1` relay tunnel.
    pub fn push_remote_gba_link_payload(&mut self, payload: GbaLinkPayload) {
        if !self.online_link_enabled {
            return;
        }
        if self.online_gba_link_rx_queue.len() >= 64 {
            let _ = self.online_gba_link_rx_queue.pop_front();
        }
        self.online_gba_link_rx_queue.push_back(payload);
    }

    /// Drains pending outbound `GbaLinkPayload`s (`SIOMULTI`) produced by `core_p1` for E2EE transmission.
    pub fn drain_outgoing_gba_link_payloads(&mut self) -> Vec<GbaLinkPayload> {
        self.online_gba_link_tx_queue.drain(..).collect()
    }

    /// Configures Nintendo DS ARM7 Wi-Fi (`Off` / `LocalNiFi` / `WfcAccessPoint`) on `core_p1`,
    /// assigns a unique MAC derived from `peer_id`, and auto-provisions `ZaimuleAP` when in WFC mode.
    pub fn configure_nds_wifi(
        &mut self,
        mode: NdsWifiMode,
        peer_id: u8,
        wfc_dns_ipv4: Option<[u8; 4]>,
    ) {
        self.nds_wifi_mode = mode;
        self.nds_wifi_peer_id = peer_id.max(1);
        self.nds_wifi_wfc_dns = wfc_dns_ipv4;
        self.apply_nds_wifi_to_core();
        if mode == NdsWifiMode::Off {
            self.online_nds_wifi_rx_queue.clear();
            self.online_nds_wifi_tx_queue.clear();
            self.nds_wfc_external_ip_rx.clear();
            self.nds_wfc_external_ip_tx.clear();
        }
    }

    fn apply_nds_wifi_to_core(&mut self) {
        if self.core_p1.console() == ConsoleKind::Nds {
            self.core_p1.configure_nds_wifi(
                self.nds_wifi_mode,
                self.nds_wifi_peer_id,
                self.nds_wifi_wfc_dns,
            );
        }
    }

    /// Queues an authenticated `NdsWifiPacket` decrypted from `ZMR1` (`RelayChannel::NdsWifi = 6`).
    pub fn push_remote_nds_wifi_packet(&mut self, packet: NdsWifiPacket) {
        if self.nds_wifi_mode == NdsWifiMode::Off {
            return;
        }
        if self.online_nds_wifi_rx_queue.len() >= 64 {
            let _ = self.online_nds_wifi_rx_queue.pop_front();
        }
        self.online_nds_wifi_rx_queue.push_back(packet);
    }

    /// Drains outgoing `NdsWifiPacket` frames ready for `ZMR1` E2EE transmission.
    pub fn drain_outgoing_nds_wifi_packets(&mut self) -> Vec<NdsWifiPacket> {
        self.online_nds_wifi_tx_queue.drain(..).collect()
    }

    /// Injects an IPv4 packet returning from the outbound-only WFC NAT gateway (`nds_wfc`).
    pub fn push_nds_wfc_external_ip(&mut self, ipv4_packet: Vec<u8>) {
        if self.nds_wifi_mode != NdsWifiMode::WfcAccessPoint {
            return;
        }
        if self.nds_wfc_external_ip_rx.len() >= 64 {
            let _ = self.nds_wfc_external_ip_rx.pop_front();
        }
        self.nds_wfc_external_ip_rx.push_back(ipv4_packet);
    }

    /// Drains external-bound WFC IPv4 packets from `ZaimuleAP` for the outbound-only NAT gateway.
    pub fn drain_nds_wfc_external_ip(&mut self) -> Vec<Vec<u8>> {
        self.nds_wfc_external_ip_tx.drain(..).collect()
    }

    /// Returns live Nintendo DS Wi-Fi hardware & `ZaimuleAP` telemetry.
    pub fn nds_wifi_telemetry(&self) -> NdsWifiTelemetry {
        if let Some(live) = self.core_p1.nds_wifi_telemetry() {
            return live;
        }
        let mac = crate::nds_core::wifi::derive_console_mac(self.nds_wifi_peer_id);
        let dns = self
            .nds_wifi_wfc_dns
            .unwrap_or(crate::nds_core::wifi::VIRTUAL_AP_IP);
        NdsWifiTelemetry {
            mode: self.nds_wifi_mode,
            mac_address: crate::nds_core::wifi::format_mac(mac),
            channel: 6,
            ap_ssid: crate::nds_core::wifi::VIRTUAL_AP_SSID.to_string(),
            ap_associated: false,
            assigned_ip: None,
            wfc_dns: crate::nds_core::wifi::format_ipv4(dns),
            tx_frames: 0,
            rx_frames: 0,
            mp_cycles: 0,
            irq_count: 0,
        }
    }

    fn pump_online_link_pre_step(&mut self, frame: u32, buttons_mask: u16) {
        if self.core_p1.console() == ConsoleKind::Nds && self.nds_wifi_mode != NdsWifiMode::Off {
            while let Some(pkt) = self.online_nds_wifi_rx_queue.pop_front() {
                self.core_p1.push_nds_wifi_rx(&pkt);
                self.link_telemetry.packets_transferred =
                    self.link_telemetry.packets_transferred.saturating_add(1);
                self.link_telemetry.baud_rate_bps = 2_000_000;
            }
            while let Some(ip) = self.nds_wfc_external_ip_rx.pop_front() {
                self.core_p1.inject_nds_wfc_external_ip(&ip);
            }
        }

        if !self.online_link_enabled {
            return;
        }
        if self.core_p1.console() == ConsoleKind::Gba {
            self.core_p1
                .set_gba_multi_connected(true, self.online_gba_player_id);
        }
        while let Some(incoming) = self.online_link_rx_queue.pop_front() {
            let prev_sb = self.core_p1.exchange_serial_byte(incoming.sio_sb);
            self.link_telemetry.packets_transferred =
                self.link_telemetry.packets_transferred.saturating_add(1);
            self.link_telemetry.last_p2_to_p1_hex = format!("0x{:02X}", incoming.sio_sb);
            self.link_telemetry.baud_rate_bps = 262_144;

            if incoming.is_master_tx() {
                if self.online_link_tx_queue.len() >= 64 {
                    let _ = self.online_link_tx_queue.pop_front();
                }
                self.online_link_tx_queue.push_back(GbLinkPayload {
                    frame,
                    buttons_mask,
                    sio_sb: prev_sb,
                    sio_sc: 0x80,
                    flags: GB_LINK_FLAG_SLAVE_REPLY,
                    seq: incoming.seq,
                });
                self.link_telemetry.last_p1_to_p2_hex = format!("0x{prev_sb:02X}");
            }
        }

        while let Some(incoming) = self.online_gba_link_rx_queue.pop_front() {
            if self.core_p1.console() != ConsoleKind::Gba {
                continue;
            }
            self.link_telemetry.packets_transferred =
                self.link_telemetry.packets_transferred.saturating_add(1);
            self.link_telemetry.last_p2_to_p1_hex = format!("0x{:04X}", incoming.siomlt_send);
            self.link_telemetry.baud_rate_bps = 115_200;

            if incoming.is_master_tx() && self.online_gba_player_id > 0 {
                let my_id = self.online_gba_player_id.clamp(1, 3);
                let (slave_word, siocnt) = self
                    .core_p1
                    .exchange_gba_multi_slave(incoming.siomlt_send, my_id);
                if self.online_gba_link_tx_queue.len() >= 64 {
                    let _ = self.online_gba_link_tx_queue.pop_front();
                }
                self.online_gba_link_tx_queue.push_back(GbaLinkPayload {
                    frame,
                    buttons_mask,
                    siomlt_send: slave_word,
                    siocnt,
                    flags: GBA_LINK_FLAG_SLAVE_REPLY,
                    player_id: my_id,
                    seq: incoming.seq,
                });
                self.link_telemetry.last_p1_to_p2_hex = format!("0x{slave_word:04X}");
            } else if incoming.is_slave_reply() && self.online_gba_player_id == 0 {
                let slave_slot = usize::from(incoming.player_id.clamp(1, 3));
                let mut slots = [0xFFFFu16; 4];
                slots[0] = self.online_gba_last_master_word;
                slots[slave_slot] = incoming.siomlt_send;
                self.core_p1.deliver_gba_multi(slots, 0);
            }
        }
    }

    fn pump_online_link_post_step(
        &mut self,
        frame: u32,
        buttons_mask: u16,
        serial_out: Option<u8>,
    ) {
        if self.core_p1.console() == ConsoleKind::Nds && self.nds_wifi_mode != NdsWifiMode::Off {
            let mut tx_batch = Vec::new();
            self.core_p1.drain_nds_wifi_tx(&mut tx_batch);
            for pkt in tx_batch {
                if self.online_nds_wifi_tx_queue.len() >= 64 {
                    let _ = self.online_nds_wifi_tx_queue.pop_front();
                }
                self.online_nds_wifi_tx_queue.push_back(pkt);
                self.link_telemetry.packets_transferred =
                    self.link_telemetry.packets_transferred.saturating_add(1);
                self.link_telemetry.baud_rate_bps = 2_000_000;
            }
            let mut ext_ips = Vec::new();
            self.core_p1.drain_nds_wfc_external_ip(&mut ext_ips);
            for ip in ext_ips {
                if self.nds_wfc_external_ip_tx.len() >= 64 {
                    let _ = self.nds_wfc_external_ip_tx.pop_front();
                }
                self.nds_wfc_external_ip_tx.push_back(ip);
            }
        }

        if !self.online_link_enabled {
            return;
        }
        if let Some(tx_byte) = serial_out {
            self.online_link_seq = self.online_link_seq.wrapping_add(1);
            if self.online_link_tx_queue.len() >= 64 {
                let _ = self.online_link_tx_queue.pop_front();
            }
            self.online_link_tx_queue.push_back(GbLinkPayload {
                frame,
                buttons_mask,
                sio_sb: tx_byte,
                sio_sc: 0x81,
                flags: GB_LINK_FLAG_MASTER_TX,
                seq: self.online_link_seq,
            });
            self.link_telemetry.packets_transferred =
                self.link_telemetry.packets_transferred.saturating_add(1);
            self.link_telemetry.last_p1_to_p2_hex = format!("0x{tx_byte:02X}");
            self.link_telemetry.baud_rate_bps = 262_144;
        }

        if self.core_p1.console() == ConsoleKind::Gba && self.online_gba_player_id == 0 {
            if let Some((send_word, siocnt)) = self.core_p1.take_gba_multi_tx() {
                self.online_gba_last_master_word = send_word;
                self.online_link_seq = self.online_link_seq.wrapping_add(1);
                if self.online_gba_link_tx_queue.len() >= 64 {
                    let _ = self.online_gba_link_tx_queue.pop_front();
                }
                self.online_gba_link_tx_queue.push_back(GbaLinkPayload {
                    frame,
                    buttons_mask,
                    siomlt_send: send_word,
                    siocnt,
                    flags: GBA_LINK_FLAG_MASTER_TX,
                    player_id: 0,
                    seq: self.online_link_seq,
                });
                self.link_telemetry.packets_transferred =
                    self.link_telemetry.packets_transferred.saturating_add(1);
                self.link_telemetry.last_p1_to_p2_hex = format!("0x{send_word:04X}");
                self.link_telemetry.baud_rate_bps = 115_200;
            }
        }
    }

    pub fn to_persisted_library(&self) -> PersistedLibrary {
        PersistedLibrary {
            active_game_id: self.active_game_id.clone(),
            catalog: self.catalog.clone(),
            rom_directories: self.settings.rom_directories.clone(),
            collections: self.collections.clone(),
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
        self.collections.clear();
        for col in lib.collections {
            if let Ok(clean) = normalize_collection_name(&col) {
                if !self
                    .collections
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(&clean))
                {
                    self.collections.push(clean);
                }
            }
        }
        for game in &lib.catalog {
            for col in &game.collections {
                if let Ok(clean) = normalize_collection_name(col) {
                    if !self
                        .collections
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(&clean))
                    {
                        self.collections.push(clean);
                    }
                }
            }
        }
        self.collections.sort_by_key(|a| a.to_ascii_lowercase());
        self.catalog = lib.catalog;
        if !lib.active_game_id.is_empty() && self.catalog.iter().any(|g| g.id == lib.active_game_id)
        {
            let _ = self.select_game(&lib.active_game_id);
        }
    }

    /// Imports user BIOS/firmware files or `.zip`/`.7z` packs into `self.system_dir`.
    pub fn import_bios_paths(
        &mut self,
        paths: &[PathBuf],
    ) -> Result<(BiosImportReport, StationBootstrap), String> {
        let report = import_bios_from_paths(paths, &self.system_dir)?;
        self.refresh_bios_cache();
        Ok((report, self.bootstrap_snapshot()))
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

    /// Imports user-supplied Nintendo Switch firmware (folder or `.zip`/`.7z` of `.nca` archives)
    /// into the Option-A Vault and synchronizes it into Torzu & Fritchii NAND directories.
    pub fn import_switch_firmware(&mut self, source: &Path) -> Result<StationBootstrap, String> {
        self.switch_vault = core_import_switch_firmware(source, &self.switch_vault_root)?;
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
        self.clear_rewind_history();
        self.frame_counter = 0;
        self.playtime_frame_accum = 0.0;
        self.last_sram_crc = None;
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

        self.core_p1 = build_core_for(game.console, &rom_bytes, &self.system_dir)?;
        let cap = rom_bytes.len().min(4 * 1024 * 1024);
        self.active_disasm_rom = rom_bytes[..cap].to_vec();
        self.core_loaded = true;
        self.apply_effective_hardware_profile();
        self.apply_nds_wifi_to_core();
        self.warning = None;
        self.sync_sram_baseline();
        self.sync_secondary_link_cores();
        Ok(self.bootstrap_snapshot())
    }

    /// Performs a hardware cold reset of the active native ROM while preserving its battery SRAM.
    pub fn reset_active_game(&mut self) -> Result<StationBootstrap, String> {
        if self.active_game_id.is_empty() || !self.core_loaded {
            return Err("aucune ROM native active à réinitialiser".into());
        }
        let game = self.active_game().clone();
        if game.console.is_external_only() || game.engine_kind == CoreEngineKind::ExternalLauncher {
            return Err(
                "le reset matériel ne s'applique qu'aux cœurs natifs (GB/GBC/GBA/NDS)".into(),
            );
        }
        self.sync_active_sram();
        let preserved_sram = if self.has_persistable_sram() {
            Some(self.core_p1.sram().to_vec())
        } else {
            None
        };
        let was_dirty = self.refresh_sram_dirty_flag();

        self.select_game(&game.id)?;
        if let Some(sram) = preserved_sram {
            let _ = self.core_p1.load_sram(&sram);
            if was_dirty {
                self.sram_dirty = true;
            } else {
                self.sync_sram_baseline();
            }
        }
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
                    cover_path: None,
                    metadata_genre: None,
                    metadata_developer: None,
                    metadata_publisher: None,
                    metadata_release_year: None,
                    metadata_description: None,
                    metadata_canonical_name: None,
                    collections: Vec::new(),
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
        self.core_p1 = build_core_for(game.console, &patched_rom, &self.system_dir)?;
        self.apply_effective_hardware_profile();
        self.apply_nds_wifi_to_core();
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

    pub fn remove_cheat_code(&mut self, cheat_id: &str) -> Result<StationBootstrap, String> {
        let before = self.cheat_codes.len();
        self.cheat_codes.retain(|c| c.id != cheat_id);
        if self.cheat_codes.len() == before {
            return Err(format!("code triche introuvable : {cheat_id}"));
        }
        Ok(self.bootstrap_snapshot())
    }

    pub fn scan_active_ram(&mut self, target_value: u8) -> Vec<String> {
        self.core_p1.sync_debug_caches();
        let slice = self.core_p1.read_ram_slice();
        scan_ram_for_exact_byte(slice, 0xC000, target_value, 24)
            .into_iter()
            .map(|addr| format!("0x{addr:04X}"))
            .collect()
    }

    /// Connects to a validated LAN UDP peer (`IP:PORT`) or disconnects when `peer_addr` is `None`/empty.
    pub fn connect_netplay_peer(
        &mut self,
        peer_addr: Option<&str>,
    ) -> Result<StationBootstrap, String> {
        match peer_addr.map(str::trim).filter(|s| !s.is_empty()) {
            Some(addr) => {
                self.udp_node.connect_peer_validated(addr)?;
                self.link_telemetry.enabled = true;
                self.sync_secondary_link_cores();
            }
            None => {
                self.udp_node.disconnect_peer();
            }
        }
        Ok(self.bootstrap_snapshot())
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
        if let Some(peer) = peer_addr
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.udp_node.connect_peer_validated(peer)?;
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
                players_current: if self.udp_node.peer_addr.is_some() {
                    2
                } else {
                    1
                },
                players_max: 2,
                ping_ms: 0,
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
        if let Some(peer) = peer_addr
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.udp_node.connect_peer_validated(peer)?;
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
        self.macro_start_frame = self.frame_counter;
        Ok(self.bootstrap_snapshot())
    }

    pub fn set_active_macro(&mut self, macro_id: Option<String>) -> StationBootstrap {
        self.active_macro_id = macro_id.filter(|id| self.macros.iter().any(|m| &m.id == id));
        self.macro_start_frame = self.frame_counter;
        self.bootstrap_snapshot()
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
    /// plus rate-matched stereo PCM samples (box-filter averaged during fast-forward).
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
        let mut acc_l = 0.0f32;
        let mut acc_r = 0.0f32;
        let mut acc_count = 0usize;

        for s in 0..steps {
            let mut single =
                self.step_single_viewport_tick(raw_buttons_p1, raw_buttons_p2, turbo_active);
            if steps > 1 {
                let pcm = std::mem::take(&mut single.primary.audio_samples);
                let inv_steps = 1.0 / (steps as f32);
                for pair in pcm.as_chunks::<2>().0 {
                    acc_l += pair[0];
                    acc_r += pair[1];
                    acc_count += 1;
                    if acc_count == steps {
                        merged_pcm.push(acc_l * inv_steps);
                        merged_pcm.push(acc_r * inv_steps);
                        acc_l = 0.0;
                        acc_r = 0.0;
                        acc_count = 0;
                    }
                }
            } else {
                merged_pcm = std::mem::take(&mut single.primary.audio_samples);
            }
            if s + 1 == steps {
                if steps > 1 && acc_count > 0 && merged_pcm.is_empty() {
                    let inv = 1.0 / (acc_count as f32);
                    merged_pcm.push(acc_l * inv);
                    merged_pcm.push(acc_r * inv);
                }
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
        self.step_viewport_binary_bytes_ex(
            raw_buttons_p1,
            raw_buttons_p2,
            turbo_active,
            speed_mult,
            true,
        )
    }

    /// Executes `step_viewport_with_speed` and serializes to `"ZMFR"` v1 binary bytes,
    /// optionally skipping the JSON debug metadata trailer on non-telemetry frames.
    ///
    /// Single-player sessions use a zero-copy fast path (`step_frame_borrowed` + `rgba_frame`)
    /// that skips PPU/3D rasterization on intermediate fast-forward frames and avoids
    /// per-frame RGBA clones and `CpuDebugSnapshot` string allocations.
    pub fn step_viewport_binary_bytes_ex(
        &mut self,
        raw_buttons_p1: u16,
        raw_buttons_p2: u16,
        turbo_active: bool,
        speed_mult: u8,
        include_debug_meta: bool,
    ) -> Vec<u8> {
        self.step_viewport_binary_bytes_with_options(
            raw_buttons_p1,
            raw_buttons_p2,
            turbo_active,
            speed_mult,
            BinaryStepOptions {
                include_debug_meta,
                prefer_rgb565: false,
                skip_render: false,
            },
        )
    }

    /// Executes `step_viewport_with_speed` and serializes to `"ZMFR"` v1 binary bytes with
    /// optional RGB565 pixel packing (`flags & 0x10`) and smart frameskip (`skip_render`,
    /// emitting `view_count = 0` with full 48 kHz PCM audio).
    pub fn step_viewport_binary_bytes_with_options(
        &mut self,
        raw_buttons_p1: u16,
        raw_buttons_p2: u16,
        turbo_active: bool,
        speed_mult: u8,
        opts: BinaryStepOptions,
    ) -> Vec<u8> {
        self.drain_completed_rewind_snapshots();
        if self.link_telemetry.enabled {
            let resp = self.step_viewport_with_speed(
                raw_buttons_p1,
                raw_buttons_p2,
                turbo_active,
                speed_mult,
            );
            return encode_viewport_binary_packet_with_meta(
                &resp,
                self.sram_dirty,
                opts.include_debug_meta,
            );
        }

        let steps = usize::from(speed_mult.clamp(1, 8));
        self.merged_pcm_scratch.clear();
        let mut acc_l = 0.0f32;
        let mut acc_r = 0.0f32;
        let mut acc_count = 0usize;
        let mut last_meta = crate::traits::FastFrameMeta::default();
        let mut newly_unlocked_achievement: Option<String> = None;
        let console = self.core_p1.console();
        let inv_steps = 1.0 / (steps as f32);

        for s in 0..steps {
            let current_frame = self.frame_counter;
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

            for cheat in &self.cheat_codes {
                if cheat.enabled {
                    match cheat.kind {
                        CheatKind::ActionReplayDs => {
                            self.core_p1.apply_nds_ar_cheat(&cheat.ar_ops);
                        }
                        CheatKind::GameSharkGb => {
                            self.core_p1.apply_ram_cheat(cheat.addr, cheat.value);
                        }
                    }
                }
            }

            for wp in &self.memory_watchpoints {
                if wp.frozen {
                    self.core_p1.apply_ram_cheat(wp.addr, wp.frozen_value);
                }
            }

            if self.udp_node.peer_addr.is_some() {
                let snap = self.core_p1.debug_snapshot();
                let sb = u8::from_str_radix(snap.serial_sb_hex.trim_start_matches("0x"), 16)
                    .unwrap_or(0);
                let _ = self.udp_node.send_frame_packet(NetplayWirePacket {
                    frame: current_frame as u32,
                    buttons_mask: mask_p1,
                    sio_sb: sb,
                    sio_sc: 0x81,
                });
                let _ = self.udp_node.poll_incoming_packet();
            }

            self.pump_online_link_pre_step(current_frame as u32, mask_p1);

            let skip_render = (s + 1 < steps) || opts.skip_render;
            self.pcm_scratch.clear();
            let meta =
                self.core_p1
                    .step_frame_borrowed(mask_p1, skip_render, &mut self.pcm_scratch);
            self.pump_online_link_post_step(current_frame as u32, mask_p1, meta.serial_out_byte);

            if steps > 1 {
                for pair in self.pcm_scratch.as_chunks::<2>().0 {
                    acc_l += pair[0];
                    acc_r += pair[1];
                    acc_count += 1;
                    if acc_count == steps {
                        self.merged_pcm_scratch.push(acc_l * inv_steps);
                        self.merged_pcm_scratch.push(acc_r * inv_steps);
                        acc_l = 0.0;
                        acc_r = 0.0;
                        acc_count = 0;
                    }
                }
            } else {
                std::mem::swap(&mut self.merged_pcm_scratch, &mut self.pcm_scratch);
            }

            self.frame_counter = self.frame_counter.saturating_add(1);
            if self.core_loaded && !self.active_game_id.is_empty() {
                self.playtime_frame_accum += 1.0 / native_fps_for_console(console);
                if self.playtime_frame_accum >= 1.0 {
                    let whole_secs = self.playtime_frame_accum.floor() as u64;
                    self.playtime_frame_accum -= whole_secs as f64;
                    if let Some(game) = self
                        .catalog
                        .iter_mut()
                        .find(|g| g.id == self.active_game_id)
                    {
                        game.playtime_seconds = game.playtime_seconds.saturating_add(whole_secs);
                        self.playtime_dirty = true;
                    }
                }
            }

            if !self.memory_watchpoints.is_empty() {
                self.core_p1.sync_debug_caches();
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
            }

            let rewind_stride = if console == ConsoleKind::Nds { 90 } else { 16 };
            if self.core_loaded
                && self.settings.rewind_enabled
                && meta.frame_number.is_multiple_of(rewind_stride)
            {
                self.capture_rewind_snapshot(meta.frame_number);
            }
            if self.recent_inputs_trace.len() >= 120 {
                self.recent_inputs_trace.remove(0);
            }
            self.recent_inputs_trace.push((mask_p1 & 0xFF) as u8);

            let sram_stride = if console == ConsoleKind::Nds { 60 } else { 16 };
            if self.core_loaded
                && self.frame_counter.is_multiple_of(sram_stride)
                && self.core_p1.sram_maybe_dirty()
            {
                self.refresh_sram_dirty_flag();
            }

            if s + 1 == steps {
                if steps > 1 && acc_count > 0 && self.merged_pcm_scratch.is_empty() {
                    let inv = 1.0 / (acc_count as f32);
                    self.merged_pcm_scratch.push(acc_l * inv);
                    self.merged_pcm_scratch.push(acc_r * inv);
                }
                last_meta = meta;
            }
        }

        if !self.achievements.is_empty() {
            for ach in &mut self.achievements {
                if !ach.unlocked && self.last_score >= ach.target_score {
                    ach.unlocked = true;
                    newly_unlocked_achievement = Some(format!(
                        "{} {} (+{} pts)",
                        ach.badge_symbol, ach.title, ach.points
                    ));
                }
            }
        }

        let emit_meta = opts.include_debug_meta
            || newly_unlocked_achievement.is_some()
            || self.active_macro_id.is_some();
        let meta_json = if emit_meta {
            let cpu = self.core_p1.debug_snapshot();
            self.last_score = cpu.score;
            serde_json::to_vec(&ViewportPacketMetadata {
                console,
                cpu: &cpu,
                link_telemetry: &self.link_telemetry,
                active_macro_id: self.active_macro_id.as_deref(),
                newly_unlocked_achievement: newly_unlocked_achievement.as_deref(),
            })
            .unwrap_or_else(|_| b"{}".to_vec())
        } else {
            Vec::new()
        };

        let emit_video = !opts.skip_render;
        let use_rgb565 = emit_video && opts.prefer_rgb565;
        let (width, height, pixel_bytes_len) = if !emit_video {
            (0u16, 0u16, 0usize)
        } else if use_rgb565 {
            self.rgb565_scratch.clear();
            let (w, h) = self.core_p1.encode_rgb565_into(&mut self.rgb565_scratch);
            (w, h, self.rgb565_scratch.len())
        } else {
            let (w, h, rgba) = self.core_p1.rgba_frame();
            (w, h, rgba.len())
        };

        let pcm_bytes_len = self.merged_pcm_scratch.len() * 4;
        let view_overhead = if emit_video { 8 + pixel_bytes_len } else { 0 };
        let total_cap = 35 + view_overhead + 4 + pcm_bytes_len + 4 + meta_json.len();
        let mut out = Vec::with_capacity(total_cap);

        let mut flags = 0u8;
        if self.sram_dirty {
            flags |= 0x04;
        }
        if self.active_macro_id.is_some() {
            flags |= 0x08;
        }
        if use_rgb565 {
            flags |= 0x10;
        }

        out.extend_from_slice(&VIEWPORT_BINARY_MAGIC);
        out.push(VIEWPORT_BINARY_VERSION);
        out.push(flags);
        out.extend_from_slice(&last_meta.sample_rate_hz.to_le_bytes());
        out.extend_from_slice(&last_meta.frame_number.to_le_bytes());
        out.extend_from_slice(&last_meta.audio_level.to_le_bytes());
        out.extend_from_slice(&last_meta.audio_freq_hz.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // reserved
        out.extend_from_slice(&(self.rewind_buffer.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.rewind_buffer.memory_usage_bytes() as u32).to_le_bytes());
        out.push(u8::from(emit_video));

        if emit_video {
            out.extend_from_slice(&width.to_le_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&(pixel_bytes_len as u32).to_le_bytes());
            if use_rgb565 {
                out.extend_from_slice(&self.rgb565_scratch);
            } else {
                let (_, _, rgba) = self.core_p1.rgba_frame();
                out.extend_from_slice(rgba);
            }
        }

        out.extend_from_slice(&(self.merged_pcm_scratch.len() as u32).to_le_bytes());
        for &sample in &self.merged_pcm_scratch {
            out.extend_from_slice(&sample.to_le_bytes());
        }

        out.extend_from_slice(&(meta_json.len() as u32).to_le_bytes());
        out.extend_from_slice(&meta_json);
        out
    }

    fn step_single_viewport_tick(
        &mut self,
        raw_buttons_p1: u16,
        raw_buttons_p2: u16,
        turbo_active: bool,
    ) -> ViewportStepResponse {
        let current_frame = self.frame_counter;
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
                match cheat.kind {
                    CheatKind::ActionReplayDs => {
                        self.core_p1.apply_nds_ar_cheat(&cheat.ar_ops);
                    }
                    CheatKind::GameSharkGb => {
                        self.core_p1.apply_ram_cheat(cheat.addr, cheat.value);
                    }
                }
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

        self.pump_online_link_pre_step(current_frame as u32, mask_p1);

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
        self.pump_online_link_post_step(current_frame as u32, mask_p1, primary.serial_out_byte);

        self.frame_counter = self.frame_counter.saturating_add(1);
        if self.core_loaded && !self.active_game_id.is_empty() {
            self.playtime_frame_accum += 1.0 / native_fps_for_console(primary.console);
            if self.playtime_frame_accum >= 1.0 {
                let whole_secs = self.playtime_frame_accum.floor() as u64;
                self.playtime_frame_accum -= whole_secs as f64;
                if let Some(game) = self
                    .catalog
                    .iter_mut()
                    .find(|g| g.id == self.active_game_id)
                {
                    game.playtime_seconds = game.playtime_seconds.saturating_add(whole_secs);
                    self.playtime_dirty = true;
                }
            }
        }

        if !self.memory_watchpoints.is_empty() {
            self.core_p1.sync_debug_caches();
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

        self.drain_completed_rewind_snapshots();
        let rewind_stride = if primary.console == ConsoleKind::Nds {
            90
        } else {
            16
        };
        if self.core_loaded
            && self.settings.rewind_enabled
            && primary.frame_number.is_multiple_of(rewind_stride)
        {
            self.capture_rewind_snapshot(primary.frame_number);
        }
        self.last_score = primary.cpu.score;
        if self.recent_inputs_trace.len() >= 120 {
            self.recent_inputs_trace.remove(0);
        }
        self.recent_inputs_trace.push((mask_p1 & 0xFF) as u8);
        let sram_stride = if primary.console == ConsoleKind::Nds {
            60
        } else {
            16
        };
        if self.core_loaded
            && self.frame_counter.is_multiple_of(sram_stride)
            && self.core_p1.sram_maybe_dirty()
        {
            self.refresh_sram_dirty_flag();
        }

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

    pub fn inspect_vram_and_disassembly(&mut self) -> DebuggerPayload {
        self.core_p1.sync_debug_caches();
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
        let frame = self.frame_counter;
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
        if !info.crc32_hex.is_empty() {
            let actual_crc = format_crc32_hex(compute_crc32(&raw));
            if !actual_crc.eq_ignore_ascii_case(&info.crc32_hex) {
                return Err(format!(
                    "Save State Slot #{slot} corrompu (CRC32 attendu {}, reçu {actual_crc})",
                    info.crc32_hex
                ));
            }
        }
        self.core_p1.load_state(&raw)?;
        self.refresh_sram_dirty_flag();
        Ok(info)
    }

    pub fn delete_save_state(&mut self, slot: u8) -> Result<Vec<SaveStateSlotInfo>, String> {
        if self.active_game_id.is_empty() {
            return Err("aucun jeu actif".into());
        }
        let slot = slot.clamp(1, 9);
        self.save_states_ram
            .remove(&(self.active_game_id.clone(), slot));
        Ok(self.list_save_slots_for(&self.active_game_id))
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
        self.drain_completed_rewind_snapshots();
        let Some((frame, raw_state)) = self.rewind_buffer.rewind_steps(steps_back)? else {
            return Err("tampon de rembobinage vide".into());
        };
        self.core_p1.load_state(&raw_state)?;
        self.rewind_generation = self.rewind_generation.wrapping_add(1);
        self.refresh_sram_dirty_flag();
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
        self.sync_sram_baseline();
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

    pub fn export_portable_pack(&mut self) -> Result<Vec<u8>, String> {
        self.sync_active_sram();
        let sram_raw = self.active_sram_bytes();
        let sram_rle = rle_compress(sram_raw);
        let payload_crc32_hex = format_crc32_hex(compute_crc32(sram_raw));
        let mut save_slots = Vec::new();
        for slot in 1..=9u8 {
            if let Some(persisted) = self.export_save_state_envelope(slot) {
                save_slots.push(persisted);
            }
        }
        let pack = ZaimulePortablePack {
            magic: PORTABLE_PACK_MAGIC.into(),
            version: "1.8.0".into(),
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
        if !pack.active_game_id.is_empty() {
            if self.catalog.iter().any(|g| g.id == pack.active_game_id) {
                self.select_game(&pack.active_game_id)?;
            } else if !self.active_game_id.is_empty() {
                return Err(format!(
                    "le pack portable cible le jeu '{}' qui n'est pas présent dans la bibliothèque",
                    pack.active_game_id
                ));
            }
        }
        if pack.sram_uncompressed_len > 0 || !pack.sram_rle.is_empty() {
            let raw_sram = rle_decompress(&pack.sram_rle, pack.sram_uncompressed_len)?;
            let actual_crc = format_crc32_hex(compute_crc32(&raw_sram));
            if !pack.payload_crc32_hex.is_empty()
                && !actual_crc.eq_ignore_ascii_case(&pack.payload_crc32_hex)
            {
                return Err(format!(
                    "somme de contrôle SRAM du pack portable invalide (attendu {}, obtenu {})",
                    pack.payload_crc32_hex, actual_crc
                ));
            }
            if self.has_persistable_sram() {
                self.core_p1.load_sram(&raw_sram)?;
                self.sram_dirty = true;
            }
        }
        for persisted in pack.save_slots {
            let target_game = if !self.active_game_id.is_empty() {
                self.active_game_id.clone()
            } else {
                pack.active_game_id.clone()
            };
            if !target_game.is_empty() {
                self.import_save_state_envelope(target_game, persisted);
            }
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
        self.core_p1.sync_debug_caches();
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
        if !self.active_game_id.is_empty() {
            let entry = self
                .settings
                .game_overrides
                .entry(self.active_game_id.clone())
                .or_default();
            entry.custom_cpu_overclock_pct = Some(clamped_oc);
            entry.custom_pgxp_subpixel = Some(pgxp_subpixel);
        }
        self.core_p1.set_cpu_overclock_pct(clamped_oc);
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
        self.macro_start_frame = self.frame_counter;
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
        self.sync_active_sram();
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
        self.sync_active_sram();
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
        self.core_p1.sync_debug_caches();
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

    pub fn remove_memory_watchpoint(&mut self, watch_id: &str) -> Result<StationBootstrap, String> {
        let before = self.memory_watchpoints.len();
        self.memory_watchpoints.retain(|wp| wp.id != watch_id);
        if self.memory_watchpoints.len() == before {
            return Err(format!("watchpoint mémoire introuvable : {watch_id}"));
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

        // Corrupting the expected CRC32 must fail restore_save_state before touching the core.
        if let Some((slot_info, _)) = session
            .save_states_ram
            .get_mut(&(session.active_game_id.clone(), 1))
        {
            slot_info.crc32_hex = "0xDEADBEEF".into();
        }
        let corrupt_err = session.restore_save_state(1).unwrap_err();
        assert!(corrupt_err.contains("CRC32"));

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

    #[test]
    fn p0_sram_dirty_frame_counter_slots_pack_crc_and_settings_roundtrip() {
        let mut session = EmulatorSession::new();
        let rom = test_gbc_battery_rom();
        let boot = session.import_custom_rom("arena.gbc", &rom).unwrap();
        assert_eq!(boot.save_slots.len(), 9);
        assert!(!session.sram_dirty);

        // Idle frames must NOT falsely mark SRAM dirty (prevents backup thrashing)
        for _ in 0..32 {
            session.step_viewport(0, 0, false);
        }
        assert_eq!(session.frame_counter, 32);
        assert!(!session.refresh_sram_dirty_flag());

        // Writing a byte into SRAM marks it dirty until mark_sram_flushed()
        session.write_active_sram_byte(0, 0x42).unwrap();
        assert!(session.sram_dirty);
        assert!(session.refresh_sram_dirty_flag());
        session.mark_sram_flushed();
        assert!(!session.sram_dirty);
        assert!(!session.refresh_sram_dirty_flag());

        // Empty merge_scanned_entries returns None instead of catalog.first()
        assert_eq!(session.merge_scanned_entries(Vec::new()), None);

        // Save slots 7..=9 work and round-trip through portable packs
        let s9 = session
            .create_save_state(9, "Slot Neuf".into(), 999)
            .unwrap();
        assert_eq!(s9.slot, 9);
        assert_eq!(s9.frame, 32);
        let pack_bytes = session.export_portable_pack().unwrap();

        // Corrupting the CRC inside the portable pack must be rejected
        let mut bad_pack = decode_portable_pack(&pack_bytes).unwrap();
        bad_pack.payload_crc32_hex = "DEADBEEF".into();
        let bad_bytes = encode_portable_pack(&bad_pack).unwrap();
        assert!(session.import_portable_pack(&bad_bytes).is_err());

        // Valid pack re-imports slot 9 and marks SRAM dirty for persistence
        session.save_states_ram.clear();
        session.mark_sram_flushed();
        let after_pack = session.import_portable_pack(&pack_bytes).unwrap();
        assert!(after_pack.save_slots[8].occupied);
        assert!(session.sram_dirty);

        // Macros advance by 1 real frame per step (not cycles / 256)
        session
            .save_custom_macro("Combo 3f".into(), vec![BTN_A, BTN_RIGHT, BTN_A])
            .unwrap();
        assert!(session.active_macro_id.is_some());
        session.step_viewport(0, 0, false);
        assert_eq!(
            session.recent_inputs_trace.last().copied(),
            Some(BTN_A as u8)
        );
        session.step_viewport(0, 0, false);
        assert_eq!(
            session.recent_inputs_trace.last().copied(),
            Some(BTN_RIGHT as u8)
        );
        session.step_viewport(0, 0, false);
        assert_eq!(
            session.recent_inputs_trace.last().copied(),
            Some(BTN_A as u8)
        );
        session.set_active_macro(None);
        assert!(session.active_macro_id.is_none());

        // Settings sync & restore round-trips keymapper, cheats, macros, and UI preferences
        session
            .add_cheat_code("01FF04C0", "Max Gold".into())
            .unwrap();
        session.update_ui_preferences(
            Some("mode-lcd".into()),
            Some("XBRZ".into()),
            Some(4),
            Some("horizontal".into()),
        );
        session.sync_runtime_to_settings();
        let saved_json = serde_json::to_string(&session.settings).unwrap();

        let mut restored = EmulatorSession::new();
        let parsed: GlobalSettings = serde_json::from_str(&saved_json).unwrap();
        restored.apply_loaded_settings(parsed);
        let snap = restored.bootstrap_snapshot();
        assert_eq!(snap.active_shader_css, "mode-lcd");
        assert_eq!(snap.upscaler_mode, "XBRZ");
        assert_eq!(snap.speed_multiplier, 4);
        assert_eq!(snap.nds_layout, "horizontal");
        assert_eq!(snap.cheat_codes.len(), 1);
        assert_eq!(snap.macros.len(), 1);
    }

    #[test]
    fn p1_bios_import_delete_slot_favorite_overclock_and_ff_audio() {
        use std::fs;
        use zaimule_core::bios::BiosStatus;

        let root = std::env::temp_dir().join(format!("zaimule-p1-sess-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sys_dir = root.join("system");
        fs::create_dir_all(&sys_dir).unwrap();

        let bios7 = vec![0x33u8; 16_384];
        let bios9 = vec![0x44u8; 4_096];
        let firmware = vec![0x55u8; 262_144];
        let zip_path = root.join("NDS BIOS & Firmware.zip");
        fs::write(
            &zip_path,
            build_stored_zip_for_test(&[
                ("bios7.bin", bios7.as_slice()),
                ("bios9.bin", bios9.as_slice()),
                ("firmware.bin", firmware.as_slice()),
            ]),
        )
        .unwrap();

        let mut session = EmulatorSession::new();
        session.configure_paths(sys_dir, root.join("sav"), root.join("cache"));

        let (report, snap_after_bios) = session.import_bios_paths(&[zip_path]).unwrap();
        assert_eq!(report.imported.len(), 3);
        let fw_entry = snap_after_bios
            .bios_entries
            .iter()
            .find(|e| e.file_name == "firmware.bin")
            .unwrap();
        assert!(matches!(fw_entry.status, BiosStatus::CustomLoaded { .. }));

        // Load NDS ROM with imported BIOS assets active + overclock profile
        let nds_boot = session
            .import_custom_rom("platine.nds", &test_nds_rom())
            .unwrap();
        let nds_id = nds_boot.active_game_id.clone();
        assert!(nds_boot.catalog[0].favorite);

        // Toggle favorite off and back on
        let toggled_off = session.toggle_game_favorite(&nds_id).unwrap();
        assert!(!toggled_off.catalog[0].favorite);
        let toggled_on = session.toggle_game_favorite(&nds_id).unwrap();
        assert!(toggled_on.catalog[0].favorite);

        // Overclock + color correction applies cleanly to NDS core and survives re-select
        let oc_snap = session.set_hardware_overclock_profile(150, true);
        assert_eq!(oc_snap.effective_config.cpu_overclock_pct, 150);
        session.select_game(&nds_id).unwrap();
        let step_1x = session.step_viewport_with_speed(0, 0, false, 1);
        let step_2x = session.step_viewport_with_speed(0, 0, false, 2);
        assert!(!step_1x.primary.audio_samples.is_empty());
        assert!(!step_2x.primary.audio_samples.is_empty());
        assert_eq!(step_2x.primary.audio_samples.len() % 2, 0);
        // Box-averaged 2x PCM should match ~1x frame sample count closely
        assert!(
            step_2x
                .primary
                .audio_samples
                .len()
                .abs_diff(step_1x.primary.audio_samples.len())
                <= 4
        );

        // Create save slots #1 and #3, then delete slot #3 while preserving slot #1
        session.create_save_state(1, "Slot 1".into(), 100).unwrap();
        session.create_save_state(3, "Slot 3".into(), 200).unwrap();
        let slots_after_del = session.delete_save_state(3).unwrap();
        assert!(slots_after_del[0].occupied);
        assert!(!slots_after_del[2].occupied);
        assert!(session.restore_save_state(3).is_err());

        // Lazy debug cache scan/inspect works without per-frame copies
        let _ = session.scan_active_ram(0x00);
        let dbg = session.inspect_vram_and_disassembly();
        assert_eq!(dbg.disassembly.len(), 10);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn p2_reset_playtime_lan_netplay_folders_and_removals() {
        use std::fs;

        let root = std::env::temp_dir().join(format!("zaimule-p2-sess-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir_a = root.join("roms_a");
        let dir_b = root.join("roms_b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        fs::write(dir_a.join("arena.gbc"), test_gbc_battery_rom()).unwrap();

        let mut session = EmulatorSession::new();
        session.configure_paths(root.join("sys"), root.join("sav"), root.join("cache"));

        // 1. Scan directory A, then add a new ROM in directory B and test rescan_all + remove_rom_directory
        let rep_a = session.scan_directory_and_merge(&dir_a);
        assert_eq!(rep_a.imported_entries.len(), 1);
        let rep_b = session.scan_directory_and_merge(&dir_b);
        assert_eq!(rep_b.imported_entries.len(), 0);
        assert_eq!(session.settings.rom_directories.len(), 2);

        fs::write(dir_b.join("horizon.gba"), test_gba_sram_rom()).unwrap();
        let rescan = session.rescan_all_rom_directories();
        assert_eq!(rescan.imported_entries.len(), 2);
        assert_eq!(session.catalog.len(), 2);

        let dir_b_str = dir_b.to_string_lossy().to_string();
        let after_rm_dir = session.remove_rom_directory(&dir_b_str).unwrap();
        assert_eq!(after_rm_dir.rom_directories.len(), 1);
        assert!(session.remove_rom_directory(&dir_b_str).is_err());

        // 2. Native playtime accumulates on stepped frames and sets playtime_dirty
        let gbc_id = session
            .catalog
            .iter()
            .find(|g| g.console == ConsoleKind::Gbc)
            .unwrap()
            .id
            .clone();
        session.select_game(&gbc_id).unwrap();
        assert!(!session.take_playtime_dirty());
        for _ in 0..65 {
            session.step_viewport(0, 0, false);
        }
        assert!(session.take_playtime_dirty());
        assert!(!session.take_playtime_dirty());
        let playtime = session
            .catalog
            .iter()
            .find(|g| g.id == gbc_id)
            .unwrap()
            .playtime_seconds;
        assert!(playtime >= 1);

        // 3. Hardware cold reset clears frame_counter & rewind buffer while preserving battery SRAM + dirty flag
        session.write_active_sram_byte(0, 0xAB).unwrap();
        assert!(session.sram_dirty);
        assert!(session.frame_counter >= 65);
        assert!(!session.rewind_buffer.is_empty());
        let reset_snap = session.reset_active_game().unwrap();
        assert_eq!(reset_snap.active_game_id, gbc_id);
        assert_eq!(session.frame_counter, 0);
        assert!(session.rewind_buffer.is_empty());
        assert_eq!(session.active_sram_bytes()[0], 0xAB);
        assert!(session.sram_dirty);

        // 4. LAN UDP peer validation, connection, and clean disconnection (no fabricated STUN 198.51.100.x)
        assert!(!reset_snap
            .stun_candidate
            .public_endpoint
            .contains("198.51.100"));
        assert!(session.connect_netplay_peer(Some("not-an-ip")).is_err());
        assert!(session.connect_netplay_peer(Some("127.0.0.1:0")).is_err());
        let connected = session
            .connect_netplay_peer(Some("127.0.0.1:54999"))
            .unwrap();
        assert_eq!(connected.udp_peer_addr.as_deref(), Some("127.0.0.1:54999"));
        let disconnected = session.connect_netplay_peer(None).unwrap();
        assert_eq!(disconnected.udp_peer_addr, None);

        // 5. Cheat code and memory watchpoint removal
        let with_cheat = session
            .add_cheat_code("01FF04C0", "Max Gold".into())
            .unwrap();
        let cheat_id = with_cheat.cheat_codes[0].id.clone();
        let without_cheat = session.remove_cheat_code(&cheat_id).unwrap();
        assert!(without_cheat.cheat_codes.is_empty());
        assert!(session.remove_cheat_code(&cheat_id).is_err());

        let with_wp = session
            .add_memory_watchpoint("0xC010", "HP", false)
            .unwrap();
        let wp_id = with_wp.memory_watchpoints[0].id.clone();
        let without_wp = session.remove_memory_watchpoint(&wp_id).unwrap();
        assert!(without_wp.memory_watchpoints.is_empty());
        assert!(session.remove_memory_watchpoint(&wp_id).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nds_binary_rgb565_skip_render_and_background_rewind_worker() {
        let mut session = EmulatorSession::new();
        session
            .import_custom_rom("heartgold_smoke.nds", &test_nds_rom())
            .unwrap();

        // 1. RGB565 binary packet halves pixel payload vs RGBA8888 and sets flags bit 0x10
        let rgba_pkt = session.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions {
                include_debug_meta: false,
                prefer_rgb565: false,
                skip_render: false,
            },
        );
        let rgb565_pkt = session.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions {
                include_debug_meta: false,
                prefer_rgb565: true,
                skip_render: false,
            },
        );
        assert_eq!(&rgb565_pkt[0..4], b"ZMFR");
        assert_eq!(rgba_pkt[5] & 0x10, 0);
        assert_ne!(rgb565_pkt[5] & 0x10, 0);
        assert_eq!(rgb565_pkt[34], 1, "view_count should be 1");
        let w = u16::from_le_bytes([rgb565_pkt[35], rgb565_pkt[36]]) as usize;
        let h = u16::from_le_bytes([rgb565_pkt[37], rgb565_pkt[38]]) as usize;
        let rgba_px_len = u32::from_le_bytes(rgba_pkt[39..43].try_into().unwrap()) as usize;
        let rgb565_px_len = u32::from_le_bytes(rgb565_pkt[39..43].try_into().unwrap()) as usize;
        assert_eq!(rgba_px_len, w * h * 4);
        assert_eq!(rgb565_px_len, w * h * 2);

        // 2. skip_render emits view_count == 0 (no pixel payload) while keeping full PCM audio
        let skipped_pkt = session.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions {
                include_debug_meta: false,
                prefer_rgb565: true,
                skip_render: true,
            },
        );
        assert_eq!(&skipped_pkt[0..4], b"ZMFR");
        assert_eq!(skipped_pkt[5] & 0x10, 0);
        assert_eq!(skipped_pkt[34], 0, "skip_render must emit view_count = 0");
        let pcm_samples = u32::from_le_bytes(skipped_pkt[35..39].try_into().unwrap());
        assert!(
            pcm_samples > 0,
            "skip_render must still emit 48 kHz PCM audio"
        );

        // 3. Stepping past NDS rewind stride (90 frames) dispatches background ZMND compression
        //    and populates the rewind buffer without stalling the emulation thread
        for _ in 0..90 {
            let _ = session.step_viewport_binary_bytes_with_options(
                0,
                0,
                false,
                1,
                BinaryStepOptions {
                    include_debug_meta: false,
                    prefer_rgb565: true,
                    skip_render: true,
                },
            );
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while session.rewind_buffer.is_empty() && std::time::Instant::now() < deadline {
            session.drain_completed_rewind_snapshots();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !session.rewind_buffer.is_empty(),
            "background rewind worker should push a compressed ZMND snapshot"
        );
        let rewound_frame = session
            .rewind(1)
            .expect("rewind(1) should restore ZMND snapshot");
        assert!(rewound_frame >= 90);
    }

    #[test]
    fn library_enrichment_collections_metadata_and_rescan_preservation() {
        let dir =
            std::env::temp_dir().join(format!("zaimule-session-enrich-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut rom = vec![0u8; 0x200];
        rom[0x0000..0x000A].copy_from_slice(b"POKEMON HG");
        rom[0x000C..0x0010].copy_from_slice(b"IPKF");
        rom[0x015C] = 0x56;
        rom[0x015D] = 0xCF;
        std::fs::write(dir.join("Pokemon_HeartGold.nds"), &rom).unwrap();

        let mut session = EmulatorSession::new();
        let rep = session.scan_directory_and_merge(&dir);
        assert_eq!(rep.imported_entries.len(), 1);
        let game_id = rep.imported_entries[0].id.clone();

        session.create_collection("  Pokémon  ").unwrap();
        session.create_collection("À finir").unwrap();
        let boot = session.toggle_game_collection(&game_id, "pokémon").unwrap();
        assert_eq!(
            boot.collections,
            vec!["Pokémon".to_string(), "À finir".to_string()]
        );
        assert_eq!(boot.catalog[0].collections, vec!["Pokémon".to_string()]);

        let boot = session
            .update_game_metadata(
                &game_id,
                GameMetadataUpdate {
                    title: Some("Pokémon Version Or HeartGold".into()),
                    genre: Some("Role-playing (RPG)".into()),
                    developer: Some("Game Freak".into()),
                    publisher: Some("Nintendo".into()),
                    release_year: Some("2010".into()),
                    description: Some("Voyage à Johto & Kanto".into()),
                    canonical_name: Some("Pokemon - HeartGold Version (France)".into()),
                },
            )
            .unwrap();
        assert_eq!(boot.catalog[0].title, "Pokémon Version Or HeartGold");
        assert_eq!(
            boot.catalog[0].metadata_release_year.as_deref(),
            Some("2010")
        );

        let boot = session
            .set_game_cover_path(&game_id, Some("rom-heartgold.jpg".into()))
            .unwrap();
        assert_eq!(
            boot.catalog[0].cover_path.as_deref(),
            Some("rom-heartgold.jpg")
        );

        // Rescanning the watched folder must preserve title, cover, metadata, and collections!
        let _ = session.rescan_all_rom_directories();
        let after = session.catalog.iter().find(|g| g.id == game_id).unwrap();
        assert_eq!(after.title, "Pokémon Version Or HeartGold");
        assert_eq!(after.cover_path.as_deref(), Some("rom-heartgold.jpg"));
        assert_eq!(after.metadata_developer.as_deref(), Some("Game Freak"));
        assert_eq!(after.collections, vec!["Pokémon".to_string()]);

        // Persisted library roundtrip preserves collections + metadata
        let persisted = session.to_persisted_library();
        let mut restored = EmulatorSession::new();
        restored.restore_persisted_library(persisted);
        assert_eq!(restored.collections.len(), 2);
        assert_eq!(restored.catalog[0].collections, vec!["Pokémon".to_string()]);

        // Deleting collection removes tag from games too
        let after_del = restored.delete_collection("pokémon").unwrap();
        assert_eq!(after_del.collections, vec!["À finir".to_string()]);
        assert!(after_del.catalog[0].collections.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn online_gb_link_cable_exchanges_serial_bytes_over_zmr1_e2ee() {
        use zaimule_core::netplay::{RelayChannel, RelaySessionKey, Zmr1Cipher};

        let build_gb_serial_rom = |sb_val: u8, sc_val: u8| -> Vec<u8> {
            let mut rom = vec![0u8; 0x8000];
            rom[0x0100] = 0xC3;
            rom[0x0101] = 0x50;
            rom[0x0102] = 0x01;
            let code: &[u8] = &[
                0x3E, sb_val, 0xE0, 0x01, // LD A, sb_val; LDH (SB), A
                0x3E, sc_val, 0xE0, 0x02, // LD A, sc_val; LDH (SC), A
                0x18, 0xFE, // JR -2
            ];
            rom[0x0150..0x0150 + code.len()].copy_from_slice(code);
            rom
        };

        let mut master = EmulatorSession::new();
        master
            .import_custom_rom("master.gb", &build_gb_serial_rom(0xA1, 0x81))
            .unwrap();
        master.set_online_link_enabled(true);

        let mut slave = EmulatorSession::new();
        slave
            .import_custom_rom("slave.gb", &build_gb_serial_rom(0xB2, 0x80))
            .unwrap();
        slave.set_online_link_enabled(true);

        let key = RelaySessionKey::generate().unwrap();
        let cipher_master = Zmr1Cipher::new(&key).unwrap();
        let cipher_slave = Zmr1Cipher::new(&key).unwrap();

        // 1. Les deux consoles avancent d'une frame : Master initie TX (0xA1), Slave charge SB=0xB2
        let _ = master.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );
        let _ = slave.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );

        let master_tx = master.drain_outgoing_link_payloads();
        assert_eq!(master_tx.len(), 1);
        assert_eq!(master_tx[0].sio_sb, 0xA1);
        assert!(master_tx[0].is_master_tx());

        // 2. Chiffrement ZMR1 Master -> Relais opaque -> Déchiffrement Slave
        let sealed_m2s = cipher_master
            .seal(RelayChannel::GbLink, "LINK42", &master_tx[0].to_bytes())
            .unwrap();
        let opened_m2s = cipher_slave
            .open(RelayChannel::GbLink, "LINK42", &sealed_m2s)
            .unwrap();
        let decoded_m2s = GbLinkPayload::from_bytes(&opened_m2s).unwrap();
        slave.push_remote_link_payload(decoded_m2s);

        // 3. Slave consomme 0xA1 et répond automatiquement son registre SB (0xB2)
        let _ = slave.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );
        assert_eq!(slave.core_p1.debug_snapshot().serial_sb_hex, "0xA1");

        let slave_replies = slave.drain_outgoing_link_payloads();
        assert_eq!(slave_replies.len(), 1);
        assert_eq!(slave_replies[0].sio_sb, 0xB2);
        assert!(slave_replies[0].is_slave_reply());

        // 4. Chiffrement ZMR1 Slave -> Relais opaque -> Déchiffrement Master
        let sealed_s2m = cipher_slave
            .seal(RelayChannel::GbLink, "LINK42", &slave_replies[0].to_bytes())
            .unwrap();
        let opened_s2m = cipher_master
            .open(RelayChannel::GbLink, "LINK42", &sealed_s2m)
            .unwrap();
        master.push_remote_link_payload(GbLinkPayload::from_bytes(&opened_s2m).unwrap());

        let _ = master.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );
        assert_eq!(master.core_p1.debug_snapshot().serial_sb_hex, "0xB2");
        // Pas de boucle infinie d'écho après réception d'une réponse esclave
        assert!(master.drain_outgoing_link_payloads().is_empty());
    }

    #[test]
    fn relay_config_validates_tls_and_updates_bootstrap_safely() {
        let mut session = EmulatorSession::new();
        assert!(session
            .set_relay_config(Some("ws://public.example.org/ws".into()), None, false, None)
            .is_err());
        assert!(session
            .set_relay_config(None, Some("short".into()), false, None)
            .is_err());

        let boot = session
            .set_relay_config(
                Some("wss://relay.example.org".into()),
                Some("0123456789abcdef0123456789abcdef".into()),
                false,
                Some("Sacha".into()),
            )
            .unwrap();
        assert_eq!(
            boot.relay_url.as_deref(),
            Some("wss://relay.example.org/ws")
        );
        assert!(boot.relay_key_configured);
        assert_eq!(boot.relay_player_name.as_deref(), Some("Sacha"));

        let cleared = session.set_relay_config(None, None, true, None).unwrap();
        assert!(!cleared.relay_key_configured);
    }

    #[test]
    fn online_gba_link_cable_exchanges_16bit_words_over_zmr1_e2ee() {
        use zaimule_core::netplay::{RelayChannel, RelaySessionKey, Zmr1Cipher};

        let build_gba_sio_rom = |send_word: u16, siocnt_val: u16| -> Vec<u8> {
            let mut rom = vec![0u8; 0x200];
            let insns: [u32; 9] = [
                0xE3A0_0404, // MOV r0, #0x04000000
                0xE380_0C01, // ORR r0, r0, #0x100
                0xE3A0_1000, // MOV r1, #0
                0xE1C0_13B4, // STRH r1, [r0, #0x34] (RCNT = 0)
                0xE1DF_20BC, // LDRH r2, [pc, #0x0C] (@0x24)
                0xE1C0_22BA, // STRH r2, [r0, #0x2A] (SIOMLT_SEND)
                0xE1DF_30B6, // LDRH r3, [pc, #0x06] (@0x26)
                0xE1C0_32B8, // STRH r3, [r0, #0x28] (SIOCNT)
                0xEAFF_FFFE, // B .
            ];
            for (idx, insn) in insns.iter().enumerate() {
                rom[idx * 4..idx * 4 + 4].copy_from_slice(&insn.to_le_bytes());
            }
            rom[0x24..0x26].copy_from_slice(&send_word.to_le_bytes());
            rom[0x26..0x28].copy_from_slice(&siocnt_val.to_le_bytes());
            rom
        };

        let mut master = EmulatorSession::new();
        master
            .import_custom_rom("master.gba", &build_gba_sio_rom(0xCAFE, 0x6083))
            .unwrap();
        master.set_online_link_enabled_with_role(true, 0);

        let mut slave = EmulatorSession::new();
        slave
            .import_custom_rom("slave.gba", &build_gba_sio_rom(0xBEEF, 0x6003))
            .unwrap();
        slave.set_online_link_enabled_with_role(true, 1);

        let key = RelaySessionKey::generate().unwrap();
        let cipher_master = Zmr1Cipher::new(&key).unwrap();
        let cipher_slave = Zmr1Cipher::new(&key).unwrap();

        let _ = master.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );
        let _ = slave.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );

        let master_tx = master.drain_outgoing_gba_link_payloads();
        assert_eq!(master_tx.len(), 1);
        assert_eq!(master_tx[0].siomlt_send, 0xCAFE);
        assert!(master_tx[0].is_master_tx());
        assert_eq!(master_tx[0].player_id, 0);

        // Chiffrement ZMR1 Master -> Slave sur RelayChannel::GbaLink
        let sealed_m2s = cipher_master
            .seal(RelayChannel::GbaLink, "GBA42P", &master_tx[0].to_bytes())
            .unwrap();
        let opened_m2s = cipher_slave
            .open(RelayChannel::GbaLink, "GBA42P", &sealed_m2s)
            .unwrap();
        slave.push_remote_gba_link_payload(GbaLinkPayload::from_bytes(&opened_m2s).unwrap());

        let _ = slave.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );
        let slave_replies = slave.drain_outgoing_gba_link_payloads();
        assert_eq!(slave_replies.len(), 1);
        assert_eq!(slave_replies[0].siomlt_send, 0xBEEF);
        assert!(slave_replies[0].is_slave_reply());
        assert_eq!(slave_replies[0].player_id, 1);
        assert_eq!(slave.link_telemetry.last_p2_to_p1_hex, "0xCAFE");

        // Chiffrement ZMR1 Slave -> Master
        let sealed_s2m = cipher_slave
            .seal(
                RelayChannel::GbaLink,
                "GBA42P",
                &slave_replies[0].to_bytes(),
            )
            .unwrap();
        let opened_s2m = cipher_master
            .open(RelayChannel::GbaLink, "GBA42P", &sealed_s2m)
            .unwrap();
        master.push_remote_gba_link_payload(GbaLinkPayload::from_bytes(&opened_s2m).unwrap());

        let _ = master.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );
        assert_eq!(master.link_telemetry.last_p2_to_p1_hex, "0xBEEF");
        // Bit 7 (Start/Busy) effacé dans SIOCNT après livraison SIOMULTI0..3
        let sc = u16::from_str_radix(
            master
                .core_p1
                .debug_snapshot()
                .serial_sc_hex
                .trim_start_matches("0x"),
            16,
        )
        .unwrap();
        assert_eq!(sc & 0x0080, 0);
        assert!(master.drain_outgoing_gba_link_payloads().is_empty());
    }

    #[test]
    fn online_nds_wifi_exchanges_80211_frames_between_two_arm7_cpus_over_zmr1_e2ee() {
        use zaimule_core::netplay::{RelayChannel, RelaySessionKey, Zmr1Cipher};

        fn build_arm7_wifi_nds_rom(arm7_code: &[u32], literals: &[u32]) -> Vec<u8> {
            let mut rom = vec![0u8; 0x1000];
            rom[0x0000..0x000C].copy_from_slice(b"ZAIMULEWIFI\0");
            rom[0x000C..0x0010].copy_from_slice(b"ZMWF");
            let arm9_off: u32 = 0x200;
            let arm9_addr: u32 = 0x0200_0800;
            let arm9_size: u32 = 0x10;
            let arm7_off: u32 = 0x400;
            let arm7_addr: u32 = 0x0380_0000;
            let arm7_size: u32 = ((arm7_code.len() + literals.len()) * 4) as u32;
            rom[0x20..0x24].copy_from_slice(&arm9_off.to_le_bytes());
            rom[0x24..0x28].copy_from_slice(&arm9_addr.to_le_bytes());
            rom[0x28..0x2C].copy_from_slice(&arm9_addr.to_le_bytes());
            rom[0x2C..0x30].copy_from_slice(&arm9_size.to_le_bytes());
            rom[0x30..0x34].copy_from_slice(&arm7_off.to_le_bytes());
            rom[0x34..0x38].copy_from_slice(&arm7_addr.to_le_bytes());
            rom[0x38..0x3C].copy_from_slice(&arm7_addr.to_le_bytes());
            rom[0x3C..0x40].copy_from_slice(&arm7_size.to_le_bytes());
            // ARM9: B .
            rom[arm9_off as usize..arm9_off as usize + 4]
                .copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
            // ARM7 code + literal pool
            let mut cursor = arm7_off as usize;
            for &w in arm7_code.iter().chain(literals.iter()) {
                rom[cursor..cursor + 4].copy_from_slice(&w.to_le_bytes());
                cursor += 4;
            }
            rom
        }

        // Helper encodant `LDR Rd, [PC, #(pool_idx * 4 - pc_plus_8)]`
        let ldr_lit = |rd: u32, instr_idx: usize, code_len: usize, lit_idx: usize| -> u32 {
            let off = ((code_len + lit_idx) * 4) as i32 - ((instr_idx * 4 + 8) as i32);
            assert!((0..4096).contains(&off));
            0xE59F_0000 | (rd << 12) | (off as u32)
        };

        // DS #1 (Hôte ARM7) : écrit une trame 802.11b broadcast dans Wi-Fi RAM 0x0480_4A00
        // (TXHDR[8..12] = rate 0x14, len 32 octets incl. FCS ; FC=0x0008 Data, Dst=FF:FF:FF:FF:FF:FF)
        // puis déclenche W_TXREQ_SET=1 et W_TXBUF_LOC1=0x8500.
        let host_lits: &[u32] = &[
            0x0480_4A00, // [0] TX buffer byte base in Wi-Fi RAM (hw 0x0500)
            0x0020_0014, // [1] TXHDR[8..12]: rate=0x14, len_with_fcs=32 (28B MPDU)
            0x0000_0008, // [2] 802.11 FC=0x0008 (Data), Duration=0
            0xFFFF_FFFF, // [3] Dst MAC[0..4] = FF:FF:FF:FF
            0x0000_FFFF, // [4] Dst MAC[4..6] = FF:FF
            0x5344_4B4D, // [5] Payload "MKDS" at MPDU+24
            0x0480_80AE, // [6] W_TXREQ_SET
            0x0480_80A0, // [7] W_TXBUF_LOC1
            0x0000_8500, // [8] Enable TX slot at hw offset 0x0500
        ];
        let host_code_len = 19usize;
        let host_code: Vec<u32> = vec![
            ldr_lit(0, 0, host_code_len, 0),  // R0 = 0x0480_4A00
            ldr_lit(1, 1, host_code_len, 1),  // R1 = 0x0020_0014
            0xE580_1008,                      // STR R1, [R0, #8]   (TXHDR rate+len)
            ldr_lit(1, 3, host_code_len, 2),  // R1 = 0x0000_0008
            0xE580_100C,                      // STR R1, [R0, #12]  (802.11 FC)
            ldr_lit(1, 5, host_code_len, 3),  // R1 = 0xFFFF_FFFF
            0xE580_1010,                      // STR R1, [R0, #16]  (Dst 0..4)
            ldr_lit(1, 7, host_code_len, 4),  // R1 = 0x0000_FFFF
            0xE580_1014,                      // STR R1, [R0, #20]  (Dst 4..6)
            ldr_lit(1, 9, host_code_len, 5),  // R1 = "MKDS"
            0xE580_1024,                      // STR R1, [R0, #36]  (MPDU+24)
            ldr_lit(2, 11, host_code_len, 6), // R2 = W_TXREQ_SET
            0xE3A0_3001,                      // MOV R3, #1
            0xE1C2_30B0,                      // STRH R3, [R2]
            ldr_lit(2, 14, host_code_len, 7), // R2 = W_TXBUF_LOC1
            ldr_lit(3, 15, host_code_len, 8), // R3 = 0x8500
            0xE1C2_30B0,                      // STRH R3, [R2]      (Kick TX!)
            0xE1A0_0000,                      // NOP
            0xEAFF_FFFE,                      // B .
        ];
        assert_eq!(host_code.len(), host_code_len);

        // DS #2 (Invité ARM7) : configure RX circulaire 0x4000..0x4800 + W_IE=1, attend W_IF&1,
        // puis écrit le marqueur 0xCAFE en Main RAM (0x0200_0100) dès réception de la trame !
        let guest_lits: &[u32] = &[
            0x0480_8000, // [0] Wi-Fi I/O base
            0x0000_4000, // [1] W_RXBUF_BEGIN
            0x0000_4800, // [2] W_RXBUF_END
            0x0000_8001, // [3] W_RXCNT (enable RX + latch WRCSR)
            0x0200_0100, // [4] Main RAM shared flag
            0x0000_CAFE, // [5] Proof marker
        ];
        let guest_code_len = 22usize;
        let guest_code: Vec<u32> = vec![
            ldr_lit(0, 0, guest_code_len, 0), // 0: R0 = 0x0480_8000
            ldr_lit(1, 1, guest_code_len, 1), // 1: R1 = 0x4000
            0xE1C0_15B0,                      // 2: STRH R1, [R0, #0x50] (W_RXBUF_BEGIN)
            ldr_lit(1, 3, guest_code_len, 2), // 3: R1 = 0x4800
            0xE1C0_15B2,                      // 4: STRH R1, [R0, #0x52] (W_RXBUF_END)
            0xE3A0_1000,                      // 5: MOV R1, #0
            0xE1C0_15B6,                      // 6: STRH R1, [R0, #0x56] (W_RXBUF_WR_ADDR)
            0xE1C0_15B8,                      // 7: STRH R1, [R0, #0x58] (W_RXBUF_RD_ADDR)
            ldr_lit(1, 8, guest_code_len, 3), // 8: R1 = 0x8001
            0xE1C0_13B0,                      // 9: STRH R1, [R0, #0x30] (W_RXCNT)
            0xE3A0_1001,                      // 10: MOV R1, #1
            0xE1C0_11B2,                      // 11: STRH R1, [R0, #0x12] (W_IE = RX_COMPLETE)
            // Poll loop at idx 12:
            0xE1D0_21B0,                       // 12: LDRH R2, [R0, #0x10] (W_IF)
            0xE312_0001,                       // 13: TST R2, #1
            0x0AFF_FFFC,                       // 14: BEQ -16 (back to idx 12)
            ldr_lit(4, 15, guest_code_len, 4), // 15: R4 = 0x0200_0100
            ldr_lit(5, 16, guest_code_len, 5), // 16: R5 = 0xCAFE
            0xE584_5000,                       // 17: STR R5, [R4]
            0xE1A0_0000,                       // 18: NOP
            0xE1A0_0000,                       // 19: NOP
            0xE1A0_0000,                       // 20: NOP
            0xEAFF_FFFE,                       // 21: B .
        ];
        assert_eq!(guest_code.len(), guest_code_len);

        let host_rom = build_arm7_wifi_nds_rom(&host_code, host_lits);
        let guest_rom = build_arm7_wifi_nds_rom(&guest_code, guest_lits);

        let mut host = EmulatorSession::new();
        let mut guest = EmulatorSession::new();
        host.configure_nds_wifi(NdsWifiMode::LocalNiFi, 1, None);
        guest.configure_nds_wifi(NdsWifiMode::LocalNiFi, 2, None);

        host.import_custom_rom("MKDS_H.nds", &host_rom).unwrap();
        guest.import_custom_rom("MKDS_G.nds", &guest_rom).unwrap();

        // MAC uniques par pair vérifiées
        assert_eq!(host.nds_wifi_telemetry().mac_address, "00:09:BF:5A:4D:01");
        assert_eq!(guest.nds_wifi_telemetry().mac_address, "00:09:BF:5A:4D:02");

        // Frame 1 : l'ARM7 de l'hôte émet la trame 802.11b et l'invité configure RX
        let _ = host.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );
        let _ = guest.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );

        let tx_pkts = host.drain_outgoing_nds_wifi_packets();
        assert_eq!(tx_pkts.len(), 1);
        assert_eq!(&tx_pkts[0].frame[24..28], b"MKDS");

        // Chiffrement bout en bout ZMR1 sur RelayChannel::NdsWifi = 6
        let key = RelaySessionKey::generate().unwrap();
        let c_host = Zmr1Cipher::new(&key).unwrap();
        let c_guest = Zmr1Cipher::new(&key).unwrap();
        let sealed = c_host
            .seal(
                RelayChannel::NdsWifi,
                "MKDS42",
                &tx_pkts[0].to_bytes().unwrap(),
            )
            .unwrap();
        let opened = c_guest
            .open(RelayChannel::NdsWifi, "MKDS42", &sealed)
            .unwrap();
        let decoded = NdsWifiPacket::from_bytes(&opened).unwrap();

        // Injecter la trame dans DS #2 et avancer d'1 frame : l'ARM7 détecte W_IF&1 et écrit 0xCAFE !
        guest.push_remote_nds_wifi_packet(decoded);
        let _ = guest.step_viewport_binary_bytes_with_options(
            0,
            0,
            false,
            1,
            BinaryStepOptions::default(),
        );

        guest.core_p1.sync_debug_caches();
        let ram = guest.core_p1.read_ram_slice();
        let marker = u32::from_le_bytes([ram[0x100], ram[0x101], ram[0x102], ram[0x103]]);
        assert_eq!(marker, 0x0000_CAFE);
        let tel = guest.nds_wifi_telemetry();
        assert_eq!(tel.rx_frames, 1);
        assert!(tel.irq_count >= 1);
    }
}
