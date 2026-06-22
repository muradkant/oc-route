# oc-route — OpenCode Model Router

A Rust proxy that sits between OpenCode's TUI and its server, intercepting each
message and routing it to the optimal LLM based on **your own natural-language
routing rules** — transparently.

You run `oc-route` instead of `opencode`. You get an interactive setup (pick a
profile, pick a session), then OpenCode's normal TUI. From then on every message
you send is silently routed to a different model based on rules *you* wrote. A
toast notification shows which model was chosen and why.

> "When I'm coding use model Y. When the conversation gets philosophical use
> model X." No heuristics, no complexity scoring — an LLM reads your rules and
> picks. The routing criteria are arbitrary and subjective, by design.

## Why this exists

RouteLLM, LiteLLM, Portkey, and friends route on objective optimization (cost,
quality, latency). None support **user fantasies** as routing criteria — "if the
conversation gets flirty, switch to Claude." oc-route is a subjective,
user-defined, natural-language LLM router for an interactive coding agent.

## How it works

OpenCode is internally client-server: `opencode` starts an HTTP server + a TUI
client that talks to it over HTTP. These are separable:

- `opencode serve --port 4096` → headless server
- `opencode attach http://localhost:4097` → TUI connects to an existing server

oc-route starts the real server, runs a Rust reverse proxy in front of it, and
launches the TUI pointed at the proxy. The proxy is a transparent HTTP middleman
that intercepts the prompt endpoints, routes, injects the chosen model, and
forwards. Everything else (SSE, session management, tool execution, LSP) passes
through untouched.

```
┌─────────────────────────────────────────────────────┐
│ oc-route wrapper                                     │
│   1. opencode serve --port 4096   (real server)      │
│   2. Rust reverse proxy on 4097  (routing lives here)│
│   3. opencode attach http://localhost:4097 (real TUI)│
└─────────────────────────────────────────────────────┘
        User types in TUI → POST /session/:id/message
                          ↓
        ┌── Proxy intercepts ──────────────────────┐
        │  • animated "Routing." → "Routing..." toast│
        │  • fetch history (GET /session/:id/message)│
        │  • strip tool calls → XML hints            │
        │  • call router model with structured XML   │
        │  • receive model choice + rationale        │
        │  • inject model into request body          │
        │  • forward modified POST to OpenCode       │
        │  • result toast: "Routed to <model>"       │
        └────────────────────────────────────────────┘
                          ↓
        SSE stream passes through untouched to the TUI
```

### The router model

The router is an LLM that reads the conversation + your rules and returns a JSON
model choice. **It should be a small, fast model** — routing is a one-line
classification, not a reasoning task.

This was learned the hard way. The original default was `nemotron-3-ultra-free`
(a 550B reasoning model), and routing took **31–55 seconds per message**.
Measured on the same free-tier endpoint, doing the identical routing task:

| Router model              | Latency  | Correct? |
|---------------------------|----------|----------|
| `nemotron-3-ultra-free`   | 31–55s   | ✅       |
| `deepseek-v4-flash-free`  | 11s      | ✅       |
| **`mimo-v2.5-free`** (default) | ~8s | ✅       |
| `north-mini-code-free`    | ~2s      | ✅       |

The default is `opencode/mimo-v2.5-free` (~8s, ~5× faster than Nemotron) — a
balance of speed and capacity for nuanced rules. For maximum speed with simple
rules, set your profile's `router_model` to `opencode/north-mini-code-free`
(~2s). Nemotron's `reasoningEffort` variants (low/medium/high) were also tested
and made no reliable difference — the variance is provider-side queueing, not
thinking budget.

> **Why routing can't be deferred past the forward.** OpenCode bakes the model
> into the user message at creation (`createUserMessage` in `prompt.ts`), and the
> generation loop reads `lastUser.model`. There is no retroactive model-switch
> endpoint. So the router's decision must complete *before* the message is
> forwarded — the routing latency is on the critical path by structural necessity.
> This is exactly why a fast router model matters.

## The two subtleties that bit us (and the fixes)

These were real bugs, documented so they don't recur:

1. **`/session/:id/message` is method-split.** The same path serves two unrelated
   purposes: `GET` = fetch session history (the TUI loads old messages this way),
   `POST` = submit a synchronous prompt. Registering only `post()` for this path
   made axum return `405` for the `GET`, which silently broke history loading for
   *every* continued session. Fix: intercept the `POST` inside the fallback
   (dispatched by method + path), letting `GET` pass through.

2. **Model source matters.** `GET /api/model` returns the models.dev **catalog**
   — advertising for models no provider actually serves (e.g.
   `opencode/minimax-m3-free` is phantom; routing to it fails with
   `ProviderModelNotFoundError`). `GET /config/providers` returns each provider's
   **actually-served** models. oc-route uses `/config/providers` for the model
   picker and pool.

## Requirements

- **OpenCode** (`opencode` on PATH, v1.17.7+). Install via
  [opencode.ai](https://opencode.ai).
- **Rust** (stable) to build oc-route.
- One or more configured providers in OpenCode (run `/connect` in the TUI, or use
  OpenCode Zen/Go). oc-route uses whatever models you've already configured — no
  extra API keys.

## Build

```bash
cargo build --release
# binary: target/release/oc-route
```

## Run

```bash
oc-route
```

This runs the default command: start `opencode serve`, run interactive setup,
start the routing proxy, launch `opencode attach` pointed at it.

### Interactive setup

1. **Profile** — pick an existing profile or create one (name, model pool, router
   model, routing prompt).
2. **Session** — "Continue last conversation" (resolved to the newest session by
   `time.updated`), pick from a list, or start new.

You then land in OpenCode's normal TUI. Every message you send is routed.

### Standalone proxy mode

If you already have an OpenCode server running, you can run just the proxy:

```bash
oc-route proxy --upstream http://127.0.0.1:4096 --profile my-profile --bind 127.0.0.1:4097
```

Then point any OpenCode TUI/SDK at `http://127.0.0.1:4097`.

## Configuration

Profiles live in `~/.config/oc-route/profiles.toml`:

```toml
[[profile]]
name = "coding-personal"
router_model = "opencode/mimo-v2.5-free"   # the model that DECIDES routing (small/fast)
sliding_window = 10                        # recent messages sent to the router
router_timeout_secs = 90

model_pool = [                              # models it may route TO (any size)
  "anthropic/claude-sonnet-4-5",
  "openai/gpt-4o",
  "opencode/nemotron-3-ultra-free",
]

routing_prompt = """
You are a model router. Route coding tasks to Claude Sonnet.
Route philosophical questions to Nemotron. Route casual conversation
to GPT-4o. Use your judgment for anything else.
"""
```

- **`router_model`** — the model that classifies each message. **Pick something
  small and fast** (default `opencode/mimo-v2.5-free`, ~8s; or
  `opencode/north-mini-code-free`, ~2s). Don't use a large reasoning model here.
- **`model_pool`** — the models the router may choose *between*. Any size. The
  router model must be a member of the pool.
- **`routing_prompt`** — your natural-language rules. Plain English. This is the
  heart of oc-route: arbitrary, subjective criteria.
- **`sliding_window`** — how many recent messages (with tool calls/reasoning
  stripped to XML hints) are sent to the router for context. The router doesn't
  need full history to detect a topic shift.

### Terminal output

OpenCode's TUI owns stdout (it's a fullscreen app). oc-route writes all its
diagnostics to **stderr** and stays quiet by default (log level `warn`), so its
output never bleeds through the TUI. Raise verbosity with
`RUST_LOG="oc_route=info"`.

## What oc-route does NOT do

- **No fork of OpenCode.** Pure external proxy. No JS/TS business logic.
- **No session management logic.** OpenCode handles storage, history, forking.
  oc-route only resolves "continue last conversation" itself (to a concrete
  `--session <id>`) because its own throwaway router sessions can briefly pollute
  the "newest session" heuristic.
- **No context management for working models.** OpenCode handles it — full
  conversation history transfers across model switches (metadata/reasoning is
  stripped for cross-model compatibility, content is preserved).
- **No caching of routing decisions.** Every message gets a fresh router call.

## Wire protocol (for contributors)

oc-route intercepts `POST /session/:id/prompt_async` and
`POST /session/:id/message` (both are prompt submissions; the latter is
synchronous and is what the TUI actually uses). It injects a `model` field
(`{ "providerID": "...", "modelID": "..." }`) into the request body and forwards.
`GET /session/:id/message` (history) and all other endpoints pass through.

The router call is itself a `POST /session/:router_session/message` to OpenCode
with the router model and an XML-structured routing task; the throwaway session is
deleted after. History for the router is fetched via
`GET /session/:id/message`, with tool calls/reasoning stripped to concise XML
hints and a sliding window applied.

Toasts (`POST /tui/show-toast`) flow through OpenCode's SSE event stream and
render in the TUI. During routing, oc-route reposts the toast every ~400ms,
cycling `Routing.` → `Routing..` → `Routing...` → back to `Routing.` — this works
because OpenCode's `toast.show()` replaces the current toast in place and resets
its auto-dismiss timer on each call.

## License

MIT
