//! Atomic JSON settings, library catalog, imported ROMs, and binary SRAM/SaveState persistence.
//! Writes to `.tmp` then renames atomically, moves corrupted JSON files aside to `.json.bad`,
//! and rotates `.srm.bak1`..`.srm.bak3` backups for cartridge SRAM saves.

use crate::catalog::{compute_crc32, format_crc32_hex, GameEntry};
use crate::cheats::CheatCodeEntry;
use crate::keymap::KeymapWidget;
use crate::macros::MacroSequence;
use crate::rewind::SaveStateSlotInfo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

const MAX_JSON_BYTES: usize = 16 << 20; // 16 MiB
const MAX_SRAM_BYTES: usize = 16 << 20; // 16 MiB
const MAX_STATE_BYTES: usize = 32 << 20; // 32 MiB
pub const PORTABLE_PACK_MAGIC: &str = "ZAIMULE_PACK_V1";

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct PersistedLibrary {
    pub active_game_id: String,
    pub catalog: Vec<GameEntry>,
    pub rom_directories: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LoadedJson {
    pub value: Value,
    pub warning: Option<String>,
}

/// On-disk envelope for a persisted Save State slot (`slot_N.zst.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedSaveState {
    pub info: SaveStateSlotInfo,
    pub uncompressed_len: usize,
    pub compressed_bytes: Vec<u8>,
}

/// Portable Cloud / USB Sync Archive (`.zaimule-pack`) bundling SRAM, Save States,
/// TAS Macros, GameShark Cheats, and Keymapper layout with CRC32 verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZaimulePortablePack {
    pub magic: String,
    pub version: String,
    pub active_game_id: String,
    pub sram_rle: Vec<u8>,
    pub sram_uncompressed_len: usize,
    pub save_slots: Vec<PersistedSaveState>,
    pub cheat_codes: Vec<CheatCodeEntry>,
    pub macros: Vec<MacroSequence>,
    pub keymapper_widgets: Vec<KeymapWidget>,
    pub payload_crc32_hex: String,
}

/// Serializes a `ZaimulePortablePack` into UTF-8 JSON bytes ready for `.zaimule-pack` export.
pub fn encode_portable_pack(pack: &ZaimulePortablePack) -> Result<Vec<u8>, String> {
    if pack.magic != PORTABLE_PACK_MAGIC {
        return Err("en-tête d'archive portable invalide".into());
    }
    serde_json::to_vec_pretty(pack).map_err(|e| e.to_string())
}

/// Deserializes and validates a `.zaimule-pack` archive buffer.
pub fn decode_portable_pack(bytes: &[u8]) -> Result<ZaimulePortablePack, String> {
    if bytes.is_empty() || bytes.len() > MAX_STATE_BYTES {
        return Err("taille d'archive .zaimule-pack invalide".into());
    }
    let pack: ZaimulePortablePack = serde_json::from_slice(bytes)
        .map_err(|e| format!("archive .zaimule-pack illisible : {e}"))?;
    if pack.magic != PORTABLE_PACK_MAGIC {
        return Err(format!(
            "signature d'archive non reconnue ({}, attendu {})",
            pack.magic, PORTABLE_PACK_MAGIC
        ));
    }
    Ok(pack)
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

/// Loads the persisted game library (`library.json`) atomically, returning `Default` if missing.
pub fn load_library_atomic(path: &Path) -> Result<(PersistedLibrary, Option<String>), String> {
    let loaded = load_json_atomic(path)?;
    if loaded.value.is_null() {
        return Ok((PersistedLibrary::default(), loaded.warning));
    }
    match serde_json::from_value::<PersistedLibrary>(loaded.value) {
        Ok(lib) => Ok((lib, loaded.warning)),
        Err(e) => {
            let backup = path.with_extension("json.bad");
            let _ = fs::rename(path, &backup);
            Ok((
                PersistedLibrary::default(),
                Some(format!(
                    "bibliothèque illisible ({e}), copie conservée dans {}",
                    backup.display()
                )),
            ))
        }
    }
}

/// Saves the user's game library atomically via `.json.tmp`.
pub fn save_library_atomic(path: &Path, library: &PersistedLibrary) -> Result<(), String> {
    let value = serde_json::to_value(library).map_err(|e| e.to_string())?;
    save_json_atomic(path, &value)
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

/// Loads binary SRAM bytes if present on disk, or returns `Ok(None)` if not found.
pub fn load_sram_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Saves a compressed Save State slot atomically to disk (`slot_N.zst.tmp` -> `slot_N.zst`).
pub fn save_state_slot_atomic(path: &Path, state: &PersistedSaveState) -> Result<(), String> {
    let encoded = serde_json::to_vec(state).map_err(|e| e.to_string())?;
    if encoded.len() > MAX_STATE_BYTES {
        return Err(format!(
            "Save State trop volumineux ({} octets)",
            encoded.len()
        ));
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("zst.tmp");
    fs::write(&tmp, encoded).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Loads a persisted Save State slot from disk if it exists.
pub fn load_state_slot_atomic(path: &Path) -> Result<Option<PersistedSaveState>, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let parsed: PersistedSaveState = serde_json::from_slice(&bytes)
                .map_err(|e| format!("Save State illisible ({}) : {e}", path.display()))?;
            Ok(Some(parsed))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

///Saves an imported ROM binary atomically (`<crc32>.rom.tmp` -> `<crc32>.rom`).
pub fn save_rom_binary_atomic(path: &Path, rom_bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("rom.tmp");
    fs::write(&tmp, rom_bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Structured hexadecimal inspection snapshot of a cartridge SRAM (`.srm`) or Memory Card (`.mcd`/`.mpk`) buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SramHexInspection {
    pub total_bytes: usize,
    pub non_zero_bytes: usize,
    pub crc32_hex: String,
    pub base_offset: usize,
    pub hex_rows: Vec<String>,
    pub score_slot_value: u8,
}

/// Formats `row_count` 8-byte hexadecimal + ASCII lines starting at `offset` from an SRAM/Memory Card buffer.
pub fn inspect_sram_hex(sram: &[u8], offset: usize, row_count: usize) -> SramHexInspection {
    let total_bytes = sram.len();
    let non_zero_bytes = sram.iter().filter(|&&b| b != 0).count();
    let crc32_hex = format_crc32_hex(compute_crc32(sram));
    let score_slot_value = sram.get(4).copied().unwrap_or(0);
    let clamped_offset = offset.min(total_bytes.saturating_sub(1)) & !0x07;
    let rows = row_count.clamp(1, 16);
    let mut hex_rows = Vec::with_capacity(rows);

    for r in 0..rows {
        let addr = clamped_offset + r * 8;
        if addr >= total_bytes && total_bytes > 0 {
            break;
        }
        let mut hex_part = String::with_capacity(24);
        let mut ascii_part = String::with_capacity(8);
        for i in 0..8 {
            if let Some(&b) = sram.get(addr + i) {
                hex_part.push_str(&format!("{b:02X} "));
                let ch = if (0x20..=0x7E).contains(&b) {
                    b as char
                } else {
                    '.'
                };
                ascii_part.push(ch);
            } else {
                hex_part.push_str("-- ");
                ascii_part.push(' ');
            }
        }
        hex_rows.push(format!(
            "0x{addr:04X}: {}|{}",
            hex_part.trim_end(),
            ascii_part
        ));
    }

    SramHexInspection {
        total_bytes,
        non_zero_bytes,
        crc32_hex,
        base_offset: clamped_offset,
        hex_rows,
        score_slot_value,
    }
}

/// Mutates a single byte inside an SRAM/Memory Card buffer and returns the updated `SramHexInspection`.
pub fn write_sram_byte_and_rehash(
    sram: &mut [u8],
    offset: usize,
    value: u8,
) -> Result<SramHexInspection, String> {
    if offset >= sram.len() {
        return Err(format!(
            "adresse SRAM hors limites (0x{offset:04X} >= {})",
            sram.len()
        ));
    }
    sram[offset] = value;
    Ok(inspect_sram_hex(sram, offset & !0x1F, 4))
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

        assert_eq!(load_sram_bytes(&path).unwrap().unwrap(), b"SAVE_V3");
        assert_eq!(
            fs::read(path.with_extension("srm.bak1")).unwrap(),
            b"SAVE_V2"
        );
        assert_eq!(
            fs::read(path.with_extension("srm.bak2")).unwrap(),
            b"SAVE_V1"
        );
    }

    #[test]
    fn persists_and_restores_save_state_slot_on_disk() {
        let path = temp_path("slot1", "zst");
        let state = PersistedSaveState {
            info: SaveStateSlotInfo {
                slot: 1,
                occupied: true,
                frame: 420,
                timestamp_ms: 1700000000,
                crc32_hex: "CAFEBABE".into(),
                compressed_bytes: 4,
                label: "Boss Final".into(),
            },
            uncompressed_len: 16,
            compressed_bytes: vec![1, 2, 3, 4],
        };
        save_state_slot_atomic(&path, &state).unwrap();
        let loaded = load_state_slot_atomic(&path).unwrap().unwrap();
        assert_eq!(loaded.info.label, "Boss Final");
        assert_eq!(loaded.compressed_bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn encodes_and_decodes_portable_pack_roundtrip() {
        let pack = ZaimulePortablePack {
            magic: PORTABLE_PACK_MAGIC.into(),
            version: "0.4.0".into(),
            active_game_id: "gb-pocket-arena".into(),
            sram_rle: vec![1, 2, 3],
            sram_uncompressed_len: 32_768,
            save_slots: vec![],
            cheat_codes: vec![],
            macros: vec![],
            keymapper_widgets: vec![],
            payload_crc32_hex: "DCE0682B".into(),
        };
        let bytes = encode_portable_pack(&pack).unwrap();
        let decoded = decode_portable_pack(&bytes).unwrap();
        assert_eq!(decoded.active_game_id, "gb-pocket-arena");
        assert_eq!(decoded.payload_crc32_hex, "DCE0682B");
    }

    #[test]
    fn sram_hex_inspector_formats_rows_and_writes_bytes_accurately() {
        let mut sram = vec![0u8; 64];
        sram[0..4].copy_from_slice(b"ZAIM");
        let updated = write_sram_byte_and_rehash(&mut sram, 4, 0x99).unwrap();
        assert_eq!(updated.score_slot_value, 0x99);
        assert_eq!(updated.non_zero_bytes, 5);
        assert!(updated.hex_rows[0].contains("5A 41 49 4D 99"));
    }

    #[test]
    fn persisted_library_roundtrips_catalog_and_directories() {
        use crate::catalog::{ConsoleKind, CoreEngineKind, SwitchEngineOverride};
        let path = temp_path("lib_roundtrip", "json");
        let lib = PersistedLibrary {
            active_game_id: "rom-platine".into(),
            rom_directories: vec!["C:/Users/Test/ROMs".into()],
            catalog: vec![GameEntry {
                id: "rom-platine".into(),
                title: "Pokémon Version Platine".into(),
                console: ConsoleKind::Nds,
                engine_kind: CoreEngineKind::NativeRust,
                core_id: "zaimule-rust-nds".into(),
                region: "EUR · FR".into(),
                crc32_hex: "AABBCCDD".into(),
                size_bytes: 134_217_728,
                sram_summary: "512 Ko Flash NDS".into(),
                playtime_seconds: 120,
                favorite: true,
                link_cable_ready: false,
                embedded_rom_key: None,
                cover_accent: "#1F2C38".into(),
                cover_symbol: "NDS".into(),
                rom_path: Some("C:/ROMs/PokemonPlatine.zip".into()),
                archive_entry: Some("Pokemon - Version Platine (FR).nds".into()),
                title_id: Some("CPUF".into()),
                fast_fingerprint: false,
                switch_engine: SwitchEngineOverride::Auto,
            }],
        };
        save_library_atomic(&path, &lib).unwrap();
        let (reloaded, warn) = load_library_atomic(&path).unwrap();
        assert!(warn.is_none());
        assert_eq!(reloaded, lib);
    }
}
