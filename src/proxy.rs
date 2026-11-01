//! The axum server: inbound auth, slot resolution, upstream forwarding,
//! SSE bridging with heartbeat injection.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
    routing::{any, get, post},
    Router,
};
use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use crate::config::{
    flat_slots, is_agent_alias, resolve_candidates, Config, NUEKeyStore, NUEUpstream, Resolved,
    SchedulingMode, AGENT_ALIASES,
    HEARTBEAT_SECS, MAX_RECTIFY,
};
use crate::rotation::{self, ActiveLoadGuard, Blacklist, RotateReason, Verdict};
use crate::streaming::StreamConverter;
use crate::translator::{anthropic_to_openai, openai_to_anthropic, MessagesRequest};

pub struct AppState {
    pub config: RwLock<Config>,
    pub client: reqwest::Client,
    /// Rotation memory: recently-failed keys/models, shared with the UI
    /// status endpoint so the panel can show what is currently parked.
    pub blacklist: Blacklist,
    /// Candidate cursor per slot, used by models configured as balanced.
    pub cursor: RwLock<HashMap<String, usize>>,
    /// Per-candidate attempt stats (ok/fail/latency), surfaced via /api/status.
    pub metrics: crate::metrics::Metrics,
    pub started: std::time::Instant,
    /// NUE client key store (persisted alongside config).
    pub nue_keys: RwLock<NUEKeyStore>,
    /// NUE upstream config (ai2 host).
    pub nue_upstream: RwLock<NUEUpstream>,
}

/// Shared state for the `serve` command and the desktop shell: the same
/// builder is used whether the runtime lives in a console or behind the
/// WebView2 window.
pub fn state_from_config(mut cfg: Config) -> Arc<AppState> {
    crate::config::normalize_slots(&mut cfg);
    let nue_keys = NUEKeyStore::load();
    let nue_upstream = cfg.nue_upstream.clone();
    Arc::new(AppState {
        config: RwLock::new(cfg),
        client: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("failed to build http client"),
        blacklist: Blacklist::new(),
        cursor: RwLock::new(HashMap::new()),
        metrics: crate::metrics::Metrics::new(),
        started: std::time::Instant::now(),
        nue_keys: RwLock::new(nue_keys),
        nue_upstream: RwLock::new(nue_upstream),
    })
}

pub fn router(state: Arc<AppState>) -> Router {
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .allow_credentials(false)
        .expose_headers([header::CONTENT_TYPE]);

    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/messages", post(messages))
        .route("/health", get(|| async { "ok" }))
        .merge(crate::ui::router())
        .layer(cors)
        .with_state(state)
}

/// NUE passthrough gateway — served on `config.nue_port`, a separate
/// listener from the Anthropic gateway so `/v1/*` paths mean the upstream
/// host's API here (not our slot API). NUE clients authenticate with
/// nue_* client keys; the gateway swaps them for the upstream AI-2 key —
/// the upstream secret never leaves this process.
pub fn nue_router(state: Arc<AppState>) -> Router {
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([Method::GET, Method::POST, Method::PUT,
                        Method::DELETE, Method::PATCH, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE,
                        header::HeaderName::from_static("x-register-token")])
        .allow_credentials(false)
        .expose_headers([header::CONTENT_TYPE]);

    Router::new()
        .route("/nue/register", post(nue_register))
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/{*path}", any(nue_passthrough))
        .layer(cors)
        .with_state(state)
}

fn nue_gen_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static CTR: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mix = nanos
        ^ ((std::process::id() as u128) << 64)
        ^ ((CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128) << 32);
    format!("nue_{mix:032x}")
}

/// POST /nue/register — issues a nue_* client key, persists it in the
/// key store, returns {"key": ...}. Guarded by config.nue_register_token
/// when set (X-Register-Token header); empty token = open registration.
async fn nue_register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let want = state.config.read().unwrap().nue_register_token.clone();
    if !want.is_empty() {
        let got = headers
            .get("x-register-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if got != want {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid register token" })),
            )
                .into_response();
        }
    }
    let key = nue_gen_key();
    let save_err = {
        let mut ks = state.nue_keys.write().unwrap();
        ks.add_key(key.clone());
        ks.save().err()
    };
    if let Some(e) = save_err {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("persist key: {e}") })),
        )
            .into_response();
    }
    (StatusCode::OK, Json(json!({ "key": key }))).into_response()
}

/// Prefixes that must never transit the client surface — key management
/// and management/destructive routes stay operator-side even though the
/// upstream key's own scopes would also refuse them.
const NUE_DENY_PREFIXES: &[&str] = &["/v1/keys", "/_bus", "/v1/startup"];

/// ANY /v1/{*path} — validate the nue_* client key, swap Bearer for the
/// configured upstream key, stream the upstream response back (covers
/// both JSON and SSE without buffering).
async fn nue_passthrough(
    State(state): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let key = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|a| a.strip_prefix("Bearer "))
        .unwrap_or("");
    if !state.nue_keys.read().unwrap().validate(key) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid or missing nue_* client key" })),
        )
            .into_response();
    }
    let path = uri.path();
    if !path.starts_with("/v1/")
        || NUE_DENY_PREFIXES.iter().any(|p| path.starts_with(p))
    {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "not found" })),
        )
            .into_response();
    }
    let up = state.nue_upstream.read().unwrap().clone();
    if up.base_url.trim().is_empty() || up.api_key.trim().is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "nue upstream not configured" })),
        )
            .into_response();
    }
    let base = up.base_url.trim_end_matches('/');
    let url = match uri.query() {
        Some(q) => format!("{base}{path}?{q}"),
        None => format!("{base}{path}"),
    };
    let mut req = state
        .client
        .request(method, url)
        .bearer_auth(up.api_key.trim())
        .body(body);
    if let Some(ct) = headers.get(header::CONTENT_TYPE) {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            let ct = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let mut r = Response::new(Body::from_stream(resp.bytes_stream()));
            *r.status_mut() = status;
            if let Some(ct) = ct {
                if let Ok(v) = HeaderValue::from_str(&ct) {
                    r.headers_mut().insert(header::CONTENT_TYPE, v);
                }
            }
            r
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("upstream: {e}") })),
        )
            .into_response(),
    }
}

fn anthropic_error(status: StatusCode, etype: &str, msg: &str) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": { "type": etype, "message": msg }
        })),
    )
        .into_response()
}

/// Phase-1 security fix: the inbound caller must authenticate.
/// ModelLink accepted anything on the loopback port; we require
/// `Authorization: Bearer <token>` or `x-api-key: <token>` matching config.
/// pub(crate): the /api management endpoints use the same token.
pub(crate) fn check_auth(headers: &HeaderMap, expected: &str) -> bool {
    if expected.is_empty() {
        return false; // token not configured = closed for business, never open
    }
    if let Some(v) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return constant_eq(v.trim(), expected);
    }
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return constant_eq(v.trim(), expected);
    }
    false
}

/// Length-then-byte compare; not a formal constant-time guarantee but avoids
/// early-exit per-byte oracles for a loopback-scoped token.
fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).all(|(x, y)| x == y)
}

/// GET /v1/models — the slot list Claude's picker reads.
async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let config = state.config.read().unwrap().clone();
    if !check_auth(&headers, &config.incoming_token) {
        return anthropic_error(StatusCode::UNAUTHORIZED, "authentication_error", "invalid api key");
    }
    let mut data: Vec<Value> = flat_slots(&config)
        .into_iter()
        .map(|(slot, name)| json!({ "id": slot, "display_name": name, "created": 0 }))
        .collect();
    for (alias, persona) in AGENT_ALIASES {
        if !resolve_candidates(alias, &config).is_empty() {
            data.push(json!({
                "id": alias,
                "display_name": format!("{persona} ranked model pool"),
                "created": 0,
            }));
        }
    }
    Json(json!({ "data": data })).into_response()
}

/// POST /v1/messages — the whole point of the program.
async fn messages(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, request_body) = request.into_parts();
    let headers = parts.headers;
    let config = state.config.read().unwrap().clone();
    let mut request_body = request_body.into_data_stream();
    let mut body_bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut request_body).await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                return anthropic_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "failed to read request body",
                )
            }
        };
        if body_bytes.len().saturating_add(chunk.len()) > config.max_body_bytes {
            return anthropic_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "body too large");
        }
        body_bytes.extend_from_slice(&chunk);
    }
    let body = Bytes::from(body_bytes);
    if !check_auth(&headers, &config.incoming_token) {
        return anthropic_error(StatusCode::UNAUTHORIZED, "authentication_error", "invalid api key");
    }

    let req: MessagesRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON: {e}"),
            )
        }
    };

    let slot = req.model.clone();
    let mut candidates = resolve_candidates(&slot, &config);
    if candidates.is_empty() && !config.fallback_slot.is_empty() {
        // Unmapped slot (e.g. Claude Code background haiku/sonnet traffic):
        // serve it from the configured fallback slot's pool instead of
        // hard-failing — the operator opted into this in config.json.
        candidates = resolve_candidates(&config.fallback_slot, &config);
        if !candidates.is_empty() {
            tracing::info!(
                slot = %slot, fallback = %config.fallback_slot,
                "unmapped slot routed via fallback_slot"
            );
        }
    }
    if candidates.is_empty() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("nexuslink: model slot {slot} is not mapped to any provider"),
        );
    }
    let is_stream = req.stream.unwrap_or(false);

    // Failover is provider-priority first; balanced prefers the least-loaded
    // provider. Within either mode, least-loaded keys go first and active
    // Retry-After cooldowns are excluded.
    // 400-class request errors are returned verbatim — every provider would
    // reject the same body, so rotating them just multiplies the failure.
    let n = candidates.len();
    let mode = if is_agent_alias(&slot) {
        // Persona aliases have a benchmark-ranked model chain. Balanced mode
        // may spread load within each model/provider tier, but must not move a
        // lower-ranked tier ahead of the primary.
        SchedulingMode::Failover
    } else {
        candidates[0].scheduling_mode
    };
    let cursor = {
        let cursors = state.cursor.read().unwrap();
        cursors.get(&slot).copied().unwrap_or(0) % n
    };
    let (order, mut first_load) = state.blacklist.active_loads.order_and_acquire(
        &slot,
        |loads| candidate_order(&candidates, cursor, mode, &state.blacklist, loads),
    );
    let mut skipped = candidates.len().saturating_sub(order.len());
    let mut failed: Vec<String> = Vec::new();
    let mut last_error: Option<Response> = None;
    for candidate_index in order {
        let cand = &candidates[candidate_index];
        let load_guard = first_load.take().unwrap_or_else(|| {
            state.blacklist.active_loads.acquire(&slot, candidate_index)
        });
        if state
            .blacklist
            .is_blocked(&cand.base_url, &cand.model, &cand.api_key)
            || state.blacklist.key_bench_remaining(&cand.base_url, &cand.api_key).is_some()
        {
            skipped += 1;
            drop(load_guard);
            continue;
        }
        let t0 = std::time::Instant::now();
        match try_candidate(&state, &req, cand).await {
            Attempt::Success(resp) => {
                state.metrics.record(&cand.provider, &cand.model, &cand.api_key,
                                     true, t0.elapsed().as_millis() as u64, "");
                let model = cand.model.clone();
                // Advance a monotonic per-slot cursor. Failover uses it only
                // to rotate keys within the primary provider; balanced uses
                // it for both provider affinity and key-pool rotation.
                state
                    .cursor
                    .write()
                    .unwrap()
                    .insert(slot.clone(), cursor.wrapping_add(1));
                return if is_stream {
                    bridge_stream(resp, &model, &slot, load_guard)
                } else {
                    let translated = translate_body(resp, &slot).await;
                    drop(load_guard);
                    translated
                };
            }
            Attempt::Rotate(reason, note, cooldown) => {
                state.metrics.record(&cand.provider, &cand.model, &cand.api_key,
                                     false, t0.elapsed().as_millis() as u64, &note);
                match reason {
                    RotateReason::Key => {
                        if let Some(duration) = cooldown {
                            state.blacklist.bench_key(&cand.base_url, &cand.api_key, duration)
                        } else {
                            state.blacklist.block_key(&cand.base_url, &cand.api_key, &note)
                        }
                    }
                    RotateReason::Model => {
                        state
                            .blacklist
                            .block_model(&cand.base_url, &cand.model, &note)
                    }
                    RotateReason::Transient => {}
                }
                drop(load_guard);
                tracing::info!(
                    slot = %slot, provider = %cand.provider, model = %cand.model,
                    key = %rotation::key_tag(&cand.api_key),
                    "rotation: {note} → next candidate"
                );
                failed.push(format!("{} · {} · …{}: {note}",
                    cand.provider, cand.model, rotation::key_tag(&cand.api_key)));
            }
            Attempt::Return(resp) => {
                drop(load_guard);
                state.metrics.record(&cand.provider, &cand.model, &cand.api_key,
                                     false, t0.elapsed().as_millis() as u64,
                                     &format!("upstream HTTP {}", resp.status().as_u16()));
                last_error = Some(resp);
                break;
            }
        }
    }

    if let Some(resp) = last_error {
        return resp;
    }
    // Every candidate is now blocked or cooling down — whether the limits
    // hit during this request or the keys were already parked. When the
    // soonest bench expiry is known the honest answer is 429 + Retry-After:
    // the client backs off instead of hammering 502s (which is what
    // produced the reconnect storm during slot starvation).
    {
        let soonest = candidates
            .iter()
            .filter_map(|c| {
                state
                    .blacklist
                    .key_bench_remaining(&c.base_url, &c.api_key)
            })
            .min();
        if let Some(remaining) = soonest {
            let secs = remaining.as_secs().max(1);
            let mut resp = anthropic_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                &format!(
                    "nexuslink: all {} candidate(s) for {slot} are cooling down \
                     after upstream rate limits — retry in {secs}s",
                    skipped + failed.len()
                ),
            );
            resp.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_str(&secs.to_string())
                    .unwrap_or_else(|_| HeaderValue::from_static("60")),
            );
            return resp;
        }
    }
    let detail = if failed.is_empty() {
        format!("{skipped} candidate(s) are unavailable (blacklisted or cooling down)")
    } else {
        format!("{} attempt(s) failed — {}", failed.len(), failed.join(" | "))
    };
    anthropic_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        &format!("nexuslink: all candidates exhausted for {slot} — {detail}"),
    )
}

fn candidate_order(
    candidates: &[Resolved],
    cursor: usize,
    mode: SchedulingMode,
    blacklist: &Blacklist,
    loads: &HashMap<usize, usize>,
) -> Vec<usize> {
    let n = candidates.len();
    if n == 0 {
        return Vec::new();
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for index in 0..n {
        let candidate = &candidates[index];
        if candidate.url.is_empty()
            || blacklist.is_blocked(&candidate.base_url, &candidate.model, &candidate.api_key)
            || blacklist.key_bench_remaining(&candidate.base_url, &candidate.api_key).is_some()
        {
            continue;
        }
        let same_provider = groups.last().and_then(|group| group.first()).is_some_and(|first| {
            candidates[*first].provider == candidate.provider
                && candidates[*first].base_url == candidate.base_url
                && candidates[*first].route_priority == candidate.route_priority
        });
        if same_provider {
            groups.last_mut().unwrap().push(index);
        } else {
            groups.push(vec![index]);
        }
    }
    if groups.is_empty() {
        return Vec::new();
    }
    let first_group = match mode {
        SchedulingMode::Failover => 0,
        SchedulingMode::Balanced => cursor % groups.len(),
    };
    let mut ordered_groups = Vec::with_capacity(groups.len());
    for offset in 0..groups.len() {
        let group = &groups[(first_group + offset) % groups.len()];
        let key_start = cursor % group.len();
        let mut keys: Vec<usize> = (0..group.len())
            .map(|key_offset| group[(key_start + key_offset) % group.len()])
            .collect();
        // Stable sorting keeps cursor rotation as the tie-breaker while
        // preferring the key with the fewest active requests.
        keys.sort_by_key(|index| loads.get(index).copied().unwrap_or(0));
        ordered_groups.push(keys);
    }
    if mode == SchedulingMode::Balanced {
        // Prefer the provider with the least active candidate load; the
        // cursor-rotated order breaks ties.
        ordered_groups.sort_by_key(|group| {
            group.iter().map(|index| loads.get(index).copied().unwrap_or(0)).sum::<usize>()
        });
    }
    ordered_groups.into_iter().flatten().collect()
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;
    use crate::config::SchedulingMode;
    use std::time::Duration;

    fn candidate(provider: &str, key: &str) -> Resolved {
        Resolved {
            model: "model".into(),
            url: "https://example.test/v1/chat/completions".into(),
            base_url: "https://example.test".into(),
            api_key: key.into(),
            provider: provider.into(),
            supports_reasoning_effort: false,
            use_max_completion_tokens: true,
            context: 0,
            auto_compact: true,
            scheduling_mode: SchedulingMode::Failover,
            route_priority: 0,
        }
    }

    #[test]
    fn failover_starts_primary_balanced_rotates_from_cursor_and_excludes_benched_keys() {
        let mut candidates = vec![candidate("primary", "a"), candidate("primary", "b"), candidate("backup", "c")];
        let blacklist = Blacklist::new();
        let loads = HashMap::new();
        assert_eq!(candidate_order(&candidates, 2, SchedulingMode::Failover, &blacklist, &loads), vec![0, 1, 2]);
        assert_eq!(candidate_order(&candidates, 1, SchedulingMode::Balanced, &blacklist, &loads), vec![2, 1, 0]);

        blacklist.bench_key(&candidates[0].base_url, "a", Duration::from_secs(60));
        assert_eq!(candidate_order(&candidates, 0, SchedulingMode::Failover, &blacklist, &loads), vec![1, 2]);
        candidates[1].api_key.clear();
        assert_eq!(candidate_order(&candidates, 0, SchedulingMode::Failover, &Blacklist::new(), &loads), vec![0, 1, 2]);
    }

    #[test]
    fn balanced_spreads_provider_starts_even_when_key_pool_sizes_differ() {
        let candidates = vec![candidate("primary", "a"), candidate("primary", "b"), candidate("backup", "c")];
        let blacklist = Blacklist::new();
        let loads = HashMap::new();
        let first = candidate_order(&candidates, 0, SchedulingMode::Balanced, &blacklist, &loads);
        let second = candidate_order(&candidates, 1, SchedulingMode::Balanced, &blacklist, &loads);
        let third = candidate_order(&candidates, 2, SchedulingMode::Balanced, &blacklist, &loads);
        assert_eq!([first[0], second[0], third[0]], [0, 2, 0]);
        assert_eq!(candidate_order(&candidates, 1, SchedulingMode::Failover, &blacklist, &loads)[0], 1);
    }

    #[test]
    fn failover_chooses_least_loaded_key_within_primary_provider() {
        let candidates = vec![candidate("primary", "a"), candidate("primary", "b"), candidate("backup", "c")];
        let blacklist = Blacklist::new();
        let loads = crate::rotation::ActiveLoads::new();
        let (order, active) = loads.order_and_acquire("slot", |counts| {
            candidate_order(&candidates, 0, SchedulingMode::Failover, &blacklist, counts)
        });

        assert_eq!(order, vec![0, 1, 2]);
        let active = active.unwrap();
        let (next_order, next_active) = loads.order_and_acquire("slot", |counts| {
            candidate_order(&candidates, 0, SchedulingMode::Failover, &blacklist, counts)
        });
        assert_eq!(next_order, vec![1, 0, 2]);
        drop(next_active);
        drop(active);
        assert_eq!(loads.load("slot", 0), 0);
    }

    #[test]
    fn balanced_prefers_provider_with_lower_active_candidate_load() {
        let candidates = vec![candidate("primary", "a"), candidate("backup", "b")];
        let blacklist = Blacklist::new();
        let loads = HashMap::from([(0, 1)]);
        assert_eq!(candidate_order(&candidates, 0, SchedulingMode::Balanced, &blacklist, &loads), vec![1, 0]);
        let loads = HashMap::new();
        assert_eq!(candidate_order(&candidates, 0, SchedulingMode::Balanced, &blacklist, &loads), vec![0, 1]);
    }
}

/// One (provider, model, key) attempt including the 4xx rectifier loop.
enum Attempt {
    /// Upstream answered 2xx — stream or translate.
    Success(reqwest::Response),
    /// Move to the next candidate; the note lands in the log and status.
    Rotate(RotateReason, String, Option<std::time::Duration>),
    /// Hand this response to the client — rotating can't fix it.
    Return(Response),
}

async fn try_candidate(
    state: &AppState,
    req: &MessagesRequest,
    cand: &Resolved,
) -> Attempt {
    let oreq = anthropic_to_openai(
        req,
        &cand.model,
        cand.supports_reasoning_effort,
        cand.use_max_completion_tokens,
    );
    let mut oreq_json = serde_json::to_value(&oreq).unwrap_or(json!({}));
    if cand.auto_compact && cand.context > 0 {
        if let Some(dropped) = compact_request(&mut oreq_json, cand.context) {
            tracing::info!(
                provider = %cand.provider, model = %cand.model,
                "compact: dropped {dropped} earlier message(s) to fit {}-token context",
                cand.context
            );
        }
    }
    let mut attempts = 0;
    loop {
        let resp = match send_upstream(&state.client, cand, &oreq_json).await {
            Ok(r) => r,
            Err(e) => {
                return Attempt::Rotate(
                    RotateReason::Transient,
                    format!("unreachable: {e}"),
                    None,
                )
            }
        };
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Attempt::Success(resp);
        }
        let retry_after = if status == StatusCode::TOO_MANY_REQUESTS.as_u16() {
            resp.headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        } else {
            None
        };
        // Read the body once — every non-2xx branch needs it (classify,
        // context-overflow sniff, rectify, or the verbatim return).
        let body = resp.bytes().await.unwrap_or_default();
        if looks_context_overflow(&body) {
            // The request exceeds THIS model's window — a bigger-context
            // candidate on the same slot can still take it. No parking:
            // the model is fine for smaller requests.
            return Attempt::Rotate(
                RotateReason::Transient,
                format!("HTTP {status} context length exceeded — {}", upstream_message(&body)),
                    None,
            );
        }
        match rotation::classify(status) {
            Verdict::Rotate(reason) => {
                let note = upstream_message(&body);
                let cooldown = (status == StatusCode::TOO_MANY_REQUESTS.as_u16()).then(|| {
                    rotation::retry_after(
                        retry_after.as_deref(),
                        std::time::SystemTime::now(),
                        rotation::BLACKLIST_TTL,
                    )
                });
                return Attempt::Rotate(reason, format!("HTTP {status} {note}"), cooldown);
            }
            Verdict::Return => {
                // 4xx request errors: the rectifiers may still rescue it by
                // stripping an unsupported field (bounded by MAX_RECTIFY).
                if attempts >= MAX_RECTIFY {
                    return Attempt::Return(upstream_error_response(status, body));
                }
                match rectify(&oreq_json, status, &body) {
                    Some(fixed) => {
                        oreq_json = fixed;
                        attempts += 1;
                        continue;
                    }
                    None => {
                        return Attempt::Return(upstream_error_response(status, body));
                    }
                }
            }
        }
    }
}

/// Sliding-window compaction for an OpenAI-shaped request body. Keeps the
/// system prefix and the first user message (task anchor), fills the tail
/// backwards within the model's context budget, never leaves an orphaned
/// `tool` result at the cut boundary, and inserts a marker line saying what
/// was dropped — silent history edits would be a lie. Returns the dropped
/// count, or None when the request already fits.
fn compact_request(oreq: &mut Value, context_tokens: u64) -> Option<usize> {
    // ~3 chars/token covers mixed EN/RU/code pessimistically enough; +8 for
    // role keys and JSON scaffolding per message.
    let est = |v: &Value| -> usize {
        serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) / 3 + 8
    };
    // Budget = context − reserved output − ~15% headroom for the estimate's
    // error and provider-side tokenizer drift.
    let out_reserve = oreq
        .get("max_completion_tokens")
        .or_else(|| oreq.get("max_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(4096) as usize;
    let budget = (context_tokens as usize)
        .saturating_sub(out_reserve)
        .saturating_mul(85)
        / 100;
    let msgs = oreq.get("messages")?.as_array()?.clone();
    if msgs.iter().map(&est).sum::<usize>() <= budget {
        return None;
    }
    fn role(v: &Value) -> &str {
        v.get("role").and_then(|r| r.as_str()).unwrap_or("")
    }
    let mut keep = vec![false; msgs.len()];
    let mut used = 0usize;
    // Head: every system message, then the first user message (task anchor)
    // when it costs under half the budget.
    let mut i = 0;
    while i < msgs.len() && role(&msgs[i]) == "system" {
        used += est(&msgs[i]);
        keep[i] = true;
        i += 1;
    }
    if i < msgs.len() && role(&msgs[i]) == "user" && used + est(&msgs[i]) <= budget / 2 {
        used += est(&msgs[i]);
        keep[i] = true;
        i += 1;
    }
    // Tail: fill backwards until the budget is spent.
    let mut j = msgs.len();
    while j > i {
        let e = est(&msgs[j - 1]);
        if used + e > budget {
            break;
        }
        j -= 1;
        used += e;
    }
    // An assistant+tool_calls group spans its `tool` results — dropping the
    // assistant but keeping the results orphans them, and OpenAI-side
    // providers 400 on that. Skip leading `tool` messages at the cut.
    while j < msgs.len() && role(&msgs[j]) == "tool" {
        j += 1;
    }
    for k in j..msgs.len() {
        keep[k] = true;
    }
    let dropped = msgs.len() - keep.iter().filter(|k| **k).count();
    if dropped == 0 {
        return None;
    }
    let head_len = keep.iter().take_while(|k| **k).count();
    let mut out: Vec<Value> = msgs
        .iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(m, _)| m.clone())
        .collect();
    out.insert(
        head_len,
        json!({ "role": "system",
            "content": format!("[nexuslink compact: {dropped} earlier message(s) omitted — request exceeded the model's context window]") }),
    );
    oreq["messages"] = json!(out);
    Some(dropped)
}

/// Does a non-2xx body complain about the context window rather than the
/// request shape? Phrases cover the common OpenAI-compatible wordings
/// (OpenAI, OpenRouter, vLLM, Together-style messages).
fn looks_context_overflow(body: &[u8]) -> bool {
    let msg = upstream_message(body).to_lowercase();
    [
        "context length",
        "context_length",
        "context window",
        "maximum context",
        "max context",
        "too many tokens",
        "exceeds the context",
        "exceed the context",
        "reduce the length",
        "token limit",
        "input is too long",
        "prompt is too long",
    ]
    .iter()
    .any(|p| msg.contains(p))
}

/// Non-2xx, non-rotatable upstream answer — pass status + body shape through.
fn upstream_error_response(status: u16, body: Bytes) -> Response {
    if err_content_type(&body) == "json" {
        let mut v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
        if v.get("type").is_none() {
            v = json!({ "type": "error", "error": { "type": "api_error",
                "message": upstream_message(&body) } });
        }
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(v),
        )
            .into_response();
    }
    anthropic_error(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        "api_error",
        &upstream_message(&body),
    )
}

/// 2xx non-streaming body → OpenAI JSON → Anthropic JSON.
async fn translate_body(resp: reqwest::Response, slot: &str) -> Response {
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("failed reading upstream body: {e}"),
            )
        }
    };
    let v: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return anthropic_error(StatusCode::BAD_GATEWAY, "api_error", "upstream returned non-JSON")
        }
    };
    Json(openai_to_anthropic(&v, slot)).into_response()
}

async fn send_upstream(
    client: &reqwest::Client,
    resolved: &Resolved,
    body: &Value,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .post(&resolved.url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", resolved.api_key))
        .body(serde_json::to_vec(body).unwrap_or_default())
        .send()
        .await
}

fn err_content_type(body: &[u8]) -> &'static str {
    if serde_json::from_slice::<Value>(body).is_ok() {
        "json"
    } else {
        "text"
    }
}

fn upstream_message(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message").or_else(|| e.get("msg")))
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string())
}

/// Look at a 4xx error + current outbound body and decide whether stripping
/// one unsupported field is worth a retry. Returns the fixed body, if so.
///
/// Rectifiers (each fires at most once per request, bounded by MAX_RECTIFY):
/// - `reasoning_effort` / `max_completion_tokens` / `stream_options` named in
///   the error -> remove that key.
/// - Generic 400 with `reasoning_effort` present -> speculate removing it
///   (last resort; wrong guesses cost one retry each).
fn rectify(body: &Value, status: u16, err: &[u8]) -> Option<Value> {
    let mut fixed = body.clone();
    let obj = fixed.as_object_mut()?;
    let msg = upstream_message(err).to_lowercase();

    let named = ["reasoning_effort", "max_completion_tokens", "stream_options", "top_p", "stop"]
        .iter()
        .find(|k| msg.contains(**k) && obj.contains_key(**k));
    if let Some(k) = named {
        obj.remove(*k);
        return Some(fixed);
    }
    // Adaptive thinking leaked through? Anthropic-only field, always strip.
    if obj.remove("thinking").is_some() {
        return Some(fixed);
    }
    if status == 400 && obj.remove("reasoning_effort").is_some() {
        return Some(fixed);
    }
    None
}

// ---------------------------------------------------------------------------
// SSE bridge: upstream OpenAI stream -> translated Anthropic stream
// ---------------------------------------------------------------------------

fn bridge_stream(
    resp: reqwest::Response,
    upstream_model: &str,
    slot: &str,
    load_guard: ActiveLoadGuard,
) -> Response {
    let converter_model = slot.to_string();
    let byte_stream = resp.bytes_stream();

    // Parse upstream SSE -> translate each data payload -> re-emit frames.
    // heartbeat: idle > HEARTBEAT_SECS inserts an SSE comment line.
    let out_stream = async_stream_bridge(byte_stream, upstream_model.to_string(), converter_model);
    let guarded_stream = futures_util::stream::unfold(
        (Box::pin(out_stream), load_guard),
        |(mut stream, guard)| async move {
            futures_util::StreamExt::next(&mut stream)
                .await
                .map(|item| (item, (stream, guard)))
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(guarded_stream))
        .unwrap()
        .into_response()
}

fn async_stream_bridge(
    upstream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    _upstream_model: String,
    slot: String,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    use eventsource_stream::Eventsource;

    // Box+pin once: the unfold loop needs to poll the same stream repeatedly.
    let es = Box::pin(upstream.eventsource());
    let conv = StreamConverter::new(slot, 0);
    let idle = std::time::Duration::from_secs(HEARTBEAT_SECS);

    futures_util::stream::unfold(
        (es, conv, false),
        move |(mut es, mut conv, done)| async move {
            if done {
                return None;
            }
            loop {
                tokio::select! {
                    item = futures_util::StreamExt::next(&mut es) => {
                        match item {
                            Some(Ok(ev)) => {
                                let data = ev.data.trim().to_string();
                                if data == "[DONE]" {
                                    let mut frames = Vec::new();
                                    conv.finish(&mut frames);
                                    return Some((Ok(Bytes::from(frames.join(""))), (es, conv, true)));
                                }
                                if let Ok(v) = serde_json::from_str::<Value>(&data) {
                                    let mut frames = Vec::new();
                                    conv.process_chunk(&v, &mut frames);
                                    if !frames.is_empty() {
                                        return Some((Ok(Bytes::from(frames.join(""))), (es, conv, done)));
                                    }
                                    continue;
                                }
                            }
                            Some(Err(_)) => continue,
                            None => {
                                let mut frames = Vec::new();
                                conv.finish(&mut frames);
                                return Some((Ok(Bytes::from(frames.join(""))), (es, conv, true)));
                            }
                        }
                    }
                    _ = tokio::time::sleep(idle), if !done => {
                        return Some((Ok(Bytes::from(": ping\n\n")), (es, conv, done)));
                    }
                }
            }
        },
    )
}
