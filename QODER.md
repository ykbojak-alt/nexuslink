# NexusLink — Qoder Handoff

You are continuing work on **NexusLink**, a local AI gateway + desktop app for Windows.
The project lives at `D:\openbridge`. Rust (edition 2021), single self-contained exe.

## What it is

A Rust/Axum HTTP gateway that translates Anthropic `/v1/messages` (incl. SSE streaming)
into OpenAI-compatible requests for pooled upstream providers — Claude Desktop talks to
`127.0.0.1:5679` as if it were Anthropic. The same exe is also:

- `nexuslink app` — desktop app: server on a background tokio thread + frameless WebView2
  window + system tray (close button hides to tray; tray menu: Открыть / Выход).
- `nexuslink mcp` — stdio MCP server exposing `generate_video` / `video_status`
  (Agnes async video-job API: `POST /v1/videos` → poll `GET /agnesapi`).
- `nexuslink serve|init|apply|slots|status` — CLI. `apply` writes gateway config to
  Claude's `Claude-3p/configLibrary` AND `mcpServers` entries into
  `claude_desktop_config.json` satellites **and `~/.claude.json`** (the Store-packaged
  desktop embeds Claude Code, which reads MCP from `~/.claude.json` user scope —
  not from `claude_desktop_config.json`).

## Source map

| File | Role |
|---|---|
| `src/main.rs` | CLI dispatch, AttachConsole pipe-detection, `windows_subsystem` |
| `src/gateway.rs` | `apply` — writes Claude configs (3p + satellites + `~/.claude.json`) |
| `src/proxy.rs` | AppState, rotation, fallback, auto-compaction, `state_from_config` |
| `src/desktop.rs` | frameless tao/wry shell, DWM rounded corners, injected titlebar JS, tray, single-instance mutex, `~/.nexuslink/window.json` geometry sidecar |
| `src/mcp.rs` | stdio MCP server (separate thread + own tokio runtime — do NOT `block_on` inside the main runtime) |
| `src/translator.rs` | Anthropic ↔ OpenAI translation, SSE |
| `src/config.rs` | config model (`~/.nexuslink/config.json`), `ModelEntry.context`, `Provider.auto_compact` |
| `src/metrics.rs`, `src/rotation.rs` | per-scope stats, key parking |
| `ui/index.html` | management panel — embedded via `include_str!` in release, re-read from disk per request in debug |
| `assets/panel-bg.webp` | Nexus Lux backdrop for the panel, served at `/ui/bg.webp` (same debug-disk / release-embed split) |

## Build & run

```powershell
cargo check            # fast typecheck
cargo test             # 31 tests, keep green
cargo build --release  # LTO fat + strip — takes ~4-5 min
```

Release profile: `lto = "fat"`, `strip`, icon embedded via `embed-resource` (`assets/`).
Result: `target/release/nexuslink.exe` ~6.7 MB, zero external deps (rustls, embedded SPA).

### UI dev-loop (no rebuild per tweak)

```powershell
cargo run -- serve            # debug: reads ui/index.html + assets/panel-bg.webp from disk
```

In a debug build `src/ui.rs` resolves the panel and the backdrop against
`env!("CARGO_MANIFEST_DIR")` and re-reads them per request (`no-cache`), so an edit to
`ui/index.html` shows up on reload; release keeps the compiled-in copy. Point
`USERPROFILE` at a scratch dir to keep the real `~/.nexuslink/config.json` out of it, and
use a spare port so the live app on `5679` is untouched.
A running debug `nexuslink.exe` locks `target\debug\` — stop it before `cargo test`.

### Deploy while the app runs (Windows locks the exe)

```powershell
mv target\release\nexuslink.exe target\release\nexuslink_prev.exe   # rename frees the name
cargo build --release
# stable path — MCP registrations and the desktop shortcut point HERE, never
# at target\release (that path briefly vanishes during rebuilds and breaks
# Claude's `nexuslink mcp` spawns):
Copy-Item target\release\nexuslink.exe D:\openbridge\nexuslink.exe -Force
# restart ONLY the app process — never `Stop-Process -Name nexuslink`: that also kills
# every `nexuslink mcp` child Claude Desktop owns (and any other agent's instance).
Stop-Process -Id (Get-NetTCPConnection -LocalPort 5679 -State Listen).OwningProcess
Start-Process D:\openbridge\nexuslink.exe -ArgumentList app
```

`D:\openbridge\nexuslink.exe` is the canonical install path — `apply`, the
desktop shortcut and all MCP registrations (`~/.claude.json`, satellites) point
to it. When it is locked by a running instance, rename it first, then copy.

Verify: `curl http://127.0.0.1:5679/health` → `ok`, `Get-Process nexuslink` shows
`MainWindowTitle = NexusLink`.

## Conventions — do not regress

- **Secrets**: never print/log API keys or `incoming_token`. Panel masks them.
- **Ports**: default `5679` everywhere (config, UI fallbacks, docs). Claude is configured for it.
- **UI style**: Nexus Lux — obsidian/graphite + gold (`--gold-500: #C9A45C`), utility CSS
  classes, **no inline `style=` attributes** (lint-clean). All user-facing strings go through
  the `I18N` table (en + ru). Scrollbars: `::-webkit-scrollbar` only (Firefox keeps native).
- **Rotation**: 4xx model-specific errors (`404 model`, context-overflow sniff, `400`) →
  rotate to next candidate without parking; auth/quota → park key 15 min. Round-robin cursor
  per slot so traffic doesn't always start at key 1.
- **Compaction**: sliding-window only when `ModelEntry.context > 0` is trustworthy —
  never invent a provider's context window. Preserves system prompt + anchor + tail,
  keeps tool_call/tool_result groups atomic.
- **Desktop**: IPC from webview uses `window.ipc.postMessage` → `UserEvent::Ctl` → handled
  in the event loop. Titlebar JS injected via `with_initialization_script` (survives
  navigations/error pages), sets `--nl-tb-h` CSS var for the sticky header.
- **Errors**: match existing style — `tracing::warn!`/`error!` for operator-facing issues,
  don't unwrap in request paths.

## Known gotchas

- Release exe stays locked by the running app — always rename before rebuilding.
- `cargo add` one dep at a time; batch feature flags break the resolver CLI.
- `windows-sys` 0.59: `HWND` is `*mut c_void` — `SetWindowPos` insertAfter takes `null_mut()`.
- `windows-sys` 0.59: `CreateMutexW` lives under `Win32::System::Threading` but is
  gated by the **`Win32_Security`** feature — enabling only `Win32_System_Threading`
  leaves the import unresolved.
- wry 0.57: `WebViewBuilder::new()` takes no window; `.build(&window)` gets it.
- MCP loop must run on its own thread + runtime (`block_on` inside `#[tokio::main]` panics).
- Claude Desktop Store build spawns MCP servers from `~/.claude.json`, user scope;
  sessions load tools at chat start — new chat required after config changes.
- `nexuslink.exe` is both the app and the MCP server: `Stop-Process -Name nexuslink`
  drops Claude's MCP children with it. Target the PID that owns `:5679`.
- A live `target\debug\nexuslink.exe` makes `cargo test`/`cargo build` fail with
  `os error 5 (Отказано в доступе)` while removing the exe — stop the dev server first.
- Screenshotting the real window (`PrintWindow`, flag 2) captures the panel **with the
  operator's real keys and incoming token in plain text** — crop to the titlebar strip
  before sharing, never attach the full client area.
- The WebView2 context menu is not observable from outside: it is not a top-level window
  (`list_windows` stays empty) and not in the host UIA tree, and per-window capture skips
  popups. Don't treat "no menu appeared" as evidence — check the wry settings call instead.

## Improvement backlog (pick from here or propose better)

1. ~~**Dev-loop for UI**~~ — done: debug serves `ui/index.html` and `assets/panel-bg.webp`
   from disk (`/ui/bg.webp`), release embeds both.
2. ~~**Window state**~~ — done: geometry lives in `~/.nexuslink/window.json`
   (`WindowState` in `src/desktop.rs`, path helper in `config.rs`). Saved on
   `Resized`/`Moved` with a 400 ms debounce (`ControlFlow::WaitUntil`) and flushed
   immediately on hide/quit; a rect that no longer intersects any monitor is dropped
   so the OS places the window (unplugged-monitor case); while maximized only the
   flag is stored, because a maximized rect overshoots the monitor.
3. ~~**Single-instance**~~ — done: `Local\NexusLink-single-instance` named mutex
   (`single_instance` in `src/desktop.rs`). A second `nexuslink app` never binds
   the port or builds a window: it finds the running panel with `FindWindowW`,
   restores it via `ShowWindow(SW_RESTORE)` + `SetForegroundWindow` (works from
   the tray) and exits in ~200 ms.
4. **Video MCP**: add `mode: "reference"` support to `generate_video` (images ≤5,
   audios ≤3 per Agnes docs); maybe expose `size` tiers if the plan allows.
5. **Metrics page**: render per-key success rate/EMA latency in the panel
   (`/api/status` already returns `metrics`); optionally persist counters to disk.
6. ~~**Context menu / zoom**~~ — done: `with_hotkeys_zoom(false)` (no ctrl/pinch page zoom)
   and `hide_browser_chrome()` → `with_default_context_menus(false)` in `src/desktop.rs`.
   wry calls `SetAreDefaultContextMenusEnabled`/`SetIsZoomControlEnabled` with `?` during
   `WebView::build` (wry-0.57.1 `webview2/mod.rs:484`), so a rejected value would take the
   whole panel down — a rendering window is the proof the settings stuck.
7. **Autostart**: Settings toggle → registry `HKCU\Run` entry for `nexuslink app`.
8. **Tray polish**: show live stats in tooltip (providers/blacklist size), maybe a
   quick "Открыть панель / Health / Выход" menu item.
9. **First-run wizard**: if no `config.json`, panel offers guided setup (incoming token
   + first provider + apply to Claude) instead of an empty form.
10. **Auto-update check**: cheap version ping; button opens releases page.

When done with a task: `cargo check` → `cargo test` → rename-swap →
`cargo build --release` → restart app → verify `/health` + the feature itself.
