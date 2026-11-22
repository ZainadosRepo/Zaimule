//! Yuzu-inspired hierarchical configuration overrides (`GlobalSettings -> Console -> Game`).

use crate::catalog::ConsoleKind;
use crate::cheats::{CheatCodeEntry, MemoryWatchpoint};
use crate::keymap::KeymapWidget;
use crate::macros::MacroSequence;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Real-time display shader presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ShaderPreset {
    PixelPerfect,
    #[default]
    CrtTrinitron,
    LcdMatrix,
    PhosphorAmber,
    ScaleFx,
}

impl ShaderPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::PixelPerfect => "Pixel-Perfect (Integer)",
            Self::CrtTrinitron => "CRT Trinitron PVM + Scanlines",
            Self::LcdMatrix => "Matrice LCD Portable",
            Self::PhosphorAmber => "Moniteur Phosphore Ambre P3",
            Self::ScaleFx => "ScaleFX Lissage Vectoriel",
        }
    }
}

fn default_upscaler_mode() -> String {
    "XBRZ".into()
}

fn default_shader_css() -> String {
    "mode-clean".into()
}

fn default_nds_layout() -> String {
    "vertical".into()
}

fn default_nds_3d_scale() -> u8 {
    1
}

/// Global user preferences across Zaimule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GlobalSettings {
    pub default_shader: ShaderPreset,
    #[serde(default = "default_shader_css")]
    pub active_shader_css: String,
    #[serde(default = "default_upscaler_mode")]
    pub upscaler_mode: String,
    #[serde(default = "default_nds_layout")]
    pub nds_layout: String,
    #[serde(default = "default_nds_3d_scale")]
    pub nds_3d_scale: u8,
    #[serde(default)]
    pub viewport_integer_scale: bool,
    pub internal_scale: u8,
    pub speed_multiplier: u8,
    pub cpu_overclock_pct: u16,
    pub pgxp_subpixel: bool,
    pub integer_scaling: bool,
    pub rewind_enabled: bool,
    pub rewind_horizon_seconds: u16,
    pub audio_volume: f32,
    pub audio_latency_ms: u16,
    pub auto_backup_sram: bool,
    pub game_overrides: HashMap<String, GameProfileOverride>,
    #[serde(default)]
    pub rom_directories: Vec<String>,
    #[serde(default)]
    pub external_emulators: HashMap<String, String>,
    #[serde(default)]
    pub keymapper_widgets: Vec<KeymapWidget>,
    #[serde(default)]
    pub cheat_codes: Vec<CheatCodeEntry>,
    #[serde(default)]
    pub memory_watchpoints: Vec<MemoryWatchpoint>,
    #[serde(default)]
    pub macros: Vec<MacroSequence>,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            default_shader: ShaderPreset::CrtTrinitron,
            active_shader_css: default_shader_css(),
            upscaler_mode: default_upscaler_mode(),
            nds_layout: default_nds_layout(),
            nds_3d_scale: default_nds_3d_scale(),
            viewport_integer_scale: false,
            internal_scale: 4,
            speed_multiplier: 1,
            cpu_overclock_pct: 100,
            pgxp_subpixel: true,
            integer_scaling: true,
            rewind_enabled: true,
            rewind_horizon_seconds: 60,
            audio_volume: 0.85,
            audio_latency_ms: 16,
            auto_backup_sram: true,
            game_overrides: HashMap::new(),
            rom_directories: Vec::new(),
            external_emulators: HashMap::new(),
            keymapper_widgets: Vec::new(),
            cheat_codes: Vec::new(),
            memory_watchpoints: Vec::new(),
            macros: Vec::new(),
        }
    }
}

/// Per-game override (Yuzu-style right-click properties) where `None` inherits from global/console defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GameProfileOverride {
    pub custom_shader: Option<ShaderPreset>,
    pub custom_scale: Option<u8>,
    pub custom_speed_multiplier: Option<u8>,
    pub custom_cpu_overclock_pct: Option<u16>,
    pub custom_pgxp_subpixel: Option<bool>,
    pub integer_scaling: Option<bool>,
    pub preferred_core_id: Option<String>,
    pub active_ips_mods: Vec<String>,
}

/// Resolved effective settings for a specific game session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveGameConfig {
    pub game_id: String,
    pub console: ConsoleKind,
    pub shader: ShaderPreset,
    pub internal_scale: u8,
    pub speed_multiplier: u8,
    pub cpu_overclock_pct: u16,
    pub pgxp_subpixel: bool,
    pub integer_scaling: bool,
    pub core_id: String,
    pub active_ips_mods: Vec<String>,
}

/// Resolves the effective configuration by merging `GlobalSettings`, console ergonomics, and `GameProfileOverride`.
pub fn resolve_effective_config(
    global: &GlobalSettings,
    game_id: &str,
    console: ConsoleKind,
) -> EffectiveGameConfig {
    let ov = global.game_overrides.get(game_id);

    // Handhelds default nicely to LCD matrix if the user hasn't overridden, while home consoles use CRT
    let console_default_shader = match console {
        ConsoleKind::Gb | ConsoleKind::Gbc | ConsoleKind::Gba | ConsoleKind::Nds => {
            ShaderPreset::LcdMatrix
        }
        _ => global.default_shader,
    };

    let shader = ov
        .and_then(|o| o.custom_shader)
        .unwrap_or(console_default_shader);
    let internal_scale = ov
        .and_then(|o| o.custom_scale)
        .unwrap_or(global.internal_scale)
        .clamp(1, 8);
    let speed_multiplier = ov
        .and_then(|o| o.custom_speed_multiplier)
        .unwrap_or(global.speed_multiplier)
        .clamp(1, 8);
    let cpu_overclock_pct = ov
        .and_then(|o| o.custom_cpu_overclock_pct)
        .unwrap_or(global.cpu_overclock_pct)
        .clamp(50, 400);
    let pgxp_subpixel = ov
        .and_then(|o| o.custom_pgxp_subpixel)
        .unwrap_or(global.pgxp_subpixel);
    let integer_scaling = ov
        .and_then(|o| o.integer_scaling)
        .unwrap_or(global.integer_scaling);
    let core_id = ov
        .and_then(|o| o.preferred_core_id.clone())
        .unwrap_or_else(|| console.default_core_id().to_string());
    let active_ips_mods = ov.map(|o| o.active_ips_mods.clone()).unwrap_or_default();

    EffectiveGameConfig {
        game_id: game_id.to_string(),
        console,
        shader,
        internal_scale,
        speed_multiplier,
        cpu_overclock_pct,
        pgxp_subpixel,
        integer_scaling,
        core_id,
        active_ips_mods,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_override_takes_precedence_over_global_and_console_defaults() {
        let mut global = GlobalSettings::default();
        let cfg_default = resolve_effective_config(&global, "gba-emerald-fr", ConsoleKind::Gba);
        assert_eq!(cfg_default.shader, ShaderPreset::LcdMatrix);
        assert_eq!(cfg_default.internal_scale, 4);
        assert_eq!(cfg_default.cpu_overclock_pct, 100);
        assert!(cfg_default.pgxp_subpixel);

        global.game_overrides.insert(
            "gba-emerald-fr".into(),
            GameProfileOverride {
                custom_shader: Some(ShaderPreset::ScaleFx),
                custom_scale: Some(6),
                custom_speed_multiplier: Some(2),
                custom_cpu_overclock_pct: Some(200),
                custom_pgxp_subpixel: Some(false),
                integer_scaling: Some(false),
                preferred_core_id: Some("zaimule-rust-gba".into()),
                active_ips_mods: vec!["patch_60fps.ips".into()],
            },
        );

        let cfg_custom = resolve_effective_config(&global, "gba-emerald-fr", ConsoleKind::Gba);
        assert_eq!(cfg_custom.shader, ShaderPreset::ScaleFx);
        assert_eq!(cfg_custom.internal_scale, 6);
        assert_eq!(cfg_custom.speed_multiplier, 2);
        assert_eq!(cfg_custom.cpu_overclock_pct, 200);
        assert!(!cfg_custom.pgxp_subpixel);
        assert!(!cfg_custom.integer_scaling);
        assert_eq!(cfg_custom.core_id, "zaimule-rust-gba");
    }
}
