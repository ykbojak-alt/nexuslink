//! The axum server: inbound auth, slot resolution, upstream forwarding,
//! SSE bridging with heartbeat injection.

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use crate::config::{
    flat_slots, resolve_candidates, Config, Resolved, HEARTBEAT_SECS, MAX_BODY, MAX_RECTIFY,
};
use crate::rotation::{self, Blacklist, RotateReason, Verdict};
use crate::streaming::StreamConverter;
use crate::translator::{anthropic_to_openai, openai_to_anthropic, MessagesRequest};

pub struct AppState {
    pub config: RwLock<Config>,
    pub client: reqwest::Client,
    /// Rotation memory: recently-failed keys/models, shared with the UI
    /// status endpoint so the panel can show what is currently parked.
    pub blacklist: Blacklist,
    /// Round-robin cursor per slot: each request starts at the next
    /// candidate after the previous winner, so free-tier RPM quota is
    /// spread across the key pool instead of always burning key #1.
    pub cursor: RwLock<HashMap<String, usize>>,
    /// Per-candidate attempt stats (ok/fail/latency), surfaced via /api/status.
    pub metrics: crate::metrics::Metrics,
    pub started: std::time::Instant,
}

/// Shared state for the `serve` command and the desktop shell: the same
/// builder is used whether the runtime lives in a console or behind the
/// WebView2 window.
pub fn state_from_config(mut cfg: Config) -> Arc<AppState> {
    crate::config::normalize_slots(&mut cfg);
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
    })
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/messages", post(messages))
        .route("/health", get(|| async { "ok" }))
        .merge(crate::ui::router())
        .with_state(state)
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
    let data: Vec<Value> = flat_slots(&config)
        .into_iter()
        .map(|(slot, name)| json!({ "id": slot, "display_name": name, "created": 0 }))
        .collect();
    Json(json!({ "data": data })).into_response()
}

/// POST /v1/messages — the whole point of the program.
async fn messages(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if body.len() > MAX_BODY {
        return anthropic_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "body too large");
    }
    let config = state.config.read().unwrap().clone();
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
    let candidates = resolve_candidates(&slot, &config);
    if candidates.is_empty() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("nexuslink: model slot {slot} is not mapped to any provider"),
        );
    }
    let is_stream = req.stream.unwrap_or(false);

    // Rotation: walk every (provider, model, key) candidate in config order.
    // Auth/quota/endpoint failures blacklist the key and move the pointer;
    // 400-class request errors are returned verbatim — every provider would
    // reject the same body, so rotating them just multiplies the failure.
    let n = candidates.len();
    let start = {
        let cursors = state.cursor.read().unwrap();
        cursors.get(&slot).copied().unwrap_or(0) % n
    };
    let mut skipped = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let mut last_error: Option<Response> = None;
    for i in 0..n {
        let cand = &candidates[(start + i) % n];
        if cand.url.is_empty() {
            continue;
        }
        if state
            .blacklist
            .is_blocked(&cand.base_url, &cand.model, &cand.api_key)
        {
            skipped += 1;
            continue;
        }
        let t0 = std::time::Instant::now();
        match try_candidate(&state, &req, cand).await {
            Attempt::Success(resp) => {
                state.metrics.record(&cand.provider, &cand.model, &cand.api_key,
                                     true, t0.elapsed().as_millis() as u64, "");
                let model = cand.model.clone();
                // Next request starts at the candidate after this winner.
                state
                    .cursor
                    .write()
                    .unwrap()
                    .insert(slot.clone(), (start + i + 1) % n);
                return if is_stream {
                    bridge_stream(resp, &model, &slot)
                } else {
                    translate_body(resp, &slot).await
                };
            }
            Attempt::Rotate(reason, note) => {
                state.metrics.record(&cand.provider, &cand.model, &cand.api_key,
                                     false, t0.elapsed().as_millis() as u64, &note);
                match reason {
                    RotateReason::Key => {
                        state
                            .blacklist
                            .block_key(&cand.base_url, &cand.api_key, &note)
                    }
                    RotateReason::Model => {
                        state
                            .blacklist
                            .block_model(&cand.base_url, &cand.model, &note)
                    }
                    RotateReason::Transient => {}
                }
                tracing::info!(
                    slot = %slot, provider = %cand.provider, model = %cand.model,
                    key = %rotation::key_tag(&cand.api_key),
                    "rotation: {note} → next candidate"
                );
                failed.push(format!("{} · {} · …{}: {note}",
                    cand.provider, cand.model, rotation::key_tag(&cand.api_key)));
            }
            Attempt::Return(resp) => {
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
    let detail = if failed.is_empty() {
        format!("{skipped} candidate(s) are currently blacklisted")
    } else {
        format!("{} attempt(s) failed — {}", failed.len(), failed.join(" | "))
    };
    anthropic_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        &format!("nexuslink: all candidates exhausted for {slot} — {detail}"),
    )
}

/// One (provider, model, key) attempt including the 4xx rectifier loop.
enum Attempt {
    /// Upstream answered 2xx — stream or translate.
    Success(reqwest::Response),
    /// Move to the next candidate; the note lands in the log and status.
    Rotate(RotateReason, String),
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
                )
            }
        };
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Attempt::Success(resp);
        }
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
            );
        }
        match rotation::classify(status) {
            Verdict::Rotate(reason) => {
                let note = upstream_message(&body);
                return Attempt::Rotate(reason, format!("HTTP {status} {note}"));
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

fn bridge_stream(resp: reqwest::Response, upstream_model: &str, slot: &str) -> Response {
    let converter_model = slot.to_string();
    let byte_stream = resp.bytes_stream();

    // Parse upstream SSE -> translate each data payload -> re-emit frames.
    // heartbeat: idle > HEARTBEAT_SECS inserts an SSE comment line.
    let out_stream = async_stream_bridge(byte_stream, upstream_model.to_string(), converter_model);

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(out_stream))
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
