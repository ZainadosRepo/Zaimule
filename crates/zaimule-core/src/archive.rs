//! Pure-Rust streaming ZIP & ZIP64 archive reader/extractor backed by `flate2` and `crc32fast`.
//! Enables zero-RAM inspection of RomStation `.zip` archives (e.g. `.nds` / `.gba` / `.sfc`)
//! and extraction of official Libretro core `.dll.zip` / `.dylib.zip` / `.so.zip` downloads.

use flate2::read::DeflateDecoder;
use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;

const EOCD_SIG: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
const ZIP64_LOCATOR_SIG: [u8; 4] = [0x50, 0x4B, 0x06, 0x07];
const ZIP64_EOCD_SIG: [u8; 4] = [0x50, 0x4B, 0x06, 0x06];
const CD_FILE_SIG: [u8; 4] = [0x50, 0x4B, 0x01, 0x02];
const LOCAL_FILE_SIG: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];

/// Central directory metadata for a single file entry inside a ZIP archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntryMeta {
    pub name: String,
    pub compression_method: u16,
    pub crc32: u32,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
}

fn read_u16_le(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

/// Lists all non-directory entries inside any seekable ZIP / ZIP64 stream.
pub fn list_zip_entries_from_reader<R: Read + Seek>(
    reader: &mut R,
) -> Result<Vec<ZipEntryMeta>, String> {
    let total_len = reader.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    if total_len < 22 {
        return Err("archive ZIP invalide (fichier trop court)".into());
    }

    let tail_len = total_len.min(65_557) as usize;
    let tail_start = total_len - tail_len as u64;
    reader
        .seek(SeekFrom::Start(tail_start))
        .map_err(|e| e.to_string())?;
    let mut tail = vec![0u8; tail_len];
    reader.read_exact(&mut tail).map_err(|e| e.to_string())?;

    let mut eocd_rel_pos = None;
    let mut i = tail_len.saturating_sub(22);
    loop {
        if tail[i..i + 4] == EOCD_SIG {
            let comment_len = read_u16_le(&tail, i + 20) as usize;
            if i + 22 + comment_len == tail_len {
                eocd_rel_pos = Some(i);
                break;
            }
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }

    let eocd_rel = eocd_rel_pos.ok_or_else(|| "signature EOCD ZIP introuvable".to_string())?;
    let eocd_abs = tail_start + eocd_rel as u64;
    let eocd = &tail[eocd_rel..eocd_rel + 22];

    let mut total_entries = u64::from(read_u16_le(eocd, 10));
    let mut cd_size = u64::from(read_u32_le(eocd, 12));
    let mut cd_offset = u64::from(read_u32_le(eocd, 16));

    // Parse ZIP64 End of Central Directory Locator if present immediately before EOCD
    if eocd_abs >= 20
        && (total_entries == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF)
    {
        let mut loc = [0u8; 20];
        reader
            .seek(SeekFrom::Start(eocd_abs - 20))
            .map_err(|e| e.to_string())?;
        reader.read_exact(&mut loc).map_err(|e| e.to_string())?;
        if loc[0..4] == ZIP64_LOCATOR_SIG {
            let z64_off = read_u64_le(&loc, 8);
            reader
                .seek(SeekFrom::Start(z64_off))
                .map_err(|e| e.to_string())?;
            let mut z64 = [0u8; 56];
            reader.read_exact(&mut z64).map_err(|e| e.to_string())?;
            if z64[0..4] == ZIP64_EOCD_SIG {
                total_entries = read_u64_le(&z64, 32);
                cd_size = read_u64_le(&z64, 40);
                cd_offset = read_u64_le(&z64, 48);
            }
        }
    }

    if cd_offset.saturating_add(cd_size) > total_len {
        return Err("répertoire central ZIP corrompu ou tronqué".into());
    }

    reader
        .seek(SeekFrom::Start(cd_offset))
        .map_err(|e| e.to_string())?;
    let cd_len_usize = usize::try_from(cd_size).map_err(|_| "répertoire ZIP trop grand")?;
    let mut cd_buf = vec![0u8; cd_len_usize];
    reader.read_exact(&mut cd_buf).map_err(|e| e.to_string())?;

    let mut entries = Vec::with_capacity(total_entries.min(4096) as usize);
    let mut pos = 0usize;
    while pos + 46 <= cd_buf.len() {
        if cd_buf[pos..pos + 4] != CD_FILE_SIG {
            break;
        }
        let compression_method = read_u16_le(&cd_buf, pos + 10);
        let crc32 = read_u32_le(&cd_buf, pos + 16);
        let mut compressed_size = u64::from(read_u32_le(&cd_buf, pos + 20));
        let mut uncompressed_size = u64::from(read_u32_le(&cd_buf, pos + 24));
        let fname_len = read_u16_le(&cd_buf, pos + 28) as usize;
        let extra_len = read_u16_le(&cd_buf, pos + 30) as usize;
        let comment_len = read_u16_le(&cd_buf, pos + 32) as usize;
        let mut local_header_offset = u64::from(read_u32_le(&cd_buf, pos + 42));

        let record_end = pos + 46 + fname_len + extra_len + comment_len;
        if record_end > cd_buf.len() {
            break;
        }

        let name = String::from_utf8_lossy(&cd_buf[pos + 46..pos + 46 + fname_len]).to_string();
        let extra = &cd_buf[pos + 46 + fname_len..pos + 46 + fname_len + extra_len];

        // Parse ZIP64 Extended Information extra field (header ID 0x0001)
        let mut ex_pos = 0usize;
        while ex_pos + 4 <= extra.len() {
            let tag = read_u16_le(extra, ex_pos);
            let sz = read_u16_le(extra, ex_pos + 2) as usize;
            let data_start = ex_pos + 4;
            let data_end = data_start + sz;
            if data_end > extra.len() {
                break;
            }
            if tag == 0x0001 {
                let mut field_off = data_start;
                if uncompressed_size == 0xFFFF_FFFF && field_off + 8 <= data_end {
                    uncompressed_size = read_u64_le(extra, field_off);
                    field_off += 8;
                }
                if compressed_size == 0xFFFF_FFFF && field_off + 8 <= data_end {
                    compressed_size = read_u64_le(extra, field_off);
                    field_off += 8;
                }
                if local_header_offset == 0xFFFF_FFFF && field_off + 8 <= data_end {
                    local_header_offset = read_u64_le(extra, field_off);
                }
            }
            ex_pos = data_end;
        }

        pos = record_end;
        if name.ends_with('/') || name.ends_with('\\') || name.contains("..") {
            continue;
        }
        entries.push(ZipEntryMeta {
            name,
            compression_method,
            crc32,
            compressed_size,
            uncompressed_size,
            local_header_offset,
        });
    }

    Ok(entries)
}

/// Lists all file entries in a ZIP file on disk without decompressing entry payloads.
pub fn list_zip_entries(zip_path: &Path) -> Result<Vec<ZipEntryMeta>, String> {
    let file = File::open(zip_path).map_err(|e| format!("{}: {e}", zip_path.display()))?;
    let mut reader = BufReader::new(file);
    list_zip_entries_from_reader(&mut reader)
}

fn open_entry_data_stream<'a, R: Read + Seek>(
    reader: &'a mut R,
    entry: &ZipEntryMeta,
) -> Result<Box<dyn Read + 'a>, String> {
    reader
        .seek(SeekFrom::Start(entry.local_header_offset))
        .map_err(|e| e.to_string())?;
    let mut local_hdr = [0u8; 30];
    reader
        .read_exact(&mut local_hdr)
        .map_err(|e| e.to_string())?;
    if local_hdr[0..4] != LOCAL_FILE_SIG {
        return Err(format!("en-tête local ZIP invalide pour {}", entry.name));
    }
    let fname_len = u64::from(read_u16_le(&local_hdr, 26));
    let extra_len = u64::from(read_u16_le(&local_hdr, 28));
    let data_offset = entry.local_header_offset + 30 + fname_len + extra_len;
    reader
        .seek(SeekFrom::Start(data_offset))
        .map_err(|e| e.to_string())?;

    let compressed = reader.take(entry.compressed_size);
    match entry.compression_method {
        0 => Ok(Box::new(compressed)),
        8 => Ok(Box::new(DeflateDecoder::new(compressed))),
        other => Err(format!(
            "méthode ZIP non supportée ({other}) pour {}",
            entry.name
        )),
    }
}

/// Reads at most `max_bytes` decompressed prefix bytes of a single entry inside a `.zip` file.
pub fn read_zip_entry_prefix(
    zip_path: &Path,
    entry: &ZipEntryMeta,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    let file = File::open(zip_path).map_err(|e| format!("{}: {e}", zip_path.display()))?;
    let mut reader = BufReader::new(file);
    let mut stream = open_entry_data_stream(&mut reader, entry)?;
    let mut out = Vec::with_capacity(max_bytes.min(entry.uncompressed_size as usize));
    let mut limited = stream.by_ref().take(max_bytes as u64);
    limited.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Streams and extracts a ZIP entry atomically onto disk (`dest_path`), verifying CRC32.
pub fn extract_zip_entry_to_file(
    zip_path: &Path,
    entry: &ZipEntryMeta,
    dest_path: &Path,
) -> Result<u64, String> {
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let file = File::open(zip_path).map_err(|e| format!("{}: {e}", zip_path.display()))?;
    let mut reader = BufReader::new(file);
    let mut stream = open_entry_data_stream(&mut reader, entry)?;

    let tmp_path = dest_path.with_extension("extract.tmp");
    let mut out_file =
        File::create(&tmp_path).map_err(|e| format!("{}: {e}", tmp_path.display()))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total_written = 0u64;

    loop {
        let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out_file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        total_written += n as u64;
    }
    out_file.flush().map_err(|e| e.to_string())?;

    if entry.crc32 != 0 && hasher.finalize() != entry.crc32 {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!(
            "empreinte CRC32 invalide lors de l'extraction de {}",
            entry.name
        ));
    }

    fs::rename(&tmp_path, dest_path).map_err(|e| format!("{}: {e}", dest_path.display()))?;
    Ok(total_written)
}

/// Extracts the first file inside an in-memory `.zip` archive whose lowercase name ends with `suffix_lower`.
pub fn extract_first_matching_file_from_zip_bytes(
    zip_bytes: &[u8],
    suffix_lower: &str,
) -> Result<(String, Vec<u8>), String> {
    let mut cursor = Cursor::new(zip_bytes);
    let entries = list_zip_entries_from_reader(&mut cursor)?;
    let target = entries
        .into_iter()
        .find(|e| e.name.to_ascii_lowercase().ends_with(suffix_lower))
        .ok_or_else(|| format!("aucun fichier *{suffix_lower} trouvé dans l'archive ZIP"))?;

    let mut stream = open_entry_data_stream(&mut cursor, &target)?;
    let mut out = Vec::with_capacity(target.uncompressed_size.min(64 << 20) as usize);
    stream.read_to_end(&mut out).map_err(|e| e.to_string())?;
    if target.crc32 != 0 && crc32fast::hash(&out) != target.crc32 {
        return Err(format!(
            "empreinte CRC32 invalide pour l'entrée ZIP {}",
            target.name
        ));
    }
    Ok((target.name, out))
}

/// Creates a minimal Stored-method ZIP archive in memory (useful for unit tests).
pub fn build_stored_zip_for_test(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cd = Vec::new();

    for (name, data) in files {
        let local_off = out.len() as u32;
        let crc = crc32fast::hash(data);
        let size = data.len() as u32;
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len() as u16;

        // Local file header
        out.extend_from_slice(&LOCAL_FILE_SIG);
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // stored
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);

        // Central directory header
        cd.extend_from_slice(&CD_FILE_SIG);
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes()); // stored
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&size.to_le_bytes());
        cd.extend_from_slice(&size.to_le_bytes());
        cd.extend_from_slice(&name_len.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&local_off.to_le_bytes());
        cd.extend_from_slice(name_bytes);
    }

    let cd_off = out.len() as u32;
    let cd_size = cd.len() as u32;
    let count = files.len() as u16;
    out.extend_from_slice(&cd);

    // EOCD
    out.extend_from_slice(&EOCD_SIG);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;

    #[test]
    fn reads_prefix_and_extracts_stored_and_deflated_zip_entries() {
        let dir = std::env::temp_dir().join(format!("zaimule-zip-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let payload = b"POKEMON PL\0\0CPUE_FAKE_NDS_ROM_PAYLOAD_DATA";
        let zip_bytes = build_stored_zip_for_test(&[
            ("readme.txt", b"RomStation info"),
            ("Pokemon - Version Platine (FR).nds", payload),
        ]);
        let zip_path = dir.join("platinum.zip");
        fs::write(&zip_path, &zip_bytes).unwrap();

        let entries = list_zip_entries(&zip_path).unwrap();
        assert_eq!(entries.len(), 2);
        let nds_entry = entries
            .iter()
            .find(|e| e.name.ends_with(".nds"))
            .unwrap()
            .clone();

        let prefix = read_zip_entry_prefix(&zip_path, &nds_entry, 16).unwrap();
        assert_eq!(&prefix, b"POKEMON PL\0\0CPUE");

        let extracted_path = dir.join("out.nds");
        let n = extract_zip_entry_to_file(&zip_path, &nds_entry, &extracted_path).unwrap();
        assert_eq!(n, payload.len() as u64);
        assert_eq!(fs::read(&extracted_path).unwrap(), payload);

        // Also test DEFLATE-compressed payload inside ZIP (method 8)
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(payload).unwrap();
        let comp = enc.finish().unwrap();
        let crc = crc32fast::hash(payload);
        let name = b"melonds_libretro.dll";
        let mut def_zip = Vec::new();
        // Local header
        def_zip.extend_from_slice(&LOCAL_FILE_SIG);
        def_zip.extend_from_slice(&20u16.to_le_bytes());
        def_zip.extend_from_slice(&0u16.to_le_bytes());
        def_zip.extend_from_slice(&8u16.to_le_bytes()); // Deflate
        def_zip.extend_from_slice(&0u32.to_le_bytes());
        def_zip.extend_from_slice(&crc.to_le_bytes());
        def_zip.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        def_zip.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        def_zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
        def_zip.extend_from_slice(&0u16.to_le_bytes());
        def_zip.extend_from_slice(name);
        def_zip.extend_from_slice(&comp);
        let cd_off = def_zip.len() as u32;
        // CD header
        let mut cd = Vec::new();
        cd.extend_from_slice(&CD_FILE_SIG);
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&8u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(name);
        let cd_size = cd.len() as u32;
        def_zip.extend_from_slice(&cd);
        def_zip.extend_from_slice(&EOCD_SIG);
        def_zip.extend_from_slice(&0u32.to_le_bytes());
        def_zip.extend_from_slice(&1u16.to_le_bytes());
        def_zip.extend_from_slice(&1u16.to_le_bytes());
        def_zip.extend_from_slice(&cd_size.to_le_bytes());
        def_zip.extend_from_slice(&cd_off.to_le_bytes());
        def_zip.extend_from_slice(&0u16.to_le_bytes());

        let (found_name, found_bytes) =
            extract_first_matching_file_from_zip_bytes(&def_zip, ".dll").unwrap();
        assert_eq!(found_name, "melonds_libretro.dll");
        assert_eq!(found_bytes, payload);

        let _ = fs::remove_dir_all(&dir);
    }
}
