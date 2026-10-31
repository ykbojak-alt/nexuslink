#!/usr/bin/env bash
# NexusLink smoke test — simulates Claude Desktop hitting the local proxy.
# Usage: ./scripts/smoke.sh [port] [token]
#   Defaults: port 5679, token read from ~/.nexuslink/config.json
set -euo pipefail

PORT="${1:-5679}"
TOKEN="${2:-$(python -c "import json,os;print(json.load(open(os.path.expanduser('~/.nexuslink/config.json')))['incoming_token'])" 2>/dev/null || echo nl-testtoken-1)}"
BASE="http://127.0.0.1:${PORT}"
AUTH="Authorization: Bearer ${TOKEN}"

pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; exit 1; }

echo "== auth: no token must be rejected =="
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/v1/messages" \
  -H "Content-Type: application/json" -d '{}')
[ "$code" = "401" ] && pass "no-token -> 401" || fail "no-token -> $code (want 401)"

echo "== auth: wrong token must be rejected =="
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/v1/messages" \
  -H "Authorization: Bearer definitely-wrong" \
  -H "Content-Type: application/json" -d '{}')
[ "$code" = "401" ] && pass "wrong-token -> 401" || fail "wrong-token -> $code"

echo "== /health =="
curl -sf "$BASE/health" >/dev/null && pass "health" || fail "health endpoint"

echo "== panel shell is served =="
curl -sf "$BASE/" | grep -qi "NexusLink" && pass "SPA at /" || fail "panel shell"

echo "== /api/status is open and reports rotation state =="
curl -sf "$BASE/api/status" | python -c \
  "import json,sys; s=json.load(sys.stdin); assert s['ok'] and 'blacklist' in s; print('PASS  status ok,', s['slots'], 'slots,', len(s['blacklist']), 'parked')" \
  || fail "status endpoint"

echo "== /api/config requires the token =="
code=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/api/config")
[ "$code" = "401" ] && pass "config unauth -> 401" || fail "config unauth -> $code"

echo "== /v1/models lists the configured slots =="
curl -sf -H "$AUTH" "$BASE/v1/models" | python -m json.tool || fail "models endpoint"

echo "== non-streaming Anthropic request -> translated -> Anthropic reply =="
echo "   (a dead first key in api_keys should rotate to a live one)"
curl -sf -X POST "$BASE/v1/messages" -H "$AUTH" -H "Content-Type: application/json" -d '{
  "model": "claude-sonnet-5",
  "max_tokens": 256,
  "system": "You are a concise assistant.",
  "messages": [{"role": "user", "content": "Reply with the single word: pong"}]
}' | python -m json.tool || fail "non-streaming request"

echo "== streaming request (SSE) =="
curl -sf -N -X POST "$BASE/v1/messages" -H "$AUTH" -H "Content-Type: application/json" -d '{
  "model": "claude-sonnet-5",
  "max_tokens": 256,
  "stream": true,
  "messages": [{"role": "user", "content": "Count to three."}]
}' || fail "streaming request"

echo
echo "== unmapped slot must 400, not silently substitute =="
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/v1/messages" \
  -H "$AUTH" -H "Content-Type: application/json" \
  -d '{"model":"claude-opus-4-8","max_tokens":8,"messages":[{"role":"user","content":"x"}]}')
[ "$code" = "400" ] && pass "unmapped -> 400" || fail "unmapped -> $code (want 400)"

echo
echo "All smoke checks done."
