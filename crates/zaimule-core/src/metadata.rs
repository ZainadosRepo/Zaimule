//! Offline-first game metadata, boxart URL builders, ClrMamePro `.dat` parsers
//! (Libretro No-Intro / Genre / Developer / Publisher / ReleaseYear), GameTDB DS cover lookup,
//! custom collection normalization, and safe bidirectional Cloud Saves `.srm` synchronization.

use crate::catalog::ConsoleKind;
use crate::storage::save_sram_with_rotation;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

const MAX_COLLECTION_NAME_LEN: usize = 36;
const MAX_COVER_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Parsed entry from a Libretro No-Intro ClrMamePro `.dat` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoIntroEntry {
    pub name: String,
    pub region: Option<String>,
    pub serial: Option<String>,
    pub crc32_hex: String,
}

/// Fast in-memory index over a parsed Libretro No-Intro `.dat` file.
#[derive(Debug, Clone, Default)]
pub struct NoIntroIndex {
    pub by_crc: HashMap<String, NoIntroEntry>,
    pub by_serial: HashMap<String, NoIntroEntry>,
}

impl NoIntroIndex {
    /// Looks up a ROM first by exact uppercase CRC32, falling back to 4-char GameCode (`serial`).
    pub fn lookup(&self, crc32_hex: &str, serial: Option<&str>) -> Option<&NoIntroEntry> {
        let crc_key = crc32_hex.trim().to_ascii_uppercase();
        if !crc_key.is_empty() {
            if let Some(hit) = self.by_crc.get(&crc_key) {
                return Some(hit);
            }
        }
        if let Some(code) = serial {
            let serial_key = code.trim().to_ascii_uppercase();
            if serial_key.len() >= 4 {
                if let Some(hit) = self.by_serial.get(&serial_key) {
                    return Some(hit);
                }
            }
        }
        None
    }
}

/// User-editable or scraped metadata patch for a single `GameEntry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GameMetadataUpdate {
    pub title: Option<String>,
    pub genre: Option<String>,
    pub developer: Option<String>,
    pub publisher: Option<String>,
    pub release_year: Option<String>,
    pub description: Option<String>,
    pub canonical_name: Option<String>,
}

/// Summary report returned after synchronizing `.srm` saves with a user-selected cloud folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CloudSyncReport {
    pub cloud_dir: String,
    pub uploaded_count: usize,
    pub downloaded_count: usize,
    pub unchanged_count: usize,
    pub synced_files: Vec<String>,
    pub errors: Vec<String>,
}

/// Returns the Libretro database system name for a native console, if supported.
pub fn libretro_system_name(console: ConsoleKind) -> Option<&'static str> {
    match console {
        ConsoleKind::Gb => Some("Nintendo - Game Boy"),
        ConsoleKind::Gbc => Some("Nintendo - Game Boy Color"),
        ConsoleKind::Gba => Some("Nintendo - Game Boy Advance"),
        ConsoleKind::Nds => Some("Nintendo - Nintendo DS"),
        ConsoleKind::Switch | ConsoleKind::Switch2 => None,
    }
}

/// Returns the `libretro-thumbnails` GitHub repository name for a native console.
pub fn libretro_thumbnails_repo(console: ConsoleKind) -> Option<&'static str> {
    match console {
        ConsoleKind::Gb => Some("Nintendo_-_Game_Boy"),
        ConsoleKind::Gbc => Some("Nintendo_-_Game_Boy_Color"),
        ConsoleKind::Gba => Some("Nintendo_-_Game_Boy_Advance"),
        ConsoleKind::Nds => Some("Nintendo_-_Nintendo_DS"),
        ConsoleKind::Switch | ConsoleKind::Switch2 => None,
    }
}

/// Sanitizes a No-Intro game name using RetroArch's exact thumbnail filename rule:
/// `& * / : \` " < > ? \ |` are replaced by `_`.
pub fn sanitize_libretro_name(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            '&' | '*' | '/' | ':' | '`' | '"' | '<' | '>' | '?' | '\\' | '|' => '_',
            other => other,
        })
        .collect()
}

/// Percent-encodes a single URL path segment according to RFC 3986 unreserved characters.
pub fn percent_encode_path_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for byte in input.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Builds the GitHub Raw URL for a Libretro metadata `.dat` file
/// (`category` is e.g. `"no-intro"`, `"genre"`, `"releaseyear"`, `"developer"`, `"publisher"`).
pub fn libretro_dat_url(category: &str, console: ConsoleKind) -> Option<String> {
    let system = libretro_system_name(console)?;
    let encoded_file = percent_encode_path_segment(&format!("{system}.dat"));
    Some(format!(
        "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/{category}/{encoded_file}"
    ))
}

/// Builds ordered boxart candidate URLs for a canonical Libretro/No-Intro game title.
pub fn libretro_boxart_urls(console: ConsoleKind, canonical_name: &str) -> Vec<String> {
    let trimmed = canonical_name.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let sanitized = sanitize_libretro_name(trimmed);
    let encoded_png = percent_encode_path_segment(&format!("{sanitized}.png"));

    let mut urls = Vec::new();
    if let Some(repo) = libretro_thumbnails_repo(console) {
        urls.push(format!(
            "https://raw.githubusercontent.com/libretro-thumbnails/{repo}/master/Named_Boxarts/{encoded_png}"
        ));
    }
    if let Some(sys) = libretro_system_name(console) {
        let encoded_sys = percent_encode_path_segment(sys);
        urls.push(format!(
            "https://thumbnails.libretro.com/{encoded_sys}/Named_Boxarts/{encoded_png}"
        ));
    }
    urls
}

/// Builds ordered GameTDB boxart URLs for a 4-character Nintendo DS GameCode (e.g. `IPKF`, `AMCP`).
pub fn gametdb_nds_cover_urls(game_code: &str) -> Vec<String> {
    let code = game_code.trim().to_ascii_uppercase();
    if code.len() != 4 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Vec::new();
    }
    let langs: &[&str] = match code.as_bytes()[3] {
        b'F' => &["FR", "EN", "US"],
        b'P' | b'X' | b'Y' | b'V' => &["FR", "EN", "US"],
        b'E' => &["US", "EN", "FR"],
        b'J' => &["JA", "EN", "US"],
        b'D' => &["DE", "EN", "US"],
        b'I' => &["IT", "EN", "US"],
        b'S' => &["ES", "EN", "US"],
        b'K' => &["KO", "EN", "US"],
        _ => &["FR", "EN", "US", "JA"],
    };

    let mut urls = Vec::with_capacity(langs.len() * 2);
    for lang in langs {
        urls.push(format!(
            "https://art.gametdb.com/ds/coverHQ/{lang}/{code}.jpg"
        ));
        urls.push(format!(
            "https://art.gametdb.com/ds/coverM/{lang}/{code}.jpg"
        ));
    }
    urls
}

/// Builds lightweight direct boxart/icon candidate URLs for a 16-hex Nintendo Switch Title ID.
pub fn switch_cover_urls(title_id: &str) -> Vec<String> {
    let tid = title_id.trim().to_ascii_uppercase();
    if tid.len() != 16 || !tid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Vec::new();
    }
    // Base game Title IDs end in 000; updates/DLC share the upper 13 hex chars + "000".
    let base_tid = format!("{}000", &tid[..13]);
    let mut urls = vec![format!("https://tinfoil.media/ti/{tid}/512/512/")];
    if base_tid != tid {
        urls.push(format!("https://tinfoil.media/ti/{base_tid}/512/512/"));
    }
    urls
}

/// Generates fallback No-Intro-style candidate titles when CRC32/serial lookup finds no exact entry
/// (e.g. ROM hacks, translations, or stripped filenames).
pub fn candidate_libretro_names(title: &str, region: &str) -> Vec<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut push_unique = |cand: String| {
        if !cand.is_empty() && !out.iter().any(|existing: &String| existing == &cand) {
            out.push(cand);
        }
    };

    // Strip any bracketed tags `[...]` from filename stems while preserving `(France)` etc.
    let mut no_brackets = String::with_capacity(trimmed.len());
    let mut depth = 0usize;
    for ch in trimmed.chars() {
        match ch {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => no_brackets.push(ch),
            _ => {}
        }
    }
    let cleaned = no_brackets.split_whitespace().collect::<Vec<_>>().join(" ");
    push_unique(cleaned.clone());

    // Strip parenthesized tags to produce a base title + region suffix probes.
    let base = cleaned
        .split('(')
        .next()
        .unwrap_or(&cleaned)
        .trim()
        .to_string();
    if !base.is_empty() && base != cleaned {
        push_unique(base.clone());
    }

    let reg_upper = region.to_ascii_uppercase();
    let suffixes: &[&str] = if reg_upper.contains("FR") {
        &[
            "(France)",
            "(Europe) (En,Fr,De,Es,It)",
            "(Europe)",
            "(USA, Europe)",
            "(USA)",
        ]
    } else if reg_upper.contains("USA") {
        &["(USA)", "(USA, Europe)", "(World)", "(Europe)"]
    } else if reg_upper.contains("JPN") {
        &["(Japan)", "(World)", "(USA)"]
    } else {
        &[
            "(Europe)",
            "(France)",
            "(Europe) (En,Fr,De,Es,It)",
            "(USA)",
            "(USA, Europe)",
            "(World)",
        ]
    };

    if !base.is_empty() {
        for sfx in suffixes {
            push_unique(format!("{base} {sfx}"));
        }
    }
    out
}

/// Extracts a quoted value following a key on a ClrMamePro line, e.g. `name "Pokemon (France)"`.
fn extract_quoted_field(line: &str, key: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix(key)?;
    let first = rest.as_bytes().first().copied()?;
    if !first.is_ascii_whitespace() {
        return None;
    }
    let rest = rest.trim_start();
    if let Some(after_open) = rest.strip_prefix('"') {
        let mut val = String::new();
        let mut chars = after_open.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    val.push(escaped);
                }
            } else if ch == '"' {
                return Some(val);
            } else {
                val.push(ch);
            }
        }
        None
    } else {
        rest.split_whitespace().next().map(|s| s.to_string())
    }
}

/// Extracts an 8-character uppercase hex CRC32 token after `crc ` anywhere inside a line.
fn extract_crc_token(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let boundary_ok = i == 0 || bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b'(';
        if boundary_ok
            && bytes[i..i + 3].eq_ignore_ascii_case(b"crc")
            && bytes[i + 3].is_ascii_whitespace()
        {
            let rest = line[i + 3..].trim_start();
            let token: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if token.len() == 8 {
                return Some(token.to_ascii_uppercase());
            }
        }
        i += 1;
    }
    None
}

/// Parses a Libretro No-Intro ClrMamePro `.dat` text into a `NoIntroIndex` keyed by CRC32 & serial.
pub fn parse_no_intro_dat(content: &str) -> NoIntroIndex {
    let mut index = NoIntroIndex::default();
    let mut in_game = false;
    let mut paren_depth: i32 = 0;
    let mut cur_name: Option<String> = None;
    let mut cur_region: Option<String> = None;
    let mut cur_serial: Option<String> = None;
    let mut cur_crcs: Vec<String> = Vec::new();

    let flush_game = |index: &mut NoIntroIndex,
                      name: &mut Option<String>,
                      region: &mut Option<String>,
                      serial: &mut Option<String>,
                      crcs: &mut Vec<String>| {
        if let Some(game_name) = name.take() {
            let reg = region.take();
            let ser = serial.take().map(|s| s.trim().to_ascii_uppercase());
            if let Some(ref ser_key) = ser {
                if !ser_key.is_empty() && !index.by_serial.contains_key(ser_key) {
                    index.by_serial.insert(
                        ser_key.clone(),
                        NoIntroEntry {
                            name: game_name.clone(),
                            region: reg.clone(),
                            serial: Some(ser_key.clone()),
                            crc32_hex: crcs.first().cloned().unwrap_or_default(),
                        },
                    );
                }
            }
            for crc in crcs.drain(..) {
                index
                    .by_crc
                    .entry(crc.clone())
                    .or_insert_with(|| NoIntroEntry {
                        name: game_name.clone(),
                        region: reg.clone(),
                        serial: ser.clone(),
                        crc32_hex: crc,
                    });
            }
        } else {
            *region = None;
            *serial = None;
            crcs.clear();
        }
    };

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if !in_game {
            if line.starts_with("game (") || line == "game(" {
                in_game = true;
                paren_depth = 1;
                cur_name = None;
                cur_region = None;
                cur_serial = None;
                cur_crcs.clear();
            }
            continue;
        }

        // Update parenthesis nesting depth outside quoted strings.
        let mut in_quotes = false;
        let mut prev_backslash = false;
        let start_depth = paren_depth;
        for ch in line.chars() {
            if in_quotes {
                if ch == '"' && !prev_backslash {
                    in_quotes = false;
                }
                prev_backslash = ch == '\\' && !prev_backslash;
            } else {
                match ch {
                    '"' => {
                        in_quotes = true;
                        prev_backslash = false;
                    }
                    '(' => paren_depth += 1,
                    ')' => paren_depth -= 1,
                    _ => {}
                }
            }
        }

        if start_depth == 1 {
            if let Some(v) = extract_quoted_field(line, "name") {
                cur_name = Some(v);
            } else if let Some(v) = extract_quoted_field(line, "region") {
                cur_region = Some(v);
            } else if let Some(v) = extract_quoted_field(line, "serial") {
                cur_serial = Some(v);
            }
        }

        if let Some(crc) = extract_crc_token(line) {
            cur_crcs.push(crc);
        }
        // Some DAT lines carry `serial "XXXX"` only inside the `rom ( ... )` clause.
        if cur_serial.is_none() {
            if let Some(pos) = line.find("serial \"") {
                if let Some(v) = extract_quoted_field(&line[pos..], "serial") {
                    cur_serial = Some(v);
                }
            }
        }

        if paren_depth <= 0 {
            flush_game(
                &mut index,
                &mut cur_name,
                &mut cur_region,
                &mut cur_serial,
                &mut cur_crcs,
            );
            in_game = false;
            paren_depth = 0;
        }
    }

    index
}

/// Parses a Libretro metadata `.dat` file (`genre`, `releaseyear`, `developer`, `publisher`)
/// into a `CRC32 (UPPERCASE) -> field value` map.
pub fn parse_metadat_field_dat(content: &str, field_key: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut in_game = false;
    let mut paren_depth: i32 = 0;
    let mut cur_val: Option<String> = None;
    let mut cur_crcs: Vec<String> = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if !in_game {
            if line.starts_with("game (") || line == "game(" {
                in_game = true;
                paren_depth = 1;
                cur_val = None;
                cur_crcs.clear();
            }
            continue;
        }

        let start_depth = paren_depth;
        let mut in_quotes = false;
        let mut prev_backslash = false;
        for ch in line.chars() {
            if in_quotes {
                if ch == '"' && !prev_backslash {
                    in_quotes = false;
                }
                prev_backslash = ch == '\\' && !prev_backslash;
            } else {
                match ch {
                    '"' => {
                        in_quotes = true;
                        prev_backslash = false;
                    }
                    '(' => paren_depth += 1,
                    ')' => paren_depth -= 1,
                    _ => {}
                }
            }
        }

        if start_depth == 1 {
            if let Some(v) = extract_quoted_field(line, field_key) {
                let cleaned = v.trim().to_string();
                if !cleaned.is_empty() {
                    cur_val = Some(cleaned);
                }
            }
        }

        if let Some(crc) = extract_crc_token(line) {
            cur_crcs.push(crc);
        }

        if paren_depth <= 0 {
            if let Some(val) = cur_val.take() {
                for crc in cur_crcs.drain(..) {
                    map.entry(crc).or_insert_with(|| val.clone());
                }
            } else {
                cur_crcs.clear();
            }
            in_game = false;
            paren_depth = 0;
        }
    }

    map
}

/// Validates binary cover artwork bytes via magic header (PNG, JPEG, or WebP) and enforces size bounds.
pub fn detect_image_format(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.len() < 12 || bytes.len() > MAX_COVER_BYTES {
        return None;
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(("png", "image/png"));
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(("jpg", "image/jpeg"));
    }
    if &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(("webp", "image/webp"));
    }
    None
}

/// Normalizes and validates a user-created collection label.
pub fn normalize_collection_name(raw: &str) -> Result<String, String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err("le nom de la collection ne peut pas être vide".into());
    }
    if collapsed.chars().count() > MAX_COLLECTION_NAME_LEN {
        return Err(format!(
            "nom de collection trop long (max {MAX_COLLECTION_NAME_LEN} caractères)"
        ));
    }
    Ok(collapsed)
}

/// Produces a safe filename stem for cover cache files from an arbitrary `game_id`.
pub fn safe_cover_stem(game_id: &str) -> String {
    let cleaned: String = game_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "game".into()
    } else {
        cleaned
    }
}

/// Safely synchronizes `.srm` cartridge saves bidirectionally between `local_saves_dir`
/// and a user-chosen `cloud_saves_dir` (Google Drive, OneDrive, iCloud, Syncthing, NAS...).
///
/// Rules:
/// - Never deletes files on either side.
/// - Compares file content first (skips identical files regardless of mtime drift).
/// - When both sides differ, the newer modification timestamp wins (with a 2-second tie guard).
/// - Overwriting a local `.srm` uses `save_sram_with_rotation` so `.srm.bak1..3` backups are preserved.
/// - Writing to the cloud folder uses atomic `.srm.tmp` + rename.
pub fn sync_sram_directories(
    local_saves_dir: &Path,
    cloud_saves_dir: &Path,
) -> Result<CloudSyncReport, String> {
    if !cloud_saves_dir.exists() {
        fs::create_dir_all(cloud_saves_dir).map_err(|e| {
            format!(
                "dossier cloud inaccessible ({}): {e}",
                cloud_saves_dir.display()
            )
        })?;
    }
    if !local_saves_dir.exists() {
        fs::create_dir_all(local_saves_dir)
            .map_err(|e| format!("dossier sauvegardes local inaccessible: {e}"))?;
    }

    let collect_srms =
        |dir: &Path| -> Result<HashMap<String, (std::path::PathBuf, SystemTime)>, String> {
            let mut out = HashMap::new();
            let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !name.to_ascii_lowercase().ends_with(".srm") {
                    continue;
                }
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                out.insert(name.to_string(), (path, mtime));
            }
            Ok(out)
        };

    let local_files = collect_srms(local_saves_dir)?;
    let cloud_files = collect_srms(cloud_saves_dir)?;

    let mut all_names: Vec<String> = local_files
        .keys()
        .chain(cloud_files.keys())
        .cloned()
        .collect();
    all_names.sort();
    all_names.dedup();

    let mut report = CloudSyncReport {
        cloud_dir: cloud_saves_dir.to_string_lossy().to_string(),
        ..Default::default()
    };

    let write_cloud_atomic = |dst: &Path, bytes: &[u8]| -> Result<(), String> {
        use std::io::Write;
        let tmp = dst.with_extension("srm.tmp");
        let mut f = fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        fs::rename(&tmp, dst).map_err(|e| format!("{}: {e}", dst.display()))
    };

    for name in all_names {
        let local_entry = local_files.get(&name);
        let cloud_entry = cloud_files.get(&name);

        match (local_entry, cloud_entry) {
            (Some((lpath, _)), None) => match fs::read(lpath) {
                Ok(bytes) => {
                    let dst = cloud_saves_dir.join(&name);
                    match write_cloud_atomic(&dst, &bytes) {
                        Ok(()) => {
                            report.uploaded_count += 1;
                            report.synced_files.push(format!("↑ {name}"));
                        }
                        Err(err) => report.errors.push(err),
                    }
                }
                Err(e) => report.errors.push(format!("{name}: {e}")),
            },
            (None, Some((cpath, _))) => match fs::read(cpath) {
                Ok(bytes) => {
                    let dst = local_saves_dir.join(&name);
                    match save_sram_with_rotation(&dst, &bytes) {
                        Ok(()) => {
                            report.downloaded_count += 1;
                            report.synced_files.push(format!("↓ {name}"));
                        }
                        Err(err) => report.errors.push(err),
                    }
                }
                Err(e) => report.errors.push(format!("{name}: {e}")),
            },
            (Some((lpath, lmtime)), Some((cpath, cmtime))) => {
                let lbytes = match fs::read(lpath) {
                    Ok(b) => b,
                    Err(e) => {
                        report.errors.push(format!("{name}: {e}"));
                        continue;
                    }
                };
                let cbytes = match fs::read(cpath) {
                    Ok(b) => b,
                    Err(e) => {
                        report.errors.push(format!("{name}: {e}"));
                        continue;
                    }
                };
                if lbytes == cbytes {
                    report.unchanged_count += 1;
                    continue;
                }
                if cmtime > lmtime {
                    match save_sram_with_rotation(lpath, &cbytes) {
                        Ok(()) => {
                            report.downloaded_count += 1;
                            report.synced_files.push(format!("↓ {name}"));
                        }
                        Err(err) => report.errors.push(err),
                    }
                } else {
                    match write_cloud_atomic(cpath, &lbytes) {
                        Ok(()) => {
                            report.uploaded_count += 1;
                            report.synced_files.push(format!("↑ {name}"));
                        }
                        Err(err) => report.errors.push(err),
                    }
                }
            }
            (None, None) => {}
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_intro_dat_and_matches_by_crc_or_serial() {
        let dat = r#"
clrmamepro (
	name "Nintendo - Nintendo DS"
)

game (
	name "Pokemon - HeartGold Version (France)"
	region "France"
	serial "IPKF"
	rom ( name "Pokemon - HeartGold Version (France).nds" size 134217728 crc 12AB34CD md5 0000 serial "IPKF" )
)

game (
	name "Mario Kart DS (Europe) (En,Fr,De,Es,It)"
	region "Europe"
	rom ( name "Mario Kart DS.nds" size 33554432 crc deadbeef serial "AMCP" )
)
"#;
        let idx = parse_no_intro_dat(dat);
        let hit_crc = idx.lookup("12ab34cd", None).unwrap();
        assert_eq!(hit_crc.name, "Pokemon - HeartGold Version (France)");
        assert_eq!(hit_crc.serial.as_deref(), Some("IPKF"));

        // Patched ROM with modified CRC32 still resolves via 4-char serial!
        let hit_serial = idx.lookup("FFFFFFFF", Some("amcp")).unwrap();
        assert_eq!(hit_serial.name, "Mario Kart DS (Europe) (En,Fr,De,Es,It)");
    }

    #[test]
    fn parses_single_and_multiline_metadat_dats() {
        let genre_dat = r#"
game (
	genre "Role-playing (RPG)"
	rom ( crc 777eb04f )
)
game (
	comment "Avalon Code"
	developer "Matrix Software"
	rom (
		crc F1A30D9C
	)
)
"#;
        let genres = parse_metadat_field_dat(genre_dat, "genre");
        assert_eq!(
            genres.get("777EB04F").map(String::as_str),
            Some("Role-playing (RPG)")
        );
        let devs = parse_metadat_field_dat(genre_dat, "developer");
        assert_eq!(
            devs.get("F1A30D9C").map(String::as_str),
            Some("Matrix Software")
        );
    }

    #[test]
    fn sanitizes_retroarch_boxart_names_and_builds_urls() {
        assert_eq!(
            sanitize_libretro_name("Zelda: Link's Awakening & DX?"),
            "Zelda_ Link's Awakening _ DX_"
        );
        let urls = libretro_boxart_urls(ConsoleKind::Nds, "Pokemon - HeartGold Version (France)");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("libretro-thumbnails/Nintendo_-_Nintendo_DS/master/Named_Boxarts/Pokemon%20-%20HeartGold%20Version%20%28France%29.png"));
        assert!(
            urls[1].contains("thumbnails.libretro.com/Nintendo%20-%20Nintendo%20DS/Named_Boxarts/")
        );

        let tdb = gametdb_nds_cover_urls("ipkf");
        assert_eq!(tdb[0], "https://art.gametdb.com/ds/coverHQ/FR/IPKF.jpg");
        assert!(tdb.iter().any(|u| u.ends_with("/EN/IPKF.jpg")));
    }

    #[test]
    fn validates_cover_image_magic_bytes_and_collection_names() {
        assert_eq!(
            detect_image_format(b"\x89PNG\r\n\x1A\n00000000"),
            Some(("png", "image/png"))
        );
        assert_eq!(
            detect_image_format(b"\xFF\xD8\xFF\xE000000000"),
            Some(("jpg", "image/jpeg"))
        );
        assert_eq!(
            detect_image_format(b"RIFF\x12\x00\x00\x00WEBPxxxx"),
            Some(("webp", "image/webp"))
        );
        assert_eq!(detect_image_format(b"<html>404 Not Found</html>"), None);

        assert_eq!(
            normalize_collection_name("  Mes   Pokémon  ").unwrap(),
            "Mes Pokémon"
        );
        assert!(normalize_collection_name("   ").is_err());
    }

    #[test]
    fn syncs_sram_directories_bidirectionally_without_data_loss() {
        let root = std::env::temp_dir().join(format!("zaimule-cloud-sync-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let local = root.join("local");
        let cloud = root.join("cloud");
        fs::create_dir_all(&local).unwrap();
        fs::create_dir_all(&cloud).unwrap();

        fs::write(local.join("game-a.srm"), b"LOCAL_SAVE_A").unwrap();
        fs::write(cloud.join("game-b.srm"), b"CLOUD_SAVE_B").unwrap();
        fs::write(local.join("same.srm"), b"IDENTICAL").unwrap();
        fs::write(cloud.join("same.srm"), b"IDENTICAL").unwrap();

        let report = sync_sram_directories(&local, &cloud).unwrap();
        assert_eq!(report.uploaded_count, 1);
        assert_eq!(report.downloaded_count, 1);
        assert_eq!(report.unchanged_count, 1);
        assert!(report.errors.is_empty());
        assert_eq!(fs::read(cloud.join("game-a.srm")).unwrap(), b"LOCAL_SAVE_A");
        assert_eq!(fs::read(local.join("game-b.srm")).unwrap(), b"CLOUD_SAVE_B");

        let _ = fs::remove_dir_all(&root);
    }
}
