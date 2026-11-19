//! Manual ROM & archive importer supporting `.gb`, `.gbc`, `.gba`, `.nds`, Switch 1 (`.nsp`,
//! `.xci`, `.nsz`, `.xcz`, `.nro`, `.nso`), Switch 2 (`.nx`, `.nup`), `.zip`, and `.7z`.
//! Starts 100% clean with zero background or startup disk scanning.

use crate::archive::{
    list_7z_entries, list_zip_entries, read_7z_entries_prefix, read_zip_entry_prefix,
};
use crate::catalog::{
    classify_rom_extension, compute_crc32, inspect_rom_file, inspect_rom_header_and_stats,
    is_known_system_or_bios_filename, ConsoleKind, CoreEngineKind, GameEntry, InspectedRomFile,
    SwitchEngineOverride,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

const MAX_SCAN_DEPTH: usize = 8;

/// Retained empty-returning discovery metadata for compatibility; Zaimule never auto-scans the disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredFolder {
    pub path: String,
    pub label: String,
    pub hint: String,
}

/// Summary report returned after importing a file or user-selected folder into Zaimule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryScanReport {
    pub scanned_root: String,
    pub imported_entries: Vec<GameEntry>,
    pub skipped_files: usize,
    pub unsupported_archives: Vec<String>,
    pub errors: Vec<String>,
}

/// Zaimule strictly never auto-scans user folders (`Documents`, `Downloads`, etc.). Always empty.
pub fn discover_known_game_folders_in_roots(
    _doc_roots: &[PathBuf],
    _home_roots: &[PathBuf],
    _romstation_roots: &[PathBuf],
) -> Vec<DiscoveredFolder> {
    Vec::new()
}

/// Zaimule strictly never auto-scans user folders (`Documents`, `Downloads`, etc.). Always empty.
pub fn discover_known_game_folders() -> Vec<DiscoveredFolder> {
    Vec::new()
}

fn cover_style_for(console: ConsoleKind) -> (&'static str, &'static str) {
    match console {
        ConsoleKind::Gb | ConsoleKind::Gbc => ("#1F2A33", "GBC"),
        ConsoleKind::Gba => ("#3A271C", "GBA"),
        ConsoleKind::Nds => ("#1F2C38", "NDS"),
        ConsoleKind::Switch => ("#4A181D", "NSW"),
        ConsoleKind::Switch2 => ("#6B1D3B", "NS2"),
    }
}

/// Converts an `InspectedRomFile` into a `GameEntry` ready for persistence and launch.
pub fn build_game_entry(
    rom_path: &Path,
    archive_entry: Option<&str>,
    inspected: &InspectedRomFile,
) -> GameEntry {
    let id = format!("rom-{}", inspected.crc32_hex.to_ascii_lowercase());
    let (engine_kind, core_id, sram_summary) = match inspected.console {
        ConsoleKind::Switch => (
            CoreEngineKind::ExternalLauncher,
            "torzu-external".to_string(),
            "Sauvegarde Switch 1 · Torzu".to_string(),
        ),
        ConsoleKind::Switch2 => (
            CoreEngineKind::ExternalLauncher,
            "fritchii-external".to_string(),
            "Sauvegarde Switch 2 · Fritchii / Lichii".to_string(),
        ),
        ConsoleKind::Nds => (
            CoreEngineKind::NativeRust,
            "zaimule-rust-nds".to_string(),
            "512 Ko Flash NDS + Écran Tactile 256×384".to_string(),
        ),
        ConsoleKind::Gb | ConsoleKind::Gbc => (
            CoreEngineKind::NativeRust,
            "zaimule-rust-gb".to_string(),
            "SRAM Cartouche (.sav) + Câble Link SIO".to_string(),
        ),
        ConsoleKind::Gba => (
            CoreEngineKind::NativeRust,
            "zaimule-rust-gba".to_string(),
            "SRAM / Flash GBA (.sav)".to_string(),
        ),
    };

    let (cover_accent, cover_symbol) = cover_style_for(inspected.console);

    GameEntry {
        id,
        title: inspected.title.clone(),
        console: inspected.console,
        engine_kind,
        core_id,
        region: inspected.region.clone(),
        crc32_hex: inspected.crc32_hex.clone(),
        size_bytes: inspected.size_bytes,
        sram_summary,
        playtime_seconds: 0,
        favorite: true,
        link_cable_ready: inspected.console.supports_link_cable(),
        embedded_rom_key: None,
        cover_accent: cover_accent.to_string(),
        cover_symbol: cover_symbol.to_string(),
        rom_path: Some(rom_path.to_string_lossy().to_string()),
        archive_entry: archive_entry.map(|s| s.to_string()),
        title_id: inspected.title_id.clone(),
        fast_fingerprint: inspected.fast_fingerprint,
        switch_engine: SwitchEngineOverride::Auto,
    }
}

fn is_skipped_subdir(entry: &DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
    matches!(
        name.as_str(),
        ".git"
            | "bios"
            | "firmware"
            | "keys"
            | "nand"
            | "sdmc"
            | "emulators"
            | "engines"
            | "cores"
            | "screenshots"
            | "logs"
            | "shader"
            | "shader_cache"
            | "saves"
            | "states"
            | "images"
            | "covers"
            | "videos"
            | "manuals"
            | "themes"
    )
}

/// Inspects a single ROM file, `.zip`, or `.7z` archive without loading large images into RAM.
pub fn inspect_path_to_entries(path: &Path) -> Result<DirectoryScanReport, String> {
    let mut report = DirectoryScanReport {
        scanned_root: path.to_string_lossy().to_string(),
        ..Default::default()
    };

    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    if file_name.is_empty() || is_known_system_or_bios_filename(&file_name) {
        report.skipped_files += 1;
        return Ok(report);
    }

    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".rar") {
        report
            .unsupported_archives
            .push(path.to_string_lossy().to_string());
        return Ok(report);
    }

    if lower.ends_with(".zip") {
        let entries = list_zip_entries(path)?;
        let mut matched_any = false;
        for z in entries {
            let entry_base = z.name.rsplit(['/', '\\']).next().unwrap_or(&z.name);
            if z.uncompressed_size == 0
                || is_known_system_or_bios_filename(entry_base)
                || classify_rom_extension(entry_base).is_none()
            {
                report.skipped_files += 1;
                continue;
            }
            matched_any = true;
            match read_zip_entry_prefix(path, &z, 64 * 1024) {
                Ok(header) => {
                    let crc = if z.crc32 != 0 {
                        z.crc32
                    } else {
                        compute_crc32(&header)
                    };
                    match inspect_rom_header_and_stats(
                        entry_base,
                        &header,
                        z.uncompressed_size,
                        crc,
                        false,
                    ) {
                        Ok(inspected) => {
                            report.imported_entries.push(build_game_entry(
                                path,
                                Some(&z.name),
                                &inspected,
                            ));
                        }
                        Err(err) => report.errors.push(format!("{}: {err}", path.display())),
                    }
                }
                Err(err) => {
                    report.errors.push(format!("{}: {err}", path.display()));
                }
            }
        }
        if !matched_any {
            report.skipped_files += 1;
        }
        return Ok(report);
    }

    if lower.ends_with(".7z") {
        let entries = list_7z_entries(path)?;
        let mut targets = Vec::new();
        for item in &entries {
            let entry_base = item.name.rsplit(['/', '\\']).next().unwrap_or(&item.name);
            if item.size == 0
                || is_known_system_or_bios_filename(entry_base)
                || classify_rom_extension(entry_base).is_none()
            {
                report.skipped_files += 1;
                continue;
            }
            targets.push(item.clone());
        }
        if targets.is_empty() {
            report.skipped_files += 1;
            return Ok(report);
        }
        let target_names: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
        match read_7z_entries_prefix(path, &target_names, 64 * 1024) {
            Ok(prefixes) => {
                for t in targets {
                    let entry_base = t.name.rsplit(['/', '\\']).next().unwrap_or(&t.name);
                    let header = prefixes.get(&t.name).map(Vec::as_slice).unwrap_or(&[]);
                    let crc = if t.crc32 != 0 {
                        t.crc32
                    } else {
                        compute_crc32(header)
                    };
                    match inspect_rom_header_and_stats(entry_base, header, t.size, crc, false) {
                        Ok(inspected) => {
                            report.imported_entries.push(build_game_entry(
                                path,
                                Some(&t.name),
                                &inspected,
                            ));
                        }
                        Err(err) => report.errors.push(format!("{}: {err}", path.display())),
                    }
                }
            }
            Err(err) => report.errors.push(format!("{}: {err}", path.display())),
        }
        return Ok(report);
    }

    if classify_rom_extension(&file_name).is_none() {
        report.skipped_files += 1;
        return Ok(report);
    }

    let inspected = inspect_rom_file(path)?;
    report
        .imported_entries
        .push(build_game_entry(path, None, &inspected));
    Ok(report)
}

/// Scans an explicitly user-chosen folder for supported ROMs, `.zip`, and `.7z` archives.
pub fn scan_directory_for_roms(root: &Path) -> DirectoryScanReport {
    let mut report = DirectoryScanReport {
        scanned_root: root.to_string_lossy().to_string(),
        ..Default::default()
    };
    if !root.exists() {
        report
            .errors
            .push(format!("dossier introuvable : {}", root.display()));
        return report;
    }

    if root.is_file() {
        match inspect_path_to_entries(root) {
            Ok(single) => return single,
            Err(err) => {
                report.errors.push(err);
                return report;
            }
        }
    }

    let walker = WalkDir::new(root)
        .follow_links(false)
        .max_depth(MAX_SCAN_DEPTH)
        .into_iter()
        .filter_entry(|e| !is_skipped_subdir(e));

    for item in walker {
        let Ok(entry) = item else {
            if let Err(err) = item {
                report.errors.push(err.to_string());
            }
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        match inspect_path_to_entries(entry.path()) {
            Ok(sub) => {
                report.imported_entries.extend(sub.imported_entries);
                report.skipped_files += sub.skipped_files;
                report.unsupported_archives.extend(sub.unsupported_archives);
                report.errors.extend(sub.errors);
            }
            Err(err) => report.errors.push(err),
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::build_stored_zip_for_test;
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
    use std::fs;

    #[test]
    fn never_auto_discovers_known_user_folders() {
        assert!(discover_known_game_folders().is_empty());
        assert!(discover_known_game_folders_in_roots(
            &[PathBuf::from("/tmp")],
            &[PathBuf::from("/tmp")],
            &[PathBuf::from("/tmp")]
        )
        .is_empty());
    }

    #[test]
    fn imports_manual_switch_switch2_zip_and_7z_roms_while_ignoring_keys_and_exes() {
        let root = std::env::temp_dir().join(format!("zaimule-scan-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let switch_dir = root.join("switch_games");
        let handheld_dir = root.join("handhelds");
        fs::create_dir_all(&switch_dir).unwrap();
        fs::create_dir_all(&handheld_dir).unwrap();
        fs::create_dir_all(switch_dir.join("keys")).unwrap();

        let mut sw1 = vec![0u8; 4096];
        sw1[0..4].copy_from_slice(b"PFS0");
        fs::write(
            switch_dir.join("Super Mario Bros. Wonder [010015100B514000][v0].nsp"),
            &sw1,
        )
        .unwrap();

        let mut sw2 = vec![0u8; 4096];
        sw2[0..4].copy_from_slice(b"PFS0");
        fs::write(
            switch_dir.join("Fritchii Showcase [0100FF0011220000].nx"),
            &sw2,
        )
        .unwrap();

        fs::write(switch_dir.join("prod.keys"), b"master_key_00 = 00").unwrap();
        fs::write(switch_dir.join("torzu.exe"), b"MZ").unwrap();
        fs::write(switch_dir.join("cover.jpg"), b"\xFF\xD8\xFF").unwrap();

        let mut nds_rom = vec![0u8; 0x400];
        nds_rom[0x0000..0x000A].copy_from_slice(b"POKEMON PL");
        nds_rom[0x000C..0x0010].copy_from_slice(b"CPUF");
        nds_rom[0x015C] = 0x56;
        nds_rom[0x015D] = 0xCF;
        let zip_bytes = build_stored_zip_for_test(&[
            ("info.txt", b"Info"),
            ("Pokemon - Version Platine (France).nds", &nds_rom),
        ]);
        fs::write(handheld_dir.join("PokemonPlatine.zip"), &zip_bytes).unwrap();

        let mut gba_rom = vec![0u8; 0x200];
        gba_rom[0x04..0x08].copy_from_slice(&[0x24, 0xFF, 0xAE, 0x51]);
        gba_rom[0xA0..0xA7].copy_from_slice(b"MINISHC");
        gba_rom[0xB2] = 0x96;
        let seven_z_path = handheld_dir.join("Minish.7z");
        {
            let mut sz = ArchiveWriter::create(&seven_z_path).unwrap();
            sz.push_archive_entry(
                ArchiveEntry::new_file("Zelda - The Minish Cap.gba"),
                Some(gba_rom.as_slice()),
            )
            .unwrap();
            sz.finish().unwrap();
        }
        fs::write(handheld_dir.join("unsupported.rar"), b"Rar!").unwrap();

        let sw_report = scan_directory_for_roms(&switch_dir);
        assert_eq!(sw_report.imported_entries.len(), 2);
        assert!(sw_report.imported_entries.iter().any(|g| {
            g.console == ConsoleKind::Switch
                && g.engine_kind == CoreEngineKind::ExternalLauncher
                && g.core_id == "torzu-external"
        }));
        assert!(sw_report.imported_entries.iter().any(|g| {
            g.console == ConsoleKind::Switch2
                && g.engine_kind == CoreEngineKind::ExternalLauncher
                && g.core_id == "fritchii-external"
        }));

        let hh_report = scan_directory_for_roms(&handheld_dir);
        assert_eq!(hh_report.imported_entries.len(), 2);
        assert_eq!(hh_report.unsupported_archives.len(), 1);
        let plat = hh_report
            .imported_entries
            .iter()
            .find(|g| g.console == ConsoleKind::Nds)
            .unwrap();
        assert_eq!(plat.engine_kind, CoreEngineKind::NativeRust);
        assert_eq!(plat.core_id, "zaimule-rust-nds");
        assert_eq!(plat.title_id.as_deref(), Some("CPUF"));
        let gba = hh_report
            .imported_entries
            .iter()
            .find(|g| g.console == ConsoleKind::Gba)
            .unwrap();
        assert_eq!(gba.engine_kind, CoreEngineKind::NativeRust);
        assert_eq!(gba.core_id, "zaimule-rust-gba");
        assert_eq!(
            gba.archive_entry.as_deref(),
            Some("Zelda - The Minish Cap.gba")
        );

        let _ = fs::remove_dir_all(&root);
    }
}
