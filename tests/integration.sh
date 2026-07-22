#!/usr/bin/env bash
# Live, quota-consuming verification against the installed OpenCode.
# The harness is deliberately isolated: it never reads or writes the user's
# oc-route profiles, and it deletes every session it creates.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/oc-route"
TEST_TMP="$(mktemp -d)"
CONFIG_DIR="$TEST_TMP/config"
SERVE_LOG="$TEST_TMP/opencode.log"
PROXY_LOG="$TEST_TMP/oc-route.log"
PROFILE="live-integration"
SERVE_PID=""
PROXY_PID=""
SIDECAR_PID=""
USER_SESSION=""
TEST_PASSED=0

free_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

SERVE_PORT="${OC_SERVE_PORT:-$(free_port)}"
PROXY_PORT="${OC_PROXY_PORT:-$(free_port)}"

cleanup() {
  local status=$?
  set +e
  if [ -n "$USER_SESSION" ]; then
    curl -sf -X DELETE "http://127.0.0.1:$SERVE_PORT/session/$USER_SESSION" >/dev/null
  fi
  if [ -n "$PROXY_PID" ]; then
    kill -INT "$PROXY_PID" 2>/dev/null
    for _ in $(seq 1 20); do
      kill -0 "$PROXY_PID" 2>/dev/null || break
      sleep 0.1
    done
    kill "$PROXY_PID" 2>/dev/null
    wait "$PROXY_PID" 2>/dev/null
  fi
  if [ -n "$SERVE_PID" ]; then
    kill "$SERVE_PID" 2>/dev/null
    wait "$SERVE_PID" 2>/dev/null
  fi
  if [ "$status" -eq 0 ] && [ "$TEST_PASSED" -eq 1 ]; then
    case "$TEST_TMP" in
      /tmp/*) rm -rf -- "$TEST_TMP" ;;
    esac
  else
    printf '\nLive-test logs preserved at %s\n' "$TEST_TMP" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

wait_for_url() {
  local url=$1
  local attempts=${2:-100}
  for _ in $(seq 1 "$attempts"); do
    if curl -sf --max-time 1 "$url" >/dev/null; then
      return 0
    fi
    sleep 0.2
  done
  return 1
}

command -v opencode >/dev/null || { echo "opencode is required" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 1; }

printf 'Building release binary...\n'
cargo build --locked --release --manifest-path "$ROOT/Cargo.toml"

printf 'Starting OpenCode %s on port %s...\n' "$(opencode --version)" "$SERVE_PORT"
opencode serve --hostname 127.0.0.1 --port "$SERVE_PORT" >"$SERVE_LOG" 2>&1 &
SERVE_PID=$!
wait_for_url "http://127.0.0.1:$SERVE_PORT/global/health" || {
  echo "OpenCode failed to start" >&2
  sed -n '1,160p' "$SERVE_LOG" >&2
  exit 1
}
CONFIG_HASH_BEFORE="$(curl -sf "http://127.0.0.1:$SERVE_PORT/config" | sha256sum | cut -d' ' -f1)"
PROVIDERS_HASH_BEFORE="$(curl -sf "http://127.0.0.1:$SERVE_PORT/config/providers" | sha256sum | cut -d' ' -f1)"

MODEL="$(curl -sf "http://127.0.0.1:$SERVE_PORT/config/providers" | python3 -c '
import json, sys
data = json.load(sys.stdin)
available = []
for provider in data.get("providers", []):
    for model in provider.get("models", {}).values():
        available.append("{}/{}".format(model.get("providerID", provider.get("id")), model["id"]))
preferred = [
    "opencode/north-mini-code-free",
    "opencode/mimo-v2.5-free",
    "opencode/laguna-s-2.1-free",
    "opencode/deepseek-v4-flash-free",
    "opencode/nemotron-3-ultra-free",
]
print(next((model for model in preferred if model in available), ""))
')"
[ -n "$MODEL" ] || {
  echo "No supported connected free model is available for the live test" >&2
  exit 1
}
printf 'Using connected free model: %s\n' "$MODEL"

mkdir -p "$CONFIG_DIR"
python3 - "$CONFIG_DIR/profiles.toml" "$PROFILE" "$MODEL" <<'PY'
import sys
path, profile, model = sys.argv[1:]
with open(path, "w", encoding="utf-8") as f:
    f.write(f'''[[profile]]
name = "{profile}"
router_model = "{model}"
sliding_window = 5
router_timeout_secs = 90
model_pool = ["{model}"]
routing_prompt = "Choose the only available model."
''')
PY

printf 'Starting isolated proxy on port %s...\n' "$PROXY_PORT"
STARTED_MS="$(date +%s%3N)"
OC_ROUTE_CONFIG_DIR="$CONFIG_DIR" RUST_LOG="oc_route=info" "$BIN" proxy \
  --upstream "http://127.0.0.1:$SERVE_PORT" \
  --profile "$PROFILE" \
  --directory "$ROOT" \
  --bind "127.0.0.1:$PROXY_PORT" >"$PROXY_LOG" 2>&1 &
PROXY_PID=$!
wait_for_url "http://127.0.0.1:$PROXY_PORT/global/health" || {
  echo "oc-route proxy failed to start" >&2
  sed -n '1,200p' "$PROXY_LOG" >&2
  exit 1
}
READY_MS="$(date +%s%3N)"
STARTUP_MS=$((READY_MS - STARTED_MS))
SIDECAR_PID="$(ps --ppid "$PROXY_PID" -o pid= | awk 'NF { print $1; exit }')"
[ -n "$SIDECAR_PID" ] || {
  echo "Could not identify the private OpenCode sidecar" >&2
  exit 1
}
PROXY_RSS_KIB="$(ps -o rss= -p "$PROXY_PID" | awk '{print $1}')"
SIDECAR_RSS_KIB="$(ps -o rss= -p "$SIDECAR_PID" | awk '{print $1}')"
SIDECAR_DB="$(tr '\0' '\n' <"/proc/$SIDECAR_PID/environ" | sed -n 's/^OPENCODE_DB=//p')"
[ -n "$SIDECAR_DB" ] && [ "${SIDECAR_DB#/}" != "$SIDECAR_DB" ] || {
  echo "Private sidecar does not have an isolated absolute database path" >&2
  exit 1
}
SIDECAR_DB_DIR="$(dirname "$SIDECAR_DB")"
printf 'Private router ready in %s ms; proxy RSS %s KiB, sidecar RSS %s KiB.\n' \
  "$STARTUP_MS" "$PROXY_RSS_KIB" "$SIDECAR_RSS_KIB"

USER_SESSION="$(curl -sf -X POST "http://127.0.0.1:$PROXY_PORT/session" \
  -H 'content-type: application/json' \
  -d '{"title":"oc-route live integration"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
printf 'Created disposable user session: %s\n' "$USER_SESSION"

curl -sf -X POST "http://127.0.0.1:$PROXY_PORT/session/$USER_SESSION/prompt_async?source=integration" \
  -H 'content-type: application/json' \
  -d '{"parts":[{"type":"text","text":"Reply with exactly the word ready."}]}' >/dev/null

RESULT=""
for _ in $(seq 1 600); do
  RESULT="$(curl -sf "http://127.0.0.1:$PROXY_PORT/session/$USER_SESSION/message" | python3 -c '
import json, sys
messages = json.load(sys.stdin)
users = [m for m in messages if m.get("info", {}).get("role") == "user"]
assistants = [m for m in messages if m.get("info", {}).get("role") == "assistant"]
if not users or not assistants:
    print("waiting")
    raise SystemExit
user = users[-1]
assistant = assistants[-1]
model = user.get("info", {}).get("model", {})
chosen = "{}/{}".format(model.get("providerID", ""), model.get("modelID", ""))
error = assistant.get("info", {}).get("error")
text = "".join(p.get("text", "") for p in assistant.get("parts", []) if p.get("type") == "text").strip()
if error:
    print("error:" + json.dumps(error, separators=(",", ":")))
elif text:
    print("done:" + chosen + ":" + text.replace("\n", " "))
else:
    print("waiting")
')"
  case "$RESULT" in
    done:*|error:*) break ;;
  esac
  sleep 0.2
done

case "$RESULT" in
  done:"$MODEL":*) printf 'Working response completed through injected model %s.\n' "$MODEL" ;;
  error:*) echo "OpenCode assistant failed: ${RESULT#error:}" >&2; exit 1 ;;
  *) echo "Timed out waiting for an assistant response (last state: $RESULT)" >&2; exit 1 ;;
esac

for _ in $(seq 1 50); do
  ROUTER_COUNT="$(curl -sf "http://127.0.0.1:$PROXY_PORT/session" | python3 -c '
import json,sys
sessions=json.load(sys.stdin)
print(sum(1 for session in sessions if session.get("title") == "oc-route-router" or session.get("metadata", {}).get("oc-route.internal") is True))
')"
  [ "$ROUTER_COUNT" -eq 0 ] && break
  sleep 0.1
done
[ "$ROUTER_COUNT" -eq 0 ] || {
  echo "$ROUTER_COUNT internal router session(s) leaked" >&2
  exit 1
}

HISTORY_STATUS="$(curl -sS -o /dev/null -w '%{http_code}' \
  "http://127.0.0.1:$PROXY_PORT/session/$USER_SESSION/message?limit=2")"
[ "$HISTORY_STATUS" = "200" ] || {
  echo "History passthrough returned HTTP $HISTORY_STATUS" >&2
  exit 1
}

CONFIG_HASH_AFTER="$(curl -sf "http://127.0.0.1:$SERVE_PORT/config" | sha256sum | cut -d' ' -f1)"
PROVIDERS_HASH_AFTER="$(curl -sf "http://127.0.0.1:$SERVE_PORT/config/providers" | sha256sum | cut -d' ' -f1)"
[ "$CONFIG_HASH_AFTER" = "$CONFIG_HASH_BEFORE" ] || {
  echo "The private router changed upstream OpenCode configuration" >&2
  exit 1
}
[ "$PROVIDERS_HASH_AFTER" = "$PROVIDERS_HASH_BEFORE" ] || {
  echo "The private router changed upstream provider state" >&2
  exit 1
}

kill -INT "$PROXY_PID"
for _ in $(seq 1 50); do
  kill -0 "$PROXY_PID" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$PROXY_PID" 2>/dev/null; then
  echo "Proxy did not stop promptly after SIGINT" >&2
  exit 1
fi
wait "$PROXY_PID" 2>/dev/null || true
PROXY_PID=""
sleep 0.2
if kill -0 "$SIDECAR_PID" 2>/dev/null; then
  echo "Private OpenCode sidecar outlived the proxy" >&2
  exit 1
fi
[ ! -e "$SIDECAR_DB_DIR" ] || {
  echo "Private sidecar database directory was not removed" >&2
  exit 1
}
curl -sf "http://127.0.0.1:$SERVE_PORT/global/health" >/dev/null || {
  echo "Stopping the proxy disturbed the upstream server" >&2
  exit 1
}

TEST_PASSED=1
printf 'Live integration passed: routing, cleanup, state isolation, and sidecar ownership.\n'
