mod update;

use core::sync::atomic::{AtomicU64, Ordering};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, WindowEvent};
use zaimule_core::keymap::KeymapWidget;
use zaimule_core::overrides::{GameProfileOverride, ShaderPreset};
use zaimule_core::rewind::SaveStateSlotInfo;
use zaimule_core::storage::{
    load_json_atomic, load_sram_bytes, load_state_slot_atomic, save_json_atomic,
    save_rom_binary_atomic, save_sram_with_rotation, save_state_slot_atomic, LoadedJson,
    SramHexInspection,
};
use zaimule_emu::debugger::DebuggerPayload;
use zaimule_emu::session::{EmulatorSession, StationBootstrap, ViewportStepResponse};

const LIVE_SRAM_FLUSH_INTERVAL_MS: u64 = 10_000;

struct AppState {
    session: Arc<Mutex<EmulatorSession>>,
    last_sram_flush_ms: AtomicU64,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn config_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_config_dir().map_err(|e| e.to_string())
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_root(app)?.join("settings.json"))
}

fn sram_path(app: &AppHandle, game_id: &str) -> Result<PathBuf, String> {
    Ok(config_root(app)?
        .join("saves")
        .join(format!("{game_id}.srm")))
}

fn state_slot_path(app: &AppHandle, game_id: &str, slot: u8) -> Result<PathBuf, String> {
    Ok(config_root(app)?
        .join("states")
        .join(game_id)
        .join(format!("slot_{slot}.zst")))
}

fn rom_archive_path(app: &AppHandle, game_id: &str) -> Result<PathBuf, String> {
    Ok(config_root(app)?
        .join("roms")
        .join(format!("{game_id}.rom")))
}

fn core_binary_path(app: &AppHandle, core_id: &str) -> Result<PathBuf, String> {
    Ok(config_root(app)?
        .join("cores")
        .join(format!("{core_id}.bin")))
}

fn flush_sram_best_effort(app: &AppHandle, session: &EmulatorSession) {
    if session.active_game_id.is_empty() {
        return;
    }
    if let Ok(path) = sram_path(app, &session.active_game_id) {
        let _ = save_sram_with_rotation(&path, session.active_sram_bytes());
    }
}

fn preload_disk_assets_for_game(app: &AppHandle, session: &mut EmulatorSession, game_id: &str) {
    if game_id.is_empty() {
        return;
    }
    if let Ok(path) = sram_path(app, game_id) {
        if let Ok(Some(sram)) = load_sram_bytes(&path) {
            session.load_active_sram(&sram);
        }
    }
    for slot in 1..=6u8 {
        if let Ok(path) = state_slot_path(app, game_id, slot) {
            if let Ok(Some(persisted)) = load_state_slot_atomic(&path) {
                session.import_save_state_envelope(game_id.to_string(), persisted);
            }
        }
    }
}

#[tauri::command]
fn get_station_bootstrap(state: tauri::State<AppState>) -> Result<StationBootstrap, String> {
    let session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
fn select_game(
    app: AppHandle,
    state: tauri::State<AppState>,
    game_id: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    flush_sram_best_effort(&app, &session);
    session.select_game(&game_id)?;
    preload_disk_assets_for_game(&app, &mut session, &game_id);
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
fn import_rom_file(
    app: AppHandle,
    state: tauri::State<AppState>,
    file_name: String,
    bytes: Vec<u8>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    flush_sram_best_effort(&app, &session);
    let snap = session.import_custom_rom(&file_name, &bytes)?;
    if let Ok(rom_path) = rom_archive_path(&app, &snap.active_game_id) {
        let _ = save_rom_binary_atomic(&rom_path, &bytes);
    }
    preload_disk_assets_for_game(&app, &mut session, &snap.active_game_id);
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
fn apply_ips_patch_file(
    state: tauri::State<AppState>,
    patch_name: String,
    patch_bytes: Vec<u8>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.apply_custom_ips_to_active_game(&patch_name, &patch_bytes)
}

#[tauri::command]
fn add_cheat_code(
    state: tauri::State<AppState>,
    raw_code: String,
    label: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.add_cheat_code(&raw_code, &label)
}

#[tauri::command]
fn toggle_cheat_code(
    state: tauri::State<AppState>,
    cheat_id: String,
    enabled: bool,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.toggle_cheat_code(&cheat_id, enabled)
}

#[tauri::command]
fn scan_ram_byte(state: tauri::State<AppState>, target_value: u8) -> Result<Vec<String>, String> {
    let session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.scan_active_ram(target_value))
}

#[tauri::command]
fn provision_libretro_core(
    app: AppHandle,
    state: tauri::State<AppState>,
    core_id: String,
    custom_bytes: Option<Vec<u8>>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let (snap, binary) = session.provision_libretro_core(&core_id, custom_bytes.as_deref())?;
    if let Ok(path) = core_binary_path(&app, &core_id) {
        let _ = save_rom_binary_atomic(&path, &binary);
    }
    Ok(snap)
}

#[tauri::command]
fn create_netplay_room(
    state: tauri::State<AppState>,
    title: String,
    host_name: String,
    peer_addr: Option<String>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.create_netplay_room(title, host_name, peer_addr)
}

#[tauri::command]
fn join_netplay_deep_link(
    state: tauri::State<AppState>,
    uri: String,
    peer_addr: Option<String>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.join_netplay_deep_link(&uri, peer_addr)
}

#[tauri::command]
fn save_recorded_macro(
    state: tauri::State<AppState>,
    name: String,
    frames: Vec<u16>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.save_custom_macro(name, frames)
}

#[tauri::command]
fn save_keymapper_layout(
    state: tauri::State<AppState>,
    widgets: Vec<KeymapWidget>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.update_keymapper_widgets(widgets)
}

#[tauri::command]
fn step_viewport(
    app: AppHandle,
    state: tauri::State<AppState>,
    buttons_p1: u16,
    buttons_p2: u16,
    turbo_active: bool,
) -> Result<ViewportStepResponse, String> {
    let now = now_unix_ms();
    let mut sram_to_flush: Option<(String, Vec<u8>)> = None;

    let resp = {
        let mut session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        let out = session.step_viewport(buttons_p1, buttons_p2, turbo_active);

        let last_flush = state.last_sram_flush_ms.load(Ordering::Relaxed);
        if !session.active_game_id.is_empty()
            && now.saturating_sub(last_flush) >= LIVE_SRAM_FLUSH_INTERVAL_MS
            && session.sram_dirty
        {
            session.sram_dirty = false;
            state.last_sram_flush_ms.store(now, Ordering::Relaxed);
            sram_to_flush = Some((
                session.active_game_id.clone(),
                session.active_sram_bytes().to_vec(),
            ));
        }
        out
    };

    if let Some((game_id, bytes)) = sram_to_flush {
        if let Ok(path) = sram_path(&app, &game_id) {
            let _ = save_sram_with_rotation(&path, &bytes);
        }
    }

    Ok(resp)
}

#[tauri::command]
fn set_dual_link_enabled(
    state: tauri::State<AppState>,
    enabled: bool,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.link_telemetry.enabled = enabled;
    if !enabled {
        session.link_telemetry.quad_mode = false;
    }
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
fn set_quad_link_enabled(
    state: tauri::State<AppState>,
    enabled: bool,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.set_quad_link_mode(enabled))
}

#[tauri::command]
fn get_vram_and_disassembly(state: tauri::State<AppState>) -> Result<DebuggerPayload, String> {
    let session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.inspect_vram_and_disassembly())
}

#[tauri::command]
fn toggle_breakpoint(state: tauri::State<AppState>, addr: u32) -> Result<DebuggerPayload, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.toggle_breakpoint(addr))
}

#[tauri::command]
fn create_save_state(
    app: AppHandle,
    state: tauri::State<AppState>,
    slot: u8,
    label: String,
) -> Result<SaveStateSlotInfo, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let info = session.create_save_state(slot, label, now_unix_ms())?;
    if let Some(persisted) = session.export_save_state_envelope(slot) {
        if let Ok(path) = state_slot_path(&app, &session.active_game_id, persisted.info.slot) {
            let _ = save_state_slot_atomic(&path, &persisted);
        }
    }
    flush_sram_best_effort(&app, &session);
    Ok(info)
}

#[tauri::command]
fn restore_save_state(
    app: AppHandle,
    state: tauri::State<AppState>,
    slot: u8,
) -> Result<SaveStateSlotInfo, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let game_id = session.active_game_id.clone();
    if let Ok(path) = state_slot_path(&app, &game_id, slot.clamp(1, 9)) {
        if let Ok(Some(persisted)) = load_state_slot_atomic(&path) {
            session.import_save_state_envelope(game_id, persisted);
        }
    }
    session.restore_save_state(slot)
}

#[tauri::command]
fn rewind_frames(state: tauri::State<AppState>, steps_back: usize) -> Result<u64, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.rewind(steps_back.max(1))
}

#[tauri::command]
fn set_active_macro(
    state: tauri::State<AppState>,
    macro_id: Option<String>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.active_macro_id = macro_id;
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
fn update_game_override(
    app: AppHandle,
    state: tauri::State<AppState>,
    game_id: String,
    shader: ShaderPreset,
    scale: u8,
    speed_multiplier: u8,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let entry = session
        .settings
        .game_overrides
        .entry(game_id)
        .or_insert_with(GameProfileOverride::default);
    entry.custom_shader = Some(shader);
    entry.custom_scale = Some(scale.clamp(1, 8));
    entry.custom_speed_multiplier = Some(speed_multiplier.clamp(1, 8));

    if let Ok(path) = settings_path(&app) {
        if let Ok(val) = serde_json::to_value(&session.settings) {
            let _ = save_json_atomic(&path, &val);
        }
    }
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
fn toggle_mod_layer(
    state: tauri::State<AppState>,
    mod_id: String,
    enabled: bool,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    for layer in &mut session.mod_layers {
        if layer.id == mod_id {
            layer.enabled = enabled;
        }
    }
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
fn export_ips_diff(state: tauri::State<AppState>) -> Result<Vec<u8>, String> {
    let session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.export_active_ips_patch()
}

#[tauri::command]
fn export_portable_pack_archive(state: tauri::State<AppState>) -> Result<Vec<u8>, String> {
    let session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.export_portable_pack()
}

#[tauri::command]
fn import_portable_pack_archive(
    state: tauri::State<AppState>,
    bytes: Vec<u8>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.import_portable_pack(&bytes)
}

#[tauri::command]
fn set_nds_stylus(
    state: tauri::State<AppState>,
    x: u8,
    y: u8,
    pressed: bool,
) -> Result<(), String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.set_nds_stylus(x, y, pressed);
    Ok(())
}

#[tauri::command]
fn paint_vram_atlas_pixel(
    state: tauri::State<AppState>,
    atlas_x: u16,
    atlas_y: u16,
    color_idx: u8,
) -> Result<DebuggerPayload, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.paint_vram_atlas_pixel(atlas_x, atlas_y, color_idx))
}

#[tauri::command]
fn post_netplay_chat_message(
    state: tauri::State<AppState>,
    sender: String,
    text: String,
    share_slot: Option<u8>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.post_netplay_chat_message(sender, text, share_slot, now_unix_ms())
}

#[tauri::command]
fn set_hardware_overclock_profile(
    app: AppHandle,
    state: tauri::State<AppState>,
    cpu_overclock_pct: u16,
    pgxp_subpixel: bool,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.set_hardware_overclock_profile(cpu_overclock_pct, pgxp_subpixel);
    if let Ok(path) = settings_path(&app) {
        if let Ok(val) = serde_json::to_value(&session.settings) {
            let _ = save_json_atomic(&path, &val);
        }
    }
    Ok(snap)
}

#[tauri::command]
fn submit_ghost_leaderboard_run(
    state: tauri::State<AppState>,
    player_name: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.submit_ghost_leaderboard_run(player_name))
}

#[tauri::command]
fn challenge_ghost_run(
    state: tauri::State<AppState>,
    ghost_id: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.challenge_ghost_run(&ghost_id)
}

#[tauri::command]
fn install_community_ips_mod(
    state: tauri::State<AppState>,
    mod_id: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.install_community_ips_mod(&mod_id)
}

#[tauri::command]
fn inspect_sram_hex_view(
    state: tauri::State<AppState>,
    offset: usize,
) -> Result<SramHexInspection, String> {
    let session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.inspect_active_sram_hex(offset))
}

#[tauri::command]
fn write_sram_byte_live(
    app: AppHandle,
    state: tauri::State<AppState>,
    offset: usize,
    value: u8,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.write_active_sram_byte(offset, value)?;
    flush_sram_best_effort(&app, &session);
    Ok(snap)
}

#[tauri::command]
fn transfer_sram_p1_to_p2(
    app: AppHandle,
    state: tauri::State<AppState>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.transfer_sram_p1_to_p2()?;
    flush_sram_best_effort(&app, &session);
    Ok(snap)
}

#[tauri::command]
fn advance_tournament_match(
    state: tauri::State<AppState>,
    match_id: String,
    score_a: u32,
    score_b: u32,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.advance_tournament_match(&match_id, score_a, score_b)
}

#[tauri::command]
fn toggle_tournament_spectator(state: tauri::State<AppState>) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.toggle_tournament_spectator_mode())
}

#[tauri::command]
fn add_memory_watchpoint(
    state: tauri::State<AppState>,
    raw_addr: String,
    label: String,
    frozen: bool,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.add_memory_watchpoint(&raw_addr, &label, frozen)
}

#[tauri::command]
fn toggle_watchpoint_freeze(
    state: tauri::State<AppState>,
    watch_id: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.toggle_watchpoint_freeze(&watch_id)
}

#[tauri::command]
fn toggle_tas_timeline_button(
    state: tauri::State<AppState>,
    macro_id: Option<String>,
    segment_index: usize,
    button_bit: u16,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.toggle_tas_timeline_button(macro_id.as_deref(), segment_index, button_bit)
}

#[tauri::command]
fn load_settings(app: AppHandle) -> Result<LoadedJson, String> {
    load_json_atomic(&settings_path(&app)?)
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Value) -> Result<(), String> {
    save_json_atomic(&settings_path(&app)?, &settings)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let session = Arc::new(Mutex::new(EmulatorSession::new_clean()));
    let session_for_setup = Arc::clone(&session);

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(AppState {
            session,
            last_sram_flush_ms: AtomicU64::new(0),
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            if let Ok(mut guard) = session_for_setup.lock() {
                if let Ok(path) = settings_path(&handle) {
                    if let Ok(loaded) = load_json_atomic(&path) {
                        guard.warning = loaded.warning;
                    }
                }
                let active_id = guard.active_game_id.clone();
                preload_disk_assets_for_game(&handle, &mut guard, &active_id);
            }
            update::check_in_background(handle);
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { .. } = event {
                let app = window.app_handle();
                if let Some(state) = app.try_state::<AppState>() {
                    if let Ok(guard) = state.session.lock() {
                        flush_sram_best_effort(app, &guard);
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_station_bootstrap,
            select_game,
            import_rom_file,
            apply_ips_patch_file,
            export_ips_diff,
            export_portable_pack_archive,
            import_portable_pack_archive,
            set_nds_stylus,
            paint_vram_atlas_pixel,
            post_netplay_chat_message,
            set_hardware_overclock_profile,
            submit_ghost_leaderboard_run,
            challenge_ghost_run,
            install_community_ips_mod,
            inspect_sram_hex_view,
            write_sram_byte_live,
            transfer_sram_p1_to_p2,
            advance_tournament_match,
            toggle_tournament_spectator,
            add_memory_watchpoint,
            toggle_watchpoint_freeze,
            toggle_tas_timeline_button,
            add_cheat_code,
            toggle_cheat_code,
            scan_ram_byte,
            provision_libretro_core,
            create_netplay_room,
            join_netplay_deep_link,
            save_recorded_macro,
            save_keymapper_layout,
            step_viewport,
            set_dual_link_enabled,
            set_quad_link_enabled,
            get_vram_and_disassembly,
            toggle_breakpoint,
            create_save_state,
            restore_save_state,
            rewind_frames,
            set_active_macro,
            update_game_override,
            toggle_mod_layer,
            load_settings,
            save_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running Zaimule application");
}
