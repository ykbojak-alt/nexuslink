//! Rotation end-to-end: real HTTP mock upstreams, the full proxy loop —
//! dead key → next pool key, dead provider → next provider, parked keys
//! skipped without spending a request.

use axum::{extract::State, http::HeaderMap, response::{IntoResponse, Json}, routing::post, Router};
use nexuslink::config::{Config, ModelEntry, NUEKeyStore, NUEUpstream, Provider};
use nexuslink::proxy::{router, AppState};
use nexuslink::rotation::{key_tag, Blacklist};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

struct Mock {
    url: String,
    hits: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<String>>>,
}

/// Mock OpenAI upstream: Authorization values found in `fail` get that
/// status back; anything else gets a valid chat.completion.
async fn mock_upstream(fail: HashMap<String, u16>) -> Mock {
    mock_upstream_with_retry_after(fail, HashMap::new()).await
}

async fn mock_upstream_with_retry_after(
    fail: HashMap<String, u16>,
    retry_after: HashMap<String, &'static str>,
) -> Mock {
    #[derive(Clone)]
    struct S {
        fail: HashMap<String, u16>,
        retry_after: HashMap<String, &'static str>,
        hits: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<String>>>,
    }
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = S { fail, retry_after, hits: hits.clone(), seen: seen.clone() };
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|State(s): State<S>, headers: HeaderMap, Json(_b): Json<Value>| async move {
            s.hits.fetch_add(1, Ordering::SeqCst);
            let key = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or("")
                .to_string();
            s.seen.lock().unwrap().push(key.clone());
            if let Some(code) = s.fail.get(&key) {
                let mut response = Json(json!({ "error": { "message": format!("mock {code}") } })).into_response();
                *response.status_mut() = axum::http::StatusCode::from_u16(*code).unwrap();
                if let Some(value) = s.retry_after.get(&key) {
                    response.headers_mut().insert(
                        axum::http::header::RETRY_AFTER,
                        axum::http::HeaderValue::from_static(value),
                    );
                }
                return response;
            }
            Json(json!({
                    "id": "chatcmpl-mock",
                    "choices": [{ "index": 0,
                        "message": { "role": "assistant", "content": "mock hello" },
                        "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 3 },
                })).into_response()
        }),
    )
    .with_state(s)
    .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mock {
        url: format!("http://127.0.0.1:{port}"),
        hits,
        seen,
    }
}

fn provider(url: &str, name: &str, keys: &[&str], model: &str) -> Provider {
    Provider {
        name: name.into(),
        base_url: url.into(),
        api_key: keys.first().unwrap_or(&"").to_string(),
        api_keys: keys.iter().map(|k| k.to_string()).collect(),
        models: vec![ModelEntry {
            name: model.into(),
            slot: "claude-sonnet-5".into(),
            context: 0,
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Serve the proxy with `providers` on an ephemeral port.
async fn serve(providers: Vec<Provider>) -> (String, Arc<AppState>) {
    let state = Arc::new(AppState {
        config: RwLock::new(Config {
            incoming_token: "tok".into(),
            port: 0,
            nue_port: 0,
            providers,
            nue_client_keys: Vec::new(),
            nue_upstream: NUEUpstream::default(),
            nue_register_token: String::new(),
            max_body_bytes: nexuslink::config::DEFAULT_MAX_BODY_BYTES,
            fallback_slot: String::new(),
        }),
        client: reqwest::Client::new(),
        blacklist: Blacklist::new(),
        cursor: RwLock::new(std::collections::HashMap::new()),
        metrics: nexuslink::metrics::Metrics::new(),
        nue_keys: RwLock::new(NUEKeyStore { keys: Vec::new() }),
        nue_upstream: RwLock::new(NUEUpstream::default()),
        started: std::time::Instant::now(),
    });
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), state)
}

async fn ask(url: &str) -> reqwest::Response {
    ask_model(url, "claude-sonnet-5").await
}

async fn ask_model(url: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/v1/messages"))
        .header("authorization", "Bearer tok")
        .json(&json!({
            "model": model,
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn dead_key_rotates_to_next_pool_key() {
    let mock = mock_upstream([("k-bad".to_string(), 401)].into_iter().collect()).await;
    let (base, _state) =
        serve(vec![provider(&mock.url, "p", &["k-bad", "k-good"], "m1")]).await;

    let resp = ask(&base).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "mock hello");
    // k-bad got 401, proxy moved to k-good — one request each, in order.
    assert_eq!(*mock.seen.lock().unwrap(), vec!["k-bad", "k-good"]);
}

#[tokio::test]
async fn dead_provider_rotates_to_next_provider() {
    let a = mock_upstream([("k-a".to_string(), 500)].into_iter().collect()).await;
    let b = mock_upstream(HashMap::new()).await;
    let (base, _state) = serve(vec![
        provider(&a.url, "provA", &["k-a"], "model-a"),
        provider(&b.url, "provB", &["k-b"], "model-b"),
    ])
    .await;

    let resp = ask(&base).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(a.hits.load(Ordering::SeqCst), 1);
    assert_eq!(b.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn agent_alias_routes_primary_then_fallback_model_slots() {
    let primary = mock_upstream([("k-opus".to_string(), 500)].into_iter().collect()).await;
    let fallback = mock_upstream(HashMap::new()).await;
    let mut opus = provider(&primary.url, "minimax", &["k-opus"], "opus-model");
    opus.models[0].slot = "claude-opus-4-8".into();
    let mut haiku = provider(&fallback.url, "mercury", &["k-haiku"], "haiku-model");
    haiku.models[0].slot = "claude-haiku-4-5".into();
    let (base, _) = serve(vec![opus, haiku]).await;

    let response = ask_model(&base, "agent/alice").await;

    assert_eq!(response.status(), 200);
    assert_eq!(*primary.seen.lock().unwrap(), vec!["k-opus"]);
    assert_eq!(*fallback.seen.lock().unwrap(), vec!["k-haiku"]);
}

#[tokio::test]
async fn models_lists_persona_aliases_when_ranked_pool_is_configured() {
    let mock = mock_upstream(HashMap::new()).await;
    let mut opus = provider(&mock.url, "minimax", &["k"], "opus-model");
    opus.models[0].slot = "claude-opus-4-8".into();
    let (base, _) = serve(vec![opus]).await;

    let response = reqwest::Client::new()
        .get(format!("{base}/v1/models"))
        .header("authorization", "Bearer tok")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let ids: Vec<&str> = body["data"].as_array().unwrap().iter()
        .filter_map(|row| row["id"].as_str()).collect();
    for alias in ["agent/alice", "agent/forge", "agent/codex", "agent/ace", "agent/qoder"] {
        assert!(ids.contains(&alias), "missing alias {alias}");
    }
}

#[tokio::test]
async fn failover_keeps_primary_first_across_successful_requests() {
    let primary = mock_upstream(HashMap::new()).await;
    let backup = mock_upstream(HashMap::new()).await;
    let (base, _) = serve(vec![
        provider(&primary.url, "primary", &["k-a"], "model-a"),
        provider(&backup.url, "backup", &["k-b"], "model-b"),
    ]).await;

    assert_eq!(ask(&base).await.status(), 200);
    assert_eq!(ask(&base).await.status(), 200);
    assert_eq!(primary.hits.load(Ordering::SeqCst), 2);
    assert_eq!(backup.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn blacklisted_key_is_skipped_without_spending_a_request() {
    let mock = mock_upstream(HashMap::new()).await;
    let (base, state) =
        serve(vec![provider(&mock.url, "p", &["k-dead", "k-live"], "m1")]).await;
    // k-dead died earlier — parked for every model on this gateway.
    state.blacklist.block_key(&mock.url, "k-dead", "402");

    let resp = ask(&base).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(*mock.seen.lock().unwrap(), vec!["k-live"]);
}

#[tokio::test]
async fn healthy_keys_spread_load_round_robin() {
    let mock = mock_upstream(HashMap::new()).await;
    let mut p = provider(&mock.url, "p", &["k-1", "k-2"], "m1");
    p.models[0].scheduling_mode = nexuslink::config::SchedulingMode::Balanced;
    let (base, _state) = serve(vec![p]).await;

    assert_eq!(ask(&base).await.status(), 200);
    assert_eq!(ask(&base).await.status(), 200);
    // First request opened on k-1 (cursor 0) and advanced the cursor —
    // the second opens on k-2 instead of burning k-1's quota again.
    assert_eq!(*mock.seen.lock().unwrap(), vec!["k-1", "k-2"]);
    // Third request wraps around to k-1.
    assert_eq!(ask(&base).await.status(), 200);
    assert_eq!(*mock.seen.lock().unwrap(), vec!["k-1", "k-2", "k-1"]);
}

#[tokio::test]
async fn retry_after_excludes_benched_keys_until_cooldown_expires() {
    let mock = mock_upstream_with_retry_after(
        [("k-a".to_string(), 429), ("k-b".to_string(), 429)].into_iter().collect(),
        [("k-a".to_string(), "120"), ("k-b".to_string(), "60")].into_iter().collect(),
    ).await;
    let (base, _state) = serve(vec![provider(&mock.url, "p", &["k-a", "k-b"], "m1")]).await;

    // Every key benched → honest 429 carrying the soonest cooldown expiry
    // (k-b benched for 60s < k-a's 120s), not a bare 502 reconnect storm.
    let first = ask(&base).await;
    assert_eq!(first.status(), 429);
    let retry_after = first
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    assert!(matches!(retry_after, Some(s) if s > 0 && s <= 60));
    let second = ask(&base).await;
    assert_eq!(second.status(), 429);
    // The first request tries each available key once. While both cooldowns
    // are active, the next request fails closed without spending another call.
    assert_eq!(*mock.seen.lock().unwrap(), vec!["k-a", "k-b"]);
}

#[tokio::test]
async fn retry_after_cools_only_the_limited_key_for_following_requests() {
    let mock = mock_upstream_with_retry_after(
        [("k-a".to_string(), 429)].into_iter().collect(),
        [("k-a".to_string(), "120")].into_iter().collect(),
    ).await;
    let (base, _state) = serve(vec![provider(&mock.url, "p", &["k-a", "k-b"], "m1")]).await;

    assert_eq!(ask(&base).await.status(), 200);
    assert_eq!(ask(&base).await.status(), 200);
    assert_eq!(*mock.seen.lock().unwrap(), vec!["k-a", "k-b", "k-b"]);
}

#[tokio::test]
async fn all_candidates_exhausted_returns_honest_502() {
    let mock = mock_upstream(
        [("k1".to_string(), 402), ("k2".to_string(), 402)]
            .into_iter()
            .collect(),
    )
    .await;
    let (base, state) = serve(vec![provider(&mock.url, "p", &["k1", "k2"], "m1")]).await;

    let resp = ask(&base).await;
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("candidates exhausted"));
    // both keys were tried once, then parked — a second request spends nothing.
    assert_eq!(mock.hits.load(Ordering::SeqCst), 2);
    let resp = ask(&base).await;
    assert_eq!(resp.status(), 502);
    assert_eq!(mock.hits.load(Ordering::SeqCst), 2);
    // and the blacklist knows why
    assert!(state.blacklist.is_blocked(&mock.url, "m1", "k1"));
    assert!(state.blacklist.is_blocked(&mock.url, "m1", "k2"));
    let _ = key_tag; // re-export sanity: masking helper used by the log
}

#[tokio::test]
async fn messages_uses_configured_max_body_bytes() {
    let (base, state) = serve(Vec::new()).await;
    state.config.write().unwrap().max_body_bytes = 8;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("authorization", "Bearer tok")
        .body("123456789")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 413);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "request_too_large");
}

#[tokio::test]
async fn messages_accepts_body_above_axum_default_when_configured() {
    let mock = mock_upstream(HashMap::new()).await;
    let (base, state) = serve(vec![provider(&mock.url, "p", &["k"], "m")]).await;
    state.config.write().unwrap().max_body_bytes = 4 * 1024 * 1024;
    let payload = json!({
        "model": "claude-sonnet-5",
        "max_tokens": 16,
        "messages": [{ "role": "user", "content": "x".repeat(3 * 1024 * 1024) }],
    });

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("authorization", "Bearer tok")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
}
