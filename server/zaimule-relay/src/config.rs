use std::fmt;
use std::time::Duration;

use ring::digest;
use subtle::ConstantTimeEq;

/// Taille maximale stricte d'une trame WebSocket acceptée par le relais (64 Kio).
pub const HARD_MAX_FRAME_BYTES: usize = 65_536;

/// Longueur minimale imposée à la clé d'accès privée du relais.
pub const MIN_ACCESS_KEY_LEN: usize = 16;

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    access_key_digest: Option<[u8; 32]>,
    pub max_rooms: usize,
    pub max_peers_per_room: usize,
    pub max_connections_total: usize,
    pub max_connections_per_ip: usize,
    pub max_frames_per_sec: u32,
    pub max_frame_bytes: usize,
    pub room_idle_ttl: Duration,
    pub ws_idle_timeout: Duration,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("port", &self.port)
            .field("access_key_configured", &self.access_key_digest.is_some())
            .field("max_rooms", &self.max_rooms)
            .field("max_peers_per_room", &self.max_peers_per_room)
            .field("max_connections_total", &self.max_connections_total)
            .field("max_connections_per_ip", &self.max_connections_per_ip)
            .field("max_frames_per_sec", &self.max_frames_per_sec)
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("room_idle_ttl", &self.room_idle_ttl)
            .field("ws_idle_timeout", &self.ws_idle_timeout)
            .finish()
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let port = env_parse("PORT", 3000u16)?;
        let raw_key = std::env::var("ZAIMULE_RELAY_ACCESS_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let access_key_digest = match raw_key {
            Some(key) if key.len() >= MIN_ACCESS_KEY_LEN => Some(sha256_bytes(key.as_bytes())),
            Some(_) => {
                tracing::warn!(
                    min_len = MIN_ACCESS_KEY_LEN,
                    "ZAIMULE_RELAY_ACCESS_KEY trop courte : endpoint /ws désactivé (fail-closed)"
                );
                None
            }
            None => {
                tracing::warn!(
                    "ZAIMULE_RELAY_ACCESS_KEY absente : endpoint /ws désactivé (fail-closed)"
                );
                None
            }
        };

        let max_frame_bytes = env_parse("ZAIMULE_MAX_FRAME_BYTES", HARD_MAX_FRAME_BYTES)?
            .clamp(1024, HARD_MAX_FRAME_BYTES);

        Ok(Self {
            port,
            access_key_digest,
            max_rooms: env_parse("ZAIMULE_MAX_ROOMS", 64usize)?.clamp(1, 512),
            max_peers_per_room: env_parse("ZAIMULE_MAX_PEERS_PER_ROOM", 8usize)?.clamp(2, 16),
            max_connections_total: env_parse("ZAIMULE_MAX_CONNECTIONS", 128usize)?.clamp(2, 1024),
            max_connections_per_ip: env_parse("ZAIMULE_MAX_CONN_PER_IP", 6usize)?.clamp(1, 32),
            max_frames_per_sec: env_parse("ZAIMULE_MAX_FRAMES_PER_SEC", 600u32)?.clamp(10, 4000),
            max_frame_bytes,
            room_idle_ttl: Duration::from_secs(
                env_parse("ZAIMULE_ROOM_IDLE_SECS", 900u64)?.clamp(30, 86_400),
            ),
            ws_idle_timeout: Duration::from_secs(
                env_parse("ZAIMULE_WS_IDLE_SECS", 120u64)?.clamp(15, 3600),
            ),
        })
    }

    #[cfg(test)]
    pub fn for_tests(access_key: Option<&str>) -> Self {
        Self {
            port: 0,
            access_key_digest: access_key
                .filter(|k| k.len() >= MIN_ACCESS_KEY_LEN)
                .map(|k| sha256_bytes(k.as_bytes())),
            max_rooms: 8,
            max_peers_per_room: 4,
            max_connections_total: 16,
            max_connections_per_ip: 4,
            max_frames_per_sec: 120,
            max_frame_bytes: HARD_MAX_FRAME_BYTES,
            room_idle_ttl: Duration::from_secs(300),
            ws_idle_timeout: Duration::from_secs(60),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.access_key_digest.is_some()
    }

    /// Vérifie la clé fournie en temps constant via comparaison des empreintes SHA-256
    /// (évite toute fuite de longueur comme de contenu).
    pub fn verify_access_key(&self, candidate: &str) -> bool {
        let Some(expected) = self.access_key_digest else {
            return false;
        };
        let actual = sha256_bytes(candidate.trim().as_bytes());
        bool::from(expected.ct_eq(&actual))
    }
}

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let digest = digest::digest(&digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

fn env_parse<T>(name: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("Variable {name} invalide ('{raw}'): {e}")),
        _ => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_key_verification_is_exact_and_fail_closed() {
        let disabled = Config::for_tests(None);
        assert!(!disabled.is_configured());
        assert!(!disabled.verify_access_key("anything-longer-than-sixteen-chars"));

        let short = Config::for_tests(Some("too-short"));
        assert!(!short.is_configured());

        let cfg = Config::for_tests(Some("0123456789abcdef0123456789abcdef"));
        assert!(cfg.is_configured());
        assert!(cfg.verify_access_key("0123456789abcdef0123456789abcdef"));
        assert!(cfg.verify_access_key("  0123456789abcdef0123456789abcdef\n"));
        assert!(!cfg.verify_access_key("0123456789abcdef0123456789abcde0"));
        assert!(!cfg.verify_access_key(""));
    }

    #[test]
    fn debug_never_leaks_secret_material() {
        let secret = "super-secret-zaimule-key-9876543210";
        let cfg = Config::for_tests(Some(secret));
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("access_key_configured: true"));
    }
}
