//! Anthropic Messages API <-> OpenAI Chat Completions translation.
//!
//! The request side is strongly typed on what we emit (OpenAI) and tolerant
//! on what we accept (Anthropic): unknown Anthropic fields are dropped rather
//! than rejected — compat providers reject unknown fields, so whitelisting is
//! the safe direction.
//!
//! The response side works on `serde_json::Value` because OpenAI-compatible
//! providers differ in the details (usage field names, cached-token nesting,
//! reasoning fields) and rejecting a slightly-off response would be worse
//! than translating it loosely.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Anthropic Messages API — inbound shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct MessagesRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub max_tokens: Option<i64>,
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub tools: Vec<ToolDef>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Anthropic-only sampler knob; OpenAI has no equivalent — dropped.
    #[serde(default)]
    #[allow(dead_code)]
    pub top_k: Option<i64>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    /// Desktop thinking block: {"type":"adaptive"|"enabled"|"disabled", ...}
    /// Accepted and dropped — no OpenAI equivalent.
    #[serde(default)]
    #[allow(dead_code)]
    pub thinking: Option<Value>,
    /// Desktop effort selector: {"effort":"low"|"medium"|"high"|"xhigh"|"max"|"ultra"}
    #[serde(default)]
    pub output_config: Option<Value>,
    /// Everything else (metadata, service_tier, betas…) is captured and dropped.
    #[serde(flatten)]
    #[allow(dead_code)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolDef {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
}

// ---------------------------------------------------------------------------
// OpenAI Chat Completions — outbound request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

// ---------------------------------------------------------------------------
// Request translation: Anthropic -> OpenAI
// ---------------------------------------------------------------------------

/// Translate a parsed Anthropic request into an OpenAI chat request Value.
///
/// `upstream_model` is the resolved real model id (the slot name is never sent
/// upstream). Provider capability flags shape the token/effort fields.
pub fn anthropic_to_openai(
    req: &MessagesRequest,
    upstream_model: &str,
    supports_reasoning_effort: bool,
    use_max_completion_tokens: bool,
) -> ChatRequest {
    let mut messages: Vec<Value> = Vec::new();

    // system -> leading system message
    if let Some(sys) = req.system.as_ref().and_then(system_to_openai) {
        messages.push(sys);
    }
    for m in &req.messages {
        convert_message(m, &mut messages);
    }

    // tools
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter(|t| !t.name.is_empty())
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description.clone().unwrap_or_default(),
                    "parameters": t.input_schema.clone().unwrap_or(json!({"type":"object"})),
                }
            })
        })
        .collect();

    let stream = req.stream.unwrap_or(false);

    // effort: desktop's output_config.effort -> OpenAI reasoning_effort,
    // only when the provider opts in (compat endpoints 400 on unknown fields).
    let reasoning_effort = if supports_reasoning_effort {
        req.output_config
            .as_ref()
            .and_then(|oc| oc.get("effort"))
            .and_then(|e| e.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
    } else {
        None
    };

    let (max_tokens, max_completion_tokens) = if use_max_completion_tokens {
        (None, req.max_tokens.filter(|t| *t > 0))
    } else {
        (req.max_tokens.filter(|t| *t > 0), None)
    };

    ChatRequest {
        model: upstream_model.to_string(),
        messages,
        max_tokens,
        max_completion_tokens,
        temperature: req.temperature.filter(|t| *t > 0.0),
        top_p: req.top_p,
        stop: req.stop_sequences.clone().filter(|s| !s.is_empty()),
        stream: stream.then_some(true),
        stream_options: stream.then(|| json!({ "include_usage": true })),
        tool_choice: if tools.is_empty() {
            None
        } else {
            Some(convert_tool_choice(req.tool_choice.as_ref()))
        },
        tools,
        reasoning_effort,
    }
}

/// Anthropic `system` (string or block array) -> OpenAI system message.
fn system_to_openai(system: &Value) -> Option<Value> {
    let text = match system {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.is_empty()).then(|| json!({ "role": "system", "content": text }))
}

/// One Anthropic message -> one or more OpenAI messages.
///
/// tool_use blocks ride on the same message as sibling text (OpenAI attaches
/// tool_calls to the assistant message); tool_result blocks become separate
/// `role:"tool"` messages after it. thinking/redacted_thinking/signature
/// fields are dropped — they're Anthropic-internal.
fn convert_message(msg: &AnthropicMessage, out: &mut Vec<Value>) {
    let role = if msg.role == "assistant" { "assistant" } else { "user" };
    match &msg.content {
        Value::String(s) => {
            out.push(json!({ "role": role, "content": s }));
        }
        Value::Array(blocks) => {
            let mut texts: Vec<String> = Vec::new();
            let mut images: Vec<Value> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut tool_results: Vec<Value> = Vec::new();

            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()).filter(|s| !s.is_empty()) {
                            texts.push(t.to_string());
                        }
                    }
                    "image" => {
                        // Anthropic: {"type":"image","source":{"type":"base64","media_type","data"}}
                        // OpenAI:    {"type":"image_url","image_url":{"url":"data:<mime>;base64,<data>"}}
                        if let Some(v) = image_block_to_openai(b) {
                            images.push(v);
                        }
                    }
                    "tool_use" => {
                        let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        if id.is_empty() || name.is_empty() {
                            continue;
                        }
                        let args = serde_json::to_string(
                            b.get("input").unwrap_or(&json!({})),
                        )
                        .unwrap_or_else(|_| "{}".into());
                        tool_calls.push(json!({
                            "id": id, "type": "function",
                            "function": { "name": name, "arguments": args }
                        }));
                    }
                    "tool_result" => {
                        tool_results.push(json!({
                            "role": "tool",
                            "tool_call_id": b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or(""),
                            "content": tool_result_text(b.get("content").unwrap_or(&Value::Null)),
                        }));
                    }
                    // thinking / redacted_thinking / server_tool_use / unknown: drop
                    _ => {}
                }
            }

            let content: Option<Value> = if images.is_empty() {
                if texts.is_empty() {
                    None
                } else {
                    Some(json!(texts.join("\n")))
                }
            } else {
                // Mixed text+image -> OpenAI parts array
                let mut parts: Vec<Value> = texts
                    .into_iter()
                    .map(|t| json!({ "type": "text", "text": t }))
                    .collect();
                parts.extend(images);
                Some(Value::Array(parts))
            };

            if content.is_some() || !tool_calls.is_empty() {
                let mut m = serde_json::Map::new();
                m.insert("role".into(), json!(role));
                if let Some(c) = content {
                    m.insert("content".into(), c);
                }
                if !tool_calls.is_empty() {
                    m.insert("tool_calls".into(), json!(tool_calls));
                }
                out.push(Value::Object(m));
            }
            out.extend(tool_results);
        }
        _ => {}
    }
}

fn image_block_to_openai(b: &Value) -> Option<Value> {
    let src = b.get("source")?;
    let url = if src.get("type").and_then(|t| t.as_str()) == Some("base64") {
        let mime = src.get("media_type").and_then(|v| v.as_str()).unwrap_or("image/png");
        let data = src.get("data").and_then(|v| v.as_str()).unwrap_or("");
        format!("data:{mime};base64,{data}")
    } else {
        src.get("url").and_then(|v| v.as_str())?.to_string()
    };
    Some(json!({ "type": "image_url", "image_url": { "url": url } }))
}

/// tool_result.content: string, or array of text/image blocks -> text.
/// (OpenAI tool messages take plain content; images degrade to a marker.)
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b.get("type").and_then(|t| t.as_str()) {
                Some("text") => b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                Some("image") => "[image]".to_string(),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn convert_tool_choice(tc: Option<&Value>) -> Value {
    match tc {
        Some(Value::Object(o)) => match o.get("type").and_then(|t| t.as_str()) {
            Some("tool") => json!({
                "type": "function",
                "function": { "name": o.get("name").and_then(|n| n.as_str()).unwrap_or("") }
            }),
            Some("any") => json!("required"),
            Some("none") => json!("none"),
            _ => json!("auto"),
        },
        Some(Value::String(s)) => json!(s),
        _ => json!("auto"),
    }
}

// ---------------------------------------------------------------------------
// Response translation: OpenAI -> Anthropic (non-streaming)
// ---------------------------------------------------------------------------

/// Map an OpenAI finish_reason to Anthropic stop_reason.
/// `has_tool_calls` disambiguates providers that report plain "stop" after
/// emitting tool_calls — Anthropic semantics say that's a tool_use stop.
pub fn map_finish_reason(fr: Option<&str>, has_tool_calls: bool) -> &'static str {
    if has_tool_calls {
        return "tool_use";
    }
    match fr {
        Some("stop") | None => "end_turn",
        Some("length") => "max_tokens",
        Some("tool_calls") | Some("function_call") => "tool_use",
        Some("content_filter") => "refusal",
        Some("stop_sequence") => "stop_sequence",
        _ => "end_turn",
    }
}

/// Non-streaming OpenAI chat response -> Anthropic message JSON.
pub fn openai_to_anthropic(resp: &Value, display_model: &str) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut stop_reason = "end_turn";
    let mut has_tool = false;

    if let Some(ch) = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
    {
        if let Some(msg) = ch.get("message") {
            // reasoning fields -> thinking block (DeepSeek reasoning_content /
            // OpenRouter reasoning), then text, then tool_use.
            for key in ["reasoning_content", "reasoning"] {
                if let Some(t) = msg.get(key).and_then(reasoning_str).filter(|s| !s.is_empty()) {
                    content.push(json!({ "type": "thinking", "thinking": t }));
                    break;
                }
            }
            if let Some(text) = msg.get("content").and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
                for blk in split_think_text(text) {
                    content.push(blk);
                }
            }
            if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let input: Value = tc
                        .pointer("/function/arguments")
                        .and_then(|a| a.as_str())
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(json!({}));
                    content.push(json!({
                        "type": "tool_use",
                        "id": tc.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                        "name": tc.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or(""),
                        "input": input,
                    }));
                    has_tool = true;
                }
            }
        }
        stop_reason = map_finish_reason(
            ch.get("finish_reason").and_then(|v| v.as_str()),
            has_tool,
        );
    }

    let usage = resp.get("usage");
    let prompt = usage_field(usage, &["prompt_tokens", "input_tokens"]);
    let cache_read = cache_read_tokens(usage);
    let output = usage_field(usage, &["completion_tokens", "output_tokens"]);

    json!({
        "id": resp.get("id").cloned().unwrap_or(json!("msg_0")),
        "type": "message",
        "role": "assistant",
        "model": display_model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": (prompt - cache_read).max(0),
            "output_tokens": output,
            "cache_creation_input_tokens": usage_field(usage, &["cache_creation_input_tokens"]),
            "cache_read_input_tokens": cache_read
        }
    })
}

fn reasoning_str(v: &Value) -> Option<&str> {
    // reasoning may be a string or {"content"/"text": "..."}
    v.as_str()
        .or_else(|| v.get("content").and_then(|c| c.as_str()))
        .or_else(|| v.get("text").and_then(|c| c.as_str()))
}

fn usage_field(usage: Option<&Value>, keys: &[&str]) -> i64 {
    keys.iter()
        .filter_map(|k| usage.and_then(|u| u.get(*k)).and_then(|v| v.as_i64()))
        .find(|v| *v > 0)
        .unwrap_or(0)
}

/// cached_tokens sits inside prompt_tokens on OpenAI — Claude's
/// input_tokens excludes cache reads, so callers subtract it.
fn cache_read_tokens(usage: Option<&Value>) -> i64 {
    usage
        .and_then(|u| {
            u.get("cache_read_input_tokens")
                .or_else(|| u.pointer("/prompt_tokens_details/cached_tokens"))
                .or_else(|| u.pointer("/input_tokens_details/cached_tokens"))
                .or_else(|| u.get("cached_tokens"))
                .and_then(|v| v.as_i64())
        })
        .unwrap_or(0)
}

/// Some providers embed <think>…</think> in plain text instead of using a
/// reasoning field — split it into thinking/text blocks so the client sees
/// real thinking content instead of markup.
pub fn split_think_text(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        let before = &rest[..start];
        if !before.is_empty() {
            out.push(json!({ "type": "text", "text": before }));
        }
        let after = &rest[start + 7..];
        match after.find("</think>") {
            Some(end) => {
                out.push(json!({ "type": "thinking", "thinking": &after[..end] }));
                rest = &after[end + 8..];
            }
            None => {
                out.push(json!({ "type": "thinking", "thinking": after }));
                return out;
            }
        }
    }
    if !rest.is_empty() {
        out.push(json!({ "type": "text", "text": rest }));
    }
    if out.is_empty() {
        out.push(json!({ "type": "text", "text": text }));
    }
    out
}
