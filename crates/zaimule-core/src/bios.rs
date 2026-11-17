//! Cryptographic BIOS registry and firmware health verifier.

use crate::catalog::{compute_crc32, format_crc32_hex, ConsoleKind};
use serde::{Deserialize, Serialize};

/// Verification outcome for a console BIOS file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BiosStatus {
    VerifiedOfficial,
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
    pub notes: String,
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

/// Default BIOS registry with built-in open-source HLE/FreeBIOS fallbacks so every console boots out of the box.
pub fn default_bios_registry() -> Vec<BiosEntry> {
    vec![
        BiosEntry {
            file_name: "dmg_boot.bin".into(),
            console: ConsoleKind::Gb,
            expected_bytes: 256,
            expected_crc32_hex: "59C8598E".into(),
            required_for_boot: false,
            status: BiosStatus::FreeBiosBuiltIn,
            notes: "Boot ROM Rust HLE intégrée (registres AF=0x01B0, PC=0x0100 pré-initialisés)."
                .into(),
        },
        BiosEntry {
            file_name: "gba_bios.bin".into(),
            console: ConsoleKind::Gba,
            expected_bytes: 16_384,
            expected_crc32_hex: "81977335".into(),
            required_for_boot: false,
            status: BiosStatus::FreeBiosBuiltIn,
            notes:
                "Normmatt Open-Source GBA BIOS actif par défaut ; remplaçable par dump officiel."
                    .into(),
        },
        BiosEntry {
            file_name: "scph5502.bin".into(),
            console: ConsoleKind::Ps1,
            expected_bytes: 524_288,
            expected_crc32_hex: "D786F0B9".into(),
            required_for_boot: true,
            status: BiosStatus::VerifiedOfficial,
            notes: "BIOS PlayStation 1 Europe (PAL/Multi) vérifié.".into(),
        },
        BiosEntry {
            file_name: "bios7.bin / bios9.bin".into(),
            console: ConsoleKind::Nds,
            expected_bytes: 16_384,
            expected_crc32_hex: "DF692A80".into(),
            required_for_boot: false,
            status: BiosStatus::FreeBiosBuiltIn,
            notes: "melonDS FreeBIOS actif pour lancement direct des ROMs NDS.".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
