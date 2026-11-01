//! NUE gateway end-to-end: /nue/register issues nue_* keys, passthrough
//! validates the client key and swaps Bearer for the configured upstream
//! key, deny-listed prefixes never reach the upstream.

use axum::{
    extract::State,
    http::HeaderMap,
    response::Json,
    routing::any,
    Router,
};
use nexuslink::config::{Config, NUEKeyStore, NUEUpstream};
use nexuslink::proxy::{nue_router, AppState};
use nexuslink::rotation::Blacklist;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

/// Point USERPROFILE/HOME at a temp dir once per test binary so
/// /nue/register's key-store save never touches the real ~/.nexuslink.
fn temp_home() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir()
            .join(format!("nexuslink-nue-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("USERPROFILE", &dir);
        std::env::set_var("HOME", &dir);
    });
}

fn state(upstream: &str, register_token: &str) -> Arc<AppState> {
    let up = NUEUpstream {
        base_url: upstream.into(),
        api_key: "upstream-secret".into(),
    };
    Arc::new(AppState {
        config: RwLock::new(Config {
            incoming_token: "tok".into(),
            port: 0,
            nue_port: 0,
            providers: Vec::new(),
            nue_client_keys: Vec::new(),
            nue_upstream: up.clone(),
            nue_register_token: register_token.into(),
            max_body_bytes: nexuslink::config::DEFAULT_MAX_BODY_BYTES,
            fallback_slot: String::new(),
        }),
        client: reqwest::Client::new(),
        blacklist: Blacklist::new(),
        cursor: RwLock::new(HashMap::new()),
        metrics: nexuslink::metrics::Metrics::new(),
        nue_keys: RwLock::new(NUEKeyStore {
            keys: vec!["nue_valid".into()],
        }),
        nue_upstream: RwLock::new(up),
        started: std::time::Instant::now(),
    })
}

async fn serve(state: Arc<AppState>) -> String {
    let app = nue_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}

/// Mock AI-2 upstream: records every Authorization header, returns JSON.
async fn mock_upstream() -> (String, Arc<Mutex<Vec<String>>>) {
    #[derive(Clone)]
    struct S {
        seen: Arc<Mutex<Vec<String>>>,
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = S { seen: seen.clone() };
    let app = Router::new()
        .route(
            "/v1/{*path}",
            any(|State(s): State<S>, headers: HeaderMap| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                s.seen.lock().unwrap().push(auth);
                Json(json!({ "ok": true }))
            }),
        )
        .with_state(s);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), seen)
}

#[tokio::test]
async fn passthrough_rejects_bad_key() {
    let (up_url, seen) = mock_upstream().await;
    let url = serve(state(&up_url, "")).await;
    let resp = reqwest::Client::new()
        .get(format!("{url}/v1/runs"))
        .header("authorization", "Bearer nue_wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn passthrough_rejects_missing_key() {
    let (up_url, seen) = mock_upstream().await;
    let url = serve(state(&up_url, "")).await;
    let resp = reqwest::Client::new()
        .get(format!("{url}/v1/runs"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn passthrough_swaps_bearer_for_upstream_key() {
    let (up_url, seen) = mock_upstream().await;
    let url = serve(state(&up_url, "")).await;
    let resp = reqwest::Client::new()
        .get(format!("{url}/v1/runs?limit=5"))
        .header("authorization", "Bearer nue_valid")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    // The upstream must see the configured AI-2 key, never the client key.
    assert_eq!(seen.lock().unwrap().as_slice(), ["Bearer upstream-secret"]);
}

#[tokio::test]
async fn denylist_blocks_key_management() {
    let (up_url, seen) = mock_upstream().await;
    let url = serve(state(&up_url, "")).await;
    for path in ["/v1/keys", "/v1/keys/abc-123"] {
        let resp = reqwest::Client::new()
            .get(format!("{url}{path}"))
            .header("authorization", "Bearer nue_valid")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{path}");
    }
    assert!(seen.lock().unwrap().is_empty());
}

/// Register covers both modes in one test because two live AppStates
/// saving to the same key file would torn-write each other — in
/// production a single process owns the store behind the RwLock.
#[tokio::test]
async fn register_issues_persists_and_honors_token_guard() {
    temp_home();
    let (up_url, _seen) = mock_upstream().await;
    let client = reqwest::Client::new();

    // Guarded instance: no X-Register-Token → 401; correct token → 200.
    let guarded = serve(state(&up_url, "rt-secret")).await;
    let resp = client
        .post(format!("{guarded}/nue/register"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = client
        .post(format!("{guarded}/nue/register"))
        .header("x-register-token", "rt-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.json::<Value>().await.unwrap()["key"]
        .as_str()
        .unwrap()
        .starts_with("nue_"));

    // Open instance: issues a working, persisted nue_* key.
    let url = serve(state(&up_url, "")).await;
    let resp = client
        .post(format!("{url}/nue/register"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let key = resp.json::<Value>().await.unwrap()["key"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(key.starts_with("nue_"), "{key}");
    let resp = client
        .get(format!("{url}/v1/runs"))
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Persisted to the (temp-home) key file — survives a restart.
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(NUEKeyStore::path()).unwrap(),
    )
    .unwrap();
    let keys: Vec<&str> = saved["keys"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|k| k.as_str())
        .collect();
    assert!(keys.contains(&key.as_str()));
}

#[tokio::test]
async fn upstream_unconfigured_is_503() {
    let st = state("", "");
    let url = serve(st).await;
    let resp = reqwest::Client::new()
        .get(format!("{url}/v1/runs"))
        .header("authorization", "Bearer nue_valid")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}
