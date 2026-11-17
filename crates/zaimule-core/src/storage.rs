//! Atomic JSON settings and binary SRAM/SaveState persistence (`settings.rs` / `prefs.rs` heritage).
//! Writes to `.tmp` then renames atomically, moves corrupted JSON files aside to `.json.bad`,
//! and rotates `.srm.bak1`..`.srm.bak3` backups for cartridge SRAM saves.

use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

const MAX_JSON_BYTES: usize = 2 << 20; // 2 MiB
const MAX_SRAM_BYTES: usize = 16 << 20; // 16 MiB

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LoadedJson {
    pub value: Value,
    pub warning: Option<String>,
}

/// Loads a JSON configuration file atomically. Missing files return `Value::Null`.
/// Corrupted files are moved aside to `.json.bad` and return a user-facing French warning.
pub fn load_json_atomic(path: &Path) -> Result<LoadedJson, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok(LoadedJson {
                value: Value::Null,
                warning: None,
            })
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };

    match serde_json::from_str(&text) {
        Ok(value) => Ok(LoadedJson {
            value,
            warning: None,
        }),
        Err(e) => {
            let backup = path.with_extension("json.bad");
            fs::rename(path, &backup)
                .map_err(|r| format!("{}: {e}, sauvegarde impossible : {r}", path.display()))?;
            let warning = format!(
                "réglages illisibles ({e}), copie conservée dans {}",
                backup.display()
            );
            Ok(LoadedJson {
                value: Value::Null,
                warning: Some(warning),
            })
        }
    }
}

/// Saves a JSON object atomically by writing to `.json.tmp` first and renaming over `path`.
pub fn save_json_atomic(path: &Path, value: &Value) -> Result<(), String> {
    if !value.is_object() {
        return Err("les réglages doivent être un objet JSON".into());
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    if text.len() > MAX_JSON_BYTES {
        return Err(format!(
            "fichier JSON trop volumineux ({} octets)",
            text.len()
        ));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Saves binary cartridge battery SRAM (`.srm`) atomically via `.srm.tmp` and rotates `.srm.bak1`..`.srm.bak3`.
pub fn save_sram_with_rotation(path: &Path, sram_bytes: &[u8]) -> Result<(), String> {
    if sram_bytes.len() > MAX_SRAM_BYTES {
        return Err(format!(
            "sauvegarde SRAM trop volumineuse ({} octets)",
            sram_bytes.len()
        ));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }

    // Rotate existing backups: .bak2 -> .bak3, .bak1 -> .bak2, .srm -> .bak1
    let bak1 = path.with_extension("srm.bak1");
    let bak2 = path.with_extension("srm.bak2");
    let bak3 = path.with_extension("srm.bak3");
    if bak2.exists() {
        let _ = fs::rename(&bak2, &bak3);
    }
    if bak1.exists() {
        let _ = fs::rename(&bak1, &bak2);
    }
    if path.exists() {
        let _ = fs::copy(path, &bak1);
    }

    let tmp = path.with_extension("srm.tmp");
    fs::write(&tmp, sram_bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_path(name: &str, ext: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zaimule-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join(format!("data.{ext}"))
    }

    #[test]
    fn missing_json_reads_as_null() {
        let loaded = load_json_atomic(&temp_path("missing", "json")).unwrap();
        assert_eq!((loaded.value, loaded.warning), (Value::Null, None));
    }

    #[test]
    fn saves_then_loads_json_roundtrip() {
        let path = temp_path("roundtrip", "json");
        let obj = json!({ "shader": "crtTrinitron", "internalScale": 4 });
        save_json_atomic(&path, &obj).unwrap();
        assert_eq!(load_json_atomic(&path).unwrap().value, obj);
    }

    #[test]
    fn moves_corrupt_json_aside_to_bad_and_warns() {
        let path = temp_path("corrupt", "json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ broken json").unwrap();
        let loaded = load_json_atomic(&path).unwrap();
        assert_eq!(loaded.value, Value::Null);
        assert!(loaded.warning.is_some());
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(path.with_extension("json.bad")).unwrap(),
            "{ broken json"
        );
    }

    #[test]
    fn sram_rotation_creates_numbered_backups_atomically() {
        let path = temp_path("sram", "srm");
        save_sram_with_rotation(&path, b"SAVE_V1").unwrap();
        save_sram_with_rotation(&path, b"SAVE_V2").unwrap();
        save_sram_with_rotation(&path, b"SAVE_V3").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"SAVE_V3");
        assert_eq!(
            fs::read(path.with_extension("srm.bak1")).unwrap(),
            b"SAVE_V2"
        );
        assert_eq!(
            fs::read(path.with_extension("srm.bak2")).unwrap(),
            b"SAVE_V1"
        );
    }
}
