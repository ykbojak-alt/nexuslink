//! NexusLink panel: the management UI and its /api endpoints, served
//! same-origin by the proxy itself — no separate port, no CORS surface.
//!
//! The SPA is a single HTML file (`ui/index.html`): compiled in via
//! include_str! for release, re-read from disk per request in debug so panel
//! tweaks land with a page reload instead of a fat-LTO rebuild. The Nexus Lux
//! backdrop (`assets/panel-bg.webp`, 12 KB) follows the same split and is
//! served at /ui/bg.webp rather than inlined as a data URI.
//!
//! Trust model: `/api/*` management endpoints require the same Bearer
//! token as `/v1/*`. The page itself carries no secrets; the token arrives
//! in the `#t=` fragment when launched via `nexuslink ui` (fragments never
//! hit the wire) or is pasted into the token gate by hand.

use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post, put},
    Router,
};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::config::{self, flat_slots, Config};
use crate::gateway;
use crate::proxy::{check_auth, AppState};

const INDEX: &str = include_str!("../ui/index.html");
const PANEL_ON_DISK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/ui/index.html");
const BG: &[u8] = include_bytes!("../assets/panel-bg.webp");
const BG_ON_DISK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/panel-bg.webp");

/// The panel document. Debug builds pick up the working copy on disk (and
/// fall back to the embedded one if it is missing or unreadable); release is
/// always the embedded copy, so the exe stays self-contained.
fn panel_html() -> String {
    #[cfg(debug_assertions)]
    {
        match std::fs::read_to_string(PANEL_ON_DISK) {
            Ok(html) => return html,
            Err(e) => tracing::warn!(
                "serving the embedded panel instead of {} ({e})",
                PANEL_ON_DISK
            ),
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = PANEL_ON_DISK;
    }
    INDEX.to_string()
}

/// Backdrop art, same debug-disk / release-embed split as the panel.
fn panel_bg() -> Vec<u8> {
    #[cfg(debug_assertions)]
    {
        match std::fs::read(BG_ON_DISK) {
            Ok(bytes) => return bytes,
            Err(e) => tracing::warn!(
                "serving the embedded backdrop instead of {} ({e})",
                BG_ON_DISK
            ),
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = BG_ON_DISK;
    }
    BG.to_vec()
}

/// Routes for the management panel. State is supplied by the parent
/// router (proxy::router merges this before `.with_state`).
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(index))
        .route("/ui/bg.webp", get(background))
        .route("/api/status", get(status))
        .route("/api/config", get(get_config))
        .route("/api/config", put(put_config))
        .route("/api/models", post(list_upstream_models))
        .route("/api/apply", post(apply))
}

async fn index() -> Html<String> {
    Html(panel_html())
}

/// GET /ui/bg.webp — the Nexus Lux backdrop the panel paints behind its
/// cards. Public: pure decoration, no secrets, and `no-cache` so a restyled
/// art file shows up on the next reload.
async fn background() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/webp"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        panel_bg(),
    )
        .into_response()
}

fn denied() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "ok": false, "error": "unauthorized" })),
    )
        .into_response()
}

/// GET /api/status — open to the page (no secrets): liveness, slot count,
/// rotation blacklist (masked entries only) and uptime for the status card.
async fn status(State(state): State<Arc<AppState>>) -> Response {
    let config = state.config.read().unwrap().clone();
    let slots = flat_slots(&config);
    let blacklist: Vec<Value> = state
        .blacklist
        .entries()
        .into_iter()
        .map(|(entry, ttl_s, reason)| {
            json!({ "entry": entry, "ttl_s": ttl_s.round() as u64, "reason": reason })
        })
        .collect();
    let metrics: Vec<Value> = state
        .metrics
        .snapshot()
        .into_iter()
        .map(|(scope, ok, fail, avg_ms, last_ms, last_note, age_s)| {
            json!({ "scope": scope, "ok": ok, "fail": fail, "avg_ms": avg_ms,
                    "last_ms": last_ms, "note": last_note, "age_s": age_s })
        })
        .collect();
    // Per-slot availability: how many candidates can serve a request right
    // now vs benched (429 cooldown) vs blocked (auth/quota). `exhausted`
    // flags the starvation state where nothing can serve — the condition
    // that made agent sessions stall invisibly before this was surfaced.
    let slot_health: Vec<Value> = slots
        .iter()
        .map(|(slot, _)| {
            let cands = crate::config::resolve_candidates(slot, &config);
            let mut ready = 0usize;
            let mut benched = 0usize;
            let mut blocked = 0usize;
            let mut next_ready_in_s: Option<u64> = None;
            for c in &cands {
                if state.blacklist.is_blocked(&c.base_url, &c.model, &c.api_key) {
                    blocked += 1;
                } else if let Some(rem) =
                    state.blacklist.key_bench_remaining(&c.base_url, &c.api_key)
                {
                    benched += 1;
                    let secs = rem.as_secs().max(1);
                    next_ready_in_s = Some(next_ready_in_s.map_or(secs, |n| n.min(secs)));
                } else {
                    ready += 1;
                }
            }
            json!({
                "slot": slot,
                "candidates": cands.len(),
                "ready": ready,
                "benched": benched,
                "blocked": blocked,
                "next_ready_in_s": next_ready_in_s,
                "exhausted": ready == 0,
            })
        })
        .collect();
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "port": config.port,
        "providers": config.providers.len(),
        "slots": slots.len(),
        "slot_health": slot_health,
        "token_set": !config.incoming_token.is_empty(),
        "blacklist": blacklist,
        "metrics": metrics,
        "uptime_s": state.started.elapsed().as_secs(),
    }))
    .into_response()
}

/// GET /api/config — the editable document itself (auth required: it
/// contains the provider keys the user manages here).
async fn get_config(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let config = state.config.read().unwrap().clone();
    if !check_auth(&headers, &config.incoming_token) {
        return denied();
    }
    Json(json!({ "ok": true, "config": config })).into_response()
}

/// PUT /api/config — validate, persist to config.json, hot-swap the
/// in-memory copy so the running proxy picks changes up with no restart.
async fn put_config(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let expected = { state.config.read().unwrap().incoming_token.clone() };
    if !check_auth(&headers, &expected) {
        return denied();
    }
    if body.len() > 256 * 1024 {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "ok": false, "error": "config too large" })),
        )
            .into_response();
    }
    let mut cfg: Config = match serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| serde_json::from_value(v.get("config").cloned().unwrap_or(v)).ok())
    {
        Some(c) => c,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": "invalid config JSON" })),
            )
                .into_response()
        }
    };
    // The UI may echo back a blank token field meaning "unchanged" —
    // never let an accidental empty write lock the proxy out.
    if cfg.incoming_token.trim().is_empty() {
        cfg.incoming_token = expected;
    }
    config::normalize_slots(&mut cfg);
    if cfg.port == 0 {
        cfg.port = config::DEFAULT_PORT;
    }
    if let Err(e) = config::save(&cfg) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e })),
        )
            .into_response();
    }
    *state.config.write().unwrap() = cfg;
    Json(json!({ "ok": true })).into_response()
}

/// POST /api/models — fetch a provider's catalog server-side. The browser
/// can't call the upstream directly (CORS), and keeping the key inside a
/// single backend hop is cleaner than shipping it in page JS. Body:
/// {"base_url": "…", "api_key": "…"}. Returns free models first — `:free`
/// suffix or all-zero pricing fields (OpenRouter convention).
async fn list_upstream_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let config = state.config.read().unwrap().clone();
    if !check_auth(&headers, &config.incoming_token) {
        return denied();
    }
    #[derive(serde::Deserialize)]
    struct Req {
        base_url: String,
        #[serde(default)]
        api_key: String,
    }
    let req: Req = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": format!("bad body: {e}") })),
            )
                .into_response()
        }
    };
    let url = format!("{}/v1/models", req.base_url.trim_end_matches('/'));
    let mut rb = state
        .client
        .get(&url)
        .timeout(std::time::Duration::from_secs(20));
    if !req.api_key.is_empty() {
        rb = rb.header("authorization", format!("Bearer {}", req.api_key));
    }
    let resp = match rb.send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "ok": false, "error": format!("upstream: {e}") })),
            )
                .into_response()
        }
    };
    if !resp.status().is_success() {
        let st = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false,
                "error": format!("upstream HTTP {st}: {}", &body[..body.len().min(200)]) })),
        )
            .into_response();
    }
    let v: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "ok": false, "error": format!("upstream non-JSON: {e}") })),
            )
                .into_response()
        }
    };
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<Value> = Vec::with_capacity(data.len());
    for m in &data {
        let id = m.get("id").and_then(|x| x.as_str()).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let zero_price = |k: &str| {
            m.get("pricing")
                .and_then(|p| p.get(k))
                .and_then(|x| x.as_str())
                .is_some_and(|s| s == "0" || s == "0.0")
        };
        let free = id.ends_with(":free") || (zero_price("prompt") && zero_price("completion"));
        out.push(json!({
            "id": id,
            "name": m.get("name").and_then(|x| x.as_str()).unwrap_or(id),
            "context": m.get("context_length").and_then(|x| x.as_u64()).unwrap_or(0),
            "free": free,
        }));
    }
    out.sort_by_key(|m| !m["free"].as_bool().unwrap_or(false));
    Json(json!({ "ok": true, "total": data.len(), "models": out })).into_response()
}

/// POST /api/apply — write the Claude Desktop gateway/library config.
async fn apply(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let config = state.config.read().unwrap().clone();
    if !check_auth(&headers, &config.incoming_token) {
        return denied();
    }
    match gateway::apply_to_claude_desktop(&config) {
        Ok(msg) => Json(json!({ "ok": true, "message": msg })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}
