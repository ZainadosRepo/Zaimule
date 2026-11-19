//! Option-A Zaimule Switch Vault manager:
//! - Isolated user-imported Switch 1 (`Torzu`) and Switch 2 (`Fritchii / Lichii`) installations
//! - Manual user import for `prod.keys` and `title.keys` (zero bundled keys or auto-scanning)
//! - Portable key synchronization into `<engine_dir>/user/keys` before process launch
//! - Shader cache management and child-process spawning with `-f -g <rom>`

use crate::archive::extract_archive_to_dir;
use crate::catalog::{ConsoleKind, SwitchEngineOverride};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use walkdir::WalkDir;

const MAX_KEY_FILE_BYTES: u64 = 16 * 1024 * 1024;

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

/// Complete Option-A Switch Vault state serialized to the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SwitchVaultStatus {
    pub vault_root: String,
    pub torzu: SwitchEngineInstall,
    pub fritchii: SwitchEngineInstall,
    pub keys: SwitchKeysStatus,
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

fn is_candidate_executable_name(file_name: &str, target: SwitchEngineTarget) -> bool {
    let lower = file_name.to_ascii_lowercase();
    for cand in candidate_binary_names(target) {
        if lower == cand.to_ascii_lowercase() {
            return true;
        }
        let stem = cand
            .strip_suffix(".exe")
            .unwrap_or(cand)
            .to_ascii_lowercase();
        if lower.starts_with(&stem) && lower.ends_with(".appimage") {
            return true;
        }
    }
    target == SwitchEngineTarget::Fritchii
        && (lower.starts_with("fritchii") || lower.starts_with("lichii"))
        && (lower.ends_with(".exe") || lower.ends_with(".appimage") || !lower.contains('.'))
}

fn find_engine_executable(dir: &Path, target: SwitchEngineTarget) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let cands = candidate_binary_names(target);
    // Prioritize shallower depth first so top-level binaries win cleanly
    let mut found: Vec<(usize, PathBuf)> = Vec::new();
    for entry in WalkDir::new(dir)
        .follow_links(false)
        .max_depth(4)
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
                .unwrap_or(cands.len());
            found.push((entry.depth() * 100 + priority, entry.into_path()));
        }
    }
    found.sort_by_key(|(score, _)| *score);
    found.into_iter().next().map(|(_, p)| p)
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

fn shader_cache_dirs(engine_dir: &Path) -> [PathBuf; 2] {
    [engine_dir.join("user/shader"), engine_dir.join("shader")]
}

fn inspect_engine_install(vault_root: &Path, target: SwitchEngineTarget) -> SwitchEngineInstall {
    let engine_dir = vault_root.join("engines").join(target.id());
    let exe = find_engine_executable(&engine_dir, target);
    let shader_cache_bytes: u64 = shader_cache_dirs(&engine_dir)
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

/// Inspects only Zaimule's isolated Switch Vault (`<vault_root>/engines/*` & `<vault_root>/keys/*`).
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

fn copy_dir_recursive(src_dir: &Path, dst_dir: &Path) -> Result<(), String> {
    let src_canon = src_dir
        .canonicalize()
        .unwrap_or_else(|_| src_dir.to_path_buf());
    let dst_canon = dst_dir
        .canonicalize()
        .unwrap_or_else(|_| dst_dir.to_path_buf());
    if src_canon == dst_canon {
        return Ok(());
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
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        } else if entry.file_type().is_file() {
            copy_file_preserving_exec(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn looks_like_generic_user_folder(dir: &Path) -> bool {
    let Some(name) = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
    else {
        return true;
    };
    matches!(
        name.as_str(),
        "downloads" | "téléchargements" | "desktop" | "bureau" | "documents" | "mes documents"
    )
}

fn is_sibling_runtime_file(path: &Path) -> bool {
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

/// Imports a user-supplied Torzu or Fritchii/Lichii build (folder, executable, `.zip`, or `.7z`)
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
        copy_dir_recursive(source_path, &engine_dir)?;
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
        // also copy sibling Qt/runtime DLLs and `platforms/` plugin dir so it launches cleanly.
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
                for sub in ["platforms", "styles", "imageformats"] {
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

    // Also make sure extracted binaries on Unix have executable bits set
    for entry in WalkDir::new(&engine_dir)
        .follow_links(false)
        .max_depth(4)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() {
            mark_executable_if_target(entry.path(), target);
        }
    }

    let _ = sync_keys_to_engine_user_dir(vault_root, &engine_dir);
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
/// and propagates them into Torzu and Fritchii portable `user/keys` folders.
pub fn import_switch_keys(
    paths: &[PathBuf],
    vault_root: &Path,
) -> Result<SwitchVaultStatus, String> {
    let keys_dir = vault_root.join("keys");
    let mut imported_count = 0usize;

    for raw in paths {
        if raw.is_dir() {
            for candidate in ["prod.keys", "title.keys"] {
                let p = raw.join(candidate);
                if p.is_file() && validate_and_copy_key_file(&p, &keys_dir)? {
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
            sync_keys_to_engine_user_dir(vault_root, &engine_dir)?;
        }
    }

    Ok(inspect_switch_vault(vault_root))
}

/// Copies vaulted `prod.keys` and `title.keys` into `<engine_dir>/user/keys/`.
pub fn sync_keys_to_engine_user_dir(vault_root: &Path, engine_dir: &Path) -> Result<(), String> {
    let vault_keys = vault_root.join("keys");
    if !vault_keys.is_dir() {
        return Ok(());
    }
    let user_keys = engine_dir.join("user/keys");
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

/// Validates vault readiness, synchronizes user keys, and builds a ready launch plan.
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

    let has_engine_keys = engine_dir.join("user/keys/prod.keys").is_file();
    if !status.keys.prod_keys_found && !has_engine_keys {
        return Err(
            "Clé 'prod.keys' absente du Coffre Switch. Importez votre propre fichier prod.keys dans Zaimule avant de lancer."
                .into(),
        );
    }

    sync_keys_to_engine_user_dir(vault_root, &engine_dir)?;

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

/// Clears shader cache folders inside `<vault_root>/engines/<target>` and returns reclaimed byte count.
pub fn clear_switch_shader_cache(
    vault_root: &Path,
    target: SwitchEngineTarget,
) -> Result<u64, String> {
    let engine_dir = vault_root.join("engines").join(target.id());
    let mut cleared = 0u64;
    for dir in shader_cache_dirs(&engine_dir) {
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

        // 1. Import Torzu from a portable .zip release archive
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

        // 3. Import user keys & verify launch plan fails before keys, succeeds after
        let fake_rom = root.join("Wonder.nsp");
        fs::write(&fake_rom, b"PFS0").unwrap();
        assert!(prepare_switch_launch(&vault_root, SwitchEngineTarget::Torzu, &fake_rom).is_err());

        let src_keys = root.join("user_keys_src");
        fs::create_dir_all(&src_keys).unwrap();
        fs::write(
            src_keys.join("prod.keys"),
            b"master_key_00 = 0123456789ABCDEF",
        )
        .unwrap();
        fs::write(
            src_keys.join("title.keys"),
            b"title_key_00 = FEDCBA9876543210",
        )
        .unwrap();

        let after_keys = import_switch_keys(
            &[src_keys.join("prod.keys"), src_keys.join("title.keys")],
            &vault_root,
        )
        .unwrap();
        assert!(after_keys.keys.prod_keys_found);
        assert!(after_keys.keys.title_keys_found);

        let plan =
            prepare_switch_launch(&vault_root, SwitchEngineTarget::Torzu, &fake_rom).unwrap();
        assert!(plan.engine_dir.join("user/keys/prod.keys").is_file());
        assert!(plan.engine_dir.join("user/keys/title.keys").is_file());

        // 4. Shader cache clearing
        let shader_dir = PathBuf::from(&after_torzu.torzu.engine_dir).join("user/shader");
        fs::create_dir_all(&shader_dir).unwrap();
        fs::write(shader_dir.join("cache.bin"), vec![0xAAu8; 2048]).unwrap();
        let freed = clear_switch_shader_cache(&vault_root, SwitchEngineTarget::Torzu).unwrap();
        assert_eq!(freed, 2048);
        assert!(!shader_dir.exists());

        // 5. Override resolution
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
}
