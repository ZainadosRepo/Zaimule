//! Universal Libretro C-ABI host bridge, pixel-format framebuffer converters,
//! and dynamic core descriptor registry.

use serde::{Deserialize, Serialize};
use zaimule_core::catalog::ConsoleKind;

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
    pub pixel_format: RetroPixelFormat,
    pub supports_savestates: bool,
    pub supports_sram: bool,
    pub status_label: String,
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

/// Registry of all official Libretro C-ABI cores orchestrated by Zaimule.
pub fn default_libretro_cores() -> Vec<LibretroCoreDescriptor> {
    vec![
        LibretroCoreDescriptor {
            core_id: "mgba_libretro".into(),
            display_name: "mGBA (Game Boy Advance)".into(),
            console: ConsoleKind::Gba,
            library_windows: "mgba_libretro.dll".into(),
            library_macos: "mgba_libretro.dylib".into(),
            library_linux: "mgba_libretro.so".into(),
            pixel_format: RetroPixelFormat::Rgb565,
            supports_savestates: true,
            supports_sram: true,
            status_label: "Pont C-ABI Prêt · Fallback Rust Actif".into(),
        },
        LibretroCoreDescriptor {
            core_id: "snes9x_libretro".into(),
            display_name: "Snes9x Current (Super Nintendo)".into(),
            console: ConsoleKind::Snes,
            library_windows: "snes9x_libretro.dll".into(),
            library_macos: "snes9x_libretro.dylib".into(),
            library_linux: "snes9x_libretro.so".into(),
            pixel_format: RetroPixelFormat::Rgb565,
            supports_savestates: true,
            supports_sram: true,
            status_label: "Pont C-ABI Prêt · Fallback Rust Actif".into(),
        },
        LibretroCoreDescriptor {
            core_id: "melonds_libretro".into(),
            display_name: "melonDS (Nintendo DS + Écran Tactile)".into(),
            console: ConsoleKind::Nds,
            library_windows: "melonds_libretro.dll".into(),
            library_macos: "melonds_libretro.dylib".into(),
            library_linux: "melonds_libretro.so".into(),
            pixel_format: RetroPixelFormat::Xrgb8888,
            supports_savestates: true,
            supports_sram: true,
            status_label: "Pont C-ABI Prêt · Keymapper Tactile Actif".into(),
        },
        LibretroCoreDescriptor {
            core_id: "swanstation_libretro".into(),
            display_name: "SwanStation (PlayStation 1 PGXP)".into(),
            console: ConsoleKind::Ps1,
            library_windows: "swanstation_libretro.dll".into(),
            library_macos: "swanstation_libretro.dylib".into(),
            library_linux: "swanstation_libretro.so".into(),
            pixel_format: RetroPixelFormat::Xrgb8888,
            supports_savestates: true,
            supports_sram: true,
            status_label: "Pont C-ABI Prêt · Upscaling 4x".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_rgb565_white_and_black_pixels_accurately() {
        let rgba = convert_rgb565_to_rgba8888(&[0xFFFF, 0x0000]);
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]);
        assert_eq!(&rgba[4..8], &[0, 0, 0, 255]);
    }

    #[test]
    fn identifies_pe_elf_and_macho_headers() {
        assert_eq!(
            detect_dynamic_library_abi(b"MZ\x90\x00").unwrap(),
            "Windows PE64 (.dll)"
        );
        assert_eq!(
            detect_dynamic_library_abi(b"\x7FELF").unwrap(),
            "Linux ELF64 (.so)"
        );
    }
}
