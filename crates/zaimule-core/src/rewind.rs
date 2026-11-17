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
    pub fn push_snapshot(&mut self, frame: u64, raw_state: &[u8]) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        let compressed = rle_compress(raw_state);
        let crc = compute_crc32(raw_state);
        self.entries.push_back(RewindFrameEntry {
            frame,
            uncompressed_len: raw_state.len(),
            crc32: crc,
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
        let raw = rle_decompress(&target.compressed_payload, target.uncompressed_len)?;
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
    let mut out = Vec::with_capacity(input.len() / 2 + 16);
    let mut i = 0;
    while i < input.len() {
        let byte = input[i];
        let mut run = 1usize;
        while i + run < input.len() && input[i + run] == byte && run < 255 {
            run += 1;
        }
        if run >= 4 || byte == RLE_MARKER {
            out.push(RLE_MARKER);
            out.push(run as u8);
            out.push(byte);
            i += run;
        } else {
            out.push(byte);
            i += 1;
        }
    }
    out
}

/// Decompresses a buffer produced by `rle_compress`.
pub fn rle_decompress(compressed: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(expected_len);
    let mut i = 0;
    while i < compressed.len() {
        let b = compressed[i];
        if b == RLE_MARKER {
            if i + 2 >= compressed.len() {
                return Err("flux RLE tronqué dans le Save State".into());
            }
            let count = compressed[i + 1] as usize;
            let val = compressed[i + 2];
            out.extend(std::iter::repeat_n(val, count));
            i += 3;
        } else {
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
