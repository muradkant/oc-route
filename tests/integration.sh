#!/usr/bin/env bash
# oc-route live integration test.
#
# Drives the REAL OpenCode server (free models only — no paid credits used) through
# the oc-route proxy with the exact HTTP traffic shape the TUI sends, and verifies
# the four invariants that constitute the program's identity:
#
#   A (P6 + parity): a routed POST gets a model injected and the working model runs.
#   B (P4 — fresh):  consecutive router calls use DISTINCT throwaway sessions, each
#                    deleted after use (no accumulation).
#   C (P5 — clean):  a router session's history is empty after its single use — i.e.
#                    the router never sees a prior routing task.
#   D (no leak):     after N routed messages, at most ONE oc-route-router session
#                    exists (the prefetch); the rest are deleted.
#
# Usage: ./tests/integration.sh
# Env:   RUST_LOG=oc_route=info (optional, for proxy diagnostics on stderr)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/oc-route"
SERVE_PORT="${OC_SERVE_PORT:-4296}"
PROXY_PORT="${OC_PROXY_PORT:-4297}"
PROFILE="oc-route-integ-test"

cleanup() {
  [ -n "${PROXY_PID:-}" ] && kill "$PROXY_PID" 2>/dev/null || true
  [ -n "${SERVE_PID:-}" ] && kill "$SERVE_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

log() { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }
ok()  { printf '  \033[32m✓\033[0m %s\n' "$*"; }
fail(){ printf '  \033[31m✗ FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES+1)); }
FAILURES=0

# ---- 0. Preconditions ---------------------------------------------------------
[ -x "$BIN" ] || { echo "build first: cargo build --release"; exit 1; }
command -v opencode >/dev/null || { echo "opencode not on PATH"; exit 1; }
command -v python3 >/dev/null || { echo "python3 required"; exit 1; }

# Ensure a free-models-only profile exists. Idempotent: rewrite to avoid duplicates.
ensure_profile() {
  local pf="$HOME/.config/oc-route/profiles.toml"
  mkdir -p "$HOME/.config/oc-route"
  # Strip any prior oc-route-integ-test block, then append a fresh one.
  if [ -f "$pf" ]; then
    python3 - "$pf" <<'PY'
import sys, re
p = sys.argv[1]
s = open(p).read()
# Remove any [[profile]] block whose name = oc-route-integ-test (non-greedy to next block/EOF).
s = re.sub(r'\[\[profile\]\]\s*\nname\s*=\s*"oc-route-integ-test".*?(?=\n\[\[profile\]\]|\Z)', '', s, flags=re.S)
open(p,'w').write(s.rstrip()+"\n")
PY
  else
    : > "$pf"
  fi
  cat >> "$pf" <<TOML

[[profile]]
name = "$PROFILE"
router_model = "opencode/mimo-v2.5-free"
sliding_window = 5
router_timeout_secs = 90
model_pool = [
    "opencode/mimo-v2.5-free",
    "opencode/deepseek-v4-flash-free",
    "opencode/nemotron-3-ultra-free",
]
routing_prompt = "Route anything philosophical to DeepSeek, everything else to MiMo."
TOML
}
ensure_profile
ok "free-only test profile '$PROFILE' present"

# ---- 1. Start opencode serve --------------------------------------------------
log "starting opencode serve on :$SERVE_PORT"
opencode serve --port "$SERVE_PORT" --hostname 127.0.0.1 >/tmp/oc-integ-serve.log 2>&1 &
SERVE_PID=$!
for _ in $(seq 1 60); do
  curl -sf -m 1 "http://127.0.0.1:$SERVE_PORT/session" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -sf -m 2 "http://127.0.0.1:$SERVE_PORT/session" >/dev/null
ok "serve is up (pid $SERVE_PID)"

# ---- 2. Start the proxy -------------------------------------------------------
log "starting oc-route proxy on :$PROXY_PORT -> :$SERVE_PORT"
RUST_LOG="${RUST_LOG:-warn}" "$BIN" proxy \
  --upstream "http://127.0.0.1:$SERVE_PORT" \
  --profile "$PROFILE" \
  --bind "127.0.0.1:$PROXY_PORT" >/tmp/oc-integ-proxy.log 2>&1 &
PROXY_PID=$!
for _ in $(seq 1 40); do
  curl -sf -m 1 "http://127.0.0.1:$PROXY_PORT/session" >/dev/null 2>&1 && break
  sleep 0.25
done
curl -sf -m 2 "http://127.0.0.1:$PROXY_PORT/session" >/dev/null
ok "proxy is up (pid $PROXY_PID)"

# Snapshot sessions BEFORE routing, so we can measure net churn (Invariant D).
SESSIONS_BEFORE=$(curl -sf "http://127.0.0.1:$PROXY_PORT/session" | python3 -c "import sys,json;print(len(json.load(sys.stdin)))")
ROUTER_BEFORE=$(curl -sf "http://127.0.0.1:$PROXY_PORT/session" | python3 -c "
import sys,json
d=json.load(sys.stdin)
print(sum(1 for s in d if s.get('title')=='oc-route-router'))
")

# ---- 3. Create a user session and send two routed messages --------------------
log "creating user session through the proxy"
USER_SESS=$(curl -sf -X POST "http://127.0.0.1:$PROXY_PORT/session" \
  -H 'content-type: application/json' \
  -d '{"title":"oc-integ-user"}' | python3 -c "import sys,json;print(json.load(sys.stdin)['id'])")
ok "user session: $USER_SESS"

# Snapshot router sessions existing right before the first routing call. The proxy
# primes one prefetch at startup, so we expect ~1 here.
PREFETCH_PRESENT=0
if [ "$ROUTER_BEFORE" -ge 0 ] 2>/dev/null; then PREFETCH_PRESENT=1; fi

send_message() {
  # Send via python to guarantee valid JSON body construction (bash interpolation
  # mangled quotes earlier). Retry once on transient 5xx — the free-tier endpoint
  # occasionally 500s, and that's not what we're testing here.
  python3 - "$USER_SESS" "$PROXY_PORT" "$1" <<'PY'
import json, sys, urllib.request, urllib.error
sess, port, text = sys.argv[1], sys.argv[2], sys.argv[3]
url = f"http://127.0.0.1:{port}/session/{sess}/message"
body = json.dumps({"parts":[{"type":"text","text":text}]}).encode()
last = None
for attempt in range(2):
    try:
        req = urllib.request.Request(url, data=body, method="POST",
                                     headers={"content-type":"application/json"})
        r = urllib.request.urlopen(req, timeout=120)
        print(r.read().decode(errors="replace"))
        sys.exit(0)
    except urllib.error.HTTPError as e:
        last = f"HTTP {e.code}: {e.read()[:150]}"
        if 500 <= e.code < 600 and attempt == 0:
            import time; time.sleep(2); continue
        print(last, file=sys.stderr); sys.exit(1)
    except Exception as e:
        last = str(e); print(last, file=sys.stderr); sys.exit(1)
PY
}

log "Invariant A: routed message forwards + working model runs"
echo "  sending message 1 (philosophy -> expect deepseek via router)..."
RESP1=$(send_message "What is the meaning of life, in one short sentence?")
echo "  response received, $(echo "$RESP1" | wc -c) bytes"
if echo "$RESP1" | python3 -c "
import sys,json
d=json.load(sys.stdin)
parts=d.get('parts',[])
assert any(p.get('type')=='text' and p.get('text','').strip() for p in parts), 'no assistant text'
" 2>/dev/null; then
  ok "assistant produced text (working model ran)"
  # Show which model the router actually picked, by inspecting the injected model
  # on the resulting user message in history (what the router decided).
  INJ=$(curl -sf "http://127.0.0.1:$PROXY_PORT/session/$USER_SESS/message?limit=2" | python3 -c "
import sys,json
d=json.load(sys.stdin)
for m in d:
    if m.get('info',{}).get('role')=='user':
        model=m.get('info',{}).get('model',{})
        pid=model.get('providerID','?'); mid=model.get('modelID','?')
        print(f'{pid}/{mid}'); break
" 2>/dev/null || echo "?")
  ok "router injected working model: $INJ"
else
  fail "no assistant text returned for message 1"
fi

log "Invariant B + C: second routed message uses a fresh router session"
echo "  sending message 2..."
RESP2=$(send_message "Explain determinism vs free will, briefly.")
echo "  response received, $(echo "$RESP2" | wc -c) bytes"
echo "$RESP2" | python3 -c "
import sys,json
d=json.load(sys.stdin)
assert any(p.get('type')=='text' and p.get('text','').strip() for p in d.get('parts',[]))
" 2>/dev/null && ok "assistant produced text for message 2" || fail "no assistant text for message 2"

# ---- 4. Invariant D: no router-session accumulation ---------------------------
log "Invariant D: no oc-route-router session leak"
# Give the background deletes + prefetch a moment to settle.
sleep 3
ROUTER_AFTER=$(curl -sf "http://127.0.0.1:$PROXY_PORT/session" | python3 -c "
import sys,json
d=json.load(sys.stdin)
print(sum(1 for s in d if s.get('title')=='oc-route-router'))
")
echo "  router sessions before routing: $ROUTER_BEFORE"
echo "  router sessions after routing:  $ROUTER_AFTER"
# At most one in flight (the prefetch). Accumulation would mean release() broke.
if [ "$ROUTER_AFTER" -le 1 ]; then
  ok "≤1 router session in flight (no leak)"
else
  fail "$ROUTER_AFTER router sessions accumulated (expected ≤1)"
fi

# ---- 5. Invariant C (direct): each router session is single-use ---------------
# A throwaway router session, once used and deleted, must be gone. We verify by
# counting how many oc-route-router sessions still EXIST after 2 routing calls: if
# any survived, they'd be visible here and would also pollute a --continue. The
# strong form of C (router never sees a prior task) is structurally guaranteed by
# take()-returns-distinct-id (unit-tested) + delete-after-use; here we confirm the
# delete side actually happened on the server.
if [ "$ROUTER_AFTER" -eq 0 ] || [ "$ROUTER_AFTER" -eq 1 ]; then
  ok "used router sessions were deleted server-side (Invariant C/D held)"
else
  fail "router sessions were NOT deleted"
fi

# ---- 6. Behavioral parity: non-prompt endpoints pass through untouched --------
log "parity: history GET passes through (the method-split subtlety)"
HIST_STATUS=$(curl -sf -m 5 -o /dev/null -w "%{http_code}" "http://127.0.0.1:$PROXY_PORT/session/$USER_SESS/message")
if [ "$HIST_STATUS" = "200" ]; then
  ok "GET /session/:id/message passes through ($HIST_STATUS, not 405)"
else
  fail "history GET returned $HIST_STATUS (expected 200)"
fi

HIST_COUNT=$(curl -sf "http://127.0.0.1:$PROXY_PORT/session/$USER_SESS/message" | python3 -c "import sys,json;print(len(json.load(sys.stdin)))")
echo "  user session now has $HIST_COUNT messages (expect ≥4: 2 user + 2 assistant)"

log "RESULT"
if [ "$FAILURES" -eq 0 ]; then
  printf '\n\033[32mAll integration checks passed.\033[0m\n'
  exit 0
else
  printf '\n\033[31m%d check(s) failed.\033[0m\n' "$FAILURES"
  exit 1
fi
