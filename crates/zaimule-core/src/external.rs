//! External emulator executable discovery, Switch encryption keys detection (`prod.keys` / `title.keys`),
//! RetroArch/RomStation core discovery, and non-blocking process launching.

use crate::catalog::ConsoleKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use walkdir::WalkDir;

/// Metadata & availability of an external standalone emulator or frontend (Yuzu/Eden/Ryujinx/melonDS/RetroArch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalEmulatorStatus {
    pub id: String,
    pub name: String,
    pub supported_consoles: Vec<ConsoleKind>,
    pub installed: bool,
    pub executable_path: Option<String>,
    pub notes: String,
}

/// Detection status for Nintendo Switch decryption keys required by Yuzu/Eden/Sudachi/Ryujinx.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SwitchKeysStatus {
    pub prod_keys_found: bool,
    pub title_keys_found: bool,
    pub prod_keys_path: Option<String>,
    pub title_keys_path: Option<String>,
}

struct EmulatorSpec {
    id: &'static str,
    name: &'static str,
    consoles: &'static [ConsoleKind],
    exe_names: &'static [&'static str],
    folder_hints: &'static [&'static str],
    notes: &'static str,
}

const EMULATOR_SPECS: &[EmulatorSpec] = &[
    EmulatorSpec {
        id: "yuzu",
        name: "Yuzu",
        consoles: &[ConsoleKind::Switch],
        exe_names: &["yuzu.exe", "yuzu"],
        folder_hints: &["yuzu", "yuzu-windows-msvc", "yuzu-early-access"],
        notes: "Requis pour lancer les jeux Nintendo Switch (.nsp/.xci) en fenêtre dédiée.",
    },
    EmulatorSpec {
        id: "eden",
        name: "Eden (Successeur Yuzu)",
        consoles: &[ConsoleKind::Switch],
        exe_names: &["eden.exe", "eden"],
        folder_hints: &["eden", "Eden"],
        notes: "Fork actif compatible profils et sauvegardes Yuzu.",
    },
    EmulatorSpec {
        id: "sudachi",
        name: "Sudachi",
        consoles: &[ConsoleKind::Switch],
        exe_names: &["sudachi.exe", "sudachi"],
        folder_hints: &["sudachi", "Sudachi"],
        notes: "Fork compatible Switch récent.",
    },
    EmulatorSpec {
        id: "citron",
        name: "Citron",
        consoles: &[ConsoleKind::Switch],
        exe_names: &["citron.exe", "citron"],
        folder_hints: &["citron", "Citron"],
        notes: "Fork Yuzu haute compatibilité Switch.",
    },
    EmulatorSpec {
        id: "suyu",
        name: "Suyu",
        consoles: &[ConsoleKind::Switch],
        exe_names: &["suyu.exe", "suyu"],
        folder_hints: &["suyu", "Suyu"],
        notes: "Fork communautaire Switch compatible clés Yuzu.",
    },
    EmulatorSpec {
        id: "torzu",
        name: "Torzu",
        consoles: &[ConsoleKind::Switch],
        exe_names: &["torzu.exe", "yuzu.exe", "torzu"],
        folder_hints: &["torzu", "Torzu"],
        notes: "Fork Switch compatible format Yuzu.",
    },
    EmulatorSpec {
        id: "ryujinx",
        name: "Ryujinx",
        consoles: &[ConsoleKind::Switch],
        exe_names: &["Ryujinx.exe", "ryujinx.exe", "Ryujinx", "ryujinx"],
        folder_hints: &["Ryujinx", "ryujinx", "publish"],
        notes: "Émulateur Switch alternatif haute fidélité (.nsp/.xci/.nsz).",
    },
    EmulatorSpec {
        id: "melonds",
        name: "melonDS Standalone",
        consoles: &[ConsoleKind::Nds],
        exe_names: &["melonDS.exe", "melonds.exe", "melonDS", "melonds"],
        folder_hints: &["melonDS", "melonds"],
        notes: "Repli externe Nintendo DS si le cœur C-ABI intégré n'est pas encore téléchargé.",
    },
    EmulatorSpec {
        id: "desmume",
        name: "DeSmuME (RomStation)",
        consoles: &[ConsoleKind::Nds],
        exe_names: &[
            "DeSmuME_VS2019_x64_release.exe",
            "DeSmuME_0.9.13_x64.exe",
            "DeSmuME.exe",
            "desmume.exe",
            "desmume",
        ],
        folder_hints: &["desmume", "DeSmuME"],
        notes: "Émulateur Nintendo DS souvent préinstallé avec RomStation.",
    },
    EmulatorSpec {
        id: "retroarch",
        name: "RetroArch",
        consoles: &[
            ConsoleKind::Nds,
            ConsoleKind::Gba,
            ConsoleKind::Snes,
            ConsoleKind::Ps1,
            ConsoleKind::N64,
            ConsoleKind::Psp,
            ConsoleKind::Megadrive,
        ],
        exe_names: &["retroarch.exe", "RetroArch.exe", "retroarch"],
        folder_hints: &["RetroArch-Win64", "RetroArch", "retroarch"],
        notes:
            "Hôte Libretro complet (fournit aussi des cœurs .dll/.so réutilisables dans Zaimule).",
    },
];

fn match_file_in_dir(dir: &Path, candidates: &[&str]) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    for name in candidates {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
    }
    // Also support case-insensitive binary matching and Linux AppImages
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let fname = entry.file_name().to_string_lossy().to_string();
        let lower = fname.to_ascii_lowercase();
        for cand in candidates {
            if lower == cand.to_ascii_lowercase() {
                return Some(path);
            }
            let base = cand
                .strip_suffix(".exe")
                .unwrap_or(cand)
                .to_ascii_lowercase();
            if lower.starts_with(&base) && lower.ends_with(".appimage") {
                return Some(path);
            }
        }
    }
    None
}

fn find_emulator_in_roots(
    spec: &EmulatorSpec,
    override_path: Option<&String>,
    search_roots: &[PathBuf],
    path_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(raw) = override_path {
        let p = PathBuf::from(raw.trim());
        if p.is_file() {
            return Some(p);
        }
        if p.is_dir() {
            if let Some(hit) = match_file_in_dir(&p, spec.exe_names) {
                return Some(hit);
            }
        }
    }

    for root in search_roots {
        if !root.is_dir() {
            continue;
        }
        if let Some(hit) = match_file_in_dir(root, spec.exe_names) {
            return Some(hit);
        }
        for hint in spec.folder_hints {
            let sub = root.join(hint);
            if let Some(hit) = match_file_in_dir(&sub, spec.exe_names) {
                return Some(hit);
            }
            // Check 1 level nested (e.g. RomStation/app/emulators/desmume/...)
            if sub.is_dir() {
                if let Ok(children) = std::fs::read_dir(&sub) {
                    for child in children.flatten() {
                        if child.path().is_dir() {
                            if let Some(hit) = match_file_in_dir(&child.path(), spec.exe_names) {
                                return Some(hit);
                            }
                        }
                    }
                }
            }
        }
    }

    for pdir in path_dirs {
        if let Some(hit) = match_file_in_dir(pdir, spec.exe_names) {
            return Some(hit);
        }
    }

    None
}

/// Core testable discovery function using explicit search and `PATH` directories.
pub fn discover_external_emulators_in_roots(
    overrides: &HashMap<String, String>,
    search_roots: &[PathBuf],
    path_dirs: &[PathBuf],
) -> Vec<ExternalEmulatorStatus> {
    EMULATOR_SPECS
        .iter()
        .map(|spec| {
            let found =
                find_emulator_in_roots(spec, overrides.get(spec.id), search_roots, path_dirs);
            ExternalEmulatorStatus {
                id: spec.id.to_string(),
                name: spec.name.to_string(),
                supported_consoles: spec.consoles.to_vec(),
                installed: found.is_some(),
                executable_path: found.map(|p| p.to_string_lossy().to_string()),
                notes: spec.notes.to_string(),
            }
        })
        .collect()
}

fn collect_default_emulator_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for var in ["LOCALAPPDATA", "APPDATA", "USERPROFILE", "HOME"] {
        if let Ok(val) = std::env::var(var) {
            if !val.trim().is_empty() {
                let p = PathBuf::from(val);
                roots.push(p.clone());
                roots.push(p.join("Programs"));
                roots.push(p.join("Documents"));
                roots.push(p.join("Downloads"));
                roots.push(p.join("Desktop"));
                roots.push(p.join("Emulators"));
                roots.push(p.join("RomStation/app/emulators"));
                roots.push(p.join("RomStation/emulators"));
                roots.push(p.join(".local/bin"));
            }
        }
    }

    for base in [
        "C:/RomStation/app/emulators",
        "C:/RomStation/emulators",
        "D:/RomStation/app/emulators",
        "D:/RomStation/emulators",
        "C:/Program Files",
        "C:/Program Files (x86)",
        "C:/RetroArch-Win64",
        "C:/Emulators",
        "D:/Emulators",
        "/Applications/yuzu.app/Contents/MacOS",
        "/Applications/Ryujinx.app/Contents/MacOS",
        "/Applications/melonDS.app/Contents/MacOS",
        "/Applications/RetroArch.app/Contents/MacOS",
        "/usr/local/bin",
        "/usr/bin",
    ] {
        roots.push(PathBuf::from(base));
    }
    roots
}

/// Discovers installed Switch & NDS standalone emulators on the host machine.
pub fn discover_external_emulators(
    overrides: &HashMap<String, String>,
) -> Vec<ExternalEmulatorStatus> {
    let search_roots = collect_default_emulator_search_roots();
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    discover_external_emulators_in_roots(overrides, &search_roots, &path_dirs)
}

/// Checks Switch encryption keys (`prod.keys` & `title.keys`) inside explicit key directories.
pub fn detect_switch_keys_in_dirs(key_dirs: &[PathBuf]) -> SwitchKeysStatus {
    let mut status = SwitchKeysStatus::default();
    for dir in key_dirs {
        let prod = dir.join("prod.keys");
        if !status.prod_keys_found && prod.is_file() {
            status.prod_keys_found = true;
            status.prod_keys_path = Some(prod.to_string_lossy().to_string());
        }
        let title = dir.join("title.keys");
        if !status.title_keys_found && title.is_file() {
            status.title_keys_found = true;
            status.title_keys_path = Some(title.to_string_lossy().to_string());
        }
        if status.prod_keys_found && status.title_keys_found {
            break;
        }
    }
    status
}

/// Checks whether Yuzu/Eden/Sudachi/Citron/Suyu/Torzu/Ryujinx encryption keys (`prod.keys`) exist.
pub fn detect_switch_keys_status(emus: &[ExternalEmulatorStatus]) -> SwitchKeysStatus {
    let mut dirs = Vec::new();

    if let Ok(appdata) = std::env::var("APPDATA") {
        for app in ["yuzu", "eden", "sudachi", "citron", "suyu", "torzu"] {
            dirs.push(PathBuf::from(&appdata).join(app).join("keys"));
        }
        dirs.push(PathBuf::from(&appdata).join("Ryujinx/system"));
    }

    if let Ok(home) = std::env::var("HOME") {
        let h = PathBuf::from(home);
        for app in ["yuzu", "eden", "sudachi", "citron", "suyu", "torzu"] {
            dirs.push(h.join(".local/share").join(app).join("keys"));
            dirs.push(h.join("Library/Application Support").join(app).join("keys"));
        }
        dirs.push(h.join(".config/Ryujinx/system"));
        dirs.push(h.join("Library/Application Support/Ryujinx/system"));
    }

    // Portable `user/keys` or `portable/system` next to detected Switch emulator binaries
    for emu in emus {
        if let Some(ref exe) = emu.executable_path {
            if let Some(parent) = Path::new(exe).parent() {
                dirs.push(parent.join("user/keys"));
                dirs.push(parent.join("keys"));
                dirs.push(parent.join("portable/system"));
                dirs.push(parent.join("system"));
            }
        }
    }

    detect_switch_keys_in_dirs(&dirs)
}

/// Searches standard RetroArch / RomStation core directories for an existing Libretro binary (`melonds_libretro.dll`, etc.).
pub fn search_external_libretro_core(library_file_name: &str) -> Option<PathBuf> {
    let mut roots = Vec::new();
    for var in ["APPDATA", "LOCALAPPDATA", "USERPROFILE", "HOME"] {
        if let Ok(val) = std::env::var(var) {
            if !val.trim().is_empty() {
                let p = PathBuf::from(val);
                roots.push(p.join("RetroArch/cores"));
                roots.push(p.join(".config/retroarch/cores"));
                roots.push(p.join("RomStation/app/emulators/retroarch/cores"));
            }
        }
    }
    roots.push(PathBuf::from("C:/RetroArch-Win64/cores"));
    roots.push(PathBuf::from("C:/RomStation/app/emulators"));
    roots.push(PathBuf::from("D:/RomStation/app/emulators"));
    roots.push(PathBuf::from("/usr/lib/libretro"));
    roots.push(PathBuf::from("/usr/lib/x86_64-linux-gnu/libretro"));

    let target_lower = library_file_name.to_ascii_lowercase();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let direct = root.join(library_file_name);
        if direct.is_file() {
            return Some(direct);
        }
        for entry in WalkDir::new(&root)
            .max_depth(4)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&target_lower)
            {
                return Some(entry.into_path());
            }
        }
    }
    None
}

/// Spawns an external emulator process targeting `rom_path` without blocking the UI/Tauri command thread.
pub fn launch_external_process(
    exe_path: &Path,
    rom_path: &Path,
    emu_id: &str,
    retroarch_core_path: Option<&Path>,
) -> Result<String, String> {
    if !exe_path.is_file() {
        return Err(format!(
            "exécutable émulateur introuvable : {}",
            exe_path.display()
        ));
    }
    if !rom_path.is_file() {
        return Err(format!(
            "fichier jeu introuvable sur disque : {}",
            rom_path.display()
        ));
    }

    let mut cmd = Command::new(exe_path);
    match emu_id {
        "yuzu" | "eden" | "sudachi" | "citron" | "suyu" | "torzu" => {
            cmd.arg("-f").arg("-g").arg(rom_path);
        }
        "retroarch" => {
            if let Some(core_path) = retroarch_core_path {
                cmd.arg("-L").arg(core_path).arg(rom_path);
            } else {
                cmd.arg(rom_path);
            }
        }
        _ => {
            cmd.arg(rom_path);
        }
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    cmd.spawn().map_err(|e| {
        format!(
            "échec du lancement de {} : {e}",
            exe_path.file_name().unwrap_or_default().to_string_lossy()
        )
    })?;

    Ok(format!(
        "{} lancé avec {}",
        emu_id,
        rom_path.file_name().unwrap_or_default().to_string_lossy()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn discovers_yuzu_melonds_and_switch_prod_keys_in_temp_dirs() {
        let root = std::env::temp_dir().join(format!("zaimule-ext-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let yuzu_dir = root.join("Local/yuzu/yuzu-windows-msvc");
        let keys_dir = yuzu_dir.join("user/keys");
        let melonds_dir = root.join("RomStation/app/emulators/melonDS");
        fs::create_dir_all(&keys_dir).unwrap();
        fs::create_dir_all(&melonds_dir).unwrap();

        let yuzu_exe = yuzu_dir.join("yuzu.exe");
        let melonds_exe = melonds_dir.join("melonDS.exe");
        fs::write(&yuzu_exe, b"MZ").unwrap();
        fs::write(&melonds_exe, b"MZ").unwrap();
        fs::write(keys_dir.join("prod.keys"), b"master_key_00 = 00").unwrap();

        let emus = discover_external_emulators_in_roots(
            &HashMap::new(),
            &[
                root.join("Local/yuzu"),
                root.join("RomStation/app/emulators"),
            ],
            &[],
        );
        let yuzu = emus.iter().find(|e| e.id == "yuzu").unwrap();
        let melonds = emus.iter().find(|e| e.id == "melonds").unwrap();
        assert!(yuzu.installed);
        assert!(melonds.installed);

        let keys = detect_switch_keys_status(&emus);
        assert!(keys.prod_keys_found);
        assert!(!keys.title_keys_found);

        let _ = fs::remove_dir_all(&root);
    }
}
