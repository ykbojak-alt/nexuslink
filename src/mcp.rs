//! `nexuslink mcp` — stdio MCP server that gives Claude Desktop a video
//! generation tool backed by the Agnes provider from ~/.nexuslink.
//!
//! The video API is a job API, not chat completions, so it cannot ride the
//! /v1/messages proxy — but Claude Desktop speaks MCP: it spawns this same
//! exe as a subprocess and calls `generate_video` / `video_status` like any
//! other tool. Protocol: newline-delimited JSON-RPC on stdin/stdout; all
//! logging must stay on stderr or it corrupts the protocol.

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::config::Config;

const VIDEO_MODEL: &str = "agnes-video-2.5-flash";
/// Wait inside generate_video before handing the task id back to Claude —
/// Agnes jobs take ~30-60s; longer than this risks a client-side timeout.
const POLL_BUDGET: Duration = Duration::from_secs(55);
const POLL_STEP: Duration = Duration::from_secs(2);
/// Retry window for `video_queue_full` at task creation: one tool call rides
/// out short queue bursts instead of failing straight away.
const CREATE_RETRY_BUDGET: Duration = Duration::from_secs(45);
const CREATE_RETRY_STEP: Duration = Duration::from_secs(10);
const PROTOCOL_VERSION: &str = "2024-11-05";

struct Agnes {
    root: String,
    keys: Vec<String>,
}

/// Find the Agnes provider: base_url mentions agnes, or a model entry is a
/// video model. Pool = api_keys + the legacy api_key, order preserved.
fn find_agnes(cfg: &Config) -> Option<Agnes> {
    for p in &cfg.providers {
        let is_agnes = p.base_url.contains("agnes")
            || p.models.iter().any(|m| m.name.starts_with("agnes-video"));
        if !is_agnes {
            continue;
        }
        let mut keys: Vec<String> = p
            .api_keys
            .iter()
            .chain(std::iter::once(&p.api_key))
            .filter(|k| !k.is_empty())
            .cloned()
            .collect();
        keys.dedup();
        if keys.is_empty() {
            continue;
        }
        let root = p.base_url.trim_end_matches('/');
        let root = root.strip_suffix("/v1").unwrap_or(root).to_string();
        return Some(Agnes { root, keys });
    }
    None
}

/// Send one request, cycling the key pool on 401/403 like the proxy does.
async fn call_json(
    client: &reqwest::Client,
    ag: &Agnes,
    method: reqwest::Method,
    url: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    let mut last = String::new();
    for key in &ag.keys {
        let mut req = client
            .request(method.clone(), url)
            .header("Authorization", format!("Bearer {key}"));
        if let Some(b) = body {
            req = req.json(b);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                if (status == 401 || status == 403) && ag.keys.len() > 1 {
                    last = format!("HTTP {status}: {text:.200}");
                    continue; // dead key — try the next one
                }
                let v: Value = serde_json::from_str(&text).unwrap_or(json!({ "raw": text }));
                return Ok((status, v));
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("all {} key(s) failed: {last}", ag.keys.len()))
}

async fn video_status(client: &reqwest::Client, ag: &Agnes, video_id: &str) -> Result<Value, String> {
    let url = format!(
        "{}/agnesapi?video_id={}&model_name={VIDEO_MODEL}",
        ag.root, video_id
    );
    call_json(client, ag, reqwest::Method::GET, &url, None)
        .await
        .map(|(_, v)| v)
}

fn describe_task(v: &Value) -> String {
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("?");
    let progress = v.get("progress").and_then(|p| p.as_i64()).unwrap_or(0);
    let url = v.get("url").and_then(|s| s.as_str()).unwrap_or("");
    let vid = v.get("video_id").and_then(|s| s.as_str()).unwrap_or("");
    match (status, url.is_empty()) {
        ("completed", false) => format!("Video ready: {url}"),
        ("failed", _) => format!(
            "Video generation failed: {}",
            v.get("error").map(|e| e.to_string()).unwrap_or_default()
        ),
        _ => format!("status={status} progress={progress}% video_id={vid}"),
    }
}

async fn generate_video(client: &reqwest::Client, ag: &Agnes, args: &Value) -> Result<String, String> {
    let started = Instant::now();
    let prompt = args
        .get("prompt")
        .and_then(|p| p.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("prompt is required")?;
    let seconds = args
        .get("seconds")
        .and_then(|s| s.as_str().map(String::from).or_else(|| s.as_i64().map(|n| n.to_string())))
        .unwrap_or_else(|| "5".into());
    let aspect = args.get("aspect_ratio").and_then(|a| a.as_str()).unwrap_or("16:9");

    let create = json!({
        "model": VIDEO_MODEL, "prompt": prompt, "seconds": seconds,
        "mode": "text", "size": "720P", "aspect_ratio": aspect,
    });
    let url = format!("{}/v1/videos", ag.root);
    let retry_deadline = Instant::now() + CREATE_RETRY_BUDGET;
    let mut attempts = 0u32;
    let (status, resp) = loop {
        attempts += 1;
        let (status, resp) = call_json(client, ag, reqwest::Method::POST, &url, Some(&create)).await?;
        let body = resp.to_string();
        let queue_full =
            status == 503 && (body.contains("queue_full") || body.contains("queue is full"));
        if !queue_full || Instant::now() >= retry_deadline {
            break (status, resp);
        }
        tracing::info!("mcp: video queue full — create retry {attempts} in {CREATE_RETRY_STEP:?}");
        tokio::time::sleep(CREATE_RETRY_STEP).await;
    };
    if status >= 400 {
        return Err(format!("create task failed (HTTP {status}): {resp}"));
    }
    let video_id = resp
        .get("video_id")
        .or_else(|| resp.get("id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("no video_id in create response: {resp}"))?
        .to_string();

    // Whole call stays under ~100s so create-retries can't blow past the
    // client-side tool timeout: poll gets whatever is left of the window.
    let deadline = (Instant::now() + POLL_BUDGET).min(started + Duration::from_secs(100));
    loop {
        match video_status(client, ag, &video_id).await {
            Ok(v) => {
                let st = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                if st == "completed" || st == "failed" {
                    return Ok(describe_task(&v));
                }
            }
            Err(e) => return Ok(format!("task {video_id} created; status poll failed: {e}")),
        }
        if Instant::now() >= deadline {
            return Ok(format!(
                "Task {video_id} is still generating — call video_status with this video_id to get the URL when it finishes."
            ));
        }
        tokio::time::sleep(POLL_STEP).await;
    }
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn tools() -> Value {
    json!([
        {
            "name": "generate_video",
            "description": "Generate a short video from a text prompt via Agnes Video 2.5 Flash (720P, 4-12s). Returns the mp4 URL; if still rendering after ~55s returns a video_id for video_status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "What the video should show (scene, motion, camera)" },
                    "seconds": { "type": "string", "description": "Duration \"4\"..\"12\", default \"5\"" },
                    "aspect_ratio": { "type": "string", "enum": ["16:9","9:16","1:1","4:3","3:4","21:9"], "description": "default 16:9" }
                },
                "required": ["prompt"]
            }
        },
        {
            "name": "video_status",
            "description": "Check an Agnes video task by video_id; returns status/progress or the mp4 URL when completed.",
            "inputSchema": {
                "type": "object",
                "properties": { "video_id": { "type": "string" } },
                "required": ["video_id"]
            }
        }
    ])
}

pub fn run(cfg: Config) -> Result<()> {
    // `main` already lives inside #[tokio::main]'s runtime — a second
    // block_on would panic. The whole stdio loop moves to a plain thread
    // that owns its own current-thread runtime.
    std::thread::Builder::new()
        .name("mcp".into())
        .spawn(move || run_inner(cfg))
        .context("spawn mcp thread")?
        .join()
        .map_err(|_| anyhow::anyhow!("mcp thread panicked"))?
}

fn run_inner(cfg: Config) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let agnes = find_agnes(&cfg);
    if agnes.is_none() {
        tracing::warn!("mcp: no Agnes provider/keys in config — generate_video will report that");
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue, // not JSON-RPC — ignore noise on stdin
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        let reply: Option<Value> = match method {
            "initialize" => Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "nexuslink", "version": env!("CARGO_PKG_VERSION") }
            })),
            "ping" => Some(json!({})),
            "tools/list" => Some(json!({ "tools": tools() })),
            "tools/call" => {
                let name = msg.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
                let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                let res = match (&agnes, name) {
                    (None, _) => Err(format!(
                        "no Agnes provider configured — add one with an API key in the NexusLink panel ({VIDEO_MODEL} needs it)"
                    )),
                    (Some(ag), "generate_video") => rt
                        .block_on(generate_video(&client, ag, &args))
                        .map_err(|e| e.to_string()),
                    (Some(ag), "video_status") => {
                        let vid = args.get("video_id").and_then(|v| v.as_str()).unwrap_or("");
                        match rt.block_on(video_status(&client, ag, vid)) {
                            Ok(v) => Ok(describe_task(&v)),
                            Err(e) => Err(e),
                        }
                    }
                    (Some(_), other) => Err(format!("unknown tool: {other}")),
                };
                Some(match res {
                    Ok(text) => tool_result(text, false),
                    Err(e) => tool_result(e, true),
                })
            }
            _ => None, // notifications/* and unknown methods — MCP wants silence
        };

        if let (Some(id), Some(rep)) = (id, reply) {
            let frame = json!({ "jsonrpc": "2.0", "id": id, "result": rep });
            let _ = out.write_all(serde_json::to_string(&frame).unwrap().as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
    }
    Ok(())
}
