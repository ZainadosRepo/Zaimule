//! Cryptographic BIOS registry, live `system/` scanner, and `.zip`/`.7z`/`.bin` importer.

use crate::archive::{extract_archive_to_dir, list_7z_entries, list_zip_entries};
use crate::catalog::{compute_crc32, format_crc32_hex, ConsoleKind};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Official Nintendo DS ARM7 BIOS CRC32 (16 KiB).
pub const NDS_BIOS7_CRC32: &str = "1280F0D5";
/// Official Nintendo DS ARM9 BIOS CRC32 (4 KiB).
pub const NDS_BIOS9_CRC32: &str = "2AB23573";
/// Official Game Boy Advance BIOS CRC32 (16 KiB).
pub const GBA_BIOS_CRC32: &str = "81977335";
/// Official Game Boy DMG boot ROM CRC32 (256 B).
pub const DMG_BOOT_CRC32: &str = "59C8598E";

const DMG_KNOWN_CRC32S: &[&str] = &["59C8598E", "C2F5CC97", "E6920754"];
const NDS_FIRMWARE_VALID_SIZES: &[usize] = &[131_072, 262_144, 524_288];

/// Verification outcome for a console BIOS or firmware file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BiosStatus {
    VerifiedOfficial,
    CustomLoaded { actual_crc32: String },
    FreeBiosBuiltIn,
    ChecksumMismatch { actual_crc32: String },
    SizeMismatch { actual_bytes: usize },
}

/// Descriptor for a known firmware / BIOS image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BiosEntry {
    pub file_name: String,
    pub console: ConsoleKind,
    pub expected_bytes: usize,
    pub expected_crc32_hex: String,
    pub required_for_boot: bool,
    pub status: BiosStatus,
    #[serde(default)]
    pub actual_crc32_hex: Option<String>,
    #[serde(default)]
    pub installed_path: Option<String>,
    pub notes: String,
}

/// Summary returned after importing BIOS/firmware files or archives into `system_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BiosImportReport {
    pub imported: Vec<String>,
    pub verified: Vec<String>,
    pub ignored: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiosKind {
    DmgBoot,
    GbaBios,
    NdsBios7,
    NdsBios9,
    NdsFirmware,
}

impl BiosKind {
    pub const fn canonical_file_name(self) -> &'static str {
        match self {
            Self::DmgBoot => "dmg_boot.bin",
            Self::GbaBios => "gba_bios.bin",
            Self::NdsBios7 => "bios7.bin",
            Self::NdsBios9 => "bios9.bin",
            Self::NdsFirmware => "firmware.bin",
        }
    }

    pub const fn console(self) -> ConsoleKind {
        match self {
            Self::DmgBoot => ConsoleKind::Gb,
            Self::GbaBios => ConsoleKind::Gba,
            Self::NdsBios7 | Self::NdsBios9 | Self::NdsFirmware => ConsoleKind::Nds,
        }
    }

    pub const fn expected_bytes(self) -> usize {
        match self {
            Self::DmgBoot => 256,
            Self::GbaBios => 16_384,
            Self::NdsBios7 => 16_384,
            Self::NdsBios9 => 4_096,
            Self::NdsFirmware => 262_144,
        }
    }
}

/// Verifies a user-supplied BIOS byte slice against expected size and CRC32.
pub fn verify_bios_bytes(
    bytes: &[u8],
    expected_bytes: usize,
    expected_crc32_hex: &str,
) -> BiosStatus {
    if bytes.len() != expected_bytes {
        return BiosStatus::SizeMismatch {
            actual_bytes: bytes.len(),
        };
    }
    let actual_crc = format_crc32_hex(compute_crc32(bytes));
    if actual_crc.eq_ignore_ascii_case(expected_crc32_hex) {
        BiosStatus::VerifiedOfficial
    } else {
        BiosStatus::ChecksumMismatch {
            actual_crc32: actual_crc,
        }
    }
}

fn verify_bios_kind(kind: BiosKind, bytes: &[u8]) -> (BiosStatus, String) {
    let actual_crc = format_crc32_hex(compute_crc32(bytes));
    let status = match kind {
        BiosKind::NdsFirmware => {
            if NDS_FIRMWARE_VALID_SIZES.contains(&bytes.len()) {
                BiosStatus::CustomLoaded {
                    actual_crc32: actual_crc.clone(),
                }
            } else {
                BiosStatus::SizeMismatch {
                    actual_bytes: bytes.len(),
                }
            }
        }
        BiosKind::DmgBoot => {
            if bytes.len() != 256 {
                BiosStatus::SizeMismatch {
                    actual_bytes: bytes.len(),
                }
            } else if DMG_KNOWN_CRC32S
                .iter()
                .any(|c| c.eq_ignore_ascii_case(&actual_crc))
            {
                BiosStatus::VerifiedOfficial
            } else {
                BiosStatus::ChecksumMismatch {
                    actual_crc32: actual_crc.clone(),
                }
            }
        }
        BiosKind::GbaBios => verify_bios_bytes(bytes, 16_384, GBA_BIOS_CRC32),
        BiosKind::NdsBios7 => verify_bios_bytes(bytes, 16_384, NDS_BIOS7_CRC32),
        BiosKind::NdsBios9 => verify_bios_bytes(bytes, 4_096, NDS_BIOS9_CRC32),
    };
    (status, actual_crc)
}

/// Built-in open-source HLE/FreeBIOS metadata for supported native Rust consoles (`GB/GBC`, `GBA`, `NDS`).
pub fn default_bios_registry() -> Vec<BiosEntry> {
    vec![
        BiosEntry {
            file_name: "bios7.bin".into(),
            console: ConsoleKind::Nds,
            expected_bytes: 16_384,
            expected_crc32_hex: NDS_BIOS7_CRC32.into(),
            required_for_boot: false,
            status: BiosStatus::FreeBiosBuiltIn,
            actual_crc32_hex: None,
            installed_path: None,
            notes: "Optionnel : BIOS ARM7 NDS (fournit la table Blowfish KEY1 & tables audio/CRC natives ; fallback HLE intégré)."
                .into(),
        },
        BiosEntry {
            file_name: "bios9.bin".into(),
            console: ConsoleKind::Nds,
            expected_bytes: 4_096,
            expected_crc32_hex: NDS_BIOS9_CRC32.into(),
            required_for_boot: false,
            status: BiosStatus::FreeBiosBuiltIn,
            actual_crc32_hex: None,
            installed_path: None,
            notes: "Optionnel : BIOS ARM9 NDS (exposé en mémoire 0xFFFF0040..0xFFFF0FFF ; fallback HLE intégré)."
                .into(),
        },
        BiosEntry {
            file_name: "firmware.bin".into(),
            console: ConsoleKind::Nds,
            expected_bytes: 262_144,
            expected_crc32_hex: "USER-FW".into(),
            required_for_boot: false,
            status: BiosStatus::FreeBiosBuiltIn,
            actual_crc32_hex: None,
            installed_path: None,
            notes: "Optionnel : Firmware flash NDS (profil utilisateur, pseudo, langue, calibration tactile ; fallback synthétique intégré)."
                .into(),
        },
        BiosEntry {
            file_name: "gba_bios.bin".into(),
            console: ConsoleKind::Gba,
            expected_bytes: 16_384,
            expected_crc32_hex: GBA_BIOS_CRC32.into(),
            required_for_boot: false,
            status: BiosStatus::FreeBiosBuiltIn,
            actual_crc32_hex: None,
            installed_path: None,
            notes: "BIOS GBA HLE Rust intégré (termboy-gba 0.5.0, SWI + DMA complets).".into(),
        },
        BiosEntry {
            file_name: "dmg_boot.bin".into(),
            console: ConsoleKind::Gb,
            expected_bytes: 256,
            expected_crc32_hex: DMG_BOOT_CRC32.into(),
            required_for_boot: false,
            status: BiosStatus::FreeBiosBuiltIn,
            actual_crc32_hex: None,
            installed_path: None,
            notes: "Séquence de démarrage DMG/CGB Rust intégrée (termboy-gb 0.5.0).".into(),
        },
    ]
}

/// Scans `system_dir` for user-installed BIOS/firmware files and returns live verification status.
pub fn scan_bios_registry(system_dir: &Path) -> Vec<BiosEntry> {
    let specs = [
        (BiosKind::NdsBios7, 0usize),
        (BiosKind::NdsBios9, 1usize),
        (BiosKind::NdsFirmware, 2usize),
        (BiosKind::GbaBios, 3usize),
        (BiosKind::DmgBoot, 4usize),
    ];
    let mut entries = default_bios_registry();
    for (kind, idx) in specs {
        let candidate = system_dir.join(kind.canonical_file_name());
        if let Ok(bytes) = fs::read(&candidate) {
            let (status, actual_crc) = verify_bios_kind(kind, &bytes);
            let entry = &mut entries[idx];
            entry.actual_crc32_hex = Some(actual_crc.clone());
            entry.installed_path = Some(candidate.to_string_lossy().to_string());
            entry.notes = match (&kind, &status) {
                (BiosKind::NdsBios7, BiosStatus::VerifiedOfficial) => {
                    "BIOS ARM7 NDS officiel chargé (table Blowfish KEY1 & tables ARM7 natives actives)."
                        .into()
                }
                (BiosKind::NdsBios9, BiosStatus::VerifiedOfficial) => {
                    "BIOS ARM9 NDS officiel chargé (mappé sur 0xFFFF0040..0xFFFF0FFF)."
                        .into()
                }
                (BiosKind::NdsFirmware, BiosStatus::CustomLoaded { .. }) => format!(
                    "Firmware NDS utilisateur chargé ({} Kio, CRC32 {actual_crc}) — paramètres utilisateur & calibration tactile actifs.",
                    bytes.len() / 1024
                ),
                (BiosKind::GbaBios, BiosStatus::VerifiedOfficial) => {
                    "BIOS GBA officiel vérifié et archivé dans system/ (le cœur GBA utilise son HLE natif haute compatibilité)."
                        .into()
                }
                (BiosKind::DmgBoot, BiosStatus::VerifiedOfficial) => {
                    "Boot ROM DMG officielle vérifiée et archivée dans system/.".into()
                }
                (_, BiosStatus::ChecksumMismatch { actual_crc32 }) => format!(
                    "Fichier présent dans system/ mais CRC32 {actual_crc32} différent de la référence {} (chargé avec prudence).",
                    entry.expected_crc32_hex
                ),
                (_, BiosStatus::SizeMismatch { actual_bytes }) => format!(
                    "Taille invalide ({actual_bytes} octets au lieu de {} attendus) — ignoré au profit du fallback HLE.",
                    entry.expected_bytes
                ),
                _ => entry.notes.clone(),
            };
            entry.status = status;
        }
    }
    entries
}

fn is_bios_name_hint(name_lower: &str) -> Option<BiosKind> {
    if name_lower.contains("dsi") {
        return None;
    }
    if name_lower == "bios7.bin"
        || name_lower.contains("bios7")
        || name_lower.contains("biosnds7")
        || name_lower.contains("arm7_bios")
        || name_lower == "arm7.bin"
    {
        return Some(BiosKind::NdsBios7);
    }
    if name_lower == "bios9.bin"
        || name_lower.contains("bios9")
        || name_lower.contains("biosnds9")
        || name_lower.contains("arm9_bios")
        || name_lower == "arm9.bin"
    {
        return Some(BiosKind::NdsBios9);
    }
    if name_lower == "firmware.bin"
        || name_lower.contains("nds_firmware")
        || name_lower.contains("ndsfirmware")
        || name_lower.contains("firmware")
    {
        return Some(BiosKind::NdsFirmware);
    }
    if name_lower == "gba_bios.bin"
        || name_lower.contains("gba_bios")
        || name_lower.contains("gbabios")
        || name_lower == "gba.bios"
    {
        return Some(BiosKind::GbaBios);
    }
    if name_lower == "dmg_boot.bin"
        || name_lower == "dmg_rom.bin"
        || name_lower == "gb_bios.bin"
        || name_lower.contains("dmg_boot")
    {
        return Some(BiosKind::DmgBoot);
    }
    None
}

/// Classifies a candidate BIOS/firmware binary by exact official CRC32 first, then filename + valid byte size.
pub fn classify_bios_bytes(file_name: &str, bytes: &[u8]) -> Option<BiosKind> {
    let base = file_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_name)
        .to_ascii_lowercase();
    let crc = format_crc32_hex(compute_crc32(bytes));

    if bytes.len() == 16_384 && crc.eq_ignore_ascii_case(NDS_BIOS7_CRC32) {
        return Some(BiosKind::NdsBios7);
    }
    if bytes.len() == 4_096 && crc.eq_ignore_ascii_case(NDS_BIOS9_CRC32) {
        return Some(BiosKind::NdsBios9);
    }
    if bytes.len() == 16_384 && crc.eq_ignore_ascii_case(GBA_BIOS_CRC32) {
        return Some(BiosKind::GbaBios);
    }
    if bytes.len() == 256
        && DMG_KNOWN_CRC32S
            .iter()
            .any(|expected| expected.eq_ignore_ascii_case(&crc))
    {
        return Some(BiosKind::DmgBoot);
    }

    let hint = is_bios_name_hint(&base)?;
    let size_ok = match hint {
        BiosKind::NdsBios7 | BiosKind::GbaBios => bytes.len() == 16_384,
        BiosKind::NdsBios9 => bytes.len() == 4_096,
        BiosKind::DmgBoot => bytes.len() == 256,
        BiosKind::NdsFirmware => NDS_FIRMWARE_VALID_SIZES.contains(&bytes.len()),
    };
    if size_ok {
        Some(hint)
    } else {
        None
    }
}

fn has_bios_binary_extension(base: &str) -> bool {
    base.ends_with(".bin") || base.ends_with(".rom") || base.ends_with(".bios")
}

/// Quick path check used by drag-and-drop routing to detect BIOS/firmware files or BIOS-only archives.
pub fn looks_like_bios_path(path: &Path) -> bool {
    let Some(name) = path
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
    else {
        return false;
    };
    if has_bios_binary_extension(&name) {
        if is_bios_name_hint(&name).is_some() {
            return true;
        }
        if let Ok(meta) = fs::metadata(path) {
            let sz = meta.len() as usize;
            if matches!(sz, 256 | 4_096 | 16_384) {
                if let Ok(bytes) = fs::read(path) {
                    return classify_bios_bytes(&name, &bytes).is_some();
                }
            }
        }
        return false;
    }
    if name.ends_with(".rar") && (name.contains("bios") || name.contains("firmware")) {
        return true;
    }
    if name.ends_with(".zip") {
        if let Ok(entries) = list_zip_entries(path) {
            return !entries.is_empty()
                && entries.iter().any(|e| {
                    let base = e
                        .name
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(&e.name)
                        .to_ascii_lowercase();
                    has_bios_binary_extension(&base) && is_bios_name_hint(&base).is_some()
                });
        }
    }
    if name.ends_with(".7z") {
        if let Ok(entries) = list_7z_entries(path) {
            return !entries.is_empty()
                && entries.iter().any(|e| {
                    let base = e
                        .name
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(&e.name)
                        .to_ascii_lowercase();
                    has_bios_binary_extension(&base) && is_bios_name_hint(&base).is_some()
                });
        }
    }
    false
}

fn write_atomic_bytes(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let tmp = dest.with_extension("bios.tmp");
    fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", dest.display())
    })
}

fn collect_files_recursive(root: &Path, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        out.push(root.to_path_buf());
        return;
    }
    if let Ok(read_dir) = fs::read_dir(root) {
        for entry in read_dir.flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect_files_recursive(&p, out);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
}

/// Imports user BIOS/firmware files from loose `.bin`/`.rom` files, folders, `.zip`, or `.7z` archives
/// into `system_dir` using canonical names (`bios7.bin`, `bios9.bin`, `firmware.bin`, `gba_bios.bin`, `dmg_boot.bin`).
pub fn import_bios_from_paths(
    paths: &[PathBuf],
    system_dir: &Path,
) -> Result<BiosImportReport, String> {
    fs::create_dir_all(system_dir).map_err(|e| format!("{}: {e}", system_dir.display()))?;
    let mut report = BiosImportReport::default();
    let mut candidate_files: Vec<PathBuf> = Vec::new();
    let mut temp_dirs: Vec<PathBuf> = Vec::new();

    for path in paths {
        if !path.exists() {
            report
                .warnings
                .push(format!("Chemin introuvable : {}", path.display()));
            continue;
        }
        if path.is_dir() {
            collect_files_recursive(path, &mut candidate_files);
            continue;
        }
        let lower = path
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if lower.ends_with(".rar") {
            report.warnings.push(format!(
                "Archive .rar non supportée nativement ({}) — importez le fichier .zip ou extrayez bios7.bin, bios9.bin et firmware.bin.",
                path.file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string())
            ));
            continue;
        }
        if lower.ends_with(".zip") || lower.ends_with(".7z") {
            let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let tmp_dir = std::env::temp_dir()
                .join(format!("zaimule-bios-import-{}-{seq}", std::process::id()));
            match extract_archive_to_dir(path, &tmp_dir) {
                Ok(extracted) => {
                    candidate_files.extend(extracted);
                    temp_dirs.push(tmp_dir);
                }
                Err(err) => {
                    let _ = fs::remove_dir_all(&tmp_dir);
                    report
                        .warnings
                        .push(format!("Impossible d'extraire {} : {err}", path.display()));
                }
            }
            continue;
        }
        candidate_files.push(path.clone());
    }

    for file_path in candidate_files {
        let file_name = file_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let meta = match fs::metadata(&file_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Skip clearly oversized files (ROMs, ISOs, etc.) without reading them into RAM.
        if meta.len() == 0 || meta.len() > 1024 * 1024 {
            report.ignored.push(file_name);
            continue;
        }
        let bytes = match fs::read(&file_path) {
            Ok(b) => b,
            Err(err) => {
                report
                    .warnings
                    .push(format!("Lecture impossible de {file_name} : {err}"));
                continue;
            }
        };
        let Some(kind) = classify_bios_bytes(&file_name, &bytes) else {
            report.ignored.push(file_name);
            continue;
        };
        let canonical = kind.canonical_file_name().to_string();
        let dest = system_dir.join(&canonical);
        write_atomic_bytes(&dest, &bytes)?;
        let (status, _) = verify_bios_kind(kind, &bytes);
        if !report.imported.contains(&canonical) {
            report.imported.push(canonical.clone());
        }
        if matches!(
            status,
            BiosStatus::VerifiedOfficial | BiosStatus::CustomLoaded { .. }
        ) && !report.verified.contains(&canonical)
        {
            report.verified.push(canonical);
        }
    }

    for tmp in temp_dirs {
        let _ = fs::remove_dir_all(tmp);
    }

    if report.imported.is_empty() {
        if !report.warnings.is_empty() {
            return Err(report.warnings.join(" | "));
        }
        return Err(
            "Aucun BIOS ou firmware compatible détecté (attendus : bios7.bin 16 Kio, bios9.bin 4 Kio, firmware.bin 128/256/512 Kio, gba_bios.bin 16 Kio, dmg_boot.bin 256 o)."
                .into(),
        );
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::build_stored_zip_for_test;

    #[test]
    fn verifies_bios_size_and_crc32() {
        let sample = b"123456789";
        assert_eq!(
            verify_bios_bytes(sample, 9, "CBF43926"),
            BiosStatus::VerifiedOfficial
        );
        assert!(matches!(
            verify_bios_bytes(sample, 256, "CBF43926"),
            BiosStatus::SizeMismatch { actual_bytes: 9 }
        ));
    }

    #[test]
    fn imports_nds_bios_and_firmware_from_zip_and_scans_system_dir() {
        let dir = std::env::temp_dir().join(format!(
            "zaimule-bios-test-{}",
            TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        let sys_dir = dir.join("system");
        fs::create_dir_all(&sys_dir).unwrap();

        let bios7 = vec![0x77u8; 16_384];
        let bios9 = vec![0x99u8; 4_096];
        let firmware = vec![0xA5u8; 262_144];
        let zip_bytes = build_stored_zip_for_test(&[
            ("NDS BIOS & Firmware/bios7.bin", bios7.as_slice()),
            ("NDS BIOS & Firmware/bios9.bin", bios9.as_slice()),
            ("NDS BIOS & Firmware/firmware.bin", firmware.as_slice()),
            ("NDS BIOS & Firmware/readme.txt", b"notes"),
        ]);
        let zip_path = dir.join("NDS BIOS & Firmware.zip");
        fs::write(&zip_path, &zip_bytes).unwrap();

        assert!(looks_like_bios_path(&zip_path));

        let report = import_bios_from_paths(&[zip_path], &sys_dir).unwrap();
        assert!(report.imported.contains(&"bios7.bin".to_string()));
        assert!(report.imported.contains(&"bios9.bin".to_string()));
        assert!(report.imported.contains(&"firmware.bin".to_string()));
        assert!(report.verified.contains(&"firmware.bin".to_string()));
        assert!(sys_dir.join("bios7.bin").is_file());
        assert!(sys_dir.join("bios9.bin").is_file());
        assert!(sys_dir.join("firmware.bin").is_file());

        let entries = scan_bios_registry(&sys_dir);
        let fw_entry = entries
            .iter()
            .find(|e| e.file_name == "firmware.bin")
            .unwrap();
        assert!(matches!(fw_entry.status, BiosStatus::CustomLoaded { .. }));
        assert!(fw_entry.installed_path.is_some());

        let b7_entry = entries.iter().find(|e| e.file_name == "bios7.bin").unwrap();
        assert!(matches!(
            b7_entry.status,
            BiosStatus::ChecksumMismatch { .. }
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_rar_archive_with_helpful_french_message() {
        let dir = std::env::temp_dir().join(format!(
            "zaimule-bios-rar-test-{}",
            TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let rar_path = dir.join("Nintendo DS Bios and Firmware.rar");
        fs::write(&rar_path, b"Rar!fake").unwrap();

        let err = import_bios_from_paths(&[rar_path], &dir.join("sys")).unwrap_err();
        assert!(err.contains(".rar"));
        assert!(err.contains(".zip"));

        let _ = fs::remove_dir_all(&dir);
    }
}
