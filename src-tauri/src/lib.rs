mod update;

use core::sync::atomic::{AtomicU64, Ordering};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use zaimule_core::bios::{looks_like_bios_path, BiosImportReport};
use zaimule_core::catalog::SwitchEngineOverride;
use zaimule_core::external::{spawn_switch_process, SwitchEngineTarget};
use zaimule_core::keymap::KeymapWidget;
use zaimule_core::overrides::{GlobalSettings, ShaderPreset};
use zaimule_core::rewind::SaveStateSlotInfo;
use zaimule_core::storage::{
    load_json_atomic, load_library_atomic, load_sram_bytes, load_state_slot_atomic,
    save_json_atomic, save_library_atomic, save_rom_binary_atomic, save_sram_with_rotation,
    save_state_slot_atomic, LoadedJson, PersistedLibrary, SramHexInspection,
};
use zaimule_emu::debugger::DebuggerPayload;
use zaimule_emu::session::{EmulatorSession, StationBootstrap, ViewportStepResponse};

const LIVE_SRAM_FLUSH_INTERVAL_MS: u64 = 10_000;
const LIVE_PLAYTIME_FLUSH_INTERVAL_MS: u64 = 30_000;

struct AppState {
    session: Arc<Mutex<EmulatorSession>>,
    last_sram_flush_ms: Arc<AtomicU64>,
    last_playtime_flush_ms: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StepViewportOptions {
    stylus_x: Option<u8>,
    stylus_y: Option<u8>,
    stylus_pressed: Option<bool>,
    include_debug_meta: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SwitchSessionEndedEvent {
    game_id: String,
    target: SwitchEngineTarget,
    elapsed_seconds: u64,
    exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SwitchShaderCacheClearResponse {
    cleared_bytes: u64,
    bootstrap: StationBootstrap,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BiosImportResponse {
    report: BiosImportReport,
    bootstrap: StationBootstrap,
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

fn library_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_root(app)?.join("library.json"))
}

fn system_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_root(app)?.join("system"))
}

fn save_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_root(app)?.join("saves"))
}

fn rom_cache_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_root(app)?.join("rom_cache"))
}

fn switch_vault_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_root(app)?.join("switch"))
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

fn persist_library_best_effort(app: &AppHandle, session: &EmulatorSession) {
    if let Ok(path) = library_path(app) {
        let lib = session.to_persisted_library();
        let _ = save_library_atomic(&path, &lib);
    }
}

fn persist_settings_best_effort(app: &AppHandle, session: &mut EmulatorSession) {
    session.sync_runtime_to_settings();
    if let Ok(path) = settings_path(app) {
        if let Ok(val) = serde_json::to_value(&session.settings) {
            let _ = save_json_atomic(&path, &val);
        }
    }
}

fn flush_sram_best_effort(app: &AppHandle, session: &mut EmulatorSession) {
    if !session.refresh_sram_dirty_flag() {
        return;
    }
    if let Ok(path) = sram_path(app, &session.active_game_id) {
        if save_sram_with_rotation(&path, session.active_sram_bytes()).is_ok() {
            session.mark_sram_flushed();
        }
    }
}

fn persist_active_pack_assets(app: &AppHandle, session: &mut EmulatorSession) {
    flush_sram_best_effort(app, session);
    let game_id = session.active_game_id.clone();
    if !game_id.is_empty() {
        for slot in 1..=9u8 {
            if let Some(persisted) = session.export_save_state_envelope(slot) {
                if let Ok(path) = state_slot_path(app, &game_id, persisted.info.slot) {
                    let _ = save_state_slot_atomic(&path, &persisted);
                }
            }
        }
    }
    persist_settings_best_effort(app, session);
}

fn maybe_collect_live_sram(
    last_sram_flush_ms: &AtomicU64,
    session: &mut EmulatorSession,
    now_ms: u64,
) -> Option<(String, Vec<u8>)> {
    let last_flush = last_sram_flush_ms.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last_flush) < LIVE_SRAM_FLUSH_INTERVAL_MS {
        return None;
    }
    // Always advance the check timestamp once the interval elapses so clean SRAM
    // is never copied and CRC32-hashed on every single video frame.
    last_sram_flush_ms.store(now_ms, Ordering::Relaxed);
    if session.refresh_sram_dirty_flag() {
        session.mark_sram_flushed();
        Some((
            session.active_game_id.clone(),
            session.active_sram_bytes().to_vec(),
        ))
    } else {
        None
    }
}

fn maybe_collect_live_library(
    last_playtime_flush_ms: &AtomicU64,
    session: &mut EmulatorSession,
    now_ms: u64,
) -> Option<PersistedLibrary> {
    let last_flush = last_playtime_flush_ms.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last_flush) < LIVE_PLAYTIME_FLUSH_INTERVAL_MS {
        return None;
    }
    last_playtime_flush_ms.store(now_ms, Ordering::Relaxed);
    if session.take_playtime_dirty() {
        Some(session.to_persisted_library())
    } else {
        None
    }
}

fn spawn_live_disk_flushes(
    app: AppHandle,
    sram_to_flush: Option<(String, Vec<u8>)>,
    lib_to_flush: Option<PersistedLibrary>,
) {
    if sram_to_flush.is_none() && lib_to_flush.is_none() {
        return;
    }
    tauri::async_runtime::spawn_blocking(move || {
        if let Some((game_id, bytes)) = sram_to_flush {
            if let Ok(path) = sram_path(&app, &game_id) {
                let _ = save_sram_with_rotation(&path, &bytes);
            }
        }
        if let Some(lib) = lib_to_flush {
            if let Ok(path) = library_path(&app) {
                let _ = save_library_atomic(&path, &lib);
            }
        }
    });
}

fn preload_disk_assets_for_game(app: &AppHandle, session: &mut EmulatorSession, game_id: &str) {
    if game_id.is_empty() {
        return;
    }
    if session.has_persistable_sram() {
        if let Ok(path) = sram_path(app, game_id) {
            if let Ok(Some(sram)) = load_sram_bytes(&path) {
                session.load_active_sram(&sram);
            }
        }
    }
    for slot in 1..=9u8 {
        if let Ok(path) = state_slot_path(app, game_id, slot) {
            if let Ok(Some(persisted)) = load_state_slot_atomic(&path) {
                session.import_save_state_envelope(game_id.to_string(), persisted);
            }
        }
    }
}

#[tauri::command]
fn get_station_bootstrap(state: tauri::State<AppState>) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.sync_active_sram();
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
    flush_sram_best_effort(&app, &mut session);
    session.select_game(&game_id)?;
    preload_disk_assets_for_game(&app, &mut session, &game_id);
    let _ = session.take_playtime_dirty();
    persist_library_best_effort(&app, &session);
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
fn reset_active_game(
    app: AppHandle,
    state: tauri::State<AppState>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    flush_sram_best_effort(&app, &mut session);
    let snap = session.reset_active_game()?;
    if session.take_playtime_dirty() {
        persist_library_best_effort(&app, &session);
    }
    Ok(snap)
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
    flush_sram_best_effort(&app, &mut session);
    let snap = session.import_custom_rom(&file_name, &bytes)?;
    if let Ok(rom_path) = rom_archive_path(&app, &snap.active_game_id) {
        let _ = save_rom_binary_atomic(&rom_path, &bytes);
    }
    preload_disk_assets_for_game(&app, &mut session, &snap.active_game_id);
    persist_library_best_effort(&app, &session);
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
    app: AppHandle,
    state: tauri::State<AppState>,
    raw_code: String,
    label: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.add_cheat_code(&raw_code, label)?;
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
fn toggle_cheat_code(
    app: AppHandle,
    state: tauri::State<AppState>,
    cheat_id: String,
    enabled: bool,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.toggle_cheat_code(&cheat_id, enabled)?;
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
fn remove_cheat_code(
    app: AppHandle,
    state: tauri::State<AppState>,
    cheat_id: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.remove_cheat_code(&cheat_id)?;
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
fn scan_ram_byte(state: tauri::State<AppState>, target_value: u8) -> Result<Vec<String>, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.scan_active_ram(target_value))
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
fn connect_netplay_peer(
    state: tauri::State<AppState>,
    peer_addr: Option<String>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.connect_netplay_peer(peer_addr.as_deref())
}

#[tauri::command]
fn save_recorded_macro(
    app: AppHandle,
    state: tauri::State<AppState>,
    name: String,
    frames: Vec<u16>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.save_custom_macro(name, frames)?;
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
fn save_keymapper_layout(
    app: AppHandle,
    state: tauri::State<AppState>,
    widgets: Vec<KeymapWidget>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.update_keymapper_widgets(widgets)?;
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
async fn step_viewport(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    buttons_p1: u16,
    buttons_p2: u16,
    turbo_active: bool,
    speed_mult: Option<u8>,
) -> Result<ViewportStepResponse, String> {
    let session_arc = Arc::clone(&state.session);
    let last_sram_flush = Arc::clone(&state.last_sram_flush_ms);
    let last_playtime_flush = Arc::clone(&state.last_playtime_flush_ms);
    let speed = speed_mult.unwrap_or(1);

    let (resp, sram_to_flush, lib_to_flush) =
        tauri::async_runtime::spawn_blocking(move || -> Result<_, String> {
            let now = now_unix_ms();
            let mut session = session_arc
                .lock()
                .map_err(|_| "verrou session indisponible".to_string())?;
            let out = session.step_viewport_with_speed(buttons_p1, buttons_p2, turbo_active, speed);
            let flush_sram = maybe_collect_live_sram(&last_sram_flush, &mut session, now);
            let flush_lib = maybe_collect_live_library(&last_playtime_flush, &mut session, now);
            Ok((out, flush_sram, flush_lib))
        })
        .await
        .map_err(|e| format!("tâche émulation interrompue : {e}"))??;

    spawn_live_disk_flushes(app, sram_to_flush, lib_to_flush);
    Ok(resp)
}

#[tauri::command]
async fn step_viewport_binary(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    buttons_p1: u16,
    buttons_p2: u16,
    turbo_active: bool,
    speed_mult: Option<u8>,
    options: Option<StepViewportOptions>,
) -> Result<tauri::ipc::Response, String> {
    let session_arc = Arc::clone(&state.session);
    let last_sram_flush = Arc::clone(&state.last_sram_flush_ms);
    let last_playtime_flush = Arc::clone(&state.last_playtime_flush_ms);
    let speed = speed_mult.unwrap_or(1);

    let (pkt, sram_to_flush, lib_to_flush) =
        tauri::async_runtime::spawn_blocking(move || -> Result<_, String> {
            let now = now_unix_ms();
            let mut session = session_arc
                .lock()
                .map_err(|_| "verrou session indisponible".to_string())?;
            let mut emit_meta = true;
            if let Some(opts) = options {
                if let (Some(x), Some(y), Some(pressed)) =
                    (opts.stylus_x, opts.stylus_y, opts.stylus_pressed)
                {
                    session.set_nds_stylus(x, y, pressed);
                }
                if let Some(flag) = opts.include_debug_meta {
                    emit_meta = flag;
                }
            }
            let out = session.step_viewport_binary_bytes_ex(
                buttons_p1,
                buttons_p2,
                turbo_active,
                speed,
                emit_meta,
            );
            let flush_sram = maybe_collect_live_sram(&last_sram_flush, &mut session, now);
            let flush_lib = maybe_collect_live_library(&last_playtime_flush, &mut session, now);
            Ok((out, flush_sram, flush_lib))
        })
        .await
        .map_err(|e| format!("tâche émulation interrompue : {e}"))??;

    spawn_live_disk_flushes(app, sram_to_flush, lib_to_flush);
    Ok(tauri::ipc::Response::new(pkt))
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
    Ok(session.set_dual_link_enabled(enabled))
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
async fn get_vram_and_disassembly(
    state: tauri::State<'_, AppState>,
) -> Result<DebuggerPayload, String> {
    let session_arc = Arc::clone(&state.session);
    tauri::async_runtime::spawn_blocking(move || -> Result<DebuggerPayload, String> {
        let mut session = session_arc
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        Ok(session.inspect_vram_and_disassembly())
    })
    .await
    .map_err(|e| format!("tâche débogueur interrompue : {e}"))?
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
async fn create_save_state(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    slot: u8,
    label: String,
) -> Result<SaveStateSlotInfo, String> {
    let session_arc = Arc::clone(&state.session);
    tauri::async_runtime::spawn_blocking(move || -> Result<SaveStateSlotInfo, String> {
        let mut session = session_arc
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        let info = session.create_save_state(slot, label, now_unix_ms())?;
        if let Some(persisted) = session.export_save_state_envelope(slot) {
            if let Ok(path) = state_slot_path(&app, &session.active_game_id, persisted.info.slot) {
                let _ = save_state_slot_atomic(&path, &persisted);
            }
        }
        flush_sram_best_effort(&app, &mut session);
        Ok(info)
    })
    .await
    .map_err(|e| format!("tâche save state interrompue : {e}"))?
}

#[tauri::command]
async fn restore_save_state(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    slot: u8,
) -> Result<SaveStateSlotInfo, String> {
    let session_arc = Arc::clone(&state.session);
    tauri::async_runtime::spawn_blocking(move || -> Result<SaveStateSlotInfo, String> {
        let mut session = session_arc
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        let game_id = session.active_game_id.clone();
        if let Ok(path) = state_slot_path(&app, &game_id, slot.clamp(1, 9)) {
            if let Ok(Some(persisted)) = load_state_slot_atomic(&path) {
                session.import_save_state_envelope(game_id, persisted);
            }
        }
        session.restore_save_state(slot)
    })
    .await
    .map_err(|e| format!("tâche restauration save state interrompue : {e}"))?
}

#[tauri::command]
fn delete_save_state(
    app: AppHandle,
    state: tauri::State<AppState>,
    slot: u8,
) -> Result<Vec<SaveStateSlotInfo>, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let game_id = session.active_game_id.clone();
    let slots = session.delete_save_state(slot)?;
    if !game_id.is_empty() {
        if let Ok(path) = state_slot_path(&app, &game_id, slot.clamp(1, 9)) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(slots)
}

#[tauri::command]
async fn rewind_frames(
    state: tauri::State<'_, AppState>,
    steps_back: usize,
) -> Result<u64, String> {
    let session_arc = Arc::clone(&state.session);
    tauri::async_runtime::spawn_blocking(move || -> Result<u64, String> {
        let mut session = session_arc
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        session.rewind(steps_back.max(1))
    })
    .await
    .map_err(|e| format!("tâche rembobinage interrompue : {e}"))?
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
    Ok(session.set_active_macro(macro_id))
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
    let entry = session.settings.game_overrides.entry(game_id).or_default();
    entry.custom_shader = Some(shader);
    entry.custom_scale = Some(scale.clamp(1, 8));
    entry.custom_speed_multiplier = Some(speed_multiplier.clamp(1, 8));

    persist_settings_best_effort(&app, &mut session);
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
fn update_ui_preferences(
    app: AppHandle,
    state: tauri::State<AppState>,
    shader_css: Option<String>,
    upscaler_mode: Option<String>,
    speed_multiplier: Option<u8>,
    nds_layout: Option<String>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap =
        session.update_ui_preferences(shader_css, upscaler_mode, speed_multiplier, nds_layout);
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
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
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.export_active_ips_patch()
}

#[tauri::command]
fn export_portable_pack_archive(state: tauri::State<AppState>) -> Result<Vec<u8>, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.export_portable_pack()
}

#[tauri::command]
fn import_portable_pack_archive(
    app: AppHandle,
    state: tauri::State<AppState>,
    bytes: Vec<u8>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.import_portable_pack(&bytes)?;
    persist_active_pack_assets(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
async fn set_nds_stylus(
    state: tauri::State<'_, AppState>,
    x: u8,
    y: u8,
    pressed: bool,
) -> Result<(), String> {
    let session_arc = Arc::clone(&state.session);
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut session = session_arc
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        session.set_nds_stylus(x, y, pressed);
        Ok(())
    })
    .await
    .map_err(|e| format!("tâche stylet interrompue : {e}"))?
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
    persist_settings_best_effort(&app, &mut session);
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
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.sync_active_sram();
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
    flush_sram_best_effort(&app, &mut session);
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
    flush_sram_best_effort(&app, &mut session);
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
    app: AppHandle,
    state: tauri::State<AppState>,
    raw_addr: String,
    label: String,
    frozen: bool,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.add_memory_watchpoint(&raw_addr, &label, frozen)?;
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
fn toggle_watchpoint_freeze(
    app: AppHandle,
    state: tauri::State<AppState>,
    watch_id: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.toggle_watchpoint_freeze(&watch_id)?;
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
fn remove_memory_watchpoint(
    app: AppHandle,
    state: tauri::State<AppState>,
    watch_id: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.remove_memory_watchpoint(&watch_id)?;
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
fn toggle_tas_timeline_button(
    app: AppHandle,
    state: tauri::State<AppState>,
    macro_id: Option<String>,
    segment_index: usize,
    button_bit: u16,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap =
        session.toggle_tas_timeline_button(macro_id.as_deref(), segment_index, button_bit)?;
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
fn load_settings(app: AppHandle) -> Result<LoadedJson, String> {
    load_json_atomic(&settings_path(&app)?)
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    state: tauri::State<AppState>,
    settings: Value,
) -> Result<(), String> {
    if let Ok(parsed) = serde_json::from_value::<GlobalSettings>(settings.clone()) {
        if let Ok(mut session) = state.session.lock() {
            session.apply_loaded_settings(parsed);
        }
    }
    save_json_atomic(&settings_path(&app)?, &settings)
}

fn import_paths_impl(
    app: &AppHandle,
    state: &AppState,
    paths: Vec<PathBuf>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    flush_sram_best_effort(app, &mut session);

    let mut rom_paths = Vec::new();
    let mut key_paths = Vec::new();
    let mut bios_paths = Vec::new();
    let mut ips_paths = Vec::new();
    let mut pack_paths = Vec::new();
    let mut special_handled = false;

    for p in paths {
        let lower_name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();

        if p.is_file() && lower_name.ends_with(".ips") {
            ips_paths.push((lower_name, p));
            continue;
        }
        if p.is_file() && lower_name.ends_with(".zaimule-pack") {
            pack_paths.push(p);
            continue;
        }
        if p.is_file() && lower_name.ends_with(".keys") {
            key_paths.push(p);
            continue;
        }
        if p.is_file() && looks_like_bios_path(&p) {
            bios_paths.push(p);
            continue;
        }
        rom_paths.push(p);
    }

    // 1. Switch encryption keys & console BIOS/firmware packs first
    if !key_paths.is_empty() {
        session.import_switch_keys(&key_paths)?;
        special_handled = true;
    }
    if !bios_paths.is_empty() {
        let _ = session.import_bios_paths(&bios_paths)?;
        special_handled = true;
    }

    // 2. ROM files and directories next so active game is selected before applying patches/packs
    if !rom_paths.is_empty() {
        let report = session.import_paths(&rom_paths)?;
        if report.imported_entries.is_empty()
            && !special_handled
            && ips_paths.is_empty()
            && pack_paths.is_empty()
        {
            if !report.errors.is_empty() {
                return Err(report.errors.join(" · "));
            }
            if report.skipped_files > 0 {
                return Err(
                    "Aucun jeu reconnu (.gb, .gbc, .gba, .nds, .nsp, .xci, .nsz, .xcz, .nro, .nso, .nx, .nup, .zip, .7z)."
                        .into(),
                );
            }
        }
    }

    // 3. Preload existing disk SRAM/save states BEFORE applying portable packs so packs aren't overwritten
    if !rom_paths.is_empty() {
        let active_id = session.active_game_id.clone();
        preload_disk_assets_for_game(app, &mut session, &active_id);
    }

    // 4. Apply IPS patches onto the newly-imported or active ROM
    for (lower_name, p) in ips_paths {
        let patch_bytes = fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let _ = session.apply_custom_ips_to_active_game(&lower_name, &patch_bytes)?;
    }

    // 5. Apply portable packs LAST and persist their SRAM/slots/settings directly to disk
    for p in pack_paths {
        let bytes = fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let _ = session.import_portable_pack(&bytes)?;
        persist_active_pack_assets(app, &mut session);
    }

    persist_library_best_effort(app, &session);
    persist_settings_best_effort(app, &mut session);
    Ok(session.bootstrap_snapshot())
}

fn scan_directory_impl(
    app: &AppHandle,
    state: &AppState,
    dir: PathBuf,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    flush_sram_best_effort(app, &mut session);
    let prev_active = session.active_game_id.clone();
    let report = session.scan_directory_and_merge(&dir);
    if session.active_game_id != prev_active {
        let active_id = session.active_game_id.clone();
        preload_disk_assets_for_game(app, &mut session, &active_id);
    }
    persist_library_best_effort(app, &session);
    persist_settings_best_effort(app, &mut session);

    if report.imported_entries.is_empty() && !report.unsupported_archives.is_empty() {
        return Err(format!(
            "Archive non supportée détectée ({}). Utilisez .zip, .7z ou le fichier ROM direct.",
            report.unsupported_archives[0]
        ));
    }
    if report.imported_entries.is_empty() && !report.errors.is_empty() {
        return Err(report.errors.join(" · "));
    }
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
async fn pick_and_import_roms(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<StationBootstrap, String> {
    let app_for_dialog = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_dialog
            .dialog()
            .file()
            .set_title("Importer des jeux ou archives (.gb, .gba, .nds, .nsp, .nx, .zip, .7z)")
            .add_filter(
                "Jeux & Archives",
                &[
                    "gb", "gbc", "dmg", "gba", "nds", "srl", "zip", "7z", "nsp", "xci", "nsz",
                    "xcz", "nro", "nso", "nx", "nup",
                ],
            )
            .blocking_pick_files()
    })
    .await
    .map_err(|e| format!("échec ouverture sélecteur de fichiers : {e}"))?;

    let Some(files) = picked else {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        return Ok(session.bootstrap_snapshot());
    };
    let paths: Vec<PathBuf> = files
        .into_iter()
        .filter_map(|f| f.into_path().ok())
        .collect();
    if paths.is_empty() {
        return Err("aucun chemin de fichier valide sélectionné".into());
    }
    import_paths_impl(&app, &state, paths)
}

#[tauri::command]
async fn pick_and_scan_folder(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<StationBootstrap, String> {
    let app_for_dialog = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_dialog
            .dialog()
            .file()
            .set_title("Choisir un dossier de jeux (.gb, .gba, .nds, .nsp, .nx, .zip, .7z)")
            .blocking_pick_folder()
    })
    .await
    .map_err(|e| format!("échec ouverture sélecteur de dossier : {e}"))?;

    let Some(folder) = picked else {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        return Ok(session.bootstrap_snapshot());
    };
    let dir = folder
        .into_path()
        .map_err(|e| format!("dossier sélectionné invalide : {e}"))?;
    scan_directory_impl(&app, &state, dir)
}

#[tauri::command]
fn import_rom_paths(
    app: AppHandle,
    state: tauri::State<AppState>,
    paths: Vec<String>,
) -> Result<StationBootstrap, String> {
    if paths.is_empty() {
        return Err("aucun fichier ou dossier reçu".into());
    }
    let path_bufs: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    import_paths_impl(&app, &state, path_bufs)
}

#[tauri::command]
fn scan_rom_directory(
    app: AppHandle,
    state: tauri::State<AppState>,
    dir_path: String,
) -> Result<StationBootstrap, String> {
    scan_directory_impl(&app, &state, PathBuf::from(dir_path))
}

#[tauri::command]
fn remove_rom_directory(
    app: AppHandle,
    state: tauri::State<AppState>,
    dir_path: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.remove_rom_directory(&dir_path)?;
    persist_library_best_effort(&app, &session);
    persist_settings_best_effort(&app, &mut session);
    Ok(snap)
}

#[tauri::command]
fn rescan_all_rom_directories(
    app: AppHandle,
    state: tauri::State<AppState>,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    flush_sram_best_effort(&app, &mut session);
    let prev_active = session.active_game_id.clone();
    let report = session.rescan_all_rom_directories();
    if session.active_game_id != prev_active {
        let active_id = session.active_game_id.clone();
        preload_disk_assets_for_game(&app, &mut session, &active_id);
    }
    let _ = session.take_playtime_dirty();
    persist_library_best_effort(&app, &session);
    persist_settings_best_effort(&app, &mut session);

    if report.imported_entries.is_empty() && !report.errors.is_empty() {
        return Err(report.errors.join(" · "));
    }
    Ok(session.bootstrap_snapshot())
}

#[tauri::command]
fn remove_game_from_library(
    app: AppHandle,
    state: tauri::State<AppState>,
    game_id: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    flush_sram_best_effort(&app, &mut session);
    let snap = session.remove_game(&game_id)?;
    if let Ok(rp) = rom_archive_path(&app, &game_id) {
        let _ = fs::remove_file(rp);
    }
    persist_library_best_effort(&app, &session);
    Ok(snap)
}

#[tauri::command]
fn toggle_game_favorite(
    app: AppHandle,
    state: tauri::State<AppState>,
    game_id: String,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.toggle_game_favorite(&game_id)?;
    persist_library_best_effort(&app, &session);
    Ok(snap)
}

#[tauri::command]
fn import_bios_paths(
    state: tauri::State<AppState>,
    paths: Vec<String>,
) -> Result<BiosImportResponse, String> {
    if paths.is_empty() {
        return Err("aucun fichier BIOS ou firmware sélectionné".into());
    }
    let path_bufs: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let (report, bootstrap) = session.import_bios_paths(&path_bufs)?;
    Ok(BiosImportResponse { report, bootstrap })
}

#[tauri::command]
async fn pick_and_import_bios(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<BiosImportResponse, String> {
    let app_for_dialog = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_dialog
            .dialog()
            .file()
            .set_title("Importer BIOS / Firmware (.zip, .7z, .bin, .rom)")
            .add_filter(
                "BIOS & Firmware (.zip, .7z, .bin, .rom)",
                &["zip", "7z", "bin", "rom", "bios"],
            )
            .blocking_pick_files()
    })
    .await
    .map_err(|e| format!("échec ouverture sélecteur BIOS : {e}"))?;

    let Some(files) = picked else {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        return Ok(BiosImportResponse {
            report: BiosImportReport::default(),
            bootstrap: session.bootstrap_snapshot(),
        });
    };
    let paths: Vec<PathBuf> = files
        .into_iter()
        .filter_map(|f| f.into_path().ok())
        .collect();
    if paths.is_empty() {
        return Err("aucun fichier BIOS valide sélectionné".into());
    }

    let sys_dir = {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        session.system_dir.clone()
    };
    let report = tauri::async_runtime::spawn_blocking(move || {
        zaimule_core::bios::import_bios_from_paths(&paths, &sys_dir)
    })
    .await
    .map_err(|e| format!("échec importation BIOS : {e}"))??;

    let session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(BiosImportResponse {
        report,
        bootstrap: session.bootstrap_snapshot(),
    })
}

#[tauri::command]
fn refresh_switch_vault(state: tauri::State<AppState>) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.refresh_switch_vault())
}

#[tauri::command]
async fn import_switch_engine_path(
    state: tauri::State<'_, AppState>,
    target: SwitchEngineTarget,
    source_path: String,
) -> Result<StationBootstrap, String> {
    let vault_root = {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        session.switch_vault_root.clone()
    };
    let src = PathBuf::from(source_path);
    tauri::async_runtime::spawn_blocking(move || {
        zaimule_core::external::import_switch_engine(&src, &vault_root, target)
    })
    .await
    .map_err(|e| format!("échec tâche d'import Switch : {e}"))??;

    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.refresh_switch_vault())
}

#[tauri::command]
async fn pick_and_import_switch_engine(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    target: SwitchEngineTarget,
    mode: Option<String>,
) -> Result<StationBootstrap, String> {
    let app_for_dialog = app.clone();
    let pick_folder = mode.as_deref() == Some("folder");
    let title = format!("Importer {}", target.label());

    let picked = tauri::async_runtime::spawn_blocking(move || {
        let builder = app_for_dialog.dialog().file().set_title(&title);
        if pick_folder {
            builder.blocking_pick_folder()
        } else {
            builder
                .add_filter(
                    "Émulateur / Archive (.exe, .zip, .7z)",
                    &["exe", "zip", "7z", "AppImage", "app", "bin"],
                )
                .blocking_pick_file()
        }
    })
    .await
    .map_err(|e| format!("échec ouverture sélecteur Switch : {e}"))?;

    let Some(picked_item) = picked else {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        return Ok(session.bootstrap_snapshot());
    };
    let src = picked_item
        .into_path()
        .map_err(|e| format!("chemin sélectionné invalide : {e}"))?;

    let vault_root = {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        session.switch_vault_root.clone()
    };
    tauri::async_runtime::spawn_blocking(move || {
        zaimule_core::external::import_switch_engine(&src, &vault_root, target)
    })
    .await
    .map_err(|e| format!("échec importation moteur Switch : {e}"))??;

    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.refresh_switch_vault())
}

#[tauri::command]
fn import_switch_keys_path(
    state: tauri::State<AppState>,
    paths: Vec<String>,
) -> Result<StationBootstrap, String> {
    if paths.is_empty() {
        return Err("aucun fichier de clés sélectionné".into());
    }
    let path_bufs: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.import_switch_keys(&path_bufs)
}

#[tauri::command]
async fn pick_and_import_switch_keys(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<StationBootstrap, String> {
    let app_for_dialog = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_dialog
            .dialog()
            .file()
            .set_title("Importer vos clés Switch (prod.keys / title.keys)")
            .add_filter("Clés Switch (.keys)", &["keys"])
            .blocking_pick_files()
    })
    .await
    .map_err(|e| format!("échec ouverture sélecteur de clés : {e}"))?;

    let Some(files) = picked else {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        return Ok(session.bootstrap_snapshot());
    };
    let paths: Vec<PathBuf> = files
        .into_iter()
        .filter_map(|f| f.into_path().ok())
        .collect();
    if paths.is_empty() {
        return Err("aucun fichier clé valide sélectionné".into());
    }
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    session.import_switch_keys(&paths)
}

#[tauri::command]
async fn import_switch_firmware_path(
    state: tauri::State<'_, AppState>,
    source_path: String,
) -> Result<StationBootstrap, String> {
    let vault_root = {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        session.switch_vault_root.clone()
    };
    let src = PathBuf::from(source_path);
    tauri::async_runtime::spawn_blocking(move || {
        zaimule_core::external::import_switch_firmware(&src, &vault_root)
    })
    .await
    .map_err(|e| format!("échec tâche d'import firmware Switch : {e}"))??;

    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.refresh_switch_vault())
}

#[tauri::command]
async fn pick_and_import_switch_firmware(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    mode: Option<String>,
) -> Result<StationBootstrap, String> {
    let app_for_dialog = app.clone();
    let pick_folder = mode.as_deref() == Some("folder");

    let picked = tauri::async_runtime::spawn_blocking(move || {
        let builder = app_for_dialog
            .dialog()
            .file()
            .set_title("Importer le Firmware Nintendo Switch (.zip, .7z ou dossier .nca)");
        if pick_folder {
            builder.blocking_pick_folder()
        } else {
            builder
                .add_filter("Archive Firmware Switch (.zip, .7z)", &["zip", "7z"])
                .blocking_pick_file()
        }
    })
    .await
    .map_err(|e| format!("échec ouverture sélecteur firmware Switch : {e}"))?;

    let Some(picked_item) = picked else {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        return Ok(session.bootstrap_snapshot());
    };
    let src = picked_item
        .into_path()
        .map_err(|e| format!("chemin firmware invalide : {e}"))?;

    let vault_root = {
        let session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        session.switch_vault_root.clone()
    };
    tauri::async_runtime::spawn_blocking(move || {
        zaimule_core::external::import_switch_firmware(&src, &vault_root)
    })
    .await
    .map_err(|e| format!("échec importation firmware Switch : {e}"))??;

    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    Ok(session.refresh_switch_vault())
}

#[tauri::command]
fn set_game_switch_engine(
    app: AppHandle,
    state: tauri::State<AppState>,
    game_id: String,
    mode: SwitchEngineOverride,
) -> Result<StationBootstrap, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let snap = session.set_game_switch_engine(&game_id, mode)?;
    persist_library_best_effort(&app, &session);
    Ok(snap)
}

#[tauri::command]
fn clear_switch_shader_cache(
    state: tauri::State<AppState>,
    target: SwitchEngineTarget,
) -> Result<SwitchShaderCacheClearResponse, String> {
    let mut session = state
        .session
        .lock()
        .map_err(|_| "verrou session indisponible".to_string())?;
    let (cleared_bytes, bootstrap) = session.clear_switch_shader_cache(target)?;
    Ok(SwitchShaderCacheClearResponse {
        cleared_bytes,
        bootstrap,
    })
}

#[tauri::command]
fn launch_switch_game(
    app: AppHandle,
    state: tauri::State<AppState>,
    game_id: Option<String>,
) -> Result<String, String> {
    let (resolved_game_id, plan) = {
        let mut session = state
            .session
            .lock()
            .map_err(|_| "verrou session indisponible".to_string())?;
        flush_sram_best_effort(&app, &mut session);
        session.prepare_switch_launch(game_id.as_deref())?
    };

    let mut child = spawn_switch_process(&plan.executable_path, &plan.rom_path, plan.target)?;
    let started_at = Instant::now();
    let session_arc = Arc::clone(&state.session);
    let app_handle = app.clone();
    let game_id_for_wait = resolved_game_id;
    let target = plan.target;

    std::thread::spawn(move || {
        let exit_code = child.wait().ok().and_then(|s| s.code());
        let elapsed_seconds = started_at.elapsed().as_secs();
        if let Ok(mut guard) = session_arc.lock() {
            guard.record_switch_playtime(&game_id_for_wait, elapsed_seconds);
            persist_library_best_effort(&app_handle, &guard);
        }
        let _ = app_handle.emit(
            "switch-session-ended",
            SwitchSessionEndedEvent {
                game_id: game_id_for_wait,
                target,
                elapsed_seconds,
                exit_code,
            },
        );
    });

    Ok(format!(
        "{} lancé : {} ({})",
        plan.target.label(),
        plan.rom_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        plan.executable_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ))
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
            last_sram_flush_ms: Arc::new(AtomicU64::new(0)),
            last_playtime_flush_ms: Arc::new(AtomicU64::new(0)),
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            if let Ok(mut guard) = session_for_setup.lock() {
                if let (Ok(sys), Ok(sav), Ok(cache), Ok(vault)) = (
                    system_dir(&handle),
                    save_dir(&handle),
                    rom_cache_dir(&handle),
                    switch_vault_dir(&handle),
                ) {
                    let _ = fs::create_dir_all(&sys);
                    let _ = fs::create_dir_all(&sav);
                    let _ = fs::create_dir_all(&cache);
                    let _ = fs::create_dir_all(&vault);
                    guard.configure_paths(sys, sav, cache);
                    guard.set_switch_vault_root(vault);
                }

                if let Ok(path) = settings_path(&handle) {
                    if let Ok(loaded) = load_json_atomic(&path) {
                        if loaded.warning.is_some() {
                            guard.warning = loaded.warning;
                        }
                        if !loaded.value.is_null() {
                            if let Ok(parsed) =
                                serde_json::from_value::<GlobalSettings>(loaded.value)
                            {
                                guard.apply_loaded_settings(parsed);
                            }
                        }
                    }
                }

                if let Ok(lp) = library_path(&handle) {
                    if let Ok((lib, lib_warn)) = load_library_atomic(&lp) {
                        if lib_warn.is_some() && guard.warning.is_none() {
                            guard.warning = lib_warn;
                        }
                        for g in &lib.catalog {
                            if g.rom_path.is_none() {
                                if let Ok(rp) = rom_archive_path(&handle, &g.id) {
                                    if rp.is_file() {
                                        if let Ok(bytes) = fs::read(&rp) {
                                            guard.custom_roms.insert(g.id.clone(), bytes);
                                        }
                                    }
                                }
                            }
                        }
                        if !lib.catalog.is_empty() || !lib.rom_directories.is_empty() {
                            guard.restore_persisted_library(lib);
                        }
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
                    if let Ok(mut guard) = state.session.lock() {
                        flush_sram_best_effort(app, &mut guard);
                        let _ = guard.take_playtime_dirty();
                        persist_library_best_effort(app, &guard);
                        persist_settings_best_effort(app, &mut guard);
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_station_bootstrap,
            select_game,
            reset_active_game,
            import_rom_file,
            pick_and_import_roms,
            pick_and_scan_folder,
            import_rom_paths,
            scan_rom_directory,
            remove_rom_directory,
            rescan_all_rom_directories,
            remove_game_from_library,
            toggle_game_favorite,
            import_bios_paths,
            pick_and_import_bios,
            refresh_switch_vault,
            import_switch_engine_path,
            pick_and_import_switch_engine,
            import_switch_keys_path,
            pick_and_import_switch_keys,
            import_switch_firmware_path,
            pick_and_import_switch_firmware,
            set_game_switch_engine,
            clear_switch_shader_cache,
            launch_switch_game,
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
            remove_memory_watchpoint,
            toggle_tas_timeline_button,
            add_cheat_code,
            toggle_cheat_code,
            remove_cheat_code,
            scan_ram_byte,
            create_netplay_room,
            join_netplay_deep_link,
            connect_netplay_peer,
            save_recorded_macro,
            save_keymapper_layout,
            step_viewport,
            step_viewport_binary,
            set_dual_link_enabled,
            set_quad_link_enabled,
            get_vram_and_disassembly,
            toggle_breakpoint,
            create_save_state,
            restore_save_state,
            delete_save_state,
            rewind_frames,
            set_active_macro,
            update_game_override,
            update_ui_preferences,
            toggle_mod_layer,
            load_settings,
            save_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running Zaimule application");
}
