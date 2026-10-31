//! End-to-end translation tests: Anthropic request in -> OpenAI request out,
//! and back. Run: `cargo test`.

use nexuslink::translator::*;
use serde_json::json;

fn parse(v: serde_json::Value) -> MessagesRequest {
    serde_json::from_value(v).unwrap()
}

#[test]
fn system_prompt_becomes_first_openai_message() {
    let req = parse(json!({
        "model": "claude-sonnet-5",
        "max_tokens": 1024,
        "system": "You are terse.",
        "messages": [{"role": "user", "content": "hi"}]
    }));
    let out = anthropic_to_openai(&req, "real-model", false, true);
    let v = serde_json::to_value(&out).unwrap();
    assert_eq!(v["model"], "real-model");
    assert_eq!(v["messages"][0], json!({"role":"system","content":"You are terse."}));
    assert_eq!(v["messages"][1], json!({"role":"user","content":"hi"}));
    assert_eq!(v["max_completion_tokens"], 1024);
    assert!(v.get("max_tokens").is_none());
}

#[test]
fn system_block_array_joins_texts() {
    let req = parse(json!({
        "messages": [],
        "system": [
            {"type":"text","text":"part one"},
            {"type":"text","text":"part two","cache_control":{"type":"ephemeral"}}
        ]
    }));
    let out = anthropic_to_openai(&req, "m", false, true);
    let v = serde_json::to_value(&out).unwrap();
    assert_eq!(v["messages"][0]["content"], "part one\npart two");
}

#[test]
fn tool_use_and_tool_result_roundtrip() {
    let req = parse(json!({
        "messages": [
            {"role":"user","content":"weather?"},
            {"role":"assistant","content":[
                {"type":"text","text":"checking"},
                {"type":"tool_use","id":"tu_1","name":"get_weather","input":{"city":"SF"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_1","content":"sunny"}
            ]}
        ]
    }));
    let out = anthropic_to_openai(&req, "m", false, true);
    let v = serde_json::to_value(&out).unwrap();
    let msgs = v["messages"].as_array().unwrap();

        // assistant: text + tool_calls attached to one message
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], "checking");
    assert_eq!(msgs[1]["tool_calls"][0]["id"], "tu_1");
    assert_eq!(msgs[1]["tool_calls"][0]["type"], "function");
    assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_weather");
    // arguments are a JSON *string* in OpenAI
    let args: serde_json::Value =
        serde_json::from_str(msgs[1]["tool_calls"][0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args, json!({"city":"SF"}));

    // tool_result becomes a role:"tool" message
    assert_eq!(msgs[2]["role"], "tool");
    assert_eq!(msgs[2]["tool_call_id"], "tu_1");
    assert_eq!(msgs[2]["content"], "sunny");
}

#[test]
fn tool_defs_and_choice_translate() {
    let req = parse(json!({
        "messages": [{"role":"user","content":"x"}],
        "tools": [{"name":"f","description":"d","input_schema":{"type":"object","properties":{"a":{"type":"string"}}}}],
        "tool_choice": {"type":"tool","name":"f"}
    }));
    let out = anthropic_to_openai(&req, "m", false, true);
    let v = serde_json::to_value(&out).unwrap();
    assert_eq!(v["tools"][0]["function"]["name"], "f");
    assert_eq!(v["tools"][0]["function"]["parameters"]["properties"]["a"]["type"], "string");
    assert_eq!(v["tool_choice"], json!({"type":"function","function":{"name":"f"}}));
}

#[test]
fn stream_flag_requests_usage_chunk() {
    let req = parse(json!({"messages": [], "stream": true}));
    let v = serde_json::to_value(anthropic_to_openai(&req, "m", false, true)).unwrap();
    assert_eq!(v["stream"], true);
    assert_eq!(v["stream_options"]["include_usage"], true);
}

#[test]
fn effort_only_forwarded_when_provider_opts_in() {
    let req = parse(json!({
        "messages": [],
        "output_config": {"effort": "High"}
    }));
    let off = serde_json::to_value(anthropic_to_openai(&req, "m", false, true)).unwrap();
    assert!(off.get("reasoning_effort").is_none());
    let on = serde_json::to_value(anthropic_to_openai(&req, "m", true, true)).unwrap();
    assert_eq!(on["reasoning_effort"], "high");
}

#[test]
fn thinking_blocks_and_signatures_are_dropped() {
    let req = parse(json!({
        "messages": [{"role":"assistant","content":[
            {"type":"thinking","thinking":"hmm","signature":"sig"},
            {"type":"redacted_thinking","data":"xyz"},
            {"type":"text","text":"answer"}
        ]}]
    }));
    let v = serde_json::to_value(anthropic_to_openai(&req, "m", false, true)).unwrap();
    assert_eq!(v["messages"][0]["content"], "answer");
    assert!(v["messages"][0].get("signature").is_none());
}

#[test]
fn anthropic_fields_with_no_openai_home_are_dropped() {
    let req = parse(json!({
        "messages": [], "max_tokens": 5, "top_k": 40,
        "thinking": {"type":"enabled","budget_tokens":8192},
        "metadata": {"user_id": "u"}
    }));
    let v = serde_json::to_value(anthropic_to_openai(&req, "m", false, true)).unwrap();
    for k in ["top_k", "thinking", "metadata"] {
        assert!(v.get(k).is_none(), "{k} leaked upstream");
    }
}

// --- response side -----------------------------------------------------------

#[test]
fn openai_response_maps_text_tool_calls_and_usage() {
    let resp = json!({
        "id": "chatcmpl-1", "model": "m",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "let me check",
                "tool_calls": [{"id":"c1","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}}]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 20,
                  "prompt_tokens_details": {"cached_tokens": 30}}
    });
    let out = openai_to_anthropic(&resp, "claude-sonnet-5");
    assert_eq!(out["type"], "message");
    assert_eq!(out["model"], "claude-sonnet-5");
    assert_eq!(out["content"][0]["type"], "text");
    assert_eq!(out["content"][1]["type"], "tool_use");
    assert_eq!(out["content"][1]["input"], json!({"a":1}));
    assert_eq!(out["stop_reason"], "tool_use");
    // prompt(100) includes cache(30) -> net input 70
    assert_eq!(out["usage"]["input_tokens"], 70);
    assert_eq!(out["usage"]["cache_read_input_tokens"], 30);
}

#[test]
fn finish_reason_length_maps_to_max_tokens() {
    assert_eq!(map_finish_reason(Some("length"), false), "max_tokens");
    assert_eq!(map_finish_reason(Some("stop"), false), "end_turn");
    // provider says "stop" but emitted tool calls — Anthropic calls that tool_use
    assert_eq!(map_finish_reason(Some("stop"), true), "tool_use");
    assert_eq!(map_finish_reason(Some("content_filter"), false), "refusal");
}

#[test]
fn reasoning_content_becomes_thinking_block() {
    let resp = json!({
        "choices": [{"message": {"content": "answer", "reasoning_content": "hmm"}, "finish_reason": "stop"}]
    });
    let out = openai_to_anthropic(&resp, "m");
    assert_eq!(out["content"][0], json!({"type":"thinking","thinking":"hmm"}));
    assert_eq!(out["content"][1]["text"], "answer");
}

#[test]
fn think_tags_inside_text_split_into_blocks() {
    let blocks = split_think_text("before<think>secret</think>after");
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "thinking");
    assert_eq!(blocks[1]["thinking"], "secret");
    assert_eq!(blocks[2]["text"], "after");
}
