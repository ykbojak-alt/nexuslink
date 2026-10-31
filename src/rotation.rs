//! Provider/key rotation: the request loop's memory.
//!
//! Same trust model as the swarm's `llm_factory`: a failing key is
//! blacklisted for `BLACKLIST_TTL`, scoped so a dead key stops being tried
//! across every model on the same gateway, not just the one that caught
//! the error first. In-memory only — a restart is a clean slate, which is
//! honest: the blacklist records "this key failed *recently*", and 15
//! minutes later that may no longer be true.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// How long a key/model stays out of rotation after a classified failure.
pub const BLACKLIST_TTL: Duration = Duration::from_secs(15 * 60);

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
                && Self::hit_locked(&entries, &key_entry(url, api_key)))
    }

    /// Block the key for every model on this gateway (auth/quota failure).
    pub fn block_key(&self, url: &str, api_key: &str, reason: &str) {
        if api_key.is_empty() {
            return;
        }
        self.entries
            .write()
            .unwrap()
            .insert(key_entry(url, api_key), (Instant::now() + BLACKLIST_TTL, reason.to_string()));
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
}
