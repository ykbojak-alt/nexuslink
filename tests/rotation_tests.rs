//! Rotation end-to-end: real HTTP mock upstreams, the full proxy loop —
//! dead key → next pool key, dead provider → next provider, parked keys
//! skipped without spending a request.

use axum::{extract::State, http::HeaderMap, response::Json, routing::post, Router};
use nexuslink::config::{Config, ModelEntry, Provider};
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
    #[derive(Clone)]
    struct S {
        fail: HashMap<String, u16>,
        hits: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<String>>>,
    }
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = S { fail, hits: hits.clone(), seen: seen.clone() };
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
                return (
                    axum::http::StatusCode::from_u16(*code).unwrap(),
                    Json(json!({ "error": { "message": format!("mock {code}") } })),
                );
            }
            (
                axum::http::StatusCode::OK,
                Json(json!({
                    "id": "chatcmpl-mock",
                    "choices": [{ "index": 0,
                        "message": { "role": "assistant", "content": "mock hello" },
                        "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 3 },
                })),
            )
        }),
    )
    .with_state(s);
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
            providers,
        }),
        client: reqwest::Client::new(),
        blacklist: Blacklist::new(),
        cursor: RwLock::new(std::collections::HashMap::new()),
        metrics: nexuslink::metrics::Metrics::new(),
        started: std::time::Instant::now(),
    });
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), state)
}

async fn ask(url: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/v1/messages"))
        .header("authorization", "Bearer tok")
        .json(&json!({
            "model": "claude-sonnet-5",
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
    let (base, _state) =
        serve(vec![provider(&mock.url, "p", &["k-1", "k-2"], "m1")]).await;

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
