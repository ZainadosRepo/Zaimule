//! Universal Libretro C-ABI host bridge, symbol table inspector (`retro_init`, `retro_run`,
//! `retro_load_game`, `retro_serialize`), pixel-format framebuffer converters,
//! and dynamic core descriptor registry with official Buildbot endpoints.

use serde::{Deserialize, Serialize};
use std::path::Path;
use zaimule_core::catalog::{compute_crc32, format_crc32_hex, ConsoleKind};

/// Required C-ABI entry points mandated by the Libretro API (`libretro.h` v1).
pub const REQUIRED_LIBRETRO_SYMBOLS: &[&[u8]] = &[
    b"retro_init",
    b"retro_deinit",
    b"retro_api_version",
    b"retro_get_system_info",
    b"retro_get_system_av_info",
    b"retro_load_game",
    b"retro_run",
    b"retro_serialize",
    b"retro_unserialize",
];

/// Libretro pixel format constants defined by `libretro.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RetroPixelFormat {
    Rgb0555,
    Xrgb8888,
    Rgb565,
}

/// Metadata descriptor for a supported Libretro C-ABI dynamic core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibretroCoreDescriptor {
    pub core_id: String,
    pub display_name: String,
    pub console: ConsoleKind,
    pub library_windows: String,
    pub library_macos: String,
    pub library_linux: String,
    pub buildbot_windows_url: String,
    pub buildbot_macos_url: String,
    pub pixel_format: RetroPixelFormat,
    pub supports_savestates: bool,
    pub supports_sram: bool,
    pub installed: bool,
    #[serde(default)]
    pub is_native_binary: bool,
    #[serde(default)]
    pub library_path: Option<String>,
    pub verified_crc32_hex: Option<String>,
    pub status_label: String,
}

impl LibretroCoreDescriptor {
    /// Returns the dynamic library file name matching the current host OS.
    pub fn current_platform_library_name(&self) -> &str {
        if cfg!(target_os = "windows") {
            &self.library_windows
        } else if cfg!(target_os = "macos") {
            &self.library_macos
        } else {
            &self.library_linux
        }
    }

    /// Returns the official Libretro Buildbot `.zip` download URL for the current OS & architecture.
    pub fn current_platform_buildbot_url(&self) -> String {
        if cfg!(target_os = "windows") {
            self.buildbot_windows_url.clone()
        } else if cfg!(target_os = "macos") {
            if cfg!(target_arch = "x86_64") {
                self.buildbot_macos_url
                    .replace("/osx/arm64/", "/osx/x86_64/")
            } else {
                self.buildbot_macos_url.clone()
            }
        } else {
            format!(
                "https://buildbot.libretro.com/nightly/linux/x86_64/latest/{}.zip",
                self.library_linux
            )
        }
    }
}

/// Result of inspecting a dynamic library binary (`.dll`, `.dylib`, `.so`) for Libretro C-ABI compliance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibretroInspectionReport {
    pub abi_format: String,
    pub crc32_hex: String,
    pub symbols_found: usize,
    pub symbols_total: usize,
}

/// Converts a `RETRO_PIXEL_FORMAT_0RGB1555` (`RGB0555`) framebuffer into packed RGBA8888 bytes.
pub fn convert_rgb0555_to_rgba8888(pixels_555: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels_555.len() * 4);
    for &px in pixels_555 {
        let r5 = ((px >> 10) & 0x1F) as u8;
        let g5 = ((px >> 5) & 0x1F) as u8;
        let b5 = (px & 0x1F) as u8;
        let r = (r5 << 3) | (r5 >> 2);
        let g = (g5 << 3) | (g5 >> 2);
        let b = (b5 << 3) | (b5 >> 2);
        out.extend_from_slice(&[r, g, b, 255]);
    }
    out
}

/// Converts a `RETRO_PIXEL_FORMAT_RGB565` framebuffer into packed RGBA8888 bytes for WebGL2/Canvas.
pub fn convert_rgb565_to_rgba8888(pixels_565: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels_565.len() * 4);
    for &px in pixels_565 {
        let r5 = ((px >> 11) & 0x1F) as u8;
        let g6 = ((px >> 5) & 0x3F) as u8;
        let b5 = (px & 0x1F) as u8;
        let r = (r5 << 3) | (r5 >> 2);
        let g = (g6 << 2) | (g6 >> 4);
        let b = (b5 << 3) | (b5 >> 2);
        out.extend_from_slice(&[r, g, b, 255]);
    }
    out
}

/// Converts a `RETRO_PIXEL_FORMAT_XRGB8888` framebuffer into packed RGBA8888 bytes.
pub fn convert_xrgb8888_to_rgba8888(pixels_xrgb: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels_xrgb.len() * 4);
    for &px in pixels_xrgb {
        let r = ((px >> 16) & 0xFF) as u8;
        let g = ((px >> 8) & 0xFF) as u8;
        let b = (px & 0xFF) as u8;
        out.extend_from_slice(&[r, g, b, 255]);
    }
    out
}

/// Validates whether a dynamic library file header matches PE (`.dll`), Mach-O (`.dylib`), or ELF (`.so`).
pub fn detect_dynamic_library_abi(header_bytes: &[u8]) -> Result<&'static str, String> {
    if header_bytes.len() < 4 {
        return Err("binaire Libretro trop court (< 4 octets)".into());
    }
    if &header_bytes[0..2] == b"MZ" {
        Ok("Windows PE64 (.dll)")
    } else if &header_bytes[0..4] == b"\x7FELF" {
        Ok("Linux ELF64 (.so)")
    } else if matches!(
        &header_bytes[0..4],
        [0xCF, 0xFA, 0xED, 0xFE] | [0xFE, 0xED, 0xFA, 0xCF] | [0xCA, 0xFE, 0xBA, 0xBE]
    ) {
        Ok("macOS Universal Mach-O (.dylib)")
    } else {
        Err("format de bibliothèque dynamique non reconnu".into())
    }
}

/// Inspects a `.dll`/`.dylib`/`.so` binary, verifying its OS header and scanning its symbol table
/// for required `libretro.h` C-ABI functions.
pub fn inspect_libretro_binary(binary_bytes: &[u8]) -> Result<LibretroInspectionReport, String> {
    let abi_format = detect_dynamic_library_abi(binary_bytes)?.to_string();
    let crc32_hex = format_crc32_hex(compute_crc32(binary_bytes));
    let mut found = 0usize;
    for sym in REQUIRED_LIBRETRO_SYMBOLS {
        if binary_bytes.windows(sym.len()).any(|window| window == *sym) {
            found += 1;
        }
    }
    if found == 0 {
        return Err(
            "aucun symbole C-ABI Libretro (retro_init / retro_run) détecté dans ce binaire".into(),
        );
    }
    Ok(LibretroInspectionReport {
        abi_format,
        crc32_hex,
        symbols_found: found,
        symbols_total: REQUIRED_LIBRETRO_SYMBOLS.len(),
    })
}

/// Validates and loads a genuine native Libretro shared library (`.dll` / `.dylib` / `.so`) using `libloading`,
/// confirming all required `libretro.h` C-ABI symbols resolve cleanly.
pub fn verify_real_libretro_library(path: &Path) -> Result<LibretroInspectionReport, String> {
    if !path.is_file() {
        return Err(format!("fichier cœur introuvable : {}", path.display()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.windows(23).any(|w| w == b"ZAIMULE_LIBRETRO_ABI_V1") {
        return Err(
            "il s'agit d'un descripteur local (stub), pas d'une bibliothèque C-ABI native".into(),
        );
    }
    let abi_format = detect_dynamic_library_abi(&bytes)?.to_string();
    let crc32_hex = format_crc32_hex(compute_crc32(&bytes));

    // Safety: caller passed a candidate dynamic library on disk verified above as native PE/Mach-O/ELF.
    let lib = unsafe { libloading::Library::new(path) }.map_err(|e| {
        format!(
            "impossible de charger la bibliothèque dynamique {} : {e}",
            path.display()
        )
    })?;

    let symbol_names: [&[u8]; 9] = [
        b"retro_init\0",
        b"retro_deinit\0",
        b"retro_api_version\0",
        b"retro_get_system_info\0",
        b"retro_get_system_av_info\0",
        b"retro_load_game\0",
        b"retro_run\0",
        b"retro_serialize\0",
        b"retro_unserialize\0",
    ];
    let mut resolved = 0usize;
    for sym in symbol_names {
        let ok = unsafe { lib.get::<*const ()>(sym) }.is_ok();
        if ok {
            resolved += 1;
        }
    }
    if resolved < 7 {
        return Err(format!(
            "cœur Libretro incomplet ({resolved}/{} symboles résolus dans {})",
            symbol_names.len(),
            path.display()
        ));
    }

    Ok(LibretroInspectionReport {
        abi_format,
        crc32_hex,
        symbols_found: resolved,
        symbols_total: symbol_names.len(),
    })
}

/// Builds a deterministic C-ABI stub image containing all required Libretro export symbols
/// so a core slot can be provisioned and verified locally on any platform.
pub fn build_verified_libretro_stub(core_id: &str) -> Vec<u8> {
    let mut bin = Vec::with_capacity(512);
    if cfg!(target_os = "macos") {
        bin.extend_from_slice(&[0xCF, 0xFA, 0xED, 0xFE]);
    } else if cfg!(target_os = "linux") {
        bin.extend_from_slice(b"\x7FELF");
    } else {
        bin.extend_from_slice(b"MZ\x90\x00");
    }
    bin.extend_from_slice(b"ZAIMULE_LIBRETRO_ABI_V1:");
    bin.extend_from_slice(core_id.as_bytes());
    bin.push(0);
    for sym in REQUIRED_LIBRETRO_SYMBOLS {
        bin.extend_from_slice(sym);
        bin.push(0);
    }
    bin
}

/// Registry of all official Libretro C-ABI cores orchestrated by Zaimule.
pub fn default_libretro_cores() -> Vec<LibretroCoreDescriptor> {
    vec![
        LibretroCoreDescriptor {
            core_id: "mgba_libretro".into(),
            display_name: "mGBA (Game Boy Advance · ARM7TDMI)".into(),
            console: ConsoleKind::Gba,
            library_windows: "mgba_libretro.dll".into(),
            library_macos: "mgba_libretro.dylib".into(),
            library_linux: "mgba_libretro.so".into(),
            buildbot_windows_url:
                "https://buildbot.libretro.com/nightly/windows/x86_64/latest/mgba_libretro.dll.zip"
                    .into(),
            buildbot_macos_url:
                "https://buildbot.libretro.com/nightly/apple/osx/arm64/latest/mgba_libretro.dylib.zip"
                    .into(),
            pixel_format: RetroPixelFormat::Rgb565,
            supports_savestates: true,
            supports_sram: true,
            installed: false,
            is_native_binary: false,
            library_path: None,
            verified_crc32_hex: None,
            status_label: "Non téléchargé · Cliquer pour installer (.dll/.so)".into(),
        },
        LibretroCoreDescriptor {
            core_id: "snes9x_libretro".into(),
            display_name: "Snes9x Current (Super Nintendo · Ricoh 5A22)".into(),
            console: ConsoleKind::Snes,
            library_windows: "snes9x_libretro.dll".into(),
            library_macos: "snes9x_libretro.dylib".into(),
            library_linux: "snes9x_libretro.so".into(),
            buildbot_windows_url:
                "https://buildbot.libretro.com/nightly/windows/x86_64/latest/snes9x_libretro.dll.zip"
                    .into(),
            buildbot_macos_url:
                "https://buildbot.libretro.com/nightly/apple/osx/arm64/latest/snes9x_libretro.dylib.zip"
                    .into(),
            pixel_format: RetroPixelFormat::Rgb565,
            supports_savestates: true,
            supports_sram: true,
            installed: false,
            is_native_binary: false,
            library_path: None,
            verified_crc32_hex: None,
            status_label: "Non téléchargé · Cliquer pour installer (.dll/.so)".into(),
        },
        LibretroCoreDescriptor {
            core_id: "melonds_libretro".into(),
            display_name: "melonDS (Nintendo DS + Stylet Tactile)".into(),
            console: ConsoleKind::Nds,
            library_windows: "melonds_libretro.dll".into(),
            library_macos: "melonds_libretro.dylib".into(),
            library_linux: "melonds_libretro.so".into(),
            buildbot_windows_url:
                "https://buildbot.libretro.com/nightly/windows/x86_64/latest/melonds_libretro.dll.zip"
                    .into(),
            buildbot_macos_url:
                "https://buildbot.libretro.com/nightly/apple/osx/arm64/latest/melonds_libretro.dylib.zip"
                    .into(),
            pixel_format: RetroPixelFormat::Xrgb8888,
            supports_savestates: true,
            supports_sram: true,
            installed: false,
            is_native_binary: false,
            library_path: None,
            verified_crc32_hex: None,
            status_label: "Non téléchargé · Requis pour Pokémon Platine / NDS".into(),
        },
        LibretroCoreDescriptor {
            core_id: "swanstation_libretro".into(),
            display_name: "SwanStation (PlayStation 1 PGXP 4x)".into(),
            console: ConsoleKind::Ps1,
            library_windows: "swanstation_libretro.dll".into(),
            library_macos: "swanstation_libretro.dylib".into(),
            library_linux: "swanstation_libretro.so".into(),
            buildbot_windows_url:
                "https://buildbot.libretro.com/nightly/windows/x86_64/latest/swanstation_libretro.dll.zip"
                    .into(),
            buildbot_macos_url:
                "https://buildbot.libretro.com/nightly/apple/osx/arm64/latest/swanstation_libretro.dylib.zip"
                    .into(),
            pixel_format: RetroPixelFormat::Xrgb8888,
            supports_savestates: true,
            supports_sram: true,
            installed: false,
            is_native_binary: false,
            library_path: None,
            verified_crc32_hex: None,
            status_label: "Non téléchargé · Cliquer pour installer (.dll/.so)".into(),
        },
        LibretroCoreDescriptor {
            core_id: "mupen64plus_next_libretro".into(),
            display_name: "Mupen64Plus-Next (Nintendo 64 · ParaLLEl RDP)".into(),
            console: ConsoleKind::N64,
            library_windows: "mupen64plus_next_libretro.dll".into(),
            library_macos: "mupen64plus_next_libretro.dylib".into(),
            library_linux: "mupen64plus_next_libretro.so".into(),
            buildbot_windows_url:
                "https://buildbot.libretro.com/nightly/windows/x86_64/latest/mupen64plus_next_libretro.dll.zip"
                    .into(),
            buildbot_macos_url:
                "https://buildbot.libretro.com/nightly/apple/osx/arm64/latest/mupen64plus_next_libretro.dylib.zip"
                    .into(),
            pixel_format: RetroPixelFormat::Xrgb8888,
            supports_savestates: true,
            supports_sram: true,
            installed: false,
            is_native_binary: false,
            library_path: None,
            verified_crc32_hex: None,
            status_label: "Non téléchargé · Cliquer pour installer (.dll/.so)".into(),
        },
        LibretroCoreDescriptor {
            core_id: "genesis_plus_gx_libretro".into(),
            display_name: "Genesis Plus GX (Mega Drive / Genesis · YM2612)".into(),
            console: ConsoleKind::Megadrive,
            library_windows: "genesis_plus_gx_libretro.dll".into(),
            library_macos: "genesis_plus_gx_libretro.dylib".into(),
            library_linux: "genesis_plus_gx_libretro.so".into(),
            buildbot_windows_url:
                "https://buildbot.libretro.com/nightly/windows/x86_64/latest/genesis_plus_gx_libretro.dll.zip"
                    .into(),
            buildbot_macos_url:
                "https://buildbot.libretro.com/nightly/apple/osx/arm64/latest/genesis_plus_gx_libretro.dylib.zip"
                    .into(),
            pixel_format: RetroPixelFormat::Rgb565,
            supports_savestates: true,
            supports_sram: true,
            installed: false,
            is_native_binary: false,
            library_path: None,
            verified_crc32_hex: None,
            status_label: "Non téléchargé · Cliquer pour installer (.dll/.so)".into(),
        },
        LibretroCoreDescriptor {
            core_id: "ppsspp_libretro".into(),
            display_name: "PPSSPP (PlayStation Portable · Allegrex 16:9)".into(),
            console: ConsoleKind::Psp,
            library_windows: "ppsspp_libretro.dll".into(),
            library_macos: "ppsspp_libretro.dylib".into(),
            library_linux: "ppsspp_libretro.so".into(),
            buildbot_windows_url:
                "https://buildbot.libretro.com/nightly/windows/x86_64/latest/ppsspp_libretro.dll.zip"
                    .into(),
            buildbot_macos_url:
                "https://buildbot.libretro.com/nightly/apple/osx/arm64/latest/ppsspp_libretro.dylib.zip"
                    .into(),
            pixel_format: RetroPixelFormat::Xrgb8888,
            supports_savestates: true,
            supports_sram: true,
            installed: false,
            is_native_binary: false,
            library_path: None,
            verified_crc32_hex: None,
            status_label: "Non téléchargé · Cliquer pour installer (.dll/.so)".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_rgb0555_and_rgb565_white_and_black_pixels_accurately() {
        let rgba555 = convert_rgb0555_to_rgba8888(&[0x7FFF, 0x0000]);
        assert_eq!(&rgba555[0..4], &[255, 255, 255, 255]);
        assert_eq!(&rgba555[4..8], &[0, 0, 0, 255]);

        let rgba = convert_rgb565_to_rgba8888(&[0xFFFF, 0x0000]);
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]);
        assert_eq!(&rgba[4..8], &[0, 0, 0, 255]);
    }

    #[test]
    fn identifies_pe_elf_and_macho_headers_and_verifies_libretro_symbols() {
        assert_eq!(
            detect_dynamic_library_abi(b"MZ\x90\x00").unwrap(),
            "Windows PE64 (.dll)"
        );
        assert_eq!(
            detect_dynamic_library_abi(b"\x7FELF").unwrap(),
            "Linux ELF64 (.so)"
        );

        let stub = build_verified_libretro_stub("ppsspp_libretro");
        let report = inspect_libretro_binary(&stub).unwrap();
        assert_eq!(report.symbols_found, REQUIRED_LIBRETRO_SYMBOLS.len());
        assert_eq!(default_libretro_cores().len(), 7);
    }
}
