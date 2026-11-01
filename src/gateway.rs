//! Claude Desktop config writer.
//!
//! Claude Desktop's "third-party inference" mode reads a JSON file under
//! `Claude-3p/configLibrary/<uuid>.json`, selected via `_meta.json ->
//! appliedId`. We write only the keys we own (read-modify-write), register
//! our entry in _meta.json, and set `deploymentMode:"3p"` in the satellite
//! `claude_desktop_config.json` files so the desktop never asks for a mode.
//!
//! Model name validation (Desktop >= ~1.7196): `inferenceModels[].name` must
//! be a real Anthropic catalogue id — our slot names are exactly that;
//! `labelOverride` carries the real upstream model name for display.

use serde_json::{json, Value};
use std::path::PathBuf;

use crate::config::{flat_slots, Config};

/// ModelLink used a fixed UUID for its config-library entry; we use our own
/// fixed id so both tools can coexist without clobbering each other's file.
const OUR_ID: &str = "b0b0b0b0-c2c2-4d3d-8e8e-f5f5f5f5f5f5";

/// Locate the Claude-3p data dir across platforms/install flavors.
pub fn claude_3p_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let home = PathBuf::from(home);

    #[cfg(target_os = "macos")]
    let dir = home.join("Library/Application Support/Claude-3p");

    #[cfg(target_os = "windows")]
    let dir = {
        // Microsoft Store install: sandboxed package path.
        let store = (|| -> Option<PathBuf> {
            let packages = PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("Packages");
            let known = packages.join("Claude_pzs8sxrjxfjjc");
            if known.exists() {
                return Some(known.join("LocalCache").join("Roaming").join("Claude-3p"));
            }
            std::fs::read_dir(&packages).ok()?.flatten()
                .find(|e| e.file_name().to_string_lossy().starts_with("Claude_"))
                .map(|e| e.path().join("LocalCache").join("Roaming").join("Claude-3p"))
        })();
        store.unwrap_or_else(|| {
            let local = std::env::var("LOCALAPPDATA").ok().map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Local")).join("Claude-3p");
            if local.exists() {
                return local;
            }
            let roaming = std::env::var("APPDATA").ok().map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Roaming")).join("Claude-3p");
            roaming // default to Roaming even if absent — create on write
        })
    };

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let dir = home.join(".config/Claude-3p");

    Some(dir)
}

/// Keys we own in the gateway config — merged into whatever is already there.
fn write_gateway_keys(existing: &mut Value, port: u16, token: &str) {
    existing["inferenceProvider"] = json!("gateway");
    existing["inferenceGatewayBaseUrl"] = json!(format!("http://127.0.0.1:{port}"));
    existing["inferenceGatewayApiKey"] = json!(token);
    existing["inferenceGatewayAuthScheme"] = json!("bearer");
    existing["coworkEgressAllowedHosts"] = json!(["*"]);
    existing["disableDeploymentModeChooser"] = json!(true);
    existing["chatTabEnabled"] = json!(true);
    existing["modelDiscoveryEnabled"] = json!(false);
    // Only meaningful because the proxy injects real keep-alive bytes; a
    // silent response dies at ~5min regardless of this key.
    existing["inferenceStreamIdleTimeoutSec"] = json!(1800);
}

/// inferenceModels entries: slot name for validation, real name for display.
fn model_entries(config: &Config) -> Vec<Value> {
    flat_slots(config)
        .into_iter()
        .map(|(slot, name)| {
            json!({ "name": slot, "supports1m": false, "labelOverride": name })
        })
        .collect()
}

/// Write everything Claude needs and return a human summary.
pub fn apply_to_claude_desktop(config: &Config) -> Result<String, String> {
    if config.incoming_token.is_empty() {
        return Err("incoming_token is empty — set one in config.json first".into());
    }
    if config.providers.is_empty() {
        return Err("add at least one provider first".into());
    }

    let claude_dir = claude_3p_dir().ok_or("cannot locate home directory")?;
    let lib = claude_dir.join("configLibrary");
    std::fs::create_dir_all(&lib).map_err(|e| format!("create {}: {e}", lib.display()))?;

    // Prefer Claude's currently-applied file; fall back to our own entry.
    let meta_path = lib.join("_meta.json");
    let mut meta: Value = read_json(&meta_path).unwrap_or(json!({}));
    let applied_id = meta.get("appliedId").and_then(|v| v.as_str()).unwrap_or("");
    let target_id = if !applied_id.is_empty() && lib.join(format!("{applied_id}.json")).exists() {
        applied_id.to_string()
    } else {
        OUR_ID.to_string()
    };

    let config_file = lib.join(format!("{target_id}.json"));
    let mut existing = read_json(&config_file).unwrap_or(json!({}));
    write_gateway_keys(&mut existing, config.port, &config.incoming_token);
    existing["inferenceModels"] = json!(model_entries(config));

    write_atomic(&config_file, &serde_json::to_string_pretty(&existing).map_err(|e| e.to_string())?)?;

    if target_id == OUR_ID {
        meta["appliedId"] = json!(OUR_ID);
        let mut entries = meta.get("entries").and_then(|e| e.as_array()).cloned().unwrap_or_default();
        if !entries.iter().any(|e| e.get("id").and_then(|i| i.as_str()) == Some(OUR_ID)) {
            entries.push(json!({ "id": OUR_ID, "name": "nexuslink" }));
        }
        meta["entries"] = json!(entries);
        write_atomic(&meta_path, &serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?)?;
    }

    // deploymentMode satellites — every plausible location, as ModelLink did.
    // Same files get the MCP entry: Claude spawns `nexuslink mcp` as a stdio
    // server so the model can call generate_video / video_status.
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "nexuslink.exe".into());
    let mut wrote = vec![config_file.display().to_string()];
    for dir in satellite_dirs(&claude_dir) {
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("claude_desktop_config.json");
        let mut j = read_json(&p).unwrap_or(json!({}));
        let mut changed = false;
        if j.get("deploymentMode").and_then(|v| v.as_str()) != Some("3p") {
            j["deploymentMode"] = json!("3p");
            changed = true;
        }
        let mcp = json!({ "command": &exe, "args": ["mcp"] });
        if j.pointer("/mcpServers/nexuslink") != Some(&mcp) {
            j["mcpServers"]["nexuslink"] = mcp;
            changed = true;
        }
        if changed
            && write_atomic(&p, &serde_json::to_string_pretty(&j).unwrap_or_default()).is_ok()
        {
            wrote.push(p.display().to_string());
        }
    }

    // The Store-packaged desktop runs embedded Claude Code sessions which read
    // MCP servers from ~/.claude.json (user scope), not claude_desktop_config.
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let home = PathBuf::from(home);
        let p = home.join(".claude.json");
        let mut j = read_json(&p).unwrap_or(json!({}));
        let mcp = json!({ "type": "stdio", "command": &exe, "args": ["mcp"], "env": {} });
        if j.pointer("/mcpServers/nexuslink") != Some(&mcp) {
            j["mcpServers"]["nexuslink"] = mcp;
            if write_atomic(&p, &serde_json::to_string_pretty(&j).unwrap_or_default()).is_ok() {
                wrote.push(p.display().to_string());
            }
        }

        // Claude Code CLI (any terminal, incl. Herdr) takes auth from
        // ANTHROPIC_* env; settings.json's env block applies over the parent
        // process env, so this makes the bridge work without touching the
        // user's registry env or restarting shells.
        let p = home.join(".claude").join("settings.json");
        let mut j = read_json(&p).unwrap_or(json!({}));
        let mut changed = false;
        for (k, v) in [
            ("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{}", config.port)),
            ("ANTHROPIC_AUTH_TOKEN", config.incoming_token.clone()),
        ] {
            if j.pointer(&format!("/env/{k}")).and_then(|x| x.as_str()) != Some(&v) {
                j["env"][k] = json!(v);
                changed = true;
            }
        }
        if changed
            && write_atomic(&p, &serde_json::to_string_pretty(&j).unwrap_or_default()).is_ok()
        {
            wrote.push(p.display().to_string());
        }

        // Ship the video skill alongside the MCP registration so Claude Code
        // knows both the tool and the direct-API fallback playbook.
        let skill_dir = home.join(".claude").join("skills").join("nexuslink-video");
        let skill_file = skill_dir.join("SKILL.md");
        let skill_src = include_str!("../skills/nexuslink-video/SKILL.md");
        let current = std::fs::read_to_string(&skill_file).unwrap_or_default();
        if current != skill_src && std::fs::create_dir_all(&skill_dir).is_ok()
            && write_atomic(&skill_file, skill_src).is_ok()
        {
            wrote.push(skill_file.display().to_string());
        }
    }

    Ok(format!("applied to {}", wrote.join(", ")))
}

fn satellite_dirs(claude_dir: &std::path::Path) -> Vec<PathBuf> {
    let mut dirs = vec![claude_dir.to_path_buf()];
    if let Some(parent) = claude_dir.parent() {
        dirs.push(parent.join("Claude"));
    }
    #[cfg(target_os = "windows")]
    {
        for env in ["APPDATA", "LOCALAPPDATA"] {
            if let Ok(v) = std::env::var(env) {
                let p = PathBuf::from(v);
                dirs.push(p.join("Claude-3p"));
                dirs.push(p.join("Claude"));
            }
        }
    }
    dirs
}

fn read_json(p: &std::path::Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn write_atomic(path: &std::path::Path, data: &str) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}
