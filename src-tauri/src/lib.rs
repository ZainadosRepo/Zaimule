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
};
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

fn flush_sram_best_effort(app: &AppHandle, session: &EmulatorSession) {
    if let Ok(path) = sram_path(app, &session.active_game_id) {
        let _ = save_sram_with_rotation(&path, session.active_sram_bytes());
    }
}

fn preload_disk_assets_for_game(app: &AppHandle, session: &mut EmulatorSession, game_id: &str) {
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
        if now.saturating_sub(last_flush) >= LIVE_SRAM_FLUSH_INTERVAL_MS && session.sram_dirty {
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
    Ok(session.bootstrap_snapshot())
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
fn load_settings(app: AppHandle) -> Result<LoadedJson, String> {
    load_json_atomic(&settings_path(&app)?)
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Value) -> Result<(), String> {
    save_json_atomic(&settings_path(&app)?, &settings)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let session = Arc::new(Mutex::new(EmulatorSession::new()));
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
            save_recorded_macro,
            save_keymapper_layout,
            step_viewport,
            set_dual_link_enabled,
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
