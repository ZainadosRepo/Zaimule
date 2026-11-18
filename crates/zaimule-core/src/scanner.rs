//! Local filesystem and RomStation archive scanner for multi-GB Switch images (`.nsp`, `.xci`, `.nsz`, `.xcz`, `.nro`)
//! and compressed `.zip` / raw ROM libraries (e.g., Pokémon Platinum `.nds` in RomStation).

use crate::archive::{list_zip_entries, read_zip_entry_prefix};
use crate::catalog::{
    classify_rom_extension, compute_crc32, default_catalog, inspect_rom_file,
    inspect_rom_header_and_stats, is_known_system_or_bios_filename, ConsoleKind, CoreEngineKind,
    GameEntry, InspectedRomFile,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

const MAX_SCAN_DEPTH: usize = 8;

/// Auto-detected local folder likely holding user games (`Documents/jeuxyuzu`, `RomStation/games`, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredFolder {
    pub path: String,
    pub label: String,
    pub hint: String,
}

/// Summary report returned after scanning a folder or dropping paths into Zaimule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryScanReport {
    pub scanned_root: String,
    pub imported_entries: Vec<GameEntry>,
    pub skipped_files: usize,
    pub unsupported_archives: Vec<String>,
    pub errors: Vec<String>,
}

fn push_unique_folder(
    out: &mut Vec<DiscoveredFolder>,
    seen: &mut HashSet<String>,
    path: PathBuf,
    label: String,
    hint: String,
) {
    if !path.is_dir() {
        return;
    }
    let normalized = path
        .canonicalize()
        .unwrap_or_else(|_| path.clone())
        .to_string_lossy()
        .to_string();
    let dedup_key = normalized.to_ascii_lowercase();
    if seen.insert(dedup_key) {
        out.push(DiscoveredFolder {
            path: path.to_string_lossy().to_string(),
            label,
            hint,
        });
    }
}

fn is_keyword_game_dir(name_lower: &str) -> bool {
    name_lower.contains("yuzu")
        || name_lower.contains("switch")
        || name_lower.contains("eden")
        || name_lower.contains("sudachi")
        || name_lower.contains("citron")
        || name_lower.contains("suyu")
        || name_lower.contains("ryujinx")
        || name_lower.contains("romstation")
        || name_lower == "roms"
        || name_lower == "nds"
        || name_lower == "pokemon"
}

fn scan_doc_children(doc_root: &Path, out: &mut Vec<DiscoveredFolder>, seen: &mut HashSet<String>) {
    if !doc_root.is_dir() {
        return;
    }
    let exact = [
        (
            "jeuxyuzu",
            "Jeux Switch (Documents/jeuxyuzu)",
            "Nintendo Switch · NSP/XCI",
        ),
        (
            "Jeux Yuzu",
            "Jeux Switch (Documents/Jeux Yuzu)",
            "Nintendo Switch · NSP/XCI",
        ),
        ("yuzu", "Dossier Yuzu (Documents/yuzu)", "Nintendo Switch"),
        (
            "Switch",
            "Jeux Switch (Documents/Switch)",
            "Nintendo Switch",
        ),
        ("Nintendo Switch", "Jeux Nintendo Switch", "Nintendo Switch"),
        ("ROMs", "Dossier ROMs (Documents)", "Multi-Consoles"),
        ("Roms", "Dossier Roms (Documents)", "Multi-Consoles"),
        ("RomStation", "Dossier RomStation (Documents)", "RomStation"),
    ];
    for (sub, label, hint) in exact {
        push_unique_folder(out, seen, doc_root.join(sub), label.into(), hint.into());
    }

    if let Ok(entries) = std::fs::read_dir(doc_root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let lower = name.to_ascii_lowercase();
            if is_keyword_game_dir(&lower) {
                let hint = if lower.contains("yuzu")
                    || lower.contains("switch")
                    || lower.contains("eden")
                    || lower.contains("sudachi")
                    || lower.contains("citron")
                    || lower.contains("ryujinx")
                {
                    "Nintendo Switch · NSP/XCI"
                } else if lower.contains("romstation") {
                    "Bibliothèque RomStation"
                } else {
                    "Dossier ROMs détecté"
                };
                push_unique_folder(
                    out,
                    seen,
                    p.clone(),
                    format!("Documents/{name}"),
                    hint.into(),
                );
                // If Documents/RomStation exists, also check its games subfolders
                if lower.contains("romstation") {
                    for sub in ["games", "app/games", "roms", "app/roms"] {
                        push_unique_folder(
                            out,
                            seen,
                            p.join(sub),
                            format!("RomStation ({sub})"),
                            "RomStation · NDS / Rétro".into(),
                        );
                    }
                }
            }
        }
    }
}

/// Discovers standard user game directories given explicit Documents/Home/RomStation search roots.
pub fn discover_known_game_folders_in_roots(
    doc_roots: &[PathBuf],
    home_roots: &[PathBuf],
    romstation_roots: &[PathBuf],
) -> Vec<DiscoveredFolder> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for doc_root in doc_roots {
        scan_doc_children(doc_root, &mut out, &mut seen);
    }

    for home in home_roots {
        for sub in [
            ("jeuxyuzu", "Jeux Switch (jeuxyuzu)"),
            ("Desktop/jeuxyuzu", "Jeux Switch (Bureau/jeuxyuzu)"),
            ("Downloads", "Téléchargements"),
        ] {
            push_unique_folder(
                &mut out,
                &mut seen,
                home.join(sub.0),
                sub.1.into(),
                "Dossier utilisateur".into(),
            );
        }
        // Check Documents under home if not already included
        scan_doc_children(&home.join("Documents"), &mut out, &mut seen);
        scan_doc_children(&home.join("Mes documents"), &mut out, &mut seen);
    }

    for rs_root in romstation_roots {
        for sub in ["games", "app/games", "roms", "app/roms"] {
            push_unique_folder(
                &mut out,
                &mut seen,
                rs_root.join(sub),
                format!("RomStation · {sub}"),
                "RomStation (NDS / Rétro / Archives)".into(),
            );
        }
    }

    out
}

/// Discovers likely local game folders (`Documents/jeuxyuzu`, `RomStation/games`, etc.) across OS conventions.
pub fn discover_known_game_folders() -> Vec<DiscoveredFolder> {
    let mut home_roots = Vec::new();
    for var in ["USERPROFILE", "HOME", "OneDrive", "OneDriveConsumer"] {
        if let Ok(val) = std::env::var(var) {
            if !val.trim().is_empty() {
                home_roots.push(PathBuf::from(val));
            }
        }
    }

    let mut doc_roots = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_DOCUMENTS_DIR") {
        if !xdg.trim().is_empty() {
            doc_roots.push(PathBuf::from(xdg));
        }
    }
    for home in &home_roots {
        doc_roots.push(home.join("Documents"));
    }
    if let Ok(public) = std::env::var("PUBLIC") {
        if !public.trim().is_empty() {
            doc_roots.push(PathBuf::from(public).join("Documents"));
        }
    }

    let mut romstation_roots = vec![
        PathBuf::from("C:/RomStation"),
        PathBuf::from("D:/RomStation"),
        PathBuf::from("E:/RomStation"),
        PathBuf::from("C:/Program Files/RomStation"),
        PathBuf::from("C:/Program Files (x86)/RomStation"),
    ];
    for home in &home_roots {
        romstation_roots.push(home.join("RomStation"));
        romstation_roots.push(home.join("Documents/RomStation"));
    }
    for var in ["LOCALAPPDATA", "APPDATA"] {
        if let Ok(val) = std::env::var(var) {
            if !val.trim().is_empty() {
                romstation_roots.push(PathBuf::from(val).join("RomStation"));
            }
        }
    }

    discover_known_game_folders_in_roots(&doc_roots, &home_roots, &romstation_roots)
}

fn cover_style_for(console: ConsoleKind) -> (&'static str, &'static str) {
    match console {
        ConsoleKind::Gb | ConsoleKind::Gbc => ("#1F2A33", "GBC"),
        ConsoleKind::Gba => ("#3A271C", "GBA"),
        ConsoleKind::Nes => ("#3B1F1E", "NES"),
        ConsoleKind::Snes => ("#1F2F27", "SNES"),
        ConsoleKind::Nds => ("#1F2C38", "NDS"),
        ConsoleKind::Switch => ("#4A181D", "NSW"),
        ConsoleKind::Ps1 => ("#25232E", "PS1"),
        ConsoleKind::N64 => ("#1C2E2B", "N64"),
        ConsoleKind::Psp => ("#1E2D3B", "PSP"),
        ConsoleKind::Megadrive => ("#1D283A", "MD"),
        ConsoleKind::Chip8 => ("#262925", "CH8"),
    }
}

/// Converts an `InspectedRomFile` into a `GameEntry` ready for persistence and launch.
pub fn build_game_entry(
    rom_path: &Path,
    archive_entry: Option<&str>,
    inspected: &InspectedRomFile,
    is_homebrew_dual_phantom: bool,
) -> GameEntry {
    let id = format!("rom-{}", inspected.crc32_hex.to_ascii_lowercase());
    let (engine_kind, core_id, sram_summary) = match inspected.console {
        ConsoleKind::Switch => (
            CoreEngineKind::ExternalLauncher,
            "yuzu-external".to_string(),
            "Sauvegarde Yuzu / Eden / Ryujinx".to_string(),
        ),
        ConsoleKind::Nds if !is_homebrew_dual_phantom => (
            CoreEngineKind::LibretroAbi,
            "melonds_libretro".to_string(),
            "256 Ko EEPROM / Flash NDS".to_string(),
        ),
        other if other.is_native_rust() => (
            CoreEngineKind::NativeRust,
            other.default_core_id().to_string(),
            "32 Ko SRAM Atomique (.tmp → .srm)".to_string(),
        ),
        other => (
            CoreEngineKind::LibretroAbi,
            other.default_core_id().to_string(),
            "Mémoire Libretro (.srm)".to_string(),
        ),
    };

    // Preserve cover style consistency with default_catalog where relevant
    let _ = default_catalog;
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

/// Inspects a single ROM file or `.zip` archive without allocating full multi-GB buffers in RAM.
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
    if lower.ends_with(".7z") || lower.ends_with(".rar") {
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
                    let inspected = inspect_rom_header_and_stats(
                        entry_base,
                        &header,
                        z.uncompressed_size,
                        crc,
                        false,
                    );
                    let is_homebrew_dp = header.len() >= 16 && &header[0..12] == b"DUALPHANTOM\0";
                    report.imported_entries.push(build_game_entry(
                        path,
                        Some(&z.name),
                        &inspected,
                        is_homebrew_dp,
                    ));
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

    if classify_rom_extension(&file_name).is_none() {
        report.skipped_files += 1;
        return Ok(report);
    }

    let inspected = inspect_rom_file(path)?;
    // Peek tiny header if NDS to distinguish embedded Dual Phantom homebrew from commercial ROMs
    let is_homebrew_dp = if inspected.console == ConsoleKind::Nds {
        let mut buf = [0u8; 16];
        std::fs::File::open(path)
            .and_then(|mut f| {
                use std::io::Read;
                f.read(&mut buf)
            })
            .map(|n| n >= 12 && &buf[0..12] == b"DUALPHANTOM\0")
            .unwrap_or(false)
    } else {
        false
    };
    report
        .imported_entries
        .push(build_game_entry(path, None, &inspected, is_homebrew_dp));
    Ok(report)
}

/// Recursively scans a folder (`Documents/jeuxyuzu`, `RomStation/games`, etc.) for supported ROMs and `.zip` archives.
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
    use std::fs;

    #[test]
    fn scans_switch_jeuxyuzu_and_romstation_zip_pokemon_platinum_ignoring_keys_and_art() {
        let root = std::env::temp_dir().join(format!("zaimule-scan-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let docs = root.join("UserProfile/Documents");
        let jeuxyuzu = docs.join("jeuxyuzu");
        let rs_games = root.join("RomStation/games");
        fs::create_dir_all(&jeuxyuzu).unwrap();
        fs::create_dir_all(&rs_games).unwrap();
        fs::create_dir_all(jeuxyuzu.join("keys")).unwrap();

        // Two Nintendo Switch games inside Documents/jeuxyuzu
        let mut sw1 = vec![0u8; 4096];
        sw1[0..4].copy_from_slice(b"PFS0");
        fs::write(
            jeuxyuzu.join("Super Mario Bros. Wonder [010015100B514000][v0].nsp"),
            &sw1,
        )
        .unwrap();

        let mut sw2 = vec![0u8; 4096];
        sw2[0x0100..0x0104].copy_from_slice(b"HEAD");
        fs::write(
            jeuxyuzu.join("Pokemon Legends Arceus [01001F5010DFA000].xci"),
            &sw2,
        )
        .unwrap();

        // Switch keys & non-ROM assets should be skipped cleanly
        fs::write(jeuxyuzu.join("prod.keys"), b"master_key_00 = 00").unwrap();
        fs::write(jeuxyuzu.join("keys/prod.keys"), b"master_key_00 = 00").unwrap();
        fs::write(jeuxyuzu.join("cover.jpg"), b"\xFF\xD8\xFF").unwrap();

        // Pokémon Platinum NDS compressed inside RomStation .zip archive
        let mut nds_rom = vec![0u8; 0x400];
        nds_rom[0x0000..0x000A].copy_from_slice(b"POKEMON PL");
        nds_rom[0x000C..0x0010].copy_from_slice(b"CPUF");
        nds_rom[0x015C] = 0x56;
        nds_rom[0x015D] = 0xCF;
        let zip_bytes = build_stored_zip_for_test(&[
            ("info.txt", b"RomStation"),
            ("Pokemon - Version Platine (France).nds", &nds_rom),
        ]);
        fs::write(rs_games.join("PokemonPlatine.zip"), &zip_bytes).unwrap();
        fs::write(rs_games.join("boxart.png"), b"PNG").unwrap();
        fs::write(rs_games.join("archive.7z"), b"7z").unwrap();

        // Discovery test
        let discovered = discover_known_game_folders_in_roots(
            std::slice::from_ref(&docs),
            &[],
            &[root.join("RomStation")],
        );
        assert!(discovered.iter().any(|d| d.path.ends_with("jeuxyuzu")));
        assert!(discovered.iter().any(|d| d.path.ends_with("games")));

        // Directory scans
        let yuzu_report = scan_directory_for_roms(&jeuxyuzu);
        assert_eq!(yuzu_report.imported_entries.len(), 2);
        assert!(yuzu_report
            .imported_entries
            .iter()
            .all(|g| g.console == ConsoleKind::Switch
                && g.engine_kind == CoreEngineKind::ExternalLauncher));
        assert!(yuzu_report
            .imported_entries
            .iter()
            .any(|g| g.title == "Super Mario Bros. Wonder"
                && g.title_id.as_deref() == Some("010015100B514000")));

        let rs_report = scan_directory_for_roms(&rs_games);
        assert_eq!(rs_report.imported_entries.len(), 1);
        assert_eq!(rs_report.unsupported_archives.len(), 1);
        let plat = &rs_report.imported_entries[0];
        assert_eq!(plat.console, ConsoleKind::Nds);
        assert_eq!(plat.engine_kind, CoreEngineKind::LibretroAbi);
        assert_eq!(plat.core_id, "melonds_libretro");
        assert_eq!(plat.title_id.as_deref(), Some("CPUF"));
        assert!(plat
            .rom_path
            .as_deref()
            .unwrap()
            .ends_with("PokemonPlatine.zip"));
        assert_eq!(
            plat.archive_entry.as_deref(),
            Some("Pokemon - Version Platine (France).nds")
        );

        let _ = fs::remove_dir_all(&root);
    }
}
