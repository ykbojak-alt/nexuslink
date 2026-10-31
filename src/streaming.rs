//! Stateful stream converter: consumes OpenAI chat-completion chunks one at a
//! time and emits Anthropic SSE event frames.
//!
//! Anthropic streams are *block-indexed*: every content block gets an index,
//! opened by `content_block_start`, fed by `content_block_delta`, closed by
//! `content_block_stop`. OpenAI streams are *delta-indexed*: text arrives in
//! `delta.content`, tool calls arrive under `delta.tool_calls[].index`.
//! The two index spaces don't line up — OpenAI tool index 5 might be the
//! first tool call — so we remap: our own `block_index` counter allocates
//! Anthropic indexes, `tool_blocks` maps OpenAI index -> Anthropic index.
//!
//! Feed each upstream `data:` payload to [`StreamConverter::process_chunk`];
//! on `data: [DONE]` call [`StreamConverter::finish`].

use serde_json::{json, Value};

use crate::translator::map_finish_reason;

/// Build one SSE frame (`event:` + `data:` lines, blank-line terminated).
fn event(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {}\n\n", serde_json::to_string(data).unwrap_or_default())
}

#[derive(Default)]
struct Ctx {
    message_start_sent: bool,
    message_stop_sent: bool,
    text_open: bool,
    thinking_open: bool,
    any_tool_started: bool,
    block_index: i64,
    /// OpenAI tool_calls[].index -> Anthropic content-block index.
    tool_blocks: std::collections::HashMap<i64, i64>,
    model: String,
    msg_id: Value,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
}

pub struct StreamConverter {
    ctx: Ctx,
    stop_reason: &'static str,
}

impl StreamConverter {
    /// `model`/`input_tokens` are what message_start advertises before the
    /// upstream has reported real numbers.
    pub fn new(model: String, input_tokens: i64) -> Self {
        Self {
            ctx: Ctx {
                model,
                input_tokens,
                ..Default::default()
            },
            stop_reason: "end_turn",
        }
    }

    /// Accumulated usage so far (input, output, cache_creation, cache_read).
    /// Public for request logging / stats; the HTTP layer doesn't read it yet.
    #[allow(dead_code)]
    pub fn usage(&self) -> (i64, i64, i64, i64) {
        (
            self.ctx.input_tokens,
            self.ctx.output_tokens,
            self.ctx.cache_creation_tokens,
            self.ctx.cache_read_tokens,
        )
    }

    fn message_start(&mut self, id: &Value, out: &mut Vec<String>) {
        if self.ctx.message_start_sent {
            return;
        }
        self.ctx.message_start_sent = true;
        self.ctx.msg_id = id.clone();
        out.push(event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.ctx.model,
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {
                        "input_tokens": self.ctx.input_tokens,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": self.ctx.cache_creation_tokens,
                        "cache_read_input_tokens": self.ctx.cache_read_tokens
                    }
                }
            }),
        ));
    }

    /// Close whichever singleton block (text or thinking) is open.
    /// Both share `block_index`: only one can be open at a time.
    fn close_singletons(&mut self, out: &mut Vec<String>) {
        if self.ctx.text_open || self.ctx.thinking_open {
            out.push(event(
                "content_block_stop",
                &json!({ "type": "content_block_stop", "index": self.ctx.block_index }),
            ));
            self.ctx.text_open = false;
            self.ctx.thinking_open = false;
            self.ctx.block_index += 1;
        }
    }

    fn open_block(&mut self, out: &mut Vec<String>, block: Value) {
        out.push(event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.ctx.block_index,
                "content_block": block
            }),
        ));
    }

    fn ensure_thinking(&mut self, out: &mut Vec<String>) {
        if self.ctx.thinking_open {
            return;
        }
        self.close_singletons(out);
        self.ctx.thinking_open = true;
        self.open_block(out, json!({ "type": "thinking", "thinking": "" }));
    }

    fn ensure_text(&mut self, out: &mut Vec<String>) {
        if self.ctx.text_open {
            return;
        }
        self.close_singletons(out);
        self.ctx.text_open = true;
        self.open_block(out, json!({ "type": "text", "text": "" }));
    }

    fn delta(&mut self, out: &mut Vec<String>, index: i64, delta: Value) {
        out.push(event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": delta
            }),
        ));
    }

    /// Close every block that's still open (text/thinking + all tool blocks).
    fn close_all(&mut self, out: &mut Vec<String>) {
        self.close_singletons(out);
        if !self.ctx.tool_blocks.is_empty() {
            let mut idxs: Vec<i64> = self.ctx.tool_blocks.values().copied().collect();
            idxs.sort_unstable();
            for i in idxs {
                out.push(event(
                    "content_block_stop",
                    &json!({ "type": "content_block_stop", "index": i }),
                ));
            }
            self.ctx.tool_blocks.clear();
        }
    }

    fn handle_tool_call(&mut self, tc: &Value, out: &mut Vec<String>) {
        let oi = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let func = tc.get("function");

        // First chunk for this call carries id (+ usually name): open the block.
        if !id.is_empty() && !self.ctx.tool_blocks.contains_key(&oi) {
            self.close_singletons(out);
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ai = self.ctx.block_index;
            self.ctx.block_index += 1;
            self.ctx.tool_blocks.insert(oi, ai);
            self.ctx.any_tool_started = true;
            out.push(event(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": ai,
                    "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
                }),
            ));
        }

        // Argument fragments route to the block by OpenAI index.
        if let Some(args) = func
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            if let Some(&ai) = self.ctx.tool_blocks.get(&oi) {
                self.delta(
                    out,
                    ai,
                    json!({ "type": "input_json_delta", "partial_json": args }),
                );
            }
        }
    }

    /// Fold one upstream usage object into our running counters.
    /// OpenAI's prompt_tokens *includes* cached tokens; Anthropic's
    /// input_tokens excludes them — keep the net value.
    fn take_usage(&mut self, usage: &Value) {
        if let Some(cr) = [
            usage.get("cache_read_input_tokens"),
            usage.pointer("/prompt_tokens_details/cached_tokens"),
            usage.pointer("/input_tokens_details/cached_tokens"),
            usage.get("cached_tokens"),
        ]
        .iter()
        .find_map(|v| v.and_then(|v| v.as_i64()))
        {
            self.ctx.cache_read_tokens = cr;
        }
        if let Some(it) = usage
            .get("prompt_tokens")
            .or_else(|| usage.get("input_tokens"))
            .and_then(|v| v.as_i64())
        {
            self.ctx.input_tokens = (it - self.ctx.cache_read_tokens).max(0);
        }
        if let Some(ot) = usage
            .get("completion_tokens")
            .or_else(|| usage.get("output_tokens"))
            .and_then(|v| v.as_i64())
        {
            self.ctx.output_tokens = ot;
        }
        if let Some(cc) = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_i64())
        {
            self.ctx.cache_creation_tokens = cc;
        }
    }

    /// Consume one OpenAI `data:` JSON chunk; returns SSE frames to send.
    pub fn process_chunk(&mut self, chunk: &Value, out: &mut Vec<String>) {
        let id = chunk.get("id").cloned().unwrap_or(json!("msg_stream"));
        self.message_start(&id, out);

        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.take_usage(usage);
        }

        let Some(ch) = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        else {
            return;
        };

        if let Some(delta) = ch.get("delta") {
            // reasoning_content (DeepSeek) / reasoning (OpenRouter et al.)
            // -> Anthropic thinking blocks.
            if let Some(reason) = reasoning_text(delta).filter(|s| !s.is_empty()) {
                self.ensure_thinking(out);
                let idx = self.ctx.block_index;
                self.delta(out, idx, json!({ "type": "thinking_delta", "thinking": reason }));
            }

            if let Some(text) = delta
                .get("content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                self.ensure_text(out);
                let idx = self.ctx.block_index;
                self.delta(out, idx, json!({ "type": "text_delta", "text": text }));
            }

            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    self.handle_tool_call(tc, out);
                }
            }
        }

        // Empty-string finish_reason (some providers) is "still going", not a stop.
        if let Some(fr) = ch
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            self.stop_reason = map_finish_reason(Some(fr), self.ctx.any_tool_started);
            self.close_all(out);
        }
    }

    /// Emit on `data: [DONE]` (or stream end): closes blocks, sends
    /// message_delta with final usage + message_stop.
    pub fn finish(&mut self, out: &mut Vec<String>) {
        if !self.ctx.message_start_sent {
            self.message_start(&json!("msg_stream"), out);
        }
        self.close_all(out);
        if !self.ctx.message_stop_sent {
            out.push(event(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": self.stop_reason, "stop_sequence": Value::Null },
                    "usage": {
                        "input_tokens": self.ctx.input_tokens,
                        "output_tokens": self.ctx.output_tokens,
                        "cache_creation_input_tokens": self.ctx.cache_creation_tokens,
                        "cache_read_input_tokens": self.ctx.cache_read_tokens
                    }
                }),
            ));
            out.push(event("message_stop", &json!({ "type": "message_stop" })));
            self.ctx.message_stop_sent = true;
        }
    }
}

/// Provider-specific reasoning field shapes -> a text string.
fn reasoning_text(delta: &Value) -> Option<&str> {
    for key in ["reasoning_content", "reasoning"] {
        let v = delta.get(key)?;
        if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
            return Some(s);
        }
        for sub in ["content", "text"] {
            if let Some(s) = v.get(sub).and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
                return Some(s);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(c: &mut StreamConverter, chunk: Value) -> String {
        let mut v = Vec::new();
        c.process_chunk(&chunk, &mut v);
        v.join("")
    }

    #[test]
    fn text_stream_lifecycle() {
        let mut c = StreamConverter::new("gpt-4o".into(), 0);
        let e1 = joined(
            &mut c,
            json!({"id":"x","choices":[{"delta":{"content":"Hi"},"finish_reason":null}]}),
        );
        assert!(e1.contains("event: message_start"));
        assert!(e1.contains("\"type\":\"text\""));
        assert!(e1.contains("text_delta"));

        let e2 = joined(&mut c, json!({"choices":[{"delta":{},"finish_reason":"stop"}]}));
        assert!(e2.contains("event: content_block_stop"));

        let mut fin = Vec::new();
        c.finish(&mut fin);
        let fin = fin.join("");
        assert!(fin.contains("\"stop_reason\":\"end_turn\""));
        assert!(fin.contains("event: message_stop"));
    }

    #[test]
    fn tool_calls_route_arguments_by_openai_index() {
        let mut c = StreamConverter::new("m".into(), 0);
        let e0 = joined(&mut c, json!({
            "id":"x","choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"a","function":{"name":"fa","arguments":"{\"x\":"}},
                {"index":1,"id":"b","function":{"name":"fb","arguments":"{\"y\":"}}
            ]}}]
        }));
        assert!(e0.contains("\"id\":\"a\"") && e0.contains("\"id\":\"b\""));

        // interleaved continuation goes back to the right block
        let e1 = joined(&mut c, json!({
            "choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"1}"}}
            ]}}]
        }));
        assert!(e1.contains("\"index\":0") && e1.contains("1}"));
    }

    #[test]
    fn reasoning_becomes_thinking_then_text_at_next_index() {
        let mut c = StreamConverter::new("m".into(), 0);
        let e0 = joined(&mut c, json!({
            "id":"x","choices":[{"delta":{"reasoning_content":"hmm"}}]
        }));
        assert!(e0.contains("thinking_delta"));
        let e1 = joined(&mut c, json!({"choices":[{"delta":{"content":"hi"}}]}));
        assert!(e1.contains("content_block_stop") && e1.contains("text_delta"));
    }

    #[test]
    fn usage_net_input_excludes_cache() {
        let mut c = StreamConverter::new("m".into(), 0);
        c.process_chunk(&json!({"id":"x","choices":[{"delta":{"content":"a"}}]}), &mut Vec::new());
        c.process_chunk(&json!({
            "choices":[],
            "usage":{"prompt_tokens":100,"completion_tokens":50,
                     "prompt_tokens_details":{"cached_tokens":30}}
        }), &mut Vec::new());
        assert_eq!(c.usage(), (70, 50, 0, 30));
    }
}
