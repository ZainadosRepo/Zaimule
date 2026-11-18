//! Genuine native Libretro C-ABI (`libretro.h` v1) dynamic host backed by `libloading`.
//! Loads real `.dll` / `.dylib` / `.so` cores (e.g. `melonds_libretro.dll`, `mgba_libretro.dll`),
//! runs frames via `retro_run()`, converts `RGB0555`/`RGB565`/`XRGB8888` framebuffers with `pitch`,
//! forwards Joypad + NDS Pointer touchscreen inputs, and exposes SRAM & Save States.

use crate::libretro::{
    convert_rgb0555_to_rgba8888, convert_rgb565_to_rgba8888, convert_xrgb8888_to_rgba8888,
    RetroPixelFormat,
};
use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use libloading::Library;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{c_char, c_double, c_float, c_uint, c_void, CStr, CString};
use std::path::Path;
use std::sync::atomic::{AtomicPtr, Ordering};
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{
    BTN_A, BTN_B, BTN_DOWN, BTN_L, BTN_LEFT, BTN_R, BTN_RIGHT, BTN_SELECT, BTN_START, BTN_UP,
    BTN_X, BTN_Y,
};

const RETRO_MEMORY_SAVE_RAM: c_uint = 0;

const RETRO_DEVICE_JOYPAD: c_uint = 1;
const RETRO_DEVICE_POINTER: c_uint = 6;

const RETRO_DEVICE_ID_JOYPAD_B: c_uint = 0;
const RETRO_DEVICE_ID_JOYPAD_Y: c_uint = 1;
const RETRO_DEVICE_ID_JOYPAD_SELECT: c_uint = 2;
const RETRO_DEVICE_ID_JOYPAD_START: c_uint = 3;
const RETRO_DEVICE_ID_JOYPAD_UP: c_uint = 4;
const RETRO_DEVICE_ID_JOYPAD_DOWN: c_uint = 5;
const RETRO_DEVICE_ID_JOYPAD_LEFT: c_uint = 6;
const RETRO_DEVICE_ID_JOYPAD_RIGHT: c_uint = 7;
const RETRO_DEVICE_ID_JOYPAD_A: c_uint = 8;
const RETRO_DEVICE_ID_JOYPAD_X: c_uint = 9;
const RETRO_DEVICE_ID_JOYPAD_L: c_uint = 10;
const RETRO_DEVICE_ID_JOYPAD_R: c_uint = 11;
const RETRO_DEVICE_ID_JOYPAD_MASK: c_uint = 256;

const RETRO_DEVICE_ID_POINTER_X: c_uint = 0;
const RETRO_DEVICE_ID_POINTER_Y: c_uint = 1;
const RETRO_DEVICE_ID_POINTER_PRESSED: c_uint = 2;

const RETRO_ENVIRONMENT_GET_CAN_DUPE: c_uint = 3;
const RETRO_ENVIRONMENT_GET_SYSTEM_DIRECTORY: c_uint = 9;
const RETRO_ENVIRONMENT_SET_PIXEL_FORMAT: c_uint = 10;
const RETRO_ENVIRONMENT_SET_INPUT_DESCRIPTORS: c_uint = 11;
const RETRO_ENVIRONMENT_SET_HW_RENDER: c_uint = 14;
const RETRO_ENVIRONMENT_GET_VARIABLE: c_uint = 15;
const RETRO_ENVIRONMENT_SET_VARIABLES: c_uint = 16;
const RETRO_ENVIRONMENT_GET_VARIABLE_UPDATE: c_uint = 17;
const RETRO_ENVIRONMENT_SET_SUPPORT_NO_GAME: c_uint = 18;
const RETRO_ENVIRONMENT_GET_LIBRETRO_PATH: c_uint = 19;
const RETRO_ENVIRONMENT_GET_LOG_INTERFACE: c_uint = 27;
const RETRO_ENVIRONMENT_GET_CORE_ASSETS_DIRECTORY: c_uint = 30;
const RETRO_ENVIRONMENT_GET_SAVE_DIRECTORY: c_uint = 31;
const RETRO_ENVIRONMENT_SET_SYSTEM_AV_INFO: c_uint = 32;
const RETRO_ENVIRONMENT_SET_CONTROLLER_INFO: c_uint = 35;
const RETRO_ENVIRONMENT_SET_GEOMETRY: c_uint = 37;
const RETRO_ENVIRONMENT_GET_USERNAME: c_uint = 38;
const RETRO_ENVIRONMENT_GET_LANGUAGE: c_uint = 39;
const RETRO_ENVIRONMENT_SET_SUPPORT_ACHIEVEMENTS: c_uint = 42;
const RETRO_ENVIRONMENT_SET_SERIALIZATION_QUIRKS: c_uint = 44;
const RETRO_ENVIRONMENT_GET_INPUT_BITMASKS: c_uint = 51;
const RETRO_ENVIRONMENT_GET_CORE_OPTIONS_VERSION: c_uint = 52;

#[repr(C)]
struct RetroSystemInfo {
    library_name: *const c_char,
    library_version: *const c_char,
    valid_extensions: *const c_char,
    need_fullpath: bool,
    block_extract: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroGameGeometry {
    base_width: c_uint,
    base_height: c_uint,
    max_width: c_uint,
    max_height: c_uint,
    aspect_ratio: c_float,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroSystemTiming {
    fps: c_double,
    sample_rate: c_double,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroSystemAvInfo {
    geometry: RetroGameGeometry,
    timing: RetroSystemTiming,
}

#[repr(C)]
struct RetroGameInfo {
    path: *const c_char,
    data: *const c_void,
    size: usize,
    meta: *const c_char,
}

#[repr(C)]
struct RetroVariable {
    key: *const c_char,
    value: *const c_char,
}

type EnvironmentCb = unsafe extern "C" fn(cmd: c_uint, data: *mut c_void) -> bool;
type VideoRefreshCb =
    unsafe extern "C" fn(data: *const c_void, width: c_uint, height: c_uint, pitch: usize);
type AudioSampleCb = unsafe extern "C" fn(left: i16, right: i16);
type AudioSampleBatchCb = unsafe extern "C" fn(data: *const i16, frames: usize) -> usize;
type InputPollCb = unsafe extern "C" fn();
type InputStateCb =
    unsafe extern "C" fn(port: c_uint, device: c_uint, index: c_uint, id: c_uint) -> i16;

type FnRetroSetEnv = unsafe extern "C" fn(cb: EnvironmentCb);
type FnRetroSetVideo = unsafe extern "C" fn(cb: VideoRefreshCb);
type FnRetroSetAudio = unsafe extern "C" fn(cb: AudioSampleCb);
type FnRetroSetAudioBatch = unsafe extern "C" fn(cb: AudioSampleBatchCb);
type FnRetroSetInputPoll = unsafe extern "C" fn(cb: InputPollCb);
type FnRetroSetInputState = unsafe extern "C" fn(cb: InputStateCb);
type FnRetroVoid = unsafe extern "C" fn();
type FnRetroApiVersion = unsafe extern "C" fn() -> c_uint;
type FnRetroGetSysInfo = unsafe extern "C" fn(info: *mut RetroSystemInfo);
type FnRetroGetAvInfo = unsafe extern "C" fn(info: *mut RetroSystemAvInfo);
type FnRetroLoadGame = unsafe extern "C" fn(game: *const RetroGameInfo) -> bool;
type FnRetroSerializeSize = unsafe extern "C" fn() -> usize;
type FnRetroSerialize = unsafe extern "C" fn(data: *mut c_void, size: usize) -> bool;
type FnRetroUnserialize = unsafe extern "C" fn(data: *const c_void, size: usize) -> bool;
type FnRetroGetMemData = unsafe extern "C" fn(id: c_uint) -> *mut c_void;
type FnRetroGetMemSize = unsafe extern "C" fn(id: c_uint) -> usize;

struct LibretroCallbackState {
    pixel_format: RetroPixelFormat,
    frame_width: u16,
    frame_height: u16,
    last_rgba: Vec<u8>,
    last_audio_rms: f32,
    buttons_mask: u16,
    stylus_x: u8,
    stylus_y: u8,
    stylus_down: bool,
    system_dir_c: CString,
    save_dir_c: CString,
    core_dir_c: CString,
    username_c: CString,
    variables: HashMap<String, CString>,
}

impl LibretroCallbackState {
    fn new(console: ConsoleKind, system_dir: &Path, save_dir: &Path, core_path: &Path) -> Self {
        let (default_w, default_h) = match console {
            ConsoleKind::Nds => (256u16, 384u16),
            ConsoleKind::Gba => (240, 160),
            ConsoleKind::Snes | ConsoleKind::Nes => (256, 224),
            ConsoleKind::Psp => (480, 272),
            _ => (320, 240),
        };
        let core_dir = core_path.parent().unwrap_or(system_dir);
        let mut variables = HashMap::new();
        for (k, v) in [
            ("melonds_boot_directly", "enabled"),
            ("melonds_screen_layout", "Top/Bottom"),
            ("melonds_touch_mode", "Touch"),
            ("melonds_screen_gap", "0"),
            ("melonds_jit_enable", "enabled"),
            ("melonds_threaded_renderer", "disabled"),
            ("melonds_opengl_renderer", "disabled"),
            ("mgba_skip_bios", "OFF"),
            ("mgba_use_bios", "ON"),
        ] {
            if let Ok(c) = CString::new(v) {
                variables.insert(k.to_string(), c);
            }
        }

        Self {
            pixel_format: RetroPixelFormat::Rgb0555,
            frame_width: default_w,
            frame_height: default_h,
            last_rgba: vec![0u8; (default_w as usize) * (default_h as usize) * 4],
            last_audio_rms: 0.0,
            buttons_mask: 0,
            stylus_x: 128,
            stylus_y: 96,
            stylus_down: false,
            system_dir_c: CString::new(system_dir.to_string_lossy().as_bytes())
                .unwrap_or_else(|_| CString::new(".").unwrap()),
            save_dir_c: CString::new(save_dir.to_string_lossy().as_bytes())
                .unwrap_or_else(|_| CString::new(".").unwrap()),
            core_dir_c: CString::new(core_dir.to_string_lossy().as_bytes())
                .unwrap_or_else(|_| CString::new(".").unwrap()),
            username_c: CString::new("Zainados").unwrap(),
            variables,
        }
    }
}

thread_local! {
    static HOST_STATE_PTR: Cell<*mut LibretroCallbackState> = const { Cell::new(std::ptr::null_mut()) };
}
static GLOBAL_FALLBACK_PTR: AtomicPtr<LibretroCallbackState> = AtomicPtr::new(std::ptr::null_mut());

struct ActiveHostGuard {
    prev_tl: *mut LibretroCallbackState,
    prev_global: *mut LibretroCallbackState,
}

impl ActiveHostGuard {
    fn enter(state: &mut LibretroCallbackState) -> Self {
        let ptr: *mut LibretroCallbackState = state;
        let prev_tl = HOST_STATE_PTR.with(|c| {
            let prev = c.get();
            c.set(ptr);
            prev
        });
        let prev_global = GLOBAL_FALLBACK_PTR.swap(ptr, Ordering::AcqRel);
        Self {
            prev_tl,
            prev_global,
        }
    }
}

impl Drop for ActiveHostGuard {
    fn drop(&mut self) {
        HOST_STATE_PTR.with(|c| c.set(self.prev_tl));
        GLOBAL_FALLBACK_PTR.store(self.prev_global, Ordering::Release);
    }
}

#[inline]
unsafe fn current_cb_state<'a>() -> Option<&'a mut LibretroCallbackState> {
    let mut ptr = HOST_STATE_PTR.with(|c| c.get());
    if ptr.is_null() {
        ptr = GLOBAL_FALLBACK_PTR.load(Ordering::Acquire);
    }
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &mut *ptr })
    }
}

/// Converts a pitch-strided Libretro video frame buffer into tightly packed RGBA8888 pixels.
pub fn decode_strided_libretro_video(
    raw_bytes: &[u8],
    width: usize,
    height: usize,
    pitch: usize,
    format: RetroPixelFormat,
) -> Vec<u8> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(width * height * 4);
    match format {
        RetroPixelFormat::Rgb0555 | RetroPixelFormat::Rgb565 => {
            let row_bytes = width * 2;
            let mut line_u16 = vec![0u16; width];
            for y in 0..height {
                let start = y * pitch;
                if start + row_bytes > raw_bytes.len() {
                    out.resize(out.len() + width * 4, 0);
                    continue;
                }
                let src = &raw_bytes[start..start + row_bytes];
                let (chunks, _) = src.as_chunks::<2>();
                for (dst, chunk) in line_u16.iter_mut().zip(chunks) {
                    *dst = u16::from_le_bytes(*chunk);
                }
                let converted = if format == RetroPixelFormat::Rgb565 {
                    convert_rgb565_to_rgba8888(&line_u16)
                } else {
                    convert_rgb0555_to_rgba8888(&line_u16)
                };
                out.extend_from_slice(&converted);
            }
        }
        RetroPixelFormat::Xrgb8888 => {
            let row_bytes = width * 4;
            let mut line_u32 = vec![0u32; width];
            for y in 0..height {
                let start = y * pitch;
                if start + row_bytes > raw_bytes.len() {
                    out.resize(out.len() + width * 4, 0);
                    continue;
                }
                let src = &raw_bytes[start..start + row_bytes];
                let (chunks, _) = src.as_chunks::<4>();
                for (dst, chunk) in line_u32.iter_mut().zip(chunks) {
                    *dst = u32::from_le_bytes(*chunk);
                }
                out.extend_from_slice(&convert_xrgb8888_to_rgba8888(&line_u32));
            }
        }
    }
    out
}

/// Scales Zaimule NDS bottom touchscreen coordinates (`0..255`, `0..191`) into Libretro
/// `RETRO_DEVICE_POINTER` coordinates (`-0x7FFF..=0x7FFF`) spanning the 256x384 dual display.
pub fn stylus_to_libretro_pointer(stylus_x: u8, stylus_y: u8) -> (i16, i16) {
    let norm_x = (stylus_x as f32 / 255.0).clamp(0.0, 1.0);
    let full_y = 192.0 + (stylus_y.min(191) as f32);
    let norm_y = (full_y / 383.0).clamp(0.0, 1.0);
    let px = ((norm_x * 2.0 - 1.0) * 32767.0).round() as i16;
    let py = ((norm_y * 2.0 - 1.0) * 32767.0).round() as i16;
    (px, py)
}

fn joypad_mask_to_retro_bitmask(mask: u16) -> u16 {
    let mut out = 0u16;
    if (mask & BTN_B) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_B;
    }
    if (mask & BTN_Y) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_Y;
    }
    if (mask & BTN_SELECT) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_SELECT;
    }
    if (mask & BTN_START) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_START;
    }
    if (mask & BTN_UP) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_UP;
    }
    if (mask & BTN_DOWN) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_DOWN;
    }
    if (mask & BTN_LEFT) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_LEFT;
    }
    if (mask & BTN_RIGHT) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_RIGHT;
    }
    if (mask & BTN_A) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_A;
    }
    if (mask & BTN_X) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_X;
    }
    if (mask & BTN_L) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_L;
    }
    if (mask & BTN_R) != 0 {
        out |= 1 << RETRO_DEVICE_ID_JOYPAD_R;
    }
    out
}

unsafe extern "C" fn retro_environment_cb(cmd: c_uint, data: *mut c_void) -> bool {
    let Some(state) = (unsafe { current_cb_state() }) else {
        return false;
    };
    let clean_cmd = cmd & 0xFFFF;
    match clean_cmd {
        RETRO_ENVIRONMENT_GET_CAN_DUPE => {
            if !data.is_null() {
                unsafe { *(data as *mut bool) = true };
            }
            true
        }
        RETRO_ENVIRONMENT_GET_SYSTEM_DIRECTORY | RETRO_ENVIRONMENT_GET_CORE_ASSETS_DIRECTORY => {
            if !data.is_null() {
                unsafe { *(data as *mut *const c_char) = state.system_dir_c.as_ptr() };
                true
            } else {
                false
            }
        }
        RETRO_ENVIRONMENT_GET_SAVE_DIRECTORY => {
            if !data.is_null() {
                unsafe { *(data as *mut *const c_char) = state.save_dir_c.as_ptr() };
                true
            } else {
                false
            }
        }
        RETRO_ENVIRONMENT_GET_LIBRETRO_PATH => {
            if !data.is_null() {
                unsafe { *(data as *mut *const c_char) = state.core_dir_c.as_ptr() };
                true
            } else {
                false
            }
        }
        RETRO_ENVIRONMENT_GET_USERNAME => {
            if !data.is_null() {
                unsafe { *(data as *mut *const c_char) = state.username_c.as_ptr() };
                true
            } else {
                false
            }
        }
        RETRO_ENVIRONMENT_GET_LANGUAGE => {
            if !data.is_null() {
                // RETRO_LANGUAGE_FRENCH = 2
                unsafe { *(data as *mut c_uint) = 2 };
                true
            } else {
                false
            }
        }
        RETRO_ENVIRONMENT_SET_PIXEL_FORMAT => {
            if data.is_null() {
                return false;
            }
            let fmt = unsafe { *(data as *const c_uint) };
            match fmt {
                0 => {
                    state.pixel_format = RetroPixelFormat::Rgb0555;
                    true
                }
                1 => {
                    state.pixel_format = RetroPixelFormat::Xrgb8888;
                    true
                }
                2 => {
                    state.pixel_format = RetroPixelFormat::Rgb565;
                    true
                }
                _ => false,
            }
        }
        RETRO_ENVIRONMENT_SET_VARIABLES => {
            if data.is_null() {
                return true;
            }
            let mut ptr = data as *const RetroVariable;
            loop {
                let item = unsafe { &*ptr };
                if item.key.is_null() || item.value.is_null() {
                    break;
                }
                let key_str = unsafe { CStr::from_ptr(item.key) }
                    .to_string_lossy()
                    .to_string();
                let desc_str = unsafe { CStr::from_ptr(item.value) }
                    .to_string_lossy()
                    .to_string();
                // Format is "Description; default|opt2|opt3"
                let default_val = desc_str
                    .split_once(';')
                    .map(|(_, rhs)| rhs.trim().split('|').next().unwrap_or("").trim())
                    .unwrap_or("")
                    .to_string();
                if !default_val.is_empty() && !state.variables.contains_key(&key_str) {
                    if let Ok(c) = CString::new(default_val) {
                        state.variables.insert(key_str, c);
                    }
                }
                ptr = unsafe { ptr.add(1) };
            }
            true
        }
        RETRO_ENVIRONMENT_GET_VARIABLE => {
            if data.is_null() {
                return false;
            }
            let var = unsafe { &mut *(data as *mut RetroVariable) };
            if var.key.is_null() {
                return false;
            }
            let key_str = unsafe { CStr::from_ptr(var.key) }.to_string_lossy();
            if let Some(val_c) = state.variables.get(key_str.as_ref()) {
                var.value = val_c.as_ptr();
                true
            } else {
                var.value = std::ptr::null();
                false
            }
        }
        RETRO_ENVIRONMENT_GET_VARIABLE_UPDATE => {
            if !data.is_null() {
                unsafe { *(data as *mut bool) = false };
            }
            true
        }
        RETRO_ENVIRONMENT_GET_INPUT_BITMASKS => true,
        RETRO_ENVIRONMENT_GET_CORE_OPTIONS_VERSION => {
            if !data.is_null() {
                unsafe { *(data as *mut c_uint) = 0 };
            }
            true
        }
        RETRO_ENVIRONMENT_SET_GEOMETRY => {
            if !data.is_null() {
                let geom = unsafe { &*(data as *const RetroGameGeometry) };
                if geom.base_width > 0 && geom.base_height > 0 {
                    state.frame_width = geom.base_width as u16;
                    state.frame_height = geom.base_height as u16;
                }
            }
            true
        }
        RETRO_ENVIRONMENT_SET_SYSTEM_AV_INFO => {
            if !data.is_null() {
                let av = unsafe { &*(data as *const RetroSystemAvInfo) };
                if av.geometry.base_width > 0 && av.geometry.base_height > 0 {
                    state.frame_width = av.geometry.base_width as u16;
                    state.frame_height = av.geometry.base_height as u16;
                }
            }
            true
        }
        RETRO_ENVIRONMENT_SET_INPUT_DESCRIPTORS
        | RETRO_ENVIRONMENT_SET_CONTROLLER_INFO
        | RETRO_ENVIRONMENT_SET_SUPPORT_NO_GAME
        | RETRO_ENVIRONMENT_SET_SUPPORT_ACHIEVEMENTS
        | RETRO_ENVIRONMENT_SET_SERIALIZATION_QUIRKS => true,
        RETRO_ENVIRONMENT_SET_HW_RENDER | RETRO_ENVIRONMENT_GET_LOG_INTERFACE => false,
        _ => false,
    }
}

unsafe extern "C" fn retro_video_refresh_cb(
    data: *const c_void,
    width: c_uint,
    height: c_uint,
    pitch: usize,
) {
    // NULL (dupe frame) or RETRO_HW_FRAME_BUFFER_VALID (-1) -> keep previous framebuffer intact
    if data.is_null() || (data as usize) == usize::MAX || width == 0 || height == 0 {
        return;
    }
    let Some(state) = (unsafe { current_cb_state() }) else {
        return;
    };
    let w = width as usize;
    let h = height as usize;
    let total_bytes = h.saturating_mul(pitch.max(w * 2));
    if total_bytes == 0 || total_bytes > 64 * 1024 * 1024 {
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, total_bytes) };
    state.frame_width = width as u16;
    state.frame_height = height as u16;
    state.last_rgba = decode_strided_libretro_video(slice, w, h, pitch, state.pixel_format);
}

unsafe extern "C" fn retro_audio_sample_cb(left: i16, right: i16) {
    if let Some(state) = unsafe { current_cb_state() } {
        let avg = (f32::from(left.abs()) + f32::from(right.abs())) / 65536.0;
        state.last_audio_rms = avg.clamp(0.0, 1.0);
    }
}

unsafe extern "C" fn retro_audio_sample_batch_cb(data: *const i16, frames: usize) -> usize {
    if data.is_null() || frames == 0 {
        return frames;
    }
    if let Some(state) = unsafe { current_cb_state() } {
        let count = frames.min(2048) * 2;
        let samples = unsafe { std::slice::from_raw_parts(data, count) };
        let sum_abs: f32 = samples.iter().map(|&s| f32::from(s.saturating_abs())).sum();
        state.last_audio_rms = (sum_abs / (samples.len().max(1) as f32 * 32768.0)).clamp(0.0, 1.0);
    }
    frames
}

unsafe extern "C" fn retro_input_poll_cb() {}

unsafe extern "C" fn retro_input_state_cb(
    port: c_uint,
    device: c_uint,
    _index: c_uint,
    id: c_uint,
) -> i16 {
    if port != 0 {
        return 0;
    }
    let Some(state) = (unsafe { current_cb_state() }) else {
        return 0;
    };
    if device == RETRO_DEVICE_JOYPAD {
        let retro_mask = joypad_mask_to_retro_bitmask(state.buttons_mask);
        if id == RETRO_DEVICE_ID_JOYPAD_MASK {
            return retro_mask as i16;
        }
        if id < 16 && (retro_mask & (1 << id)) != 0 {
            return 1;
        }
    } else if device == RETRO_DEVICE_POINTER {
        let (px, py) = stylus_to_libretro_pointer(state.stylus_x, state.stylus_y);
        match id {
            RETRO_DEVICE_ID_POINTER_X => return px,
            RETRO_DEVICE_ID_POINTER_Y => return py,
            RETRO_DEVICE_ID_POINTER_PRESSED => return if state.stylus_down { 1 } else { 0 },
            _ => {}
        }
    }
    0
}

/// Real Libretro C-ABI emulator instance executing frames through a loaded `.dll`/`.dylib`/`.so`.
pub struct LibretroHostCore {
    console: ConsoleKind,
    core_name: String,
    frame_number: u64,
    game_loaded: bool,
    initialized: bool,
    sram_shadow: Vec<u8>,
    cb_state: Box<LibretroCallbackState>,
    _library: Library,
    fn_deinit: FnRetroVoid,
    fn_run: FnRetroVoid,
    fn_unload_game: FnRetroVoid,
    fn_serialize_size: FnRetroSerializeSize,
    fn_serialize: FnRetroSerialize,
    fn_unserialize: FnRetroUnserialize,
    fn_get_mem_data: FnRetroGetMemData,
    fn_get_mem_size: FnRetroGetMemSize,
}

impl LibretroHostCore {
    /// Loads `core_path` via `libloading`, initializes Libretro callbacks, and boots `rom_path`.
    pub fn load(
        console: ConsoleKind,
        core_path: &Path,
        rom_path: &Path,
        system_dir: &Path,
        save_dir: &Path,
    ) -> Result<Self, String> {
        if !core_path.is_file() {
            return Err(format!("cœur C-ABI introuvable : {}", core_path.display()));
        }
        if !rom_path.is_file() {
            return Err(format!(
                "ROM introuvable sur disque : {}",
                rom_path.display()
            ));
        }
        let _ = std::fs::create_dir_all(system_dir);
        let _ = std::fs::create_dir_all(save_dir);

        let library = unsafe { Library::new(core_path) }
            .map_err(|e| format!("échec ouverture dynamique {} : {e}", core_path.display()))?;

        let fn_set_env: FnRetroSetEnv = unsafe {
            *library
                .get(b"retro_set_environment\0")
                .map_err(|e| format!("retro_set_environment absent : {e}"))?
        };
        let fn_set_video: FnRetroSetVideo = unsafe {
            *library
                .get(b"retro_set_video_refresh\0")
                .map_err(|e| format!("retro_set_video_refresh absent : {e}"))?
        };
        let fn_set_audio: FnRetroSetAudio = unsafe {
            *library
                .get(b"retro_set_audio_sample\0")
                .map_err(|e| format!("retro_set_audio_sample absent : {e}"))?
        };
        let fn_set_audio_batch: FnRetroSetAudioBatch = unsafe {
            *library
                .get(b"retro_set_audio_sample_batch\0")
                .map_err(|e| format!("retro_set_audio_sample_batch absent : {e}"))?
        };
        let fn_set_input_poll: FnRetroSetInputPoll = unsafe {
            *library
                .get(b"retro_set_input_poll\0")
                .map_err(|e| format!("retro_set_input_poll absent : {e}"))?
        };
        let fn_set_input_state: FnRetroSetInputState = unsafe {
            *library
                .get(b"retro_set_input_state\0")
                .map_err(|e| format!("retro_set_input_state absent : {e}"))?
        };
        let fn_init: FnRetroVoid = unsafe {
            *library
                .get(b"retro_init\0")
                .map_err(|e| format!("retro_init absent : {e}"))?
        };
        let fn_deinit: FnRetroVoid = unsafe {
            *library
                .get(b"retro_deinit\0")
                .map_err(|e| format!("retro_deinit absent : {e}"))?
        };
        let fn_api_version: FnRetroApiVersion = unsafe {
            *library
                .get(b"retro_api_version\0")
                .map_err(|e| format!("retro_api_version absent : {e}"))?
        };
        let fn_get_sys_info: FnRetroGetSysInfo = unsafe {
            *library
                .get(b"retro_get_system_info\0")
                .map_err(|e| format!("retro_get_system_info absent : {e}"))?
        };
        let fn_get_av_info: FnRetroGetAvInfo = unsafe {
            *library
                .get(b"retro_get_system_av_info\0")
                .map_err(|e| format!("retro_get_system_av_info absent : {e}"))?
        };
        let fn_load_game: FnRetroLoadGame = unsafe {
            *library
                .get(b"retro_load_game\0")
                .map_err(|e| format!("retro_load_game absent : {e}"))?
        };
        let fn_unload_game: FnRetroVoid = unsafe {
            *library
                .get(b"retro_unload_game\0")
                .map_err(|e| format!("retro_unload_game absent : {e}"))?
        };
        let fn_run: FnRetroVoid = unsafe {
            *library
                .get(b"retro_run\0")
                .map_err(|e| format!("retro_run absent : {e}"))?
        };
        let fn_serialize_size: FnRetroSerializeSize = unsafe {
            *library
                .get(b"retro_serialize_size\0")
                .map_err(|e| format!("retro_serialize_size absent : {e}"))?
        };
        let fn_serialize: FnRetroSerialize = unsafe {
            *library
                .get(b"retro_serialize\0")
                .map_err(|e| format!("retro_serialize absent : {e}"))?
        };
        let fn_unserialize: FnRetroUnserialize = unsafe {
            *library
                .get(b"retro_unserialize\0")
                .map_err(|e| format!("retro_unserialize absent : {e}"))?
        };
        let fn_get_mem_data: FnRetroGetMemData = unsafe {
            *library
                .get(b"retro_get_memory_data\0")
                .map_err(|e| format!("retro_get_memory_data absent : {e}"))?
        };
        let fn_get_mem_size: FnRetroGetMemSize = unsafe {
            *library
                .get(b"retro_get_memory_size\0")
                .map_err(|e| format!("retro_get_memory_size absent : {e}"))?
        };

        let mut cb_state = Box::new(LibretroCallbackState::new(
            console, system_dir, save_dir, core_path,
        ));
        let api_ver = unsafe { fn_api_version() };
        if api_ver != 1 {
            return Err(format!("version ABI Libretro non supportée : {api_ver}"));
        }

        let mut sys_info = RetroSystemInfo {
            library_name: std::ptr::null(),
            library_version: std::ptr::null(),
            valid_extensions: std::ptr::null(),
            need_fullpath: true,
            block_extract: false,
        };
        unsafe { fn_get_sys_info(&mut sys_info) };
        let core_name = if !sys_info.library_name.is_null() {
            unsafe { CStr::from_ptr(sys_info.library_name) }
                .to_string_lossy()
                .to_string()
        } else {
            core_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        };

        let rom_path_c = CString::new(rom_path.to_string_lossy().as_bytes())
            .map_err(|_| "chemin ROM invalide (caractère nul)".to_string())?;

        // Read ROM bytes only if the core explicitly asks for memory buffer (`need_fullpath == false`)
        let rom_bytes_opt = if !sys_info.need_fullpath {
            Some(std::fs::read(rom_path).map_err(|e| format!("{}: {e}", rom_path.display()))?)
        } else {
            None
        };

        let (game_loaded, sram_shadow) = {
            let _guard = ActiveHostGuard::enter(&mut cb_state);
            unsafe {
                fn_set_env(retro_environment_cb);
                fn_set_video(retro_video_refresh_cb);
                fn_set_audio(retro_audio_sample_cb);
                fn_set_audio_batch(retro_audio_sample_batch_cb);
                fn_set_input_poll(retro_input_poll_cb);
                fn_set_input_state(retro_input_state_cb);
                fn_init();
            }

            let game_info = RetroGameInfo {
                path: rom_path_c.as_ptr(),
                data: rom_bytes_opt
                    .as_ref()
                    .map(|b| b.as_ptr() as *const c_void)
                    .unwrap_or(std::ptr::null()),
                size: rom_bytes_opt.as_ref().map(|b| b.len()).unwrap_or(0),
                meta: std::ptr::null(),
            };
            let ok = unsafe { fn_load_game(&game_info) };
            if !ok {
                unsafe { fn_deinit() };
                return Err(format!(
                    "{core_name} a refusé la ROM {} (vérifiez le format/extension ou les BIOS requis)",
                    rom_path.file_name().unwrap_or_default().to_string_lossy()
                ));
            }

            let mut av = RetroSystemAvInfo {
                geometry: RetroGameGeometry {
                    base_width: cb_state.frame_width as c_uint,
                    base_height: cb_state.frame_height as c_uint,
                    max_width: 1024,
                    max_height: 1024,
                    aspect_ratio: 0.0,
                },
                timing: RetroSystemTiming {
                    fps: 60.0,
                    sample_rate: 44100.0,
                },
            };
            unsafe { fn_get_av_info(&mut av) };
            if av.geometry.base_width > 0 && av.geometry.base_height > 0 {
                cb_state.frame_width = av.geometry.base_width as u16;
                cb_state.frame_height = av.geometry.base_height as u16;
                cb_state.last_rgba.resize(
                    (cb_state.frame_width as usize) * (cb_state.frame_height as usize) * 4,
                    0,
                );
            }

            let mem_sz = unsafe { fn_get_mem_size(RETRO_MEMORY_SAVE_RAM) }.min(4 * 1024 * 1024);
            let mem_ptr = unsafe { fn_get_mem_data(RETRO_MEMORY_SAVE_RAM) };
            let shadow = if mem_sz > 0 && !mem_ptr.is_null() {
                unsafe { std::slice::from_raw_parts(mem_ptr as *const u8, mem_sz) }.to_vec()
            } else {
                Vec::new()
            };
            (ok, shadow)
        };

        Ok(Self {
            console,
            core_name,
            frame_number: 0,
            game_loaded,
            initialized: true,
            sram_shadow,
            cb_state,
            _library: library,
            fn_deinit,
            fn_run,
            fn_unload_game,
            fn_serialize_size,
            fn_serialize,
            fn_unserialize,
            fn_get_mem_data,
            fn_get_mem_size,
        })
    }

    fn sync_sram_from_core(&mut self) {
        let _guard = ActiveHostGuard::enter(&mut self.cb_state);
        let mem_sz = unsafe { (self.fn_get_mem_size)(RETRO_MEMORY_SAVE_RAM) }.min(4 * 1024 * 1024);
        let mem_ptr = unsafe { (self.fn_get_mem_data)(RETRO_MEMORY_SAVE_RAM) };
        if mem_sz > 0 && !mem_ptr.is_null() {
            let live = unsafe { std::slice::from_raw_parts(mem_ptr as *const u8, mem_sz) };
            self.sram_shadow.resize(mem_sz, 0);
            self.sram_shadow.copy_from_slice(live);
        }
    }
}

impl Drop for LibretroHostCore {
    fn drop(&mut self) {
        let _guard = ActiveHostGuard::enter(&mut self.cb_state);
        if self.game_loaded {
            unsafe { (self.fn_unload_game)() };
            self.game_loaded = false;
        }
        if self.initialized {
            unsafe { (self.fn_deinit)() };
            self.initialized = false;
        }
    }
}

impl EmulatorBackend for LibretroHostCore {
    fn console(&self) -> ConsoleKind {
        self.console
    }

    fn load_rom(&mut self, _rom: &[u8]) -> Result<(), String> {
        // Disk-backed Libretro cores load directly from path in `LibretroHostCore::load`.
        Ok(())
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        self.frame_number += 1;
        self.cb_state.buttons_mask = buttons_mask;
        {
            let _guard = ActiveHostGuard::enter(&mut self.cb_state);
            unsafe { (self.fn_run)() };
        }
        if (self.frame_number % 60) == 1 {
            self.sync_sram_from_core();
        }
        FrameStepResult {
            console: self.console,
            frame_number: self.frame_number,
            width: self.cb_state.frame_width,
            height: self.cb_state.frame_height,
            rgba: self.cb_state.last_rgba.clone(),
            audio_level: self.cb_state.last_audio_rms,
            audio_freq_hz: 0,
            serial_out_byte: None,
            cpu: self.debug_snapshot(),
        }
    }

    fn save_state(&self) -> Result<Vec<u8>, String> {
        let size = unsafe { (self.fn_serialize_size)() };
        if size == 0 {
            return Err("ce cœur Libretro ne supporte pas retro_serialize_size".into());
        }
        let mut buf = vec![0u8; size];
        let ok = unsafe { (self.fn_serialize)(buf.as_mut_ptr() as *mut c_void, size) };
        if !ok {
            return Err("retro_serialize a échoué".into());
        }
        Ok(buf)
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        if data.is_empty() {
            return Err("Save State Libretro vide".into());
        }
        let _guard = ActiveHostGuard::enter(&mut self.cb_state);
        let ok = unsafe { (self.fn_unserialize)(data.as_ptr() as *const c_void, data.len()) };
        if !ok {
            return Err("retro_unserialize a échoué (état incompatible)".into());
        }
        Ok(())
    }

    fn sram(&self) -> &[u8] {
        &self.sram_shadow
    }

    fn load_sram(&mut self, data: &[u8]) -> Result<(), String> {
        let _guard = ActiveHostGuard::enter(&mut self.cb_state);
        let mem_sz = unsafe { (self.fn_get_mem_size)(RETRO_MEMORY_SAVE_RAM) };
        let mem_ptr = unsafe { (self.fn_get_mem_data)(RETRO_MEMORY_SAVE_RAM) };
        if mem_sz > 0 && !mem_ptr.is_null() {
            let copy_len = data.len().min(mem_sz);
            let dst = unsafe { std::slice::from_raw_parts_mut(mem_ptr as *mut u8, mem_sz) };
            dst[..copy_len].copy_from_slice(&data[..copy_len]);
            self.sram_shadow = dst.to_vec();
        }
        Ok(())
    }

    fn exchange_serial_byte(&mut self, _incoming: u8) -> u8 {
        0
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        CpuDebugSnapshot {
            arch: format!("Libretro C-ABI · {}", self.core_name),
            pc_hex: "0xCABI0001".into(),
            sp_hex: format!(
                "{}x{}",
                self.cb_state.frame_width, self.cb_state.frame_height
            ),
            reg_a_hex: format!("{:?}", self.cb_state.pixel_format),
            reg_bc_hex: format!("F:{}", self.frame_number),
            reg_de_hex: format!("STY:{},{}", self.cb_state.stylus_x, self.cb_state.stylus_y),
            reg_hl_hex: format!("SRAM:{}o", self.sram_shadow.len()),
            flags_str: if self.cb_state.stylus_down {
                "CABI_RUN+TOUCH".into()
            } else {
                "CABI_RUN".into()
            },
            ly_scanline: (self.frame_number % 192) as u8,
            serial_sb_hex: "0x00".into(),
            serial_sc_hex: "0x00".into(),
            cycles: self.frame_number.saturating_mul(256),
            score: 0,
        }
    }

    fn set_touch_stylus(&mut self, x: u8, y: u8, pressed: bool) {
        self.cb_state.stylus_x = x;
        self.cb_state.stylus_y = y.min(191);
        self.cb_state.stylus_down = pressed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_strided_xrgb8888_and_rgb565_buffers_cleanly() {
        // 2x2 XRGB8888 image with extra padding bytes (pitch = 12 instead of 8)
        let red_px = 0x00FF_0000u32.to_le_bytes();
        let blue_px = 0x0000_00FFu32.to_le_bytes();
        let pad = [0xAAu8; 4];
        let mut raw = Vec::new();
        raw.extend_from_slice(&red_px);
        raw.extend_from_slice(&blue_px);
        raw.extend_from_slice(&pad);
        raw.extend_from_slice(&blue_px);
        raw.extend_from_slice(&red_px);
        raw.extend_from_slice(&pad);

        let rgba = decode_strided_libretro_video(&raw, 2, 2, 12, RetroPixelFormat::Xrgb8888);
        assert_eq!(rgba.len(), 2 * 2 * 4);
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&rgba[4..8], &[0, 0, 255, 255]);
        assert_eq!(&rgba[8..12], &[0, 0, 255, 255]);
        assert_eq!(&rgba[12..16], &[255, 0, 0, 255]);
    }

    #[test]
    fn maps_nds_bottom_touchscreen_stylus_to_libretro_pointer_coordinates() {
        let (x_mid, y_top_of_bottom) = stylus_to_libretro_pointer(127, 0);
        assert!(x_mid.abs() < 300);
        // Bottom screen starts halfway down full 256x384 dual layout -> y around 0
        assert!(y_top_of_bottom.abs() < 300);

        let (x_right, y_bottom) = stylus_to_libretro_pointer(255, 191);
        assert_eq!(x_right, 32767);
        assert_eq!(y_bottom, 32767);
    }
}
