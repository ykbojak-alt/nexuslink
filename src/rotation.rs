//! Provider/key rotation: the request loop's memory.
//!
//! Same trust model as the swarm's `llm_factory`: a failing key is
//! blacklisted for `BLACKLIST_TTL`, scoped so a dead key stops being tried
//! across every model on the same gateway, not just the one that caught
//! the error first. In-memory only — a restart is a clean slate, which is
//! honest: the blacklist records "this key failed *recently*", and 15
//! minutes later that may no longer be true.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use std::time::SystemTime;

/// How long a key/model stays out of rotation after a classified failure.
pub const BLACKLIST_TTL: Duration = Duration::from_secs(15 * 60);
/// Fatal auth/quota reasons ("API key expired", "daily check-in required")
/// do not heal in 15 minutes — retrying on the generic TTL just churns the
/// same failure. Bench them for hours instead; a config edit or /api/status
/// blacklist reset still clears them early.
pub const AUTH_FATAL_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_AFTER_MIN: Duration = Duration::from_secs(1);
const RETRY_AFTER_MAX: Duration = Duration::from_secs(10 * 60);
const RATE_LIMIT_BENCH: &str = "429 Retry-After";

/// Counts active attempts per model candidate without retaining API secrets.
/// The candidate index is stable for one resolved model slot during a request.
#[derive(Debug, Clone, Default)]
pub struct ActiveLoads {
    entries: Arc<Mutex<HashMap<(String, usize), usize>>>,
}

#[derive(Debug)]
pub struct ActiveLoadGuard {
    entries: Arc<Mutex<HashMap<(String, usize), usize>>>,
    key: (String, usize),
}

impl ActiveLoads {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load(&self, slot: &str, candidate: usize) -> usize {
        *self.entries.lock().unwrap()
            .get(&(slot.to_string(), candidate)).unwrap_or(&0)
    }

    /// Atomically calculate an order from current loads and reserve its first
    /// candidate, so concurrent requests do not all pick the same idle key.
    pub fn order_and_acquire<F>(
        &self,
        slot: &str,
        order: F,
    ) -> (Vec<usize>, Option<ActiveLoadGuard>)
    where
        F: FnOnce(&HashMap<usize, usize>) -> Vec<usize>,
    {
        let mut entries = self.entries.lock().unwrap();
        let current: HashMap<usize, usize> = entries.iter()
            .filter_map(|((entry_slot, candidate), count)| {
                (entry_slot == slot).then_some((*candidate, *count))
            })
            .collect();
        let ordered = order(&current);
        let Some(candidate) = ordered.first().copied() else {
            return (ordered, None);
        };
        let key = (slot.to_string(), candidate);
        *entries.entry(key.clone()).or_default() += 1;
        (
            ordered,
            Some(ActiveLoadGuard { entries: Arc::clone(&self.entries), key }),
        )
    }

    pub fn acquire(&self, slot: &str, candidate: usize) -> ActiveLoadGuard {
        let key = (slot.to_string(), candidate);
        *self.entries.lock().unwrap().entry(key.clone()).or_default() += 1;
        ActiveLoadGuard { entries: Arc::clone(&self.entries), key }
    }
}

impl Drop for ActiveLoadGuard {
    fn drop(&mut self) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(count) = entries.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                entries.remove(&self.key);
            }
        }
    }
}

/// Last-N chars of the key, never the secret itself — same masking as
/// `_key_tag` in llm_factory. Entries identify keys without leaking them
/// into logs or the /api/status page.
pub fn key_tag(key: &str) -> String {
    let tail: String = key.chars().rev().take(6).collect();
    tail.chars().rev().collect()
}

/// Blacklist entry kinds, all under one map.
#[derive(Debug)]
pub struct Blacklist {
    /// entry -> (expiry, reason). Entries:
    ///   `key|{base_url}|{tag}`   — 401/402/403/429: balance/quota is
    ///                             per-key-per-gateway, so the block is
    ///                             scoped to the URL, valid for every model
    ///   `model|{url}|{model}`    — 404: this endpoint is dead, any key
    entries: RwLock<HashMap<String, (Instant, String)>>,
    /// Per-model active request counts used to select the least-loaded key.
    pub active_loads: ActiveLoads,
}

impl Default for Blacklist {
    fn default() -> Self {
        Self::new()
    }
}

impl Blacklist {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            active_loads: ActiveLoads::new(),
        }
    }

    fn hit_locked(entries: &HashMap<String, (Instant, String)>, entry: &str) -> bool {
        entries
            .get(entry)
            .is_some_and(|(expiry, _)| *expiry > Instant::now())
    }

    /// Is this key/model currently out of rotation?
    pub fn is_blocked(&self, url: &str, model: &str, api_key: &str) -> bool {
        let entries = self.entries.read().unwrap();
        Self::hit_locked(&entries, &model_entry(url, model))
            || (!api_key.is_empty()
                && entries
                    .get(&key_entry(url, api_key))
                    .is_some_and(|(expiry, reason)| *expiry > Instant::now() && reason != RATE_LIMIT_BENCH))
    }

    /// Block the key for every model on this gateway (auth/quota failure).
    pub fn block_key(&self, url: &str, api_key: &str, reason: &str) {
        if api_key.is_empty() {
            return;
        }
        let ttl = if auth_fatal(reason) {
            AUTH_FATAL_TTL
        } else {
            BLACKLIST_TTL
        };
        self.entries
            .write()
            .unwrap()
            .insert(key_entry(url, api_key), (Instant::now() + ttl, reason.to_string()));
    }

    /// Temporarily demote a rate-limited key. Unlike auth/quota blocks, a
    /// bench is still eligible when every candidate is cooling down.
    pub fn bench_key(&self, url: &str, api_key: &str, duration: Duration) {
        if api_key.is_empty() || duration.is_zero() {
            return;
        }
        self.entries.write().unwrap().insert(
            key_entry(url, api_key),
            (Instant::now() + duration, RATE_LIMIT_BENCH.to_string()),
        );
    }

    /// Remaining cooldown, used to order healthy keys ahead of benched keys.
    pub fn key_bench_remaining(&self, url: &str, api_key: &str) -> Option<Duration> {
        if api_key.is_empty() {
            return None;
        }
        let entries = self.entries.read().unwrap();
        let (expiry, reason) = entries.get(&key_entry(url, api_key))?;
        if reason != RATE_LIMIT_BENCH || *expiry <= Instant::now() {
            return None;
        }
        Some(expiry.saturating_duration_since(Instant::now()))
    }

    /// Block the whole (url, model) endpoint (404 dead route).
    pub fn block_model(&self, url: &str, model: &str, reason: &str) {
        self.entries
            .write()
            .unwrap()
            .insert(model_entry(url, model), (Instant::now() + BLACKLIST_TTL, reason.to_string()));
    }

    /// Live entries for the UI status panel: (masked entry, ttl seconds, reason).
    pub fn entries(&self) -> Vec<(String, f64, String)> {
        let now = Instant::now();
        let mut entries = self.entries.write().unwrap();
        entries.retain(|_, (expiry, _)| *expiry > now);
        let mut out: Vec<_> = entries
            .iter()
            .map(|(k, (expiry, reason))| {
                (
                    k.clone(),
                    expiry.saturating_duration_since(now).as_secs_f64(),
                    reason.clone(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Test helper: drop everything.
    pub fn reset(&self) {
        self.entries.write().unwrap().clear();
    }
}

/// Provider answers that mean "this key is dead until a human intervenes":
/// expired/revoked credentials, missing daily check-ins, billing blocks.
/// Matched on the sanitized upstream note, not on the status code alone —
/// a plain 403 Cloudflare page is not necessarily fatal.
fn auth_fatal(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    [
        "expired",
        "revoked",
        "invalid api key",
        "check-in required",
        "check in to",
        "insufficient_quota",
        "billing",
        "account disabled",
    ]
    .iter()
    .any(|needle| r.contains(needle))
}

fn key_entry(url: &str, api_key: &str) -> String {
    format!("key|{url}|{}", key_tag(api_key))
}

fn model_entry(url: &str, model: &str) -> String {
    format!("model|{url}|{model}")
}

/// Should this upstream outcome move the pointer to the next candidate?
///
/// Rotate on everything that is plausibly key/provider-specific: auth &
/// quota (401/402/403/429), dead endpoints (404), throttling and server
/// faults (5xx, transport errors). A plain 400 after the rectifiers ran
/// out means the *request body itself* is bad — every provider will
/// reject the same shape, so rotating would just multiply the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Try the next candidate (and what to blacklist, if anything).
    Rotate(RotateReason),
    /// Hand this error to the client verbatim — rotating can't help.
    Return,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateReason {
    /// 401/402/403/429 — block this key on this gateway.
    Key,
    /// 404 — the (url, model) endpoint is dead.
    Model,
    /// 5xx / transport error / timeout — nothing to blacklist, just move on.
    Transient,
}

pub fn classify(status: u16) -> Verdict {
    match status {
        401 | 402 | 403 | 429 => Verdict::Rotate(RotateReason::Key),
        404 => Verdict::Rotate(RotateReason::Model),
        s if s >= 500 => Verdict::Rotate(RotateReason::Transient),
        _ => Verdict::Return,
    }
}

/// Transport-level failure (DNS, TCP, TLS, timeout): rotate, nothing to block.
pub fn classify_transport() -> Verdict {
    Verdict::Rotate(RotateReason::Transient)
}

/// Parse RFC Retry-After delta-seconds or HTTP-date. Valid upstream values
/// are bounded to 1s..10m; malformed/missing values use the ordinary TTL.
pub fn retry_after(value: Option<&str>, now: SystemTime, fallback: Duration) -> Duration {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return fallback;
    };
    let duration = if let Ok(seconds) = value.parse::<u64>() {
        Duration::from_secs(seconds)
    } else if let Ok(at) = httpdate::parse_http_date(value) {
        at.duration_since(now).unwrap_or(Duration::ZERO)
    } else {
        return fallback;
    };
    duration.max(RETRY_AFTER_MIN).min(RETRY_AFTER_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_key_is_scoped_to_gateway() {
        let bl = Blacklist::new();
        bl.block_key("https://a/v1", "sk-secret-zzz", "402");
        assert!(bl.is_blocked("https://a/v1", "model-x", "sk-secret-zzz"));
        // the same dead key must not be retried under another model name
        assert!(bl.is_blocked("https://a/v1", "model-y", "sk-secret-zzz"));
        // another gateway: same key string is a different account entirely
        assert!(!bl.is_blocked("https://b/v1", "model-x", "sk-secret-zzz"));
        // another key on the same gateway stays usable
        assert!(!bl.is_blocked("https://a/v1", "model-x", "sk-other-111"));
    }

    #[test]
    fn blocked_model_covers_every_key() {
        let bl = Blacklist::new();
        bl.block_model("https://a/v1", "model-x", "404");
        assert!(bl.is_blocked("https://a/v1", "model-x", "sk-a"));
        assert!(bl.is_blocked("https://a/v1", "model-x", "sk-b"));
        assert!(!bl.is_blocked("https://a/v1", "model-y", "sk-a"));
    }

    #[test]
    fn classify_maps_auth_quota_and_endpoint_errors_to_rotation() {
        for s in [401, 402, 403, 429] {
            assert_eq!(classify(s), Verdict::Rotate(RotateReason::Key));
        }
        assert_eq!(classify(404), Verdict::Rotate(RotateReason::Model));
        assert_eq!(classify(500), Verdict::Rotate(RotateReason::Transient));
        assert_eq!(classify(503), Verdict::Rotate(RotateReason::Transient));
        assert_eq!(classify(400), Verdict::Return);
        assert_eq!(classify(422), Verdict::Return);
    }

    #[test]
    fn key_tag_masks_but_distinguishes() {
        assert_eq!(key_tag("sk-abcdef"), "abcdef");
        assert_ne!(key_tag("sk-aaaaaa"), key_tag("sk-bbbbbb"));
    }

    #[test]
    fn retry_after_parser_supports_seconds_dates_and_clamps() {
        let now = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(retry_after(Some("60"), now, BLACKLIST_TTL), Duration::from_secs(60));
        assert_eq!(retry_after(Some("0"), now, BLACKLIST_TTL), Duration::from_secs(1));
        assert_eq!(retry_after(Some("999999"), now, BLACKLIST_TTL), Duration::from_secs(600));
        let date = httpdate::fmt_http_date(now + Duration::from_secs(45));
        assert_eq!(retry_after(Some(&date), now, BLACKLIST_TTL), Duration::from_secs(45));
        assert_eq!(retry_after(Some("nonsense"), now, BLACKLIST_TTL), BLACKLIST_TTL);
    }

    #[test]
    fn rate_limited_key_is_benched_but_not_hard_blocked() {
        let bl = Blacklist::new();
        bl.bench_key("https://a/v1", "sk-a", Duration::from_secs(60));
        assert!(!bl.is_blocked("https://a/v1", "model", "sk-a"));
        assert!(bl.key_bench_remaining("https://a/v1", "sk-a").is_some());
        assert!(!bl.is_blocked("https://a/v1", "model", "sk-b"));
        bl.block_key("https://a/v1", "sk-b", "401");
        assert!(bl.is_blocked("https://a/v1", "model", "sk-b"));
    }
}
