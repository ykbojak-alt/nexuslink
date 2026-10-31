//! Configuration model, persistence, and slot resolution.
//!
//! Config lives at `~/.nexuslink/config.json` (pre-rename `~/.openbridge`. API keys are stored in
//! plaintext (chmod 0600 on unix) — same trust model as SSH keys: the file
//! is only as safe as the account that owns it.
//!
//! Slot names are *borrowed Anthropic catalog IDs*: Claude Desktop validates
//! `inferenceModels` against its own model catalogue, so only real Claude
//! names pass. The mapping `slot -> (provider, real model)` is stored per
//! model entry, so adding/removing models never reshuffles what Claude sees.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Pool of slot names offered to Claude, ordered by capability tier.
/// These must be real Anthropic catalogue IDs to pass Desktop validation.
pub const SLOT_POOL: &[&str] = &[
    "claude-sonnet-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-sonnet-4-5",
    "claude-haiku-4-5",
];

pub const DEFAULT_PORT: u16 = 5679;
/// SSE keep-alive interval; well under Claude Desktop's ~5min dead-connection kill.
pub const HEARTBEAT_SECS: u64 = 15;
/// Request body cap.
pub const MAX_BODY: usize = 10 * 1024 * 1024;
/// Max per-request upstream retries after a rectifier rewrites the body.
pub const MAX_RECTIFY: usize = 2;

fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Config {
    /// Bearer token the client (Claude Desktop) must send to us.
    /// Phase-1 security fix: ModelLink had none; we reject anything else.
    #[serde(default)]
    pub incoming_token: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub providers: Vec<Provider>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Provider {
    /// Display name ("OpenRouter", "Ollama", ...).
    #[serde(default)]
    pub name: String,
    /// OpenAI-compatible base URL ("https://openrouter.ai/api", "http://localhost:11434/v1"…).
    /// The proxy appends "/v1/chat/completions" — or uses `chat_path` when set.
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// Key pool for rotation: every key gets its own quota/balance, so a
    /// 401/402/403/429 on one key moves the request to the next. `api_key`
    /// stays for backward compat and is treated as the first pool member.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Optional override for the chat-completions path (e.g. providers that mount
    /// the endpoint at "/chat/completions" without a /v1 prefix). Empty = "/v1/chat/completions".
    #[serde(default)]
    pub chat_path: String,
    /// Send `reasoning_effort` to this provider. Many OpenAI-compat endpoints 400
    /// on unknown fields — opt in per provider.
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    /// Send `max_completion_tokens` (newer OpenAI semantics) instead of `max_tokens`.
    /// Some compat providers only accept one of the two.
    #[serde(default = "default_true")]
    pub use_max_completion_tokens: bool,
    /// Sliding-window compaction when a request nears the model's `context`
    /// window: oldest turns are dropped (tool exchanges atomically) and a
    /// marker line says what was cut. Disable per provider to get the raw
    /// upstream 400 instead.
    #[serde(default = "default_true")]
    pub auto_compact: bool,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct ModelEntry {
    /// Real upstream model id ("meta-llama/llama-3.1-70b-instruct").
    #[serde(default)]
    pub name: String,
    /// The Claude-catalog slot this model answers to. Stored, not positional,
    /// so edits never reshuffle other models' slots.
    #[serde(default)]
    pub slot: String,
    /// Upstream context window in tokens. 0 = unknown — no auto-compact is
    /// attempted and an oversized request gets the honest upstream error.
    /// The model-catalog fetch in the panel prefills this when known.
    #[serde(default)]
    pub context: u64,
}

/// Result of resolving a slot name against the config.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// Real upstream model id.
    pub model: String,
    /// Full chat-completions URL.
    pub url: String,
    /// Raw provider base URL — the gateway identity. Blacklist entries are
    /// scoped to it (a key's balance belongs to the account on this host,
    /// not to one particular path under it).
    pub base_url: String,
    pub api_key: String,
    /// Provider display name — needed by the rotation log and UI status.
    pub provider: String,
    pub supports_reasoning_effort: bool,
    pub use_max_completion_tokens: bool,
    /// Model's context window in tokens (0 = unknown).
    pub context: u64,
    /// Provider's auto-compact flag.
    pub auto_compact: bool,
}

#[derive(Debug)]
pub enum ResolveError {
    /// Slot name isn't mapped to any provider model — never silently substitute.
    UnmappedSlot(String),
}

/// Effective key pool of a provider: `api_keys` plus the legacy `api_key`
/// when it isn't already listed — order preserved, first key first.
pub fn key_pool(p: &Provider) -> Vec<String> {
    let mut keys: Vec<String> = p
        .api_keys
        .iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    let legacy = p.api_key.trim();
    if !legacy.is_empty() && !keys.iter().any(|k| k == legacy) {
        keys.insert(0, legacy.to_string());
    }
    keys
}

fn provider_chat_url(p: &Provider) -> String {
    let base = p.base_url.trim_end_matches('/');
    if !p.chat_path.is_empty() {
        return format!("{}{}", base, p.chat_path);
    }
    format!("{}/v1/chat/completions", base)
}

/// Normalized slot name: `claude-sonnet-5-20260101` -> `claude-sonnet-5`
/// when the date-suffixed form isn't mapped directly.
fn normalized_slot(slot: &str, config: &Config) -> String {
    if config
        .providers
        .iter()
        .flat_map(|p| p.models.iter())
        .any(|m| m.slot == slot)
    {
        return slot.to_string();
    }
    strip_date_suffix(slot)
        .map(str::to_string)
        .unwrap_or_else(|| slot.to_string())
}

/// slot ("claude-sonnet-5", optionally "-YYYYMMDD" suffixed) -> upstream routing.
/// Returns the FIRST candidate — the full rotation chain lives in
/// `resolve_candidates`; this stays for single-target call sites and tests.
pub fn resolve_model(slot: &str, config: &Config) -> Result<Resolved, ResolveError> {
    let wanted = normalized_slot(slot, config);
    resolve_candidates(slot, config)
        .into_iter()
        .next()
        .ok_or(ResolveError::UnmappedSlot(wanted))
}

/// slot -> the full ordered rotation chain.
///
/// Every provider model mapped to this slot contributes one candidate per
/// pool key, in config order: `[(provider A, model X, key 1..N),
/// (provider B, model Y, key 1..N), …]`. The request loop walks it left to
/// right — an auth/quota/endpoint failure on one key never burns the next
/// provider's quota, it just moves the pointer.
///
/// Providers without keys still get one candidate (keyless upstreams like
/// local Ollama legitimately have an empty pool).
pub fn resolve_candidates(slot: &str, config: &Config) -> Vec<Resolved> {
    let wanted = normalized_slot(slot, config);
    let mut out = Vec::new();
    for p in &config.providers {
        for m in &p.models {
            if m.slot != wanted || m.name.is_empty() {
                continue;
            }
            let url = provider_chat_url(p);
            let pool = key_pool(p);
            let keys: Vec<String> = if pool.is_empty() {
                vec![String::new()]
            } else {
                pool
            };
            for api_key in keys {
                out.push(Resolved {
                    model: m.name.clone(),
                    url: url.clone(),
                    base_url: p.base_url.clone(),
                    api_key,
                    provider: p.name.clone(),
                    supports_reasoning_effort: p.supports_reasoning_effort,
                    use_max_completion_tokens: p.use_max_completion_tokens,
                    context: m.context,
                    auto_compact: p.auto_compact,
                });
            }
        }
    }
    out
}

/// `claude-sonnet-5-20260101` -> `claude-sonnet-5`.
fn strip_date_suffix(name: &str) -> Option<&str> {
    let (head, tail) = name.rsplit_once('-')?;
    (tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit())).then_some(head)
}

/// Flattened (slot, display name) pairs for /v1/models and gateway writing.
pub fn flat_slots(config: &Config) -> Vec<(String, String)> {
    config
        .providers
        .iter()
        .flat_map(|p| p.models.iter())
        .filter(|m| !m.name.is_empty() && !m.slot.is_empty())
        .take(SLOT_POOL.len())
        .map(|m| (m.slot.clone(), m.name.clone()))
        .collect()
}

/// Models whose slot is unset/invalid get the first free name from the pool.
pub fn normalize_slots(config: &mut Config) {
    let mut used = std::collections::HashSet::new();
    for m in config.providers.iter_mut().flat_map(|p| p.models.iter_mut()) {
        if m.name.is_empty() || !SLOT_POOL.contains(&m.slot.as_str()) || !used.insert(m.slot.clone()) {
            m.slot.clear();
        }
    }
    let mut free = SLOT_POOL.iter().filter(|s| !used.contains(**s));
    for m in config.providers.iter_mut().flat_map(|p| p.models.iter_mut()) {
        if m.name.is_empty() || !m.slot.is_empty() {
            continue;
        }
        if let Some(s) = free.next() {
            m.slot = s.to_string();
        }
    }
}

/// Home dir for config purposes. On Windows `USERPROFILE` wins: Git Bash
/// exports HOME=/c/Users/…, which is an MSYS path Rust would resolve as a
/// bogus drive-relative directory. On unix HOME is the canonical source.
fn home_dir() -> String {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| ".".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into())
    }
}

/// Write path — always the NexusLink dir. Reads may come from the legacy
/// `.openbridge` dir (see `load`); the next save then lands in the new
/// home and the old file is left untouched, not destroyed.
pub fn config_path() -> PathBuf {
    PathBuf::from(home_dir()).join(".nexuslink").join("config.json")
}

/// Pre-rename config location, honored on read only.
fn legacy_config_path() -> PathBuf {
    PathBuf::from(home_dir()).join(".openbridge").join("config.json")
}

pub fn load() -> Config {
    let path = {
        let new = config_path();
        if new.exists() {
            new
        } else {
            let old = legacy_config_path();
            if old.exists() { old } else { new }
        }
    };
    let mut cfg: Config = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        Config::default()
    };
    // A fresh/absent config derives port 0 — bind the real default instead
    // of letting the OS pick a random port nobody can find.
    if cfg.port == 0 {
        cfg.port = DEFAULT_PORT;
    }
    normalize_slots(&mut cfg);
    cfg
}

pub fn save(config: &Config) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let data = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &data).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// First-run config with a generated incoming token.
pub fn scaffold() -> Config {
    let mut cfg = Config {
        incoming_token: generate_token(),
        port: DEFAULT_PORT,
        providers: Vec::new(),
    };
    normalize_slots(&mut cfg);
    cfg
}

fn generate_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Not cryptographic-grade, but the token is also stored plaintext — a real
    // secret manager is out of scope for a loopback gateway. Mix in pid+nanos
    // for uniqueness and shape it like an API key so users recognise it.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    format!("nl-{:032x}", nanos ^ (pid << 64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            incoming_token: "t".into(),
            providers: vec![Provider {
                base_url: "https://openrouter.ai/api".into(),
                api_key: "k".into(),
                models: vec![ModelEntry {
                    name: "meta-llama/llama-3.1-70b".into(),
                    slot: "claude-sonnet-5".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn resolves_slot_to_upstream_model_and_url() {
        let r = resolve_model("claude-sonnet-5", &cfg()).unwrap();
        assert_eq!(r.model, "meta-llama/llama-3.1-70b");
        assert_eq!(r.url, "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(r.api_key, "k");
    }

    #[test]
    fn date_suffixed_slot_resolves_to_same_mapping() {
        let r = resolve_model("claude-sonnet-5-20260101", &cfg()).unwrap();
        assert_eq!(r.model, "meta-llama/llama-3.1-70b");
    }

    #[test]
    fn unmapped_slot_is_an_error_not_a_fallback() {
        match resolve_model("claude-opus-5", &cfg()) {
            Err(ResolveError::UnmappedSlot(s)) => assert_eq!(s, "claude-opus-5"),
            _ => panic!("expected UnmappedSlot"),
        }
    }

    #[test]
    fn normalize_assigns_pool_names_in_order() {
        let mut c = cfg();
        c.providers[0].models[0].slot.clear();
        normalize_slots(&mut c);
        assert_eq!(c.providers[0].models[0].slot, "claude-sonnet-5");
    }

    #[test]
    fn custom_chat_path_overrides_default() {
        let mut c = cfg();
        c.providers[0].chat_path = "/chat/completions".into();
        let r = resolve_model("claude-sonnet-5", &c).unwrap();
        assert_eq!(r.url, "https://openrouter.ai/api/chat/completions");
    }
}
