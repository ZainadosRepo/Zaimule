//! Option-A Zaimule Switch Vault manager:
//! - Isolated user-imported Switch 1 (`Torzu`) and Switch 2 (`Fritchii / Lichii`) installations
//! - Manual user import for `prod.keys`, `title.keys`, and Switch system firmware (`.nca` files)
//! - Portable key & firmware synchronization into resolved engine `user/` (or Linux AppImage XDG) directories before process launch
//! - Shader cache management and child-process spawning with `-f -g <rom>`

use crate::archive::extract_archive_to_dir;
use crate::catalog::{ConsoleKind, SwitchEngineOverride};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use walkdir::WalkDir;

const MAX_KEY_FILE_BYTES: u64 = 16 * 1024 * 1024;
const FIRMWARE_VERSION_HINT_FILE: &str = ".version_hint";
const FIRMWARE_STAMP_FILE: &str = ".zaimule-fw-stamp";

/// Target external Switch engine managed inside Zaimule's isolated Switch Vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SwitchEngineTarget {
    #[default]
    Torzu,
    Fritchii,
}

impl SwitchEngineTarget {
    pub fn id(self) -> &'static str {
        match self {
            Self::Torzu => "torzu",
            Self::Fritchii => "fritchii",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Torzu => "Torzu (Switch 1)",
            Self::Fritchii => "Fritchii / Lichii (Switch 2)",
        }
    }

    pub fn default_notes(self) -> &'static str {
        match self {
            Self::Torzu => {
                "Importez votre dossier, exécutable ou archive .zip/.7z Torzu (Switch 1)."
            }
            Self::Fritchii => {
                "Importez votre build, exécutable ou archive .zip/.7z Fritchii / Lichii (Switch 2)."
            }
        }
    }
}

/// Installation state for an engine inside `<vault_root>/engines/<target>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SwitchEngineInstall {
    pub target: SwitchEngineTarget,
    pub installed: bool,
    pub engine_dir: String,
    pub executable_path: Option<String>,
    pub version_hint: Option<String>,
    pub shader_cache_bytes: u64,
    pub notes: String,
}

/// Detection status for user-imported Switch encryption keys (`prod.keys` & `title.keys`) inside the vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SwitchKeysStatus {
    pub keys_dir: String,
    pub prod_keys_found: bool,
    pub title_keys_found: bool,
    pub prod_keys_path: Option<String>,
    pub title_keys_path: Option<String>,
}

/// Detection status for user-imported Switch system firmware (`.nca` archives) inside the vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SwitchFirmwareStatus {
    pub firmware_dir: String,
    pub installed: bool,
    pub nca_count: usize,
    pub total_bytes: u64,
    pub version_hint: Option<String>,
}

/// Complete Option-A Switch Vault state serialized to the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SwitchVaultStatus {
    pub vault_root: String,
    pub torzu: SwitchEngineInstall,
    pub fritchii: SwitchEngineInstall,
    pub keys: SwitchKeysStatus,
    #[serde(default)]
    pub firmware: SwitchFirmwareStatus,
}

/// Resolved executable, target engine, and ROM path ready for spawning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchLaunchPlan {
    pub target: SwitchEngineTarget,
    pub executable_path: PathBuf,
    pub engine_dir: PathBuf,
    pub rom_path: PathBuf,
}

/// Resolves whether a Switch game should launch with `Torzu` or `Fritchii`.
pub fn resolve_switch_engine_for_game(
    console: ConsoleKind,
    override_choice: SwitchEngineOverride,
) -> Result<SwitchEngineTarget, String> {
    match override_choice {
        SwitchEngineOverride::Torzu => Ok(SwitchEngineTarget::Torzu),
        SwitchEngineOverride::Fritchii => Ok(SwitchEngineTarget::Fritchii),
        SwitchEngineOverride::Auto => match console {
            ConsoleKind::Switch => Ok(SwitchEngineTarget::Torzu),
            ConsoleKind::Switch2 => Ok(SwitchEngineTarget::Fritchii),
            _ => Err("ce jeu n'est pas un titre Nintendo Switch / Switch 2".into()),
        },
    }
}

fn candidate_binary_names(target: SwitchEngineTarget) -> &'static [&'static str] {
    match target {
        SwitchEngineTarget::Torzu => &[
            "torzu.exe",
            "torzu",
            "yuzu.exe",
            "yuzu",
            "suyu.exe",
            "suyu",
            "eden.exe",
            "eden",
            "citron.exe",
            "citron",
            "sudachi.exe",
            "sudachi",
            "ryujinx.exe",
            "ryujinx",
        ],
        SwitchEngineTarget::Fritchii => &[
            "fritchii.exe",
            "fritchii",
            "lichii.exe",
            "lichii",
            "torzu.exe",
            "torzu",
        ],
    }
}

fn candidate_binary_stems(target: SwitchEngineTarget) -> &'static [&'static str] {
    match target {
        SwitchEngineTarget::Torzu => &[
            "torzu", "yuzu", "suyu", "eden", "citron", "sudachi", "ryujinx",
        ],
        SwitchEngineTarget::Fritchii => &["fritchii", "lichii", "torzu"],
    }
}

fn has_non_executable_extension(lower: &str) -> bool {
    const IGNORED_EXTS: &[&str] = &[
        ".dll",
        ".so",
        ".dylib",
        ".txt",
        ".md",
        ".json",
        ".ini",
        ".conf",
        ".cfg",
        ".log",
        ".pdb",
        ".zip",
        ".7z",
        ".tar",
        ".gz",
        ".xz",
        ".bz2",
        ".png",
        ".jpg",
        ".jpeg",
        ".svg",
        ".ico",
        ".icns",
        ".desktop",
        ".plist",
        ".qml",
        ".qm",
        ".keys",
        ".nca",
        ".nsp",
        ".xci",
        ".nsz",
        ".xcz",
        ".manifest",
        ".xml",
        ".html",
        ".css",
        ".js",
        ".lib",
        ".a",
        ".o",
        ".debug",
    ];
    IGNORED_EXTS.iter().any(|ext| lower.ends_with(ext)) || lower.contains(".so.")
}

fn is_candidate_executable_name(file_name: &str, target: SwitchEngineTarget) -> bool {
    let lower = file_name.to_ascii_lowercase();
    if lower.is_empty() || has_non_executable_extension(&lower) {
        return false;
    }
    for cand in candidate_binary_names(target) {
        if lower == cand.to_ascii_lowercase() {
            return true;
        }
    }
    for stem in candidate_binary_stems(target) {
        if let Some(rest) = lower.strip_prefix(stem) {
            let boundary_ok = rest.is_empty()
                || rest.starts_with('-')
                || rest.starts_with('_')
                || rest.starts_with('.')
                || rest.starts_with(' ');
            if !boundary_ok {
                continue;
            }
            if lower.ends_with(".exe")
                || lower.ends_with(".appimage")
                || !lower.contains('.')
                || lower
                    .rsplit_once('.')
                    .map(|(_, ext)| !ext.is_empty() && ext.chars().all(|c| c.is_ascii_digit()))
                    .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

fn os_executable_penalty(lower_file_name: &str) -> usize {
    #[cfg(windows)]
    {
        if lower_file_name.ends_with(".exe") {
            0
        } else if lower_file_name.ends_with(".appimage") {
            20_000
        } else {
            10_000
        }
    }
    #[cfg(not(windows))]
    {
        if lower_file_name.ends_with(".exe") {
            10_000
        } else {
            0
        }
    }
}

fn find_engine_executable(dir: &Path, target: SwitchEngineTarget) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let cands = candidate_binary_names(target);
    let stems = candidate_binary_stems(target);
    // Prioritize native OS binaries first, then shallower depth, then preferred binary name.
    let mut found: Vec<(usize, PathBuf)> = Vec::new();
    for entry in WalkDir::new(dir)
        .follow_links(false)
        .max_depth(6)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let fname = entry.file_name().to_string_lossy();
        if is_candidate_executable_name(&fname, target) {
            let lower = fname.to_ascii_lowercase();
            let priority = cands
                .iter()
                .position(|c| lower == c.to_ascii_lowercase())
                .or_else(|| {
                    stems
                        .iter()
                        .position(|s| lower.starts_with(s))
                        .map(|i| cands.len() + i)
                })
                .unwrap_or(cands.len() + stems.len());
            let score = os_executable_penalty(&lower) + entry.depth() * 100 + priority;
            found.push((score, entry.into_path()));
        }
    }
    found.sort_by_key(|(score, _)| *score);
    found.into_iter().next().map(|(_, p)| p)
}

fn is_macos_bundle_macos_dir(dir: &Path) -> bool {
    dir.file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case("MacOS"))
        .unwrap_or(false)
        && dir
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().eq_ignore_ascii_case("Contents"))
            .unwrap_or(false)
}

fn is_appimage_path(path: &Path) -> bool {
    path.file_name()
        .map(|n| {
            n.to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".appimage")
        })
        .unwrap_or(false)
}

/// Resolves all portable/XDG user data roots where an engine reads `keys/`, `nand/`, and `shader/`.
///
/// For portable archives extracted into a nested folder (e.g. `<engine_dir>/Torzu/torzu.exe`),
/// the spawned emulator runs with `current_dir = <exe_dir>`, so it reads `<exe_dir>/user/...`.
/// We also keep `<engine_dir>/user/...` in sync for flat installs and backwards compatibility.
/// For Linux `.AppImage` builds, `spawn_switch_process` isolates `XDG_DATA_HOME` to
/// `<exe_dir>/xdg_data`, so we also include `<exe_dir>/xdg_data/{torzu,yuzu,fritchii}`.
fn resolve_engine_user_dirs(engine_dir: &Path, exe_path: Option<&Path>) -> Vec<PathBuf> {
    let mut user_dirs: Vec<PathBuf> = Vec::new();
    let mut push_unique = |p: PathBuf| {
        if !user_dirs.iter().any(|existing| existing == &p) {
            user_dirs.push(p);
        }
    };

    if let Some(exe) = exe_path {
        if let Some(parent) = exe.parent() {
            if !is_macos_bundle_macos_dir(parent) && parent.is_dir() {
                push_unique(parent.join("user"));
            }
            #[cfg(target_os = "linux")]
            if is_appimage_path(exe) {
                let xdg_root = parent.join("xdg_data");
                for sub in ["torzu", "yuzu", "fritchii"] {
                    push_unique(xdg_root.join(sub));
                }
            }
        }
    }

    if engine_dir.is_dir() {
        push_unique(engine_dir.join("user"));
    }

    user_dirs
}

fn directory_size_bytes(dir: &Path) -> u64 {
    if !dir.is_dir() {
        return 0;
    }
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .sum()
}

fn shader_cache_dirs(engine_dir: &Path, exe_path: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push_unique = |p: PathBuf| {
        if !dirs.iter().any(|existing| existing == &p) {
            dirs.push(p);
        }
    };

    for user_dir in resolve_engine_user_dirs(engine_dir, exe_path) {
        push_unique(user_dir.join("shader"));
    }
    if let Some(parent) = exe_path.and_then(|p| p.parent()) {
        if !is_macos_bundle_macos_dir(parent) {
            push_unique(parent.join("shader"));
        }
    }
    push_unique(engine_dir.join("shader"));
    dirs
}

fn inspect_engine_install(vault_root: &Path, target: SwitchEngineTarget) -> SwitchEngineInstall {
    let engine_dir = vault_root.join("engines").join(target.id());
    let exe = find_engine_executable(&engine_dir, target);
    let shader_cache_bytes: u64 = shader_cache_dirs(&engine_dir, exe.as_deref())
        .iter()
        .map(|d| directory_size_bytes(d))
        .sum();
    let version_hint = exe.as_ref().and_then(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
    });
    let notes = if let Some(ref exe_path) = exe {
        format!(
            "{} prêt ({})",
            target.label(),
            exe_path.file_name().unwrap_or_default().to_string_lossy()
        )
    } else {
        target.default_notes().to_string()
    };

    SwitchEngineInstall {
        target,
        installed: exe.is_some(),
        engine_dir: engine_dir.to_string_lossy().to_string(),
        executable_path: exe.map(|p| p.to_string_lossy().to_string()),
        version_hint,
        shader_cache_bytes,
        notes,
    }
}

fn is_nca_file_name(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".nca")
}

fn inspect_switch_firmware(vault_root: &Path) -> SwitchFirmwareStatus {
    let firmware_dir = vault_root.join("firmware");
    let mut nca_count = 0usize;
    let mut total_bytes = 0u64;

    if firmware_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&firmware_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.starts_with('.') {
                    continue;
                }
                if path.is_file() && is_nca_file_name(&fname) {
                    nca_count += 1;
                    total_bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                } else if path.is_dir() && is_nca_file_name(&fname) {
                    let bytes = directory_size_bytes(&path);
                    if bytes > 0 {
                        nca_count += 1;
                        total_bytes += bytes;
                    }
                }
            }
        }
    }

    let version_hint = fs::read_to_string(firmware_dir.join(FIRMWARE_VERSION_HINT_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    SwitchFirmwareStatus {
        firmware_dir: firmware_dir.to_string_lossy().to_string(),
        installed: nca_count > 0,
        nca_count,
        total_bytes,
        version_hint,
    }
}

/// Inspects only Zaimule's isolated Switch Vault (`<vault_root>/engines/*`, `<vault_root>/keys/*`, and `<vault_root>/firmware/*`).
pub fn inspect_switch_vault(vault_root: &Path) -> SwitchVaultStatus {
    let keys_dir = vault_root.join("keys");
    let prod = keys_dir.join("prod.keys");
    let title = keys_dir.join("title.keys");
    let keys = SwitchKeysStatus {
        keys_dir: keys_dir.to_string_lossy().to_string(),
        prod_keys_found: prod.is_file(),
        title_keys_found: title.is_file(),
        prod_keys_path: prod.is_file().then(|| prod.to_string_lossy().to_string()),
        title_keys_path: title.is_file().then(|| title.to_string_lossy().to_string()),
    };

    SwitchVaultStatus {
        vault_root: vault_root.to_string_lossy().to_string(),
        torzu: inspect_engine_install(vault_root, SwitchEngineTarget::Torzu),
        fritchii: inspect_engine_install(vault_root, SwitchEngineTarget::Fritchii),
        keys,
        firmware: inspect_switch_firmware(vault_root),
    }
}

fn copy_file_preserving_exec(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::copy(src, dst)
        .map_err(|e| format!("copie {} -> {} : {e}", src.display(), dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(src) {
            let mode = meta.permissions().mode();
            if mode & 0o111 != 0 {
                let _ = fs::set_permissions(dst, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

/// Resolves a path by canonicalizing its deepest existing ancestor and re-appending remaining components.
fn canonicalize_allowing_missing_leaf(path: &Path) -> PathBuf {
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    while let Some(name) = cur.file_name() {
        tail.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
            break;
        };
        cur = parent;
        if let Ok(mut canon) = cur.canonicalize() {
            for comp in tail.into_iter().rev() {
                canon.push(comp);
            }
            return canon;
        }
    }
    path.to_path_buf()
}

fn copy_dir_recursive(src_dir: &Path, dst_dir: &Path) -> Result<(), String> {
    let src_canon = canonicalize_allowing_missing_leaf(src_dir);
    let dst_canon = canonicalize_allowing_missing_leaf(dst_dir);
    if src_canon == dst_canon {
        return Ok(());
    }
    if dst_canon.starts_with(&src_canon) {
        return Err(format!(
            "copie récursive invalide : {} est à l'intérieur de {}",
            dst_dir.display(),
            src_dir.display()
        ));
    }
    fs::create_dir_all(dst_dir).map_err(|e| format!("{}: {e}", dst_dir.display()))?;
    for entry in WalkDir::new(src_dir).follow_links(false).into_iter() {
        let entry = entry.map_err(|e| e.to_string())?;
        let rel = entry
            .path()
            .strip_prefix(src_dir)
            .map_err(|e| e.to_string())?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst_dir.join(rel);
        let ft = entry.file_type();
        #[cfg(unix)]
        if ft.is_symlink() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            if target.symlink_metadata().is_ok() {
                let _ = fs::remove_file(&target).or_else(|_| fs::remove_dir_all(&target));
            }
            let link_target = fs::read_link(entry.path())
                .map_err(|e| format!("lien symbolique {}: {e}", entry.path().display()))?;
            std::os::unix::fs::symlink(&link_target, &target)
                .map_err(|e| format!("symlink {}: {e}", target.display()))?;
            continue;
        }
        if ft.is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        } else if ft.is_file() {
            copy_file_preserving_exec(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn looks_like_generic_user_folder(dir: &Path) -> bool {
    if dir.parent().is_none() {
        return true;
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        if !home.is_empty() && Path::new(&home) == dir {
            return true;
        }
    }
    // Reject shallow system roots like `/usr`, `/bin`, `/opt`, `C:\Windows`
    let depth = dir
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    let Some(name) = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
    else {
        return true;
    };
    if depth <= 1 {
        return true;
    }
    matches!(
        name.as_str(),
        "downloads"
            | "téléchargements"
            | "desktop"
            | "bureau"
            | "documents"
            | "mes documents"
            | "tmp"
            | "temp"
            | "home"
            | "users"
            | "windows"
            | "system32"
            | "program files"
            | "program files (x86)"
            | "applications"
            | "usr"
            | "bin"
            | "opt"
    )
}

fn is_sibling_runtime_file(path: &Path) -> bool {
    let lower = path
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if lower.contains(".so.") {
        return true;
    }
    let Some(ext) = path
        .extension()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
    else {
        return false;
    };
    matches!(ext.as_str(), "dll" | "so" | "dylib" | "conf" | "ini")
}

#[cfg(unix)]
fn mark_executable_if_target(path: &Path, target: SwitchEngineTarget) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(fname) = path.file_name().map(|n| n.to_string_lossy()) {
        // Never mark Windows PE `.exe` binaries executable on Unix.
        if fname.to_ascii_lowercase().ends_with(".exe") {
            return;
        }
        if is_candidate_executable_name(&fname, target) {
            if let Ok(meta) = fs::metadata(path) {
                let mut perms = meta.permissions();
                perms.set_mode(perms.mode() | 0o755);
                let _ = fs::set_permissions(path, perms);
            }
        }
    }
}

#[cfg(not(unix))]
fn mark_executable_if_target(_path: &Path, _target: SwitchEngineTarget) {}

/// Imports a user-supplied Torzu or Fritchii/Lichii build (folder, `.app` bundle, executable, `.zip`, or `.7z`)
/// into Zaimule's isolated Switch Vault (`<vault_root>/engines/<target>`).
pub fn import_switch_engine(
    source_path: &Path,
    vault_root: &Path,
    target: SwitchEngineTarget,
) -> Result<SwitchVaultStatus, String> {
    if !source_path.exists() {
        return Err(format!(
            "source émulateur introuvable : {}",
            source_path.display()
        ));
    }

    let engine_dir = vault_root.join("engines").join(target.id());
    fs::create_dir_all(&engine_dir).map_err(|e| format!("{}: {e}", engine_dir.display()))?;

    let lower = source_path
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if source_path.is_dir() {
        let Some(_) = find_engine_executable(source_path, target) else {
            return Err(format!(
                "aucun exécutable {} détecté dans {}",
                target.label(),
                source_path.display()
            ));
        };
        if lower.ends_with(".app") {
            let bundle_name = source_path
                .file_name()
                .ok_or_else(|| "nom de bundle .app manquant".to_string())?;
            copy_dir_recursive(source_path, &engine_dir.join(bundle_name))?;
        } else {
            copy_dir_recursive(source_path, &engine_dir)?;
        }
    } else if lower.ends_with(".zip") || lower.ends_with(".7z") {
        let staging = vault_root
            .join("engines")
            .join(format!(".staging-{}", target.id()));
        let _ = fs::remove_dir_all(&staging);
        extract_archive_to_dir(source_path, &staging)?;
        let Some(_) = find_engine_executable(&staging, target) else {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!(
                "l'archive {} ne contient aucun exécutable {}",
                source_path.display(),
                target.label()
            ));
        };
        copy_dir_recursive(&staging, &engine_dir)?;
        let _ = fs::remove_dir_all(&staging);
    } else if source_path.is_file() {
        if !is_candidate_executable_name(&lower, target) {
            return Err(format!(
                "fichier '{}' non reconnu comme exécutable {}",
                source_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                target.label()
            ));
        }
        let dst = engine_dir.join(
            source_path
                .file_name()
                .ok_or_else(|| "nom d'exécutable manquant".to_string())?,
        );
        copy_file_preserving_exec(source_path, &dst)?;
        mark_executable_if_target(&dst, target);

        // If user picked torzu.exe/fritchii.exe inside a dedicated portable folder (not Downloads/Desktop),
        // also copy sibling Qt/runtime libraries and plugin dirs so it launches cleanly.
        if let Some(parent) = source_path.parent() {
            if parent.is_dir() && !looks_like_generic_user_folder(parent) {
                if let Ok(entries) = fs::read_dir(parent) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if p.is_file() && is_sibling_runtime_file(&p) {
                            let _ =
                                copy_file_preserving_exec(&p, &engine_dir.join(entry.file_name()));
                        }
                    }
                }
                for sub in [
                    "platforms",
                    "styles",
                    "imageformats",
                    "tls",
                    "audio",
                    "multimedia",
                    "iconengines",
                    "translations",
                ] {
                    let sub_dir = parent.join(sub);
                    if sub_dir.is_dir() {
                        let _ = copy_dir_recursive(&sub_dir, &engine_dir.join(sub));
                    }
                }
            }
        }
    } else {
        return Err(format!("chemin invalide : {}", source_path.display()));
    }

    // Also make sure extracted native binaries on Unix have executable bits set.
    for entry in WalkDir::new(&engine_dir)
        .follow_links(false)
        .max_depth(6)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() {
            mark_executable_if_target(entry.path(), target);
        }
    }

    let exe_path = find_engine_executable(&engine_dir, target);
    let _ = sync_keys_to_engine_user_dirs(vault_root, &engine_dir, exe_path.as_deref());
    let _ = sync_firmware_to_engine_user_dirs(vault_root, &engine_dir, exe_path.as_deref());

    let status = inspect_switch_vault(vault_root);
    let ok = match target {
        SwitchEngineTarget::Torzu => status.torzu.installed,
        SwitchEngineTarget::Fritchii => status.fritchii.installed,
    };
    if !ok {
        return Err(format!(
            "installation incomplète : aucun binaire {} exécutable trouvé après import",
            target.label()
        ));
    }
    Ok(status)
}

fn validate_and_copy_key_file(src: &Path, keys_dir: &Path) -> Result<bool, String> {
    let fname = src
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let canonical = match fname.as_str() {
        "prod.keys" => "prod.keys",
        "title.keys" => "title.keys",
        _ => return Ok(false),
    };
    let meta = fs::metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if meta.len() == 0 || meta.len() > MAX_KEY_FILE_BYTES {
        return Err(format!("fichier clé '{fname}' vide ou invalide"));
    }
    let content = fs::read_to_string(src)
        .map_err(|_| format!("fichier '{fname}' invalide : format texte attendu"))?;
    if !content.contains('=') {
        return Err(format!(
            "fichier '{fname}' invalide : aucune paire clé=valeur détectée"
        ));
    }
    fs::create_dir_all(keys_dir).map_err(|e| format!("{}: {e}", keys_dir.display()))?;
    let dst = keys_dir.join(canonical);
    let tmp = keys_dir.join(format!("{canonical}.tmp"));
    fs::write(&tmp, content.as_bytes()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    Ok(true)
}

/// Imports `prod.keys` and/or `title.keys` supplied by the user into `<vault_root>/keys`
/// and propagates them into Torzu and Fritchii portable `user/keys` folders (including nested builds).
pub fn import_switch_keys(
    paths: &[PathBuf],
    vault_root: &Path,
) -> Result<SwitchVaultStatus, String> {
    let keys_dir = vault_root.join("keys");
    let mut imported_count = 0usize;

    for raw in paths {
        if raw.is_dir() {
            for entry in WalkDir::new(raw)
                .follow_links(false)
                .max_depth(2)
                .into_iter()
                .filter_map(Result::ok)
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                let lower = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if (lower == "prod.keys" || lower == "title.keys")
                    && validate_and_copy_key_file(entry.path(), &keys_dir)?
                {
                    imported_count += 1;
                }
            }
        } else if raw.is_file() {
            if !validate_and_copy_key_file(raw, &keys_dir)? {
                return Err(format!(
                    "fichier '{}' ignoré : sélectionnez 'prod.keys' ou 'title.keys'",
                    raw.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
            imported_count += 1;
        } else {
            return Err(format!("fichier clé introuvable : {}", raw.display()));
        }
    }

    if imported_count == 0 {
        return Err("aucun fichier 'prod.keys' ou 'title.keys' trouvé".into());
    }

    for target in [SwitchEngineTarget::Torzu, SwitchEngineTarget::Fritchii] {
        let engine_dir = vault_root.join("engines").join(target.id());
        if engine_dir.is_dir() {
            let exe = find_engine_executable(&engine_dir, target);
            sync_keys_to_engine_user_dirs(vault_root, &engine_dir, exe.as_deref())?;
        }
    }

    Ok(inspect_switch_vault(vault_root))
}

fn sync_keys_to_single_user_dir(vault_keys: &Path, user_dir: &Path) -> Result<(), String> {
    let user_keys = user_dir.join("keys");
    let mut created = false;
    for name in ["prod.keys", "title.keys"] {
        let src = vault_keys.join(name);
        if src.is_file() {
            if !created {
                fs::create_dir_all(&user_keys)
                    .map_err(|e| format!("{}: {e}", user_keys.display()))?;
                created = true;
            }
            let dst = user_keys.join(name);
            let tmp = user_keys.join(format!("{name}.tmp"));
            fs::copy(&src, &tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
            fs::rename(&tmp, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        }
    }
    Ok(())
}

fn sync_keys_to_engine_user_dirs(
    vault_root: &Path,
    engine_dir: &Path,
    exe_path: Option<&Path>,
) -> Result<(), String> {
    let vault_keys = vault_root.join("keys");
    if !vault_keys.is_dir() {
        return Ok(());
    }
    for user_dir in resolve_engine_user_dirs(engine_dir, exe_path) {
        sync_keys_to_single_user_dir(&vault_keys, &user_dir)?;
    }
    Ok(())
}

/// Copies vaulted `prod.keys` and `title.keys` into `<engine_dir>/user/keys/`.
pub fn sync_keys_to_engine_user_dir(vault_root: &Path, engine_dir: &Path) -> Result<(), String> {
    sync_keys_to_engine_user_dirs(vault_root, engine_dir, None)
}

fn collect_nca_entries(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut skip_prefixes: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
    {
        let p = entry.path();
        if skip_prefixes.iter().any(|pref| p.starts_with(pref)) {
            continue;
        }
        let fname = entry.file_name().to_string_lossy();
        if !is_nca_file_name(&fname) {
            continue;
        }
        if entry.file_type().is_file() {
            found.push(p.to_path_buf());
        } else if entry.file_type().is_dir() && directory_size_bytes(p) > 0 {
            found.push(p.to_path_buf());
            skip_prefixes.push(p.to_path_buf());
        }
    }
    found
}

fn compute_firmware_stamp(firmware_dir: &Path) -> Option<String> {
    if let Ok(stamp) = fs::read_to_string(firmware_dir.join(FIRMWARE_STAMP_FILE)) {
        let trimmed = stamp.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let status = inspect_switch_firmware(firmware_dir.parent()?);
    if !status.installed {
        return None;
    }
    Some(format!(
        "{}:{}:{}",
        status.nca_count,
        status.total_bytes,
        status.version_hint.unwrap_or_default()
    ))
}

/// Imports a user-supplied Nintendo Switch firmware (folder, `.zip`, or `.7z` containing `.nca` archives)
/// into `<vault_root>/firmware` and synchronizes it into Torzu and Fritchii
/// `user/nand/system/Contents/registered/` directories.
pub fn import_switch_firmware(
    source_path: &Path,
    vault_root: &Path,
) -> Result<SwitchVaultStatus, String> {
    if !source_path.exists() {
        return Err(format!(
            "source firmware Switch introuvable : {}",
            source_path.display()
        ));
    }

    let lower = source_path
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    let staging = vault_root.join(".staging-firmware");
    let _ = fs::remove_dir_all(&staging);

    let scan_root = if source_path.is_dir() {
        source_path.to_path_buf()
    } else if lower.ends_with(".zip") || lower.ends_with(".7z") {
        extract_archive_to_dir(source_path, &staging)?;
        staging.clone()
    } else {
        return Err(format!(
            "format firmware '{}' non supporté : sélectionnez un dossier ou une archive .zip/.7z contenant les fichiers .nca",
            source_path.file_name().unwrap_or_default().to_string_lossy()
        ));
    };

    let nca_entries = collect_nca_entries(&scan_root);
    if nca_entries.is_empty() {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "aucun fichier firmware Switch (.nca) trouvé dans {}",
            source_path.display()
        ));
    }

    let new_dir = vault_root.join(".firmware-new");
    let _ = fs::remove_dir_all(&new_dir);
    fs::create_dir_all(&new_dir).map_err(|e| format!("{}: {e}", new_dir.display()))?;

    let mut total_bytes = 0u64;
    let mut copied_count = 0usize;
    for src in &nca_entries {
        let Some(fname) = src.file_name() else {
            continue;
        };
        let dst = new_dir.join(fname);
        if src.is_dir() {
            copy_dir_recursive(src, &dst)?;
            total_bytes += directory_size_bytes(&dst);
        } else {
            copy_file_preserving_exec(src, &dst)?;
            total_bytes += fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
        }
        copied_count += 1;
    }

    let version_hint = source_path
        .file_stem()
        .or_else(|| source_path.file_name())
        .map(|s| s.to_string_lossy().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{copied_count} fichiers .nca"));
    let _ = fs::write(new_dir.join(FIRMWARE_VERSION_HINT_FILE), &version_hint);
    let stamp = format!("{copied_count}:{total_bytes}:{version_hint}");
    let _ = fs::write(new_dir.join(FIRMWARE_STAMP_FILE), &stamp);

    let firmware_dir = vault_root.join("firmware");
    let backup_dir = vault_root.join(".firmware-old");
    let _ = fs::remove_dir_all(&backup_dir);
    let had_old = firmware_dir.exists();
    if had_old {
        fs::rename(&firmware_dir, &backup_dir)
            .map_err(|e| format!("{}: {e}", firmware_dir.display()))?;
    }
    if let Err(err) = fs::rename(&new_dir, &firmware_dir) {
        if had_old {
            let _ = fs::rename(&backup_dir, &firmware_dir);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("{}: {err}", firmware_dir.display()));
    }
    let _ = fs::remove_dir_all(&backup_dir);
    let _ = fs::remove_dir_all(&staging);

    for target in [SwitchEngineTarget::Torzu, SwitchEngineTarget::Fritchii] {
        let engine_dir = vault_root.join("engines").join(target.id());
        if engine_dir.is_dir() {
            let exe = find_engine_executable(&engine_dir, target);
            sync_firmware_to_engine_user_dirs(vault_root, &engine_dir, exe.as_deref())?;
        }
    }

    Ok(inspect_switch_vault(vault_root))
}

fn sync_firmware_to_single_user_dir(
    vault_firmware: &Path,
    stamp: &str,
    user_dir: &Path,
) -> Result<(), String> {
    let contents_dir = user_dir.join("nand/system/Contents");
    let registered_dir = contents_dir.join("registered");
    let stamp_path = contents_dir.join(FIRMWARE_STAMP_FILE);

    if registered_dir.is_dir() {
        if let Ok(existing) = fs::read_to_string(&stamp_path) {
            if existing.trim() == stamp
                && fs::read_dir(&registered_dir)
                    .map(|mut it| it.next().is_some())
                    .unwrap_or(false)
            {
                return Ok(());
            }
        }
        fs::remove_dir_all(&registered_dir)
            .map_err(|e| format!("{}: {e}", registered_dir.display()))?;
    }

    fs::create_dir_all(&registered_dir)
        .map_err(|e| format!("{}: {e}", registered_dir.display()))?;

    let entries =
        fs::read_dir(vault_firmware).map_err(|e| format!("{}: {e}", vault_firmware.display()))?;
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let name_str = fname.to_string_lossy();
        if name_str.starts_with('.') || !is_nca_file_name(&name_str) {
            continue;
        }
        let src = entry.path();
        let dst = registered_dir.join(&fname);
        if src.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else if src.is_file() {
            copy_file_preserving_exec(&src, &dst)?;
        }
    }

    fs::write(&stamp_path, stamp).map_err(|e| format!("{}: {e}", stamp_path.display()))?;
    Ok(())
}

fn sync_firmware_to_engine_user_dirs(
    vault_root: &Path,
    engine_dir: &Path,
    exe_path: Option<&Path>,
) -> Result<(), String> {
    let vault_firmware = vault_root.join("firmware");
    if !vault_firmware.is_dir() {
        return Ok(());
    }
    let Some(stamp) = compute_firmware_stamp(&vault_firmware) else {
        return Ok(());
    };
    for user_dir in resolve_engine_user_dirs(engine_dir, exe_path) {
        sync_firmware_to_single_user_dir(&vault_firmware, &stamp, &user_dir)?;
    }
    Ok(())
}

/// Validates vault readiness, synchronizes user keys & firmware, and builds a ready launch plan.
pub fn prepare_switch_launch(
    vault_root: &Path,
    target: SwitchEngineTarget,
    rom_path: &Path,
) -> Result<SwitchLaunchPlan, String> {
    if !rom_path.is_file() {
        return Err(format!(
            "fichier jeu introuvable sur disque : {}",
            rom_path.display()
        ));
    }

    let status = inspect_switch_vault(vault_root);
    let install = match target {
        SwitchEngineTarget::Torzu => &status.torzu,
        SwitchEngineTarget::Fritchii => &status.fritchii,
    };
    let exe_str = install.executable_path.as_deref().ok_or_else(|| {
        format!(
            "{} n'est pas encore importé dans votre Coffre Switch. Importez votre build dans l'onglet Coffre Switch.",
            target.label()
        )
    })?;
    let engine_dir = PathBuf::from(&install.engine_dir);
    let exe_path = PathBuf::from(exe_str);
    if !exe_path.is_file() {
        return Err(format!(
            "exécutable {} introuvable : {}",
            target.label(),
            exe_path.display()
        ));
    }

    let is_homebrew = rom_path
        .extension()
        .map(|e| {
            let ext = e.to_string_lossy().to_ascii_lowercase();
            ext == "nro" || ext == "nso"
        })
        .unwrap_or(false);

    let user_dirs = resolve_engine_user_dirs(&engine_dir, Some(&exe_path));
    let has_engine_keys = user_dirs
        .iter()
        .any(|dir| dir.join("keys/prod.keys").is_file());
    if !is_homebrew && !status.keys.prod_keys_found && !has_engine_keys {
        return Err(
            "Clé 'prod.keys' absente du Coffre Switch. Importez votre propre fichier prod.keys dans Zaimule avant de lancer."
                .into(),
        );
    }

    sync_keys_to_engine_user_dirs(vault_root, &engine_dir, Some(&exe_path))?;
    sync_firmware_to_engine_user_dirs(vault_root, &engine_dir, Some(&exe_path))?;

    Ok(SwitchLaunchPlan {
        target,
        executable_path: exe_path,
        engine_dir,
        rom_path: rom_path.to_path_buf(),
    })
}

/// Spawns `Torzu` or `Fritchii` with `-f -g <rom_path>` and returns the child process handle for playtime tracking.
pub fn spawn_switch_process(
    exe_path: &Path,
    rom_path: &Path,
    target: SwitchEngineTarget,
) -> Result<Child, String> {
    if !exe_path.is_file() {
        return Err(format!(
            "exécutable {} introuvable : {}",
            target.label(),
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
    if let Some(dir) = exe_path.parent() {
        cmd.current_dir(dir);
        #[cfg(target_os = "linux")]
        if is_appimage_path(exe_path) {
            let xdg_data = dir.join("xdg_data");
            let _ = fs::create_dir_all(&xdg_data);
            cmd.env("XDG_DATA_HOME", xdg_data);
        }
    }
    cmd.arg("-f")
        .arg("-g")
        .arg(rom_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    cmd.spawn().map_err(|e| {
        format!(
            "échec du lancement de {} ({}) : {e}",
            target.label(),
            exe_path.file_name().unwrap_or_default().to_string_lossy()
        )
    })
}

/// Clears shader cache folders inside `<vault_root>/engines/<target>` (including nested portable dirs)
/// and returns reclaimed byte count.
pub fn clear_switch_shader_cache(
    vault_root: &Path,
    target: SwitchEngineTarget,
) -> Result<u64, String> {
    let engine_dir = vault_root.join("engines").join(target.id());
    let exe = find_engine_executable(&engine_dir, target);
    let mut cleared = 0u64;
    for dir in shader_cache_dirs(&engine_dir, exe.as_deref()) {
        if dir.is_dir() {
            cleared += directory_size_bytes(&dir);
            fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
    }
    Ok(cleared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::build_stored_zip_for_test;

    #[test]
    fn imports_torzu_zip_fritchii_exe_syncs_keys_and_clears_shader_cache() {
        let root = std::env::temp_dir().join(format!("zaimule-vault-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let vault_root = root.join("switch_vault");
        let empty = inspect_switch_vault(&vault_root);
        assert!(!empty.torzu.installed);
        assert!(!empty.fritchii.installed);
        assert!(!empty.keys.prod_keys_found);
        assert!(!empty.firmware.installed);

        // 1. Import Torzu from a portable nested .zip release archive
        let torzu_zip = root.join("CapipackTorzuV13.zip");
        let zip_bytes = build_stored_zip_for_test(&[
            ("Torzu/torzu.exe", b"MZ_FAKE_TORZU"),
            ("Torzu/Qt6Core.dll", b"MZ_QT6"),
        ]);
        fs::write(&torzu_zip, &zip_bytes).unwrap();
        let after_torzu =
            import_switch_engine(&torzu_zip, &vault_root, SwitchEngineTarget::Torzu).unwrap();
        assert!(after_torzu.torzu.installed);
        assert!(after_torzu
            .torzu
            .executable_path
            .as_deref()
            .unwrap()
            .ends_with("torzu.exe"));

        // 2. Import Fritchii directly from a folder
        let fritchii_src = root.join("fritchii-v5.7");
        fs::create_dir_all(&fritchii_src).unwrap();
        fs::write(fritchii_src.join("fritchii.exe"), b"MZ_FAKE_FRITCHII").unwrap();
        let after_fritchii =
            import_switch_engine(&fritchii_src, &vault_root, SwitchEngineTarget::Fritchii).unwrap();
        assert!(after_fritchii.fritchii.installed);

        // 3. Import user keys & verify launch plan fails for .nsp before keys, succeeds for .nro homebrew
        let fake_rom = root.join("Wonder.nsp");
        fs::write(&fake_rom, b"PFS0").unwrap();
        assert!(prepare_switch_launch(&vault_root, SwitchEngineTarget::Torzu, &fake_rom).is_err());

        let fake_homebrew = root.join("hbmenu.nro");
        fs::write(&fake_homebrew, b"NRO0").unwrap();
        assert!(
            prepare_switch_launch(&vault_root, SwitchEngineTarget::Torzu, &fake_homebrew).is_ok()
        );

        // Case-insensitive directory key import
        let src_keys = root.join("user_keys_src");
        fs::create_dir_all(&src_keys).unwrap();
        fs::write(
            src_keys.join("PROD.KEYS"),
            b"master_key_00 = 0123456789ABCDEF",
        )
        .unwrap();
        fs::write(
            src_keys.join("Title.Keys"),
            b"title_key_00 = FEDCBA9876543210",
        )
        .unwrap();

        let after_keys = import_switch_keys(std::slice::from_ref(&src_keys), &vault_root).unwrap();
        assert!(after_keys.keys.prod_keys_found);
        assert!(after_keys.keys.title_keys_found);

        let plan =
            prepare_switch_launch(&vault_root, SwitchEngineTarget::Torzu, &fake_rom).unwrap();
        // Both nested portable dir (`Torzu/user/keys/`) AND top-level `<engine_dir>/user/keys/` are synced
        assert!(plan.engine_dir.join("user/keys/prod.keys").is_file());
        assert!(plan.engine_dir.join("user/keys/title.keys").is_file());
        assert!(plan
            .executable_path
            .parent()
            .unwrap()
            .join("user/keys/prod.keys")
            .is_file());
        assert!(plan
            .executable_path
            .parent()
            .unwrap()
            .join("user/keys/title.keys")
            .is_file());

        // 4. Import Switch firmware .zip & verify sync into nested Torzu AND Fritchii NAND registered/
        let fw_zip = root.join("Firmware 19.0.1.zip");
        let fw_bytes = build_stored_zip_for_test(&[
            ("Firmware 19.0.1/0100000000000809.nca", b"NCA3_FAKE_FONT"),
            ("Firmware 19.0.1/0100000000000811.nca", b"NCA3_FAKE_MII"),
        ]);
        fs::write(&fw_zip, &fw_bytes).unwrap();
        let after_fw = import_switch_firmware(&fw_zip, &vault_root).unwrap();
        assert!(after_fw.firmware.installed);
        assert_eq!(after_fw.firmware.nca_count, 2);
        assert_eq!(
            after_fw.firmware.version_hint.as_deref(),
            Some("Firmware 19.0.1")
        );
        assert!(plan
            .executable_path
            .parent()
            .unwrap()
            .join("user/nand/system/Contents/registered/0100000000000809.nca")
            .is_file());
        assert!(PathBuf::from(&after_fritchii.fritchii.engine_dir)
            .join("user/nand/system/Contents/registered/0100000000000811.nca")
            .is_file());

        // 5. Shader cache clearing inside nested portable `Torzu/user/shader`
        let nested_shader_dir = plan.executable_path.parent().unwrap().join("user/shader");
        fs::create_dir_all(&nested_shader_dir).unwrap();
        fs::write(nested_shader_dir.join("cache.bin"), vec![0xAAu8; 2048]).unwrap();
        let inspected = inspect_switch_vault(&vault_root);
        assert_eq!(inspected.torzu.shader_cache_bytes, 2048);
        let freed = clear_switch_shader_cache(&vault_root, SwitchEngineTarget::Torzu).unwrap();
        assert_eq!(freed, 2048);
        assert!(!nested_shader_dir.exists());

        // 6. Override resolution
        assert_eq!(
            resolve_switch_engine_for_game(ConsoleKind::Switch, SwitchEngineOverride::Auto)
                .unwrap(),
            SwitchEngineTarget::Torzu
        );
        assert_eq!(
            resolve_switch_engine_for_game(ConsoleKind::Switch2, SwitchEngineOverride::Auto)
                .unwrap(),
            SwitchEngineTarget::Fritchii
        );
        assert_eq!(
            resolve_switch_engine_for_game(ConsoleKind::Switch, SwitchEngineOverride::Fritchii)
                .unwrap(),
            SwitchEngineTarget::Fritchii
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn os_aware_binary_ranking_prefers_native_binary_and_guards_recursive_copy() {
        let root =
            std::env::temp_dir().join(format!("zaimule-vault-os-rank-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let mix = root.join("mixed_torzu");
        fs::create_dir_all(&mix).unwrap();
        fs::write(mix.join("torzu.exe"), b"MZ").unwrap();
        fs::write(mix.join("torzu-v13-linux.AppImage"), b"\x7fELF").unwrap();
        fs::write(mix.join("torzu.dll"), b"NOT_EXE").unwrap();

        let picked = find_engine_executable(&mix, SwitchEngineTarget::Torzu).unwrap();
        let fname = picked.file_name().unwrap().to_string_lossy().to_string();
        #[cfg(windows)]
        assert_eq!(fname, "torzu.exe");
        #[cfg(not(windows))]
        assert_eq!(fname, "torzu-v13-linux.AppImage");

        // Guard against copying a directory into its own descendant
        let nested_dst = mix.join("sub/nested");
        assert!(copy_dir_recursive(&mix, &nested_dst).is_err());

        let _ = fs::remove_dir_all(&root);
    }
}
