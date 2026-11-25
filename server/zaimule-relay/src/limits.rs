use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

/// Contrôleur de connexions simultanées par IP et global (mémoire bornée).
pub struct ConnectionLimiter {
    max_total: usize,
    max_per_ip: usize,
    inner: Arc<Mutex<LimiterState>>,
}

#[derive(Default)]
struct LimiterState {
    total: usize,
    per_ip: HashMap<String, usize>,
}

pub struct ConnectionGuard {
    ip: String,
    inner: Arc<Mutex<LimiterState>>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.total = guard.total.saturating_sub(1);
            if let Some(count) = guard.per_ip.get_mut(&self.ip) {
                if *count <= 1 {
                    guard.per_ip.remove(&self.ip);
                } else {
                    *count -= 1;
                }
            }
        }
    }
}

impl ConnectionLimiter {
    pub fn new(max_total: usize, max_per_ip: usize) -> Self {
        Self {
            max_total,
            max_per_ip,
            inner: Arc::new(Mutex::new(LimiterState::default())),
        }
    }

    pub fn try_acquire(&self, client_ip: &str) -> Option<ConnectionGuard> {
        let mut guard = self.inner.lock().ok()?;
        if guard.total >= self.max_total {
            return None;
        }
        let current = guard.per_ip.get(client_ip).copied().unwrap_or(0);
        if current >= self.max_per_ip {
            return None;
        }
        guard.total += 1;
        *guard.per_ip.entry(client_ip.to_string()).or_insert(0) += 1;
        Some(ConnectionGuard {
            ip: client_ip.to_string(),
            inner: Arc::clone(&self.inner),
        })
    }

    #[cfg(test)]
    pub fn active_total(&self) -> usize {
        self.inner.lock().map(|g| g.total).unwrap_or(0)
    }
}

/// Extrait l'IP cliente transmise par Caddy sur le réseau interne `zaimule-edge`.
/// On lit la dernière entrée non vide de `X-Forwarded-For` (ajoutée par Caddy) afin qu'un
/// client ne puisse pas contourner la limite en forgeant le début de l'en-tête.
pub fn extract_client_ip(headers: &HeaderMap) -> String {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(last) = xff.split(',').map(str::trim).rfind(|part| !part.is_empty()) {
            return sanitize_ip(last);
        }
    }
    if let Some(real_ip) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let trimmed = real_ip.trim();
        if !trimmed.is_empty() {
            return sanitize_ip(trimmed);
        }
    }
    "unknown".to_string()
}

fn sanitize_ip(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .take(64)
        .filter(|c| c.is_ascii_hexdigit() || matches!(c, '.' | ':' | '[' | ']'))
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// Limiteur de débit par fenêtre glissante d'1 seconde sur une session WebSocket.
pub struct FrameRateLimiter {
    max_per_sec: u32,
    window_start: Instant,
    count: u32,
}

impl FrameRateLimiter {
    pub fn new(max_per_sec: u32) -> Self {
        Self {
            max_per_sec,
            window_start: Instant::now(),
            count: 0,
        }
    }

    pub fn allow_at(&mut self, now: Instant) -> bool {
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.count = 0;
        }
        if self.count >= self.max_per_sec {
            return false;
        }
        self.count += 1;
        true
    }

    pub fn allow(&mut self) -> bool {
        self.allow_at(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn connection_limiter_enforces_per_ip_and_releases_on_drop() {
        let limiter = ConnectionLimiter::new(3, 2);
        let g1 = limiter.try_acquire("203.0.113.10").expect("slot 1");
        let g2 = limiter.try_acquire("203.0.113.10").expect("slot 2");
        assert!(limiter.try_acquire("203.0.113.10").is_none());

        let g3 = limiter.try_acquire("203.0.113.11").expect("slot 3");
        // Limite globale atteinte
        assert!(limiter.try_acquire("203.0.113.12").is_none());
        assert_eq!(limiter.active_total(), 3);

        drop(g1);
        assert_eq!(limiter.active_total(), 2);
        let _g4 = limiter.try_acquire("203.0.113.10").expect("re-acquired");
        drop(g2);
        drop(g3);
    }

    #[test]
    fn extract_client_ip_uses_rightmost_xff_entry() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.1.1.1, 198.51.100.42"),
        );
        assert_eq!(extract_client_ip(&headers), "198.51.100.42");
    }

    #[test]
    fn frame_rate_limiter_resets_every_second() {
        let mut rl = FrameRateLimiter::new(2);
        let t0 = rl.window_start;
        assert!(rl.allow_at(t0));
        assert!(rl.allow_at(t0 + Duration::from_millis(100)));
        assert!(!rl.allow_at(t0 + Duration::from_millis(200)));
        assert!(rl.allow_at(t0 + Duration::from_secs(1)));
    }
}
