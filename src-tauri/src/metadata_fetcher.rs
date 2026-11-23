//! Async boxart & game metadata fetcher with local disk caching (`covers/` & `metadata_cache/`).
//! Fetches official Libretro No-Intro DATs, Libretro Thumbnails, GameTDB DS covers,
//! and Switch icons directly from Rust (zero CORS issues, validated image magic bytes).

use base64::{engine::general_purpose::STANDARD as BASE64_STD, Engine as _};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use zaimule_core::catalog::{ConsoleKind, GameEntry};
use zaimule_core::metadata::{
    candidate_libretro_names, detect_image_format, gametdb_nds_cover_urls, libretro_boxart_urls,
    libretro_dat_url, libretro_system_name, parse_metadat_field_dat, parse_no_intro_dat,
    safe_cover_stem, switch_cover_urls, GameMetadataUpdate, NoIntroIndex,
};

const HTTP_TIMEOUT_SECS: u64 = 12;
const DAT_CACHE_TTL_SECS: u64 = 7 * 24 * 3600; // 7 days
const MAX_COVER_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Cached parsed Libretro indexes for a single console platform.
#[derive(Default)]
pub struct ConsoleDatBundle {
    pub no_intro: NoIntroIndex,
    pub genres: HashMap<String, String>,
    pub years: HashMap<String, String>,
    pub developers: HashMap<String, String>,
    pub publishers: HashMap<String, String>,
}

/// Outcome for enriching a single `GameEntry`.
#[derive(Debug, Clone, Default)]
pub struct SingleEnrichOutcome {
    pub game_id: String,
    pub metadata_update: Option<GameMetadataUpdate>,
    pub cover_filename: Option<String>,
    pub cover_data_url: Option<String>,
    pub cover_found: bool,
    pub metadata_found: bool,
    pub note: Option<String>,
}

fn build_http_client() -> Result<reqwest::Client, String> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    reqwest::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent(format!("Zaimule/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("client HTTP indisponible : {e}"))
}

fn cache_file_is_fresh(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if meta.len() == 0 {
        return false;
    }
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age.as_secs() < DAT_CACHE_TTL_SECS)
        .unwrap_or(false)
}

async fn fetch_text_cached(
    client: &reqwest::Client,
    url: &str,
    cache_path: &Path,
) -> Option<String> {
    if cache_file_is_fresh(cache_path) {
        if let Ok(text) = fs::read_to_string(cache_path) {
            return Some(text);
        }
    }

    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        // Fallback to stale cache if server is unreachable / rate-limited.
        return fs::read_to_string(cache_path).ok();
    }
    let text = resp.text().await.ok()?;
    if text.is_empty() {
        return None;
    }
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = cache_path.with_extension("dat.tmp");
    if fs::write(&tmp, text.as_bytes()).is_ok() {
        let _ = fs::rename(&tmp, cache_path);
    }
    Some(text)
}

/// Loads (and caches on disk) all Libretro DAT metadata files for a native console.
pub async fn load_console_dat_bundle(
    client: &reqwest::Client,
    console: ConsoleKind,
    cache_dir: &Path,
) -> ConsoleDatBundle {
    let Some(sys_name) = libretro_system_name(console) else {
        return ConsoleDatBundle::default();
    };
    let slug: String = sys_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();

    let spawn_cat = |cat: &'static str| {
        let client = client.clone();
        let url = libretro_dat_url(cat, console);
        let path = cache_dir.join(format!("{cat}_{slug}.dat"));
        tauri::async_runtime::spawn(async move {
            let url = url?;
            fetch_text_cached(&client, &url, &path).await
        })
    };

    let h_no_intro = spawn_cat("no-intro");
    let h_genre = spawn_cat("genre");
    let h_year = spawn_cat("releaseyear");
    let h_dev = spawn_cat("developer");
    let h_pub = spawn_cat("publisher");

    let no_intro_txt = h_no_intro.await.ok().flatten();
    let genre_txt = h_genre.await.ok().flatten();
    let year_txt = h_year.await.ok().flatten();
    let dev_txt = h_dev.await.ok().flatten();
    let pub_txt = h_pub.await.ok().flatten();

    ConsoleDatBundle {
        no_intro: no_intro_txt
            .as_deref()
            .map(parse_no_intro_dat)
            .unwrap_or_default(),
        genres: genre_txt
            .as_deref()
            .map(|t| parse_metadat_field_dat(t, "genre"))
            .unwrap_or_default(),
        years: year_txt
            .as_deref()
            .map(|t| parse_metadat_field_dat(t, "releaseyear"))
            .unwrap_or_default(),
        developers: dev_txt
            .as_deref()
            .map(|t| parse_metadat_field_dat(t, "developer"))
            .unwrap_or_default(),
        publishers: pub_txt
            .as_deref()
            .map(|t| parse_metadat_field_dat(t, "publisher"))
            .unwrap_or_default(),
    }
}

async fn download_first_valid_cover(
    client: &reqwest::Client,
    urls: &[String],
) -> Option<(Vec<u8>, &'static str, &'static str)> {
    for url in urls {
        let Ok(resp) = client.get(url).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        if let Some(len) = resp.content_length() {
            if len as usize > MAX_COVER_BYTES {
                continue;
            }
        }
        let Ok(bytes) = resp.bytes().await else {
            continue;
        };
        if let Some((ext, mime)) = detect_image_format(&bytes) {
            return Some((bytes.to_vec(), ext, mime));
        }
    }
    None
}

/// Removes any older cached cover files for `game_id` across `.png`/`.jpg`/`.webp`.
pub fn remove_cached_cover_files(covers_dir: &Path, game_id: &str) {
    let stem = safe_cover_stem(game_id);
    for ext in ["png", "jpg", "webp"] {
        let candidate = covers_dir.join(format!("{stem}.{ext}"));
        if candidate.is_file() {
            let _ = fs::remove_file(candidate);
        }
    }
}

/// Persists validated cover bytes atomically inside `covers_dir` and returns `(filename, data_url)`.
pub fn save_cover_bytes(
    covers_dir: &Path,
    game_id: &str,
    bytes: &[u8],
) -> Result<(String, String), String> {
    let (ext, mime) = detect_image_format(bytes).ok_or_else(|| {
        "format d'image non pris en charge (PNG, JPEG ou WebP attendu, max 8 Mo)".to_string()
    })?;
    fs::create_dir_all(covers_dir)
        .map_err(|e| format!("impossible de créer le dossier covers : {e}"))?;
    remove_cached_cover_files(covers_dir, game_id);

    let filename = format!("{}.{ext}", safe_cover_stem(game_id));
    let dest = covers_dir.join(&filename);
    let tmp = covers_dir.join(format!("{filename}.tmp"));
    fs::write(&tmp, bytes).map_err(|e| format!("écriture jaquette impossible : {e}"))?;
    fs::rename(&tmp, &dest).map_err(|e| format!("sauvegarde jaquette impossible : {e}"))?;

    let data_url = format!("data:{mime};base64,{}", BASE64_STD.encode(bytes));
    Ok((filename, data_url))
}

/// Imports a user-selected local cover image file (`.png`, `.jpg`, `.webp`) into `covers_dir`.
pub fn import_custom_cover_file(
    covers_dir: &Path,
    game_id: &str,
    source_path: &Path,
) -> Result<(String, String), String> {
    let meta = fs::metadata(source_path)
        .map_err(|e| format!("fichier image introuvable ({}): {e}", source_path.display()))?;
    if meta.len() as usize > MAX_COVER_BYTES {
        return Err("image trop volumineuse (maximum 8 Mo)".into());
    }
    let bytes = fs::read(source_path).map_err(|e| format!("lecture image impossible : {e}"))?;
    save_cover_bytes(covers_dir, game_id, &bytes)
}

/// Reads a single cached cover file from `covers_dir` as a `data:image/...;base64,...` URI.
pub fn read_cover_data_url(covers_dir: &Path, cover_filename: &str) -> Option<String> {
    // Reject any path traversal segments.
    let fname = Path::new(cover_filename).file_name()?.to_str()?;
    let full = covers_dir.join(fname);
    let bytes = fs::read(&full).ok()?;
    let (_ext, mime) = detect_image_format(&bytes)?;
    Some(format!("data:{mime};base64,{}", BASE64_STD.encode(&bytes)))
}

/// Loads all cached covers referenced by the catalog as a `game_id -> data URL` map.
pub fn load_catalog_cover_map(covers_dir: &Path, catalog: &[GameEntry]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for game in catalog {
        if let Some(ref cover_name) = game.cover_path {
            if let Some(data_url) = read_cover_data_url(covers_dir, cover_name) {
                out.insert(game.id.clone(), data_url);
            }
        }
    }
    out
}

/// Enriches a single game entry using an already-loaded `ConsoleDatBundle` (or Switch URLs).
pub async fn enrich_game_with_bundle(
    client: &reqwest::Client,
    game: &GameEntry,
    bundle: &ConsoleDatBundle,
    covers_dir: &Path,
    force_cover_refresh: bool,
) -> SingleEnrichOutcome {
    let mut outcome = SingleEnrichOutcome {
        game_id: game.id.clone(),
        ..Default::default()
    };

    // 1. Resolve No-Intro canonical entry & Libretro metadata fields.
    let crc_upper = game.crc32_hex.trim().to_ascii_uppercase();
    let matched = bundle.no_intro.lookup(&crc_upper, game.title_id.as_deref());
    let lookup_crc = matched
        .map(|m| m.crc32_hex.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(crc_upper.as_str());

    let canonical_name = matched
        .map(|m| m.name.clone())
        .or_else(|| game.metadata_canonical_name.clone());
    let genre = bundle
        .genres
        .get(lookup_crc)
        .or_else(|| bundle.genres.get(&crc_upper))
        .cloned();
    let release_year = bundle
        .years
        .get(lookup_crc)
        .or_else(|| bundle.years.get(&crc_upper))
        .cloned();
    let developer = bundle
        .developers
        .get(lookup_crc)
        .or_else(|| bundle.developers.get(&crc_upper))
        .cloned();
    let publisher = bundle
        .publishers
        .get(lookup_crc)
        .or_else(|| bundle.publishers.get(&crc_upper))
        .cloned();

    let has_any_meta = canonical_name.is_some()
        || genre.is_some()
        || release_year.is_some()
        || developer.is_some()
        || publisher.is_some();

    if has_any_meta {
        outcome.metadata_found = true;
        outcome.metadata_update = Some(GameMetadataUpdate {
            title: None, // Never overwrite the user's display title automatically
            genre: genre.or_else(|| game.metadata_genre.clone()),
            developer: developer.or_else(|| game.metadata_developer.clone()),
            publisher: publisher.or_else(|| game.metadata_publisher.clone()),
            release_year: release_year.or_else(|| game.metadata_release_year.clone()),
            description: None,
            canonical_name: canonical_name.clone(),
        });
    }

    // 2. Download & cache boxart if missing or force-refreshed.
    let already_has_valid_cover = !force_cover_refresh
        && game
            .cover_path
            .as_deref()
            .and_then(|f| read_cover_data_url(covers_dir, f))
            .is_some();

    if already_has_valid_cover {
        if let Some(ref f) = game.cover_path {
            outcome.cover_filename = Some(f.clone());
            outcome.cover_data_url = read_cover_data_url(covers_dir, f);
            outcome.cover_found = true;
        }
        return outcome;
    }

    let mut candidate_urls: Vec<String> = Vec::new();
    let mut push_urls = |urls: Vec<String>| {
        for u in urls {
            if !candidate_urls.iter().any(|existing| existing == &u) {
                candidate_urls.push(u);
            }
        }
    };

    match game.console {
        ConsoleKind::Nds => {
            if let Some(ref cname) = canonical_name {
                push_urls(libretro_boxart_urls(ConsoleKind::Nds, cname));
            }
            if let Some(ref code) = game.title_id {
                push_urls(gametdb_nds_cover_urls(code));
            }
            for cand in candidate_libretro_names(&game.title, &game.region) {
                push_urls(libretro_boxart_urls(ConsoleKind::Nds, &cand));
            }
        }
        ConsoleKind::Gb | ConsoleKind::Gbc | ConsoleKind::Gba => {
            if let Some(ref cname) = canonical_name {
                push_urls(libretro_boxart_urls(game.console, cname));
            }
            for cand in candidate_libretro_names(&game.title, &game.region) {
                push_urls(libretro_boxart_urls(game.console, &cand));
            }
        }
        ConsoleKind::Switch | ConsoleKind::Switch2 => {
            if let Some(ref tid) = game.title_id {
                push_urls(switch_cover_urls(tid));
            }
        }
    }

    if let Some((bytes, _ext, _mime)) = download_first_valid_cover(client, &candidate_urls).await {
        match save_cover_bytes(covers_dir, &game.id, &bytes) {
            Ok((filename, data_url)) => {
                outcome.cover_filename = Some(filename);
                outcome.cover_data_url = Some(data_url);
                outcome.cover_found = true;
            }
            Err(err) => {
                outcome.note = Some(err);
            }
        }
    }

    outcome
}

/// Enriches one or multiple games, loading each needed console DAT bundle only once.
pub async fn enrich_games_list(
    games: Vec<GameEntry>,
    covers_dir: PathBuf,
    cache_dir: PathBuf,
    force_cover_refresh: bool,
) -> Result<Vec<SingleEnrichOutcome>, String> {
    let client = build_http_client()?;

    // Load DAT bundles once per distinct native console present in `games`.
    let mut bundles: HashMap<ConsoleKind, ConsoleDatBundle> = HashMap::new();
    for game in &games {
        if game.console.is_native_rust() && !bundles.contains_key(&game.console) {
            let bundle = load_console_dat_bundle(&client, game.console, &cache_dir).await;
            bundles.insert(game.console, bundle);
        }
    }

    let empty_bundle = ConsoleDatBundle::default();
    let mut outcomes = Vec::with_capacity(games.len());
    for game in &games {
        let bundle = bundles.get(&game.console).unwrap_or(&empty_bundle);
        let res =
            enrich_game_with_bundle(&client, game, bundle, &covers_dir, force_cover_refresh).await;
        outcomes.push(res);
    }

    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_https_client_with_ring_provider_and_caches_covers_atomically() {
        // Verifies rustls CryptoProvider installs cleanly even before tauri-plugin-updater runs.
        assert!(build_http_client().is_ok());

        let root =
            std::env::temp_dir().join(format!("zaimule-cover-cache-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let covers = root.join("covers");

        let fake_png = b"\x89PNG\r\n\x1A\n0123456789ABCDEF";
        let (fname, data_url) = save_cover_bytes(&covers, "rom-1234abcd", fake_png).unwrap();
        assert_eq!(fname, "rom-1234abcd.png");
        assert!(data_url.starts_with("data:image/png;base64,"));
        assert_eq!(
            read_cover_data_url(&covers, &fname).as_deref(),
            Some(data_url.as_str())
        );

        // Path traversal must never escape covers_dir
        assert!(read_cover_data_url(&covers, "../secret.png").is_none());

        // Replacing PNG with JPEG cleans stale .png file
        let src_jpg = root.join("custom.jpg");
        fs::write(&src_jpg, b"\xFF\xD8\xFF\xE0JFIF000000").unwrap();
        let (jpg_name, jpg_url) =
            import_custom_cover_file(&covers, "rom-1234abcd", &src_jpg).unwrap();
        assert_eq!(jpg_name, "rom-1234abcd.jpg");
        assert!(jpg_url.starts_with("data:image/jpeg;base64,"));
        assert!(!covers.join("rom-1234abcd.png").exists());

        remove_cached_cover_files(&covers, "rom-1234abcd");
        assert!(!covers.join("rom-1234abcd.jpg").exists());

        let _ = fs::remove_dir_all(&root);
    }
}
