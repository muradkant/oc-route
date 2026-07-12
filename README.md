# oc-route

Choose a different OpenCode model for every message from rules written in plain
language.

```text
"Use Sonnet for code review. Use model X when the conversation turns
philosophical. Use model Y for casual writing."
```

`oc-route` is a Rust reverse proxy, not an OpenCode fork. It launches the real
server and TUI, intercepts only prompt submission, asks a small router model to
apply your rules, injects the chosen model, and forwards the original request.
Sessions, tools, context, compaction, files, LSP, and streaming remain OpenCode's
work.

## Install

Requirements:

- OpenCode 1.17.7+ on `PATH`, with at least one connected provider;
- stable Rust.

```sh
git clone https://github.com/muradkant/opencode-llm-alternation.git
cd opencode-llm-alternation
cargo build --release
install -Dm755 target/release/oc-route "$HOME/.local/bin/oc-route"
```

Run:

```sh
oc-route
```

Choose or create a routing profile, then continue a real session or start a new
one. You land in OpenCode's ordinary TUI. Each submitted message shows an
animated routing toast; once the response arrives, an eight-second toast names
the selected model and rationale.

## Profile

Profiles live at `~/.config/oc-route/profiles.toml`:

```toml
[[profile]]
name = "coding-personal"
router_model = "opencode/mimo-v2.5-free"
sliding_window = 10
router_timeout_secs = 90

model_pool = [
  "anthropic/claude-sonnet-4-5",
  "openai/gpt-4o",
  "opencode/nemotron-3-ultra-free",
]

routing_prompt = """
Use Claude for code. Use Nemotron for philosophy. Use GPT-4o for casual
conversation. Apply my intent when a message crosses categories.
"""
```

- `routing_prompt` is the policy. Arbitrary, subjective criteria are the point.
- `model_pool` contains the only valid destinations.
- `router_model` decides; keep it small and include it in the pool.
- `sliding_window` limits recent conversation sent to the router.
- `router_timeout_secs` bounds each router attempt.

The interactive picker reads `/config/providers`, which lists models actually
served by connected providers. It does not use the broader models.dev catalogue,
whose entries may be unavailable locally.

## The routing tax

The decision must precede forwarding because OpenCode stores the model on the
new user message; there is no retroactive switch endpoint. One router inference
therefore sits on every message's critical path.

Measured on the same free endpoint and task:

| Router | Latency | Valid decision |
| --- | ---: | --- |
| `nemotron-3-ultra-free` | 31–55 s | Yes |
| `deepseek-v4-flash-free` | 11 s | Yes |
| `mimo-v2.5-free` (default) | ~8 s | Yes |
| `north-mini-code-free` | ~2 s | Yes |

`mimo-v2.5-free` balances speed with nuanced policy; simple rules may justify
`north-mini-code-free`. Reasoning-effort variants did not change the dominant
provider latency.

The proxy reduces its own contribution by asking OpenCode for only the newest
N history messages, prefetched one-use router sessions, deleting used sessions
off the critical path, reusing HTTP connections, and retrying one failed router
call with a fresh session. It never caches a decision: every message receives a
new judgment.

## Standalone proxy

Against an existing server:

```sh
oc-route proxy \
  --upstream http://127.0.0.1:4096 \
  --profile coding-personal \
  --bind 127.0.0.1:4097
```

Attach any compatible OpenCode client to the bind address.

## Boundaries worth preserving

- Both `POST /session/:id/prompt_async` and `POST /session/:id/message` are
  intercepted. `GET /session/:id/message` is history and must pass through;
  registering a POST-only Axum route at that path would turn the GET into 405.
- History remains in OpenCode. The proxy requests `?limit=N`, converts tool and
  reasoning parts to concise XML hints, and preserves user/assistant text.
- Working models receive the full OpenCode-managed conversation across model
  changes; only provider-specific metadata is adapted by OpenCode.
- Router sessions are hidden from continuation choices. “Continue” resolves to
  the newest non-router session ID rather than trusting a transient newest
  session.
- Non-JSON prompts and routing failures are forwarded safely; model and
  rationale are validated before injection.
- The child server is health-monitored, and routing retry is bounded.

See [Architecture](ARCHITECTURE.md) for the request path, concurrency, session
ownership, protocol decisions, and verified invariants.

## Verify

```sh
cargo test --all-targets
cargo clippy --all-targets
```

The live harness starts a real OpenCode server and consumes configured provider
quota:

```sh
./tests/integration.sh
```

Set `RUST_LOG=oc_route=info` for proxy diagnostics. Logs use stderr so the
fullscreen TUI keeps stdout.

MIT licensed.
