---
name: nexuslink-video
description: Generate short videos via the local NexusLink gateway (Agnes Video 2.5 Flash). Use when asked to create/generate/render a video, make a clip, or turn a prompt/scene into mp4. Covers the nexuslink MCP tools and the direct Agnes API fallback.
---

# NexusLink Video Generation

Video goes through **NexusLink** — a local gateway on `127.0.0.1:5679` that fronts
the Agnes video API. Two paths; prefer PATH 1.

## PATH 1 — MCP tools (preferred)

If a server named `nexuslink` appears in the tool list, call:

- **`generate_video(prompt, seconds, aspect_ratio)`**
  - `prompt` — describe scene, motion, camera; rich cinematic prompts work best
  - `seconds` — string `"4"`..`"12"`, default `"5"`
  - `aspect_ratio` — `"16:9"`, `"9:16"`, `"1:1"`, `"4:3"`, `"3:4"`, `"21:9"`; default `"16:9"`
  - The tool retries through `video_queue_full` internally (~45s), then polls the
    job up to ~55s total and returns the **mp4 URL**, or a `video_id` if still
    rendering.
- **`video_status(video_id)`** → status/progress or the mp4 URL when completed.
  Poll it when `generate_video` returns a `video_id` instead of a URL.

If the tools error with `MCP server not connected` / are absent from the tool
list → tell the user a restart is needed (the MCP server is spawned per session
by the Claude Code runtime; a failed spawn is not retried inside a session).
Meanwhile continue with PATH 2 so the task isn't blocked.

## PATH 2 — direct Agnes API (fallback)

- Model: `agnes-video-2.5-flash`
- Base: `https://apihub.agnes-ai.com`
- Key: read `~/.nexuslink/config.json` → `providers[]` where `name` matches
  `Agnes` (or `base_url` contains `agnes`) → `api_keys[0]`; on `401`/`403` try
  the next key in the array. **Never print the key.**

```http
POST https://apihub.agnes-ai.com/v1/videos
Authorization: Bearer <key>
Content-Type: application/json

{"model":"agnes-video-2.5-flash","prompt":"<text>","seconds":"10","mode":"text","size":"720P","aspect_ratio":"16:9"}
```

Response → take `video_id`.

```http
GET https://apihub.agnes-ai.com/agnesapi?video_id=<id>&model_name=agnes-video-2.5-flash
Authorization: Bearer <key>
```

Poll every ~2s until `status == "completed"` → field `url` is the mp4
(`status == "failed"` → surface the `error` field). On `503 video_queue_full`
at creation, retry every ~10s for up to a minute before giving up — the queue
is global and clears.

## Flash-model constraints

- Only `size: "720P"`; `seconds` is a **string** `"4"`–`"12"`.
- `mode: "text"` only for now; reference mode accepts `images` ≤5, `audios` ≤3,
  no input videos.
- Trial tier: queue saturates often — retry politely, don't burn the quota with
  rapid-fire requests.

## Workflow tips

- For multi-shot requests: write the shot list first, then call
  `generate_video` per shot (one clip per call — there is no batch API).
- Report the mp4 URL plainly; if a `video_id` was returned, keep polling via
  `video_status` on the user's behalf instead of making them do it.
