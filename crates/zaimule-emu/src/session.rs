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
    LibretroCoreDescriptor,
};
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
use zaimule_core::bios::{default_bios_registry, BiosEntry};
use zaimule_core::catalog::{
    compute_crc32, default_catalog, detect_console, extract_rom_title, format_crc32_hex,
    ConsoleKind, CoreEngineKind, GameEntry,
};
use zaimule_core::cheats::{
    default_achievements, default_cheat_codes, default_memory_watchpoints, parse_cheat_code,
    parse_watchpoint_address, scan_ram_for_exact_byte, AchievementEntry, CheatCodeEntry,
    MemoryWatchpoint,
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
use zaimule_core::storage::{
    decode_portable_pack, encode_portable_pack, inspect_sram_hex, write_sram_byte_and_rehash,
    PersistedSaveState, SramHexInspection, ZaimulePortablePack, PORTABLE_PACK_MAGIC,
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
    pub link_telemetry: LinkCableTelemetry,
    pub rewind_buffer: RewindBuffer,
    pub save_states_ram: HashMap<(String, u8), (SaveStateSlotInfo, Vec<u8>)>,
    pub custom_roms: HashMap<String, Vec<u8>>,
    pub breakpoints: Vec<u32>,
    pub last_score: u32,
    pub recent_inputs_trace: Vec<u8>,
    pub sram_dirty: bool,
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
        _ => {
            let mut c = GameBoyCore::new();
            c.load_rom(rom)?;
            Ok(Box::new(c))
        }
    }
}

impl EmulatorSession {
    pub fn new() -> Self {
        let catalog = default_catalog();
        let active_game_id = catalog[0].id.clone();

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
            warning: None,
            core_p1: Box::new(GameBoyCore::new()),
            core_p2: Box::new(GameBoyCore::new()),
            core_p3: Box::new(GameBoyCore::new()),
            core_p4: Box::new(GameBoyCore::new()),
        };
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
            sram_hex_inspection: inspect_sram_hex(self.core_p1.sram(), 0, 4),
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

    pub fn base_rom_for_game(&self, game_id: &str, console: ConsoleKind) -> Vec<u8> {
        if let Some(custom) = self.custom_roms.get(game_id) {
            return custom.clone();
        }
        Self::unpatched_rom_for_console(console)
    }

    /// Switches the active game and instantiates the appropriate Rust core across all 4 Link ports.
    pub fn select_game(&mut self, game_id: &str) -> Result<StationBootstrap, String> {
        let Some(game) = self.catalog.iter().find(|g| g.id == game_id).cloned() else {
            return Err(format!("jeu introuvable dans la bibliothèque : {game_id}"));
        };
        self.active_game_id = game.id.clone();
        self.rewind_buffer.clear();
        let rom = self.base_rom_for_game(&game.id, game.console);

        self.core_p1 = build_core_for(game.console, &rom)?;
        self.core_p2 = build_core_for(game.console, &rom)?;
        self.core_p3 = build_core_for(game.console, &rom)?;
        self.core_p4 = build_core_for(game.console, &rom)?;
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
        self.core_p3.load_rom(&patched_rom)?;
        self.core_p4.load_rom(&patched_rom)?;

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
        core.installed = true;
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
        self.sram_dirty = true;

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

    /// Enables or disables 4-Player Tournament Split-Screen (2x2 Quad-Link) mode.
    pub fn set_quad_link_mode(&mut self, quad_enabled: bool) -> StationBootstrap {
        self.link_telemetry.quad_mode = quad_enabled;
        if quad_enabled {
            self.link_telemetry.enabled = true;
        }
        self.bootstrap_snapshot()
    }

    /// Returns a live BGB/Mesen-style Debugger payload containing the VRAM 128x64 Tile Atlas
    /// and a 10-instruction disassembled window around the current Program Counter (`PC`).
    pub fn inspect_vram_and_disassembly(&self) -> DebuggerPayload {
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

    /// Generates a binary `.ips` patch by diffing the clean reference ROM against the active
    /// patched ROM (and any active GameShark RAM/ROM modifications).
    pub fn export_active_ips_patch(&self) -> Result<Vec<u8>, String> {
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
            version: "1.0.0".into(),
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
            .unwrap_or_else(|| "PKMN-LINK-01".into());
        let shared_state_crc32_hex = share_slot.and_then(|slot| {
            self.save_states_ram
                .get(&(self.active_game_id.clone(), slot))
                .map(|(info, _)| info.crc32_hex.clone())
                .or_else(|| Some(self.active_game().crc32_hex.clone()))
        });
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
        inspect_sram_hex(self.core_p1.sram(), offset, 4)
    }

    /// Writes a single byte into the active game's SRAM / Memory Card (`0x0000..sram.len()`),
    /// synchronizes the live score if offset `4` is edited, and returns the updated `StationBootstrap`.
    pub fn write_active_sram_byte(
        &mut self,
        offset: usize,
        value: u8,
    ) -> Result<StationBootstrap, String> {
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
}
