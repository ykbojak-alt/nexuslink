# NexusLink

[Русская версия → README.ru.md](README.ru.md)

Local API gateway that lets **Claude Desktop** talk to **any OpenAI-compatible
provider** (OpenRouter, Groq, Ollama, vLLM, LM Studio, together.ai, …) — with
provider/key rotation, a built-in management panel, and honest failure modes.

```
Claude Desktop ──Anthropic /v1/messages──▶ NexusLink (127.0.0.1:5679)
                                            │  auth + slots + rotate + translate
                                            ▼
                                     OpenAI /v1/chat/completions
                                            │
                                     provider API ──▶ real LLM
                                            │
                OpenAI SSE chunks ──▶ Anthropic SSE events ──▶ Claude Desktop
```

Clean-room implementation. Architecture lessons came from a source review of
ModelLink (CC BY-NC-ND — **no code copied**) and conversion semantics were
studied in ccMesh (Apache-2.0); everything here is original code, Apache-2.0.

## What it does

| Concern | Behavior |
|---|---|
| **Incoming auth** | Every request needs `Authorization: Bearer <incoming_token>` or `x-api-key`. Missing/wrong → `401`. Loopback bind is *not* authentication. |
| **Model slots** | Claude Desktop whitelists real Anthropic model IDs, so each upstream model gets a *slot* (`claude-sonnet-5`, `claude-haiku-4-5`, …). Unmapped slot → honest `400`, never silent substitution. Date suffixes (`claude-sonnet-5-20260101`) resolve to the same mapping. |
| **Key rotation** | A provider may list several `api_keys`. On `401/402/403/429` the failing key is parked for 15 min (masked tag `key|gateway|…tail`) and the request retries with the next key — no request is spent on a key already known dead. |
| **Provider rotation** | Several providers may map models to the **same slot** — they form a fallback chain in config order. On rotatable errors (`401/402/403/404/429/5xx/connect-fail`) NexusLink falls through to the next candidate. Dead endpoints park model-wide (`model|gateway|model-name`). |
| **Honest exhaustion** | All candidates dead → `502` saying so — never a fabricated success. Non-rotatable upstream errors (e.g. `400`) pass through unchanged. |
| **Translation** | `system` → first `role:system` message · content blocks → OpenAI messages · `tool_use` → `tool_calls` · `tool_result` → `role:"tool"` · `tools`+`tool_choice` → OpenAI function tools · `max_tokens` → `max_completion_tokens` (or `max_tokens` per provider flag) |
| **Streaming** | OpenAI delta chunks → Anthropic SSE (`message_start` → `content_block_*` → `message_delta` → `message_stop`). Tool-call argument fragments arrive by `index` and are re-emitted as `input_json_delta`. `reasoning_content` becomes a `thinking` block. Rotation only happens **before** a stream starts — bytes in flight are never switched mid-response. |
| **Heartbeat** | If the upstream stream is silent for **15 s**, an SSE comment `: ping` is injected so Claude Desktop's `inferenceStreamIdleTimeoutSec` doesn't kill the connection. |
| **Management panel** | `nexuslink ui` serves a Nexus Lux panel at `/` (EN/RU): edit providers, key pools and slots, watch the rotation blacklist live, apply to Claude Desktop. The token arrives via URL fragment — never in query, logs or history. |
| **Claude Desktop apply** | `nexuslink apply` writes `Claude-3p/configLibrary/<uuid>.json` with `inferenceProvider: gateway`, gateway URL+key, and `inferenceModels` slots — preserving unrelated keys, atomic write. |

## Install

Grab an installer from [Releases](../../releases/latest):

| Platform | File | v0.2.0 |
|---|---|---|
| Windows | `.msi` installer (Start-menu + desktop shortcuts) | [download](../../releases/download/v0.2.0/nexuslink-0.2.0-x86_64.msi) |
| Windows | `.zip` standalone exe, zero dependencies | [download](../../releases/download/v0.2.0/nexuslink-0.2.0-windows-x86_64.zip) |
| macOS | `.dmg` universal — Apple Silicon + Intel | [download](../../releases/download/v0.2.0/nexuslink-0.2.0-macos-universal.dmg) |

The macOS bundle is unsigned: right-click → **Open** on first launch.

**From source**

```bash
cargo build --release   # one binary: server + desktop app + MCP + CLI
```

## First run

```bash
# 1) create ~/.nexuslink/config.json with a generated incoming_token
./nexuslink init

# 2) edit the config — or run the panel and do it there
./nexuslink app             # desktop app: background proxy + WebView2 panel,
                            # X minimizes to tray (right-click → Quit)

./nexuslink ui              # serves proxy + opens the panel in a browser

# Claude Desktop spawns this itself via mcpServers (written by `apply`):
./nexuslink mcp             # stdio MCP: generate_video / video_status (Agnes)

# headless: just the proxy
./nexuslink serve           # binds 127.0.0.1:5679

# 3) wire Claude Desktop (writes its configLibrary file)
./nexuslink apply           # then restart Claude Desktop
```

Other subcommand: `nexuslink slots` — print current slot → model mappings.

**Migrating from openbridge:** if `~/.openbridge/config.json` exists and
`~/.nexuslink/config.json` does not, the old file is read automatically; the
next save lands in the new location. The legacy file is never modified.

## Configuration (`~/.nexuslink/config.json`)

```jsonc
{
  "incoming_token": "nl-…",            // generated by `init`; put this in
                                        // Claude Desktop's gateway API key
  "port": 5679,
  "providers": [
    {
      "name": "OpenRouter",
      "base_url": "https://openrouter.ai/api",   // no trailing /v1 needed
      "api_keys": ["sk-or-…A", "sk-or-…B"],       // pool: rotates on 401/402/403/429
      // "api_key": "sk-or-…"                    // single-key shorthand still works
      // optional knobs:
      "chat_path": "",                            // default "/v1/chat/completions"
      "supports_reasoning_effort": false,         // many providers 400 on it
      "use_max_completion_tokens": true,          // false -> send max_tokens
      "models": [
        { "name": "meta-llama/llama-3.1-70b-instruct", "slot": "claude-sonnet-5" },
        { "name": "qwen/qwen-2.5-72b-instruct",        "slot": "claude-haiku-4-5" }
      ]
    },
    {
      // second provider on the SAME slot -> fallback for claude-sonnet-5
      "name": "Groq",
      "base_url": "https://api.groq.com/openai",
      "api_key": "gsk_…",
      "models": [
        { "name": "llama-3.3-70b-versatile", "slot": "claude-sonnet-5" }
      ]
    },
    {
      "name": "Ollama (local)",
      "base_url": "http://localhost:11434",
      "chat_path": "/v1/chat/completions",
      "api_key": "ollama",                        // any non-empty string
      "models": [
        { "name": "llama3.1:70b", "slot": "claude-opus-4-6" }
      ]
    }
  ]
}
```

**Rotation semantics** (same model as a trading-grade LLM pool):

- Candidates for a slot = every `(provider, model, key)` triple that maps to
  it, in provider order; keys within a provider rotate round-robin per model.
- `401/402/403/429` → park **that key** on **that gateway** for 15 min.
- `404`/`5xx`/connect-fail → park **that model** on **that gateway**; after
  repeated failures the model is skipped without spending requests.
- Parked keys are skipped before the request is even sent; the panel shows
  them with masked tails (`…tail6`) and remaining TTL.
- Blacklist lives in memory — a restart clears it (a crash-looping provider
  re-earns its chance on restart, which is the desired behavior).

**Slot pool** (Claude Desktop's recognized IDs — reuse them, don't invent):

```
claude-opus-5  claude-sonnet-5  claude-opus-4-8  claude-opus-4-7
claude-opus-4-6  claude-sonnet-4-6  claude-sonnet-4-5  claude-haiku-4-5
```

New models get slots in preference order (`claude-sonnet-5` first — Claude's
Auto mode safety checks go there). Slot assignments are **stored**, not
positional, so adding/removing models never reshuffles existing mappings.

## NUE gateway

The separate NUE listener is an authentication wrapper between a NEXUS client
and an AI-2 host. Client `nue_*` keys are a separate credential chain; the
upstream AI-2 API key stays in NexusLink and is never exposed to the client.

**Request flow:** register a client key once, send it as a Bearer token on
`/v1/*`, and NexusLink replaces it with the configured upstream Bearer key.
Responses are streamed without buffering, including SSE, and query strings are
preserved.

| Config field | Purpose |
|---|---|
| `nue_port` | NUE listener port (default `5680`); binds only to `127.0.0.1`. If it equals `port`, the NUE gateway is disabled with a warning. |
| `nue_upstream.base_url` | AI-2 base URL, e.g. `https://ai2.example:4433`. |
| `nue_upstream.api_key` | Scoped AI-2 key used only by the gateway for upstream requests. |
| `nue_register_token` | Optional registration guard. If non-empty, registration requires `X-Register-Token`; otherwise registration is open. |

Registered client keys are stored in `~/.nexuslink/nue_keys.json`, atomically
written and restricted to mode `0600` on Unix. They survive restarts.

```bash
NUE_BASE=http://127.0.0.1:5680

# Open registration (nue_register_token is empty):
curl -sS -X POST "$NUE_BASE/nue/register"

# If a registration guard is configured, use this instead:
curl -sS -X POST "$NUE_BASE/nue/register" \
  -H "X-Register-Token: $NUE_REGISTER_TOKEN"
# {"key":"nue_<hex>"}

# Save the returned value as NUE_KEY, then use a request body accepted by AI-2.
curl -i -X POST "$NUE_BASE/v1/runs" \
  -H "Authorization: Bearer $NUE_KEY" \
  -H "Content-Type: application/json" \
  --data-binary @run.json

# SSE is passed through as received:
curl -N "$NUE_BASE/v1/runs/$RUN_ID/events" \
  -H "Authorization: Bearer $NUE_KEY"
```

The gateway returns `401` for a missing/invalid client key (or invalid
registration token), `404` for denied paths (`/v1/keys*`, `/_bus*`,
`/v1/startup*`), `503` when the upstream URL/key is not configured, and `502`
when the upstream cannot be reached. The deny-list is rejected before any
upstream request. CORS permits any origin, the `GET`, `POST`, `PUT`, `DELETE`,
`PATCH`, and `OPTIONS` methods, and the `Authorization` and `Content-Type`
headers. The current CORS allowlist does not include `X-Register-Token`, so a
guarded cross-origin browser registration needs a CORS change or a
same-origin/server-side registration flow.

The main Anthropic gateway on `127.0.0.1:5679` is unchanged; NUE is a second
listener, not a route on that port. Keep the NUE listener loopback-only. For
remote clients, put a TLS reverse proxy in front of it; use HTTPS for
`nue_upstream.base_url` whenever the AI-2 host is reached over a network.

## Management panel

`nexuslink ui` opens `http://127.0.0.1:<port>/#t=<token>` — the token rides in
the fragment so it never reaches logs, history or the wire. Without the
fragment the panel shows a token gate; `/api/status` stays open (it exposes
only masked blacklist tails, counts and uptime) while `/api/config` and
`/api/apply` require the token.

The panel (English/Русский) edits the same `config.json` — PUT persists
atomically and hot-swaps the running proxy, no restart needed.

## Claude Desktop setup

1. `nexuslink apply` writes a configLibrary file and prints its path.
   It sets: `inferenceProvider: "gateway"`,
   `inferenceGatewayBaseUrl: "http://127.0.0.1:<port>"`,
   `inferenceGatewayApiKey: "<incoming_token>"`, `deploymentMode: "3p"`,
   `inferenceModels: [your slots]`. It also registers the `nexuslink` MCP
   server (`generate_video` / `video_status`) in `claude_desktop_config.json`
   **and** `~/.claude.json` (Claude Code user scope — that's where embedded
   sessions actually read MCP), and installs the `nexuslink-video` skill into
   `~/.claude/skills/`.
2. Restart Claude Desktop completely (tray → quit, not just close window).
3. Pick a model — the selector shows the real model name via `labelOverride`.

Manual route (no `apply`): in Claude Desktop's third-party/gateway settings,
set base URL `http://127.0.0.1:5679`, API key = your `incoming_token`, and add
the slot names as models.

## Smoke test

```bash
./scripts/smoke.sh                 # auth, models, stream, unmapped-400, status
```

With a fake upstream (any OpenAI mock), point a provider's `base_url` at it
and list a dead key first in `api_keys` — the smoke shows rotation to the
live key and the parked entry in `/api/status`.

## Security notes

- **Bind is loopback-only** — that stops remote access but *not* local
  processes; the bearer token is what authenticates requests.
- Keys are stored plaintext in `~/.nexuslink/config.json` — same threat
  model as ModelLink; chmod 600 on Unix. Don't commit it.
- The upstream provider API key is sent as `Authorization`; the incoming
  bearer token is **never** forwarded upstream.
- Provider URLs/keys are not logged; blacklist entries carry only the last
  6 chars of a key. Request bodies are not logged.

## Layout

```
src/config.rs      config file, slot pool, candidate resolution, atomic save
src/rotation.rs    TTL blacklist, error classification, candidate ordering
src/translator.rs  typed Anthropic<->OpenAI structs + request/response mapping
src/streaming.rs   stateful OpenAI-chunk -> Anthropic-SSE converter
src/proxy.rs       axum router, bearer auth, rotation loop, SSE bridge,
                   sliding-window auto-compaction at context limits
src/ui.rs          management API (/api/status|config|apply) + embedded SPA
src/gateway.rs     Claude-3p configLibrary + mcpServers + skill installer
src/desktop.rs     frameless WebView2 shell, injected titlebar, tray, DWM corners
src/mcp.rs         stdio MCP server — generate_video / video_status (Agnes)
src/metrics.rs     per provider/model/key stats (attempts, EMA latency)
src/main.rs        CLI: serve / app / ui / init / apply / slots / status / mcp
ui/index.html      Nexus Lux panel — EN/RU, providers, key pools, blacklist,
                   metrics; debug builds re-read it from disk per request
skills/            Claude Code skill installed by `apply` (nexuslink-video)
wix/main.wxs       MSI definition (cargo wix)
packaging/macos/   Info.plist + bundle.sh (.app + .dmg via system tools)
tests/             translation contract + rotation integration tests
scripts/smoke.sh   end-to-end curl checks
```

## Releasing

CI builds everything on `v*` tags: `cargo wix` MSI + zipped exe on
`windows-latest`, a lipo'd universal binary → `NexusLink.app` → `.dmg` on
`macos-latest`, then publishes a GitHub release. Local checks before tagging:
`cargo check`, `cargo test`, `cargo build --release`.

## Known limits

- `thinking` / `output_config.effort` blocks are accepted and dropped unless
  the provider opts into `reasoning_effort` — there's no OpenAI equivalent;
  silently rewriting thinking budgets would be a lie.
- Vision (`image` blocks) and `cache_control` aren't translated yet — the
  blocks parse fine, they're just not emitted upstream.
- Once a stream has started emitting bytes, a mid-flight upstream failure
  surfaces as a truncated SSE — rotation never silently swaps providers
  mid-response.
