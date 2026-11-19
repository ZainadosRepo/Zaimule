//! Non-destructive in-memory IPS ROM patcher (Yuzu LayeredFS style) and RAM cheat codes.
//! The original ROM file on disk is never modified; patches are applied on the fly in RAM.

use serde::{Deserialize, Serialize};

const IPS_MAGIC: &[u8] = b"PATCH";
const IPS_EOF: &[u8] = b"EOF";
const MAX_PAT_ROM_BYTES: usize = 64 * 1024 * 1024;

/// Metadata for a virtual ROM patch or mod layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModLayerEntry {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub enabled: bool,
    pub description: String,
}

/// Applies an official IPS binary patch (`PATCH` ... `EOF`) onto a copy of `base_rom` in memory.
pub fn apply_ips_patch(base_rom: &[u8], ips_patch: &[u8]) -> Result<Vec<u8>, String> {
    if ips_patch.len() < 8 || &ips_patch[0..5] != IPS_MAGIC {
        return Err("en-tête IPS invalide (attendu 'PATCH')".into());
    }

    let mut out = base_rom.to_vec();
    let mut cursor = 5usize;

    while cursor < ips_patch.len() {
        if cursor + 3 <= ips_patch.len() && &ips_patch[cursor..cursor + 3] == IPS_EOF {
            return Ok(out);
        }
        if cursor + 5 > ips_patch.len() {
            return Err("patch IPS tronqué avant marqueur EOF".into());
        }

        let offset = ((ips_patch[cursor] as usize) << 16)
            | ((ips_patch[cursor + 1] as usize) << 8)
            | (ips_patch[cursor + 2] as usize);
        let size = ((ips_patch[cursor + 3] as usize) << 8) | (ips_patch[cursor + 4] as usize);
        cursor += 5;

        if size == 0 {
            // RLE hunk: 2-byte run length + 1-byte fill value
            if cursor + 3 > ips_patch.len() {
                return Err("bloc RLE IPS tronqué".into());
            }
            let rle_len = ((ips_patch[cursor] as usize) << 8) | (ips_patch[cursor + 1] as usize);
            let val = ips_patch[cursor + 2];
            cursor += 3;

            let end = offset.saturating_add(rle_len);
            if end > MAX_PAT_ROM_BYTES {
                return Err("patch IPS dépasse la taille maximale autorisée (64 Mo)".into());
            }
            if end > out.len() {
                out.resize(end, 0);
            }
            out[offset..end].fill(val);
        } else {
            // Standard payload hunk
            if cursor + size > ips_patch.len() {
                return Err("bloc de données IPS tronqué".into());
            }
            let end = offset.saturating_add(size);
            if end > MAX_PAT_ROM_BYTES {
                return Err("patch IPS dépasse la taille maximale autorisée (64 Mo)".into());
            }
            if end > out.len() {
                out.resize(end, 0);
            }
            out[offset..end].copy_from_slice(&ips_patch[cursor..cursor + size]);
            cursor += size;
        }
    }

    Err("marqueur 'EOF' manquant à la fin du patch IPS".into())
}

/// Builds a synthetic IPS patch that writes `payload` at `offset` (useful for testing and quick hot-patches).
pub fn build_ips_patch(offset: u32, payload: &[u8]) -> Result<Vec<u8>, String> {
    if offset > 0x00FF_FFFF {
        return Err("l'offset IPS dépasse la limite 24 bits (16 Mo)".into());
    }
    if payload.is_empty() || payload.len() > 0xFFFF {
        return Err("la taille du payload IPS doit être comprise entre 1 et 65535 octets".into());
    }
    let mut patch = Vec::with_capacity(8 + 5 + payload.len());
    patch.extend_from_slice(IPS_MAGIC);
    patch.push(((offset >> 16) & 0xFF) as u8);
    patch.push(((offset >> 8) & 0xFF) as u8);
    patch.push((offset & 0xFF) as u8);
    let len = payload.len() as u16;
    patch.push((len >> 8) as u8);
    patch.push((len & 0xFF) as u8);
    patch.extend_from_slice(payload);
    patch.extend_from_slice(IPS_EOF);
    Ok(patch)
}

/// Diffs `original` and `modified` ROM buffers and generates a standards-compliant IPS binary patch (`PATCH` ... `EOF`).
pub fn diff_roms_to_ips(original: &[u8], modified: &[u8]) -> Result<Vec<u8>, String> {
    if modified.is_empty() {
        return Err("le tampon ROM modifié est vide".into());
    }
    let limit = modified.len().min(0x00FF_FFFF);
    let mut patch = Vec::with_capacity(64);
    patch.extend_from_slice(IPS_MAGIC);

    let mut i = 0usize;
    while i < limit {
        let orig_b = original.get(i).copied();
        if orig_b == Some(modified[i]) {
            i += 1;
            continue;
        }
        let mut offset = i;
        if offset == 0x0045_4F46 && offset > 0 {
            // Avoid 0x454F46 ("EOF") magic collision per IPS specification
            offset -= 1;
        }
        let mut end = i + 1;
        while end < limit && (end - offset) < 0xFFFF {
            if original.get(end).copied() == Some(modified[end]) {
                break;
            }
            end += 1;
        }
        let hunk_len = (end - offset) as u16;
        patch.push(((offset >> 16) & 0xFF) as u8);
        patch.push(((offset >> 8) & 0xFF) as u8);
        patch.push((offset & 0xFF) as u8);
        patch.push((hunk_len >> 8) as u8);
        patch.push((hunk_len & 0xFF) as u8);
        patch.extend_from_slice(&modified[offset..end]);
        i = end;
    }

    patch.extend_from_slice(IPS_EOF);
    Ok(patch)
}

/// Starts empty so only user-imported `.ips` patches are tracked.
pub fn default_mod_layers() -> Vec<ModLayerEntry> {
    Vec::new()
}

/// Curated Community Mod Hub entry with an embedded standards-compliant `.IPS` binary patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityModPackage {
    pub id: String,
    pub title: String,
    pub author: String,
    pub target_console: String,
    pub category: String,
    pub downloads: u32,
    pub description: String,
    pub installed: bool,
    pub ips_bytes: Vec<u8>,
}

/// Starts empty with no pre-installed fake community mods.
pub fn default_community_mods() -> Vec<CommunityModPackage> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_standard_and_rle_ips_hunks_without_mutating_source() {
        let original = b"HELLO_RETRO_WORLD".to_vec();
        let patch = build_ips_patch(6, b"ZAIMULE").unwrap();
        let patched = apply_ips_patch(&original, &patch).unwrap();
        assert_eq!(&original, b"HELLO_RETRO_WORLD");
        assert_eq!(&patched, b"HELLO_ZAIMULEORLD");
    }

    #[test]
    fn diffs_two_rom_buffers_into_valid_ips_patch_that_roundtrips() {
        let orig = b"POKEMON_EMERALD_ORIGINAL_ROM".to_vec();
        let mut modified = orig.clone();
        modified[8..15].copy_from_slice(b"ZAIMULE");
        let ips = diff_roms_to_ips(&orig, &modified).unwrap();
        let rebuilt = apply_ips_patch(&orig, &ips).unwrap();
        assert_eq!(rebuilt, modified);
    }

    #[test]
    fn mod_defaults_start_clean_and_rle_hunk_applies() {
        assert!(default_mod_layers().is_empty());
        assert!(default_community_mods().is_empty());
        let mut ips = Vec::new();
        ips.extend_from_slice(IPS_MAGIC);
        ips.extend_from_slice(&[0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x06, 0xAA]);
        ips.extend_from_slice(IPS_EOF);
        let patched = apply_ips_patch(&[0u8; 16], &ips).unwrap();
        assert_eq!(&patched[4..10], &[0xAA; 6]);
    }

    #[test]
    fn rejects_invalid_ips_header() {
        assert!(apply_ips_patch(b"ABCD", b"NOT_IPS_EOF").is_err());
    }
}
