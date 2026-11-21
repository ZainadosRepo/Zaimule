//! 60-second circular rewind ring-buffer with fast RLE state compression
//! and atomic Save State slot metadata.

use crate::catalog::compute_crc32;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

const RLE_MARKER: u8 = 0xFE;

/// Metadata for a user-visible Save State slot (`slot_1.zst` .. `slot_9.zst`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveStateSlotInfo {
    pub slot: u8,
    pub occupied: bool,
    pub frame: u64,
    pub timestamp_ms: u64,
    pub crc32_hex: String,
    pub compressed_bytes: usize,
    pub label: String,
}

/// A single compressed snapshot stored in the circular rewind buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindFrameEntry {
    pub frame: u64,
    pub uncompressed_len: usize,
    pub crc32: u32,
    pub already_compressed: bool,
    pub compressed_payload: Vec<u8>,
}

/// Circular rewind buffer holding compressed emulator snapshots over a configurable time horizon.
#[derive(Debug, Clone)]
pub struct RewindBuffer {
    max_entries: usize,
    entries: VecDeque<RewindFrameEntry>,
}

impl RewindBuffer {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(4),
            entries: VecDeque::with_capacity(max_entries.max(4)),
        }
    }

    /// Captures and compresses a raw emulator state snapshot at `frame`.
    /// Payloads already carrying their own RLE+CRC32 container (`"ZMND"`) are stored directly
    /// to avoid double-compressing multi-megabyte NDS savestates.
    pub fn push_snapshot(&mut self, frame: u64, raw_state: &[u8]) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        let already_compressed = raw_state.starts_with(b"ZMND");
        let compressed = if already_compressed {
            raw_state.to_vec()
        } else {
            rle_compress(raw_state)
        };
        let crc = compute_crc32(raw_state);
        self.entries.push_back(RewindFrameEntry {
            frame,
            uncompressed_len: raw_state.len(),
            crc32: crc,
            already_compressed,
            compressed_payload: compressed,
        });
    }

    /// Owned counterpart of [`Self::push_snapshot`] that moves already-compressed `"ZMND"`
    /// containers without an extra allocation or copy.
    pub fn push_snapshot_owned(&mut self, frame: u64, raw_state: Vec<u8>) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        let already_compressed = raw_state.starts_with(b"ZMND");
        let uncompressed_len = raw_state.len();
        let crc = compute_crc32(&raw_state);
        let compressed = if already_compressed {
            raw_state
        } else {
            rle_compress(&raw_state)
        };
        self.entries.push_back(RewindFrameEntry {
            frame,
            uncompressed_len,
            crc32: crc,
            already_compressed,
            compressed_payload: compressed,
        });
    }

    /// Pops and decompresses the state `steps_back` snapshots in the past, verifying its CRC32.
    pub fn rewind_steps(&mut self, steps_back: usize) -> Result<Option<(u64, Vec<u8>)>, String> {
        if self.entries.is_empty() {
            return Ok(None);
        }
        let drop_count = steps_back.min(self.entries.len().saturating_sub(1));
        for _ in 0..drop_count {
            self.entries.pop_back();
        }
        let Some(target) = self.entries.back() else {
            return Ok(None);
        };
        let raw = if target.already_compressed {
            target.compressed_payload.clone()
        } else {
            rle_decompress(&target.compressed_payload, target.uncompressed_len)?
        };
        if compute_crc32(&raw) != target.crc32 {
            return Err("intégrité du snapshot de rewind compromise (CRC32 invalide)".into());
        }
        Ok(Some((target.frame, raw)))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Total RAM footprint used by all compressed snapshots in bytes.
    pub fn memory_usage_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|e| e.compressed_payload.len() + 32)
            .sum()
    }

    pub fn oldest_and_newest_frame(&self) -> Option<(u64, u64)> {
        let first = self.entries.front()?.frame;
        let last = self.entries.back()?.frame;
        Some((first, last))
    }
}

/// Fast byte-aligned Run-Length Encoding tuned for sparse emulator RAM/VRAM buffers.
pub fn rle_compress(input: &[u8]) -> Vec<u8> {
    let len = input.len();
    let mut out = Vec::with_capacity(len / 2 + 16);
    let mut i = 0;
    while i < len {
        let byte = input[i];
        let max_end = (i + 255).min(len);
        let mut end = i + 1;
        if end + 2 < max_end && input[end] == byte && input[end + 1] == byte {
            end += 2;
            let splat = u64::from_ne_bytes([byte; 8]);
            while end + 8 <= max_end {
                let word = u64::from_ne_bytes(input[end..end + 8].try_into().unwrap());
                if word != splat {
                    break;
                }
                end += 8;
            }
            while end < max_end && input[end] == byte {
                end += 1;
            }
        } else if byte == RLE_MARKER {
            while end < max_end && input[end] == byte {
                end += 1;
            }
        }
        let run = end - i;
        if run >= 4 || byte == RLE_MARKER {
            out.push(RLE_MARKER);
            out.push(run as u8);
            out.push(byte);
            i = end;
        } else {
            out.push(byte);
            i += 1;
        }
    }
    out
}

/// Maximum uncompressed payload allowed when decoding a Save State or Rewind snapshot (64 MiB).
pub const MAX_RLE_UNCOMPRESSED_BYTES: usize = 64 * 1024 * 1024;

/// Decompresses a buffer produced by `rle_compress`, rejecting oversized or overflowing payloads.
pub fn rle_decompress(compressed: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    if expected_len > MAX_RLE_UNCOMPRESSED_BYTES {
        return Err(format!(
            "Save State trop volumineux ({expected_len} octets, max {MAX_RLE_UNCOMPRESSED_BYTES})"
        ));
    }
    let cap = if expected_len > 0 {
        expected_len.min(MAX_RLE_UNCOMPRESSED_BYTES)
    } else {
        compressed.len().min(MAX_RLE_UNCOMPRESSED_BYTES)
    };
    let max_len = if expected_len > 0 {
        expected_len
    } else {
        MAX_RLE_UNCOMPRESSED_BYTES
    };
    let mut out = Vec::with_capacity(cap);
    let mut i = 0;
    while i < compressed.len() {
        let b = compressed[i];
        if b == RLE_MARKER {
            if i + 2 >= compressed.len() {
                return Err("flux RLE tronqué dans le Save State".into());
            }
            let count = compressed[i + 1] as usize;
            let val = compressed[i + 2];
            if out.len().saturating_add(count) > max_len {
                return Err(format!(
                    "dépassement RLE dans le Save State (limite {max_len} octets)"
                ));
            }
            out.extend(std::iter::repeat_n(val, count));
            i += 3;
        } else {
            if out.len() >= max_len {
                return Err(format!(
                    "dépassement RLE dans le Save State (limite {max_len} octets)"
                ));
            }
            out.push(b);
            i += 1;
        }
    }
    if expected_len > 0 && out.len() != expected_len {
        return Err(format!(
            "taille décompressée inattendue ({} octets au lieu de {})",
            out.len(),
            expected_len
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_roundtrip_compresses_sparse_ram_and_restores_exact_bytes() {
        let mut fake_ram = vec![0u8; 4096];
        fake_ram[120..126].copy_from_slice(&[0xFE, 0xFE, 0x42, 0x99, 0x10, 0xFE]);
        let compressed = rle_compress(&fake_ram);
        assert!(compressed.len() < 200);
        let restored = rle_decompress(&compressed, fake_ram.len()).unwrap();
        assert_eq!(restored, fake_ram);
    }

    #[test]
    fn rle_decompress_rejects_oversized_expected_len_and_stream_overflow() {
        let err = rle_decompress(&[0x01, 0x02], MAX_RLE_UNCOMPRESSED_BYTES + 1).unwrap_err();
        assert!(err.contains("trop volumineux"));

        // Encoded run of 200 zero bytes, but expected_len claims only 16 bytes.
        let bomb = [RLE_MARKER, 200, 0x00];
        let err = rle_decompress(&bomb, 16).unwrap_err();
        assert!(err.contains("dépassement RLE"));
    }

    #[test]
    fn rewind_buffer_steps_back_and_verifies_crc32() {
        let mut buf = RewindBuffer::new(5);
        buf.push_snapshot(60, &[1, 1, 1, 1]);
        buf.push_snapshot(120, &[2, 2, 2, 2]);
        buf.push_snapshot(180, &[3, 3, 3, 3]);

        let (frame, state) = buf.rewind_steps(1).unwrap().unwrap();
        assert_eq!(frame, 120);
        assert_eq!(state, vec![2, 2, 2, 2]);
    }
}
