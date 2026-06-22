# oc-route — OpenCode Model Router

A Rust proxy that sits between OpenCode's TUI and its server, intercepting each user message, routing it to the optimal LLM model based on user-defined natural language rules, and injecting the chosen model into the request — all transparent to the user.

---

## What it does

The user runs `oc-route` instead of `opencode`. They get an interactive setup (profile selection + session selection), then dropped into OpenCode's normal TUI. From that point on, every message they send is silently routed to a different model based on their personal routing rules. A toast notification appears showing which model was chosen and why.

## Core philosophy

- **OpenCode knows best.** We add exactly one functionality: model routing. Everything else — session management, context assembly, compaction, tool execution, LSP, file handling — stays with OpenCode. We are not invasive.
- **No fork, no JS business logic.** Pure Rust. We never touch OpenCode's source code.
- **User fantasies drive routing.** The routing logic is arbitrary, subjective, and user-defined. "When I'm sexting use model X. When I'm coding use model Y." No heuristics, no trained models, no complexity scoring. An LLM interprets the user's natural language routing rules.

---

## Integration architecture

```
┌──────────────────────────────────────────────────────────┐
│  oc-route wrapper command (Rust binary)                   │
│                                                           │
│  1. Starts: opencode serve --port 4096                    │
│     (real OpenCode server, uses user's configured providers)│
│                                                           │
│  2. Starts: Rust reverse proxy on port 4097               │
│     (routing logic lives here)                            │
│                                                           │
│  3. Launches: opencode attach http://localhost:4097       │
│     (real OpenCode TUI, connects to our proxy)            │
│     flags: --session <id> or --continue (if user chose to │
│     resume an existing session)                           │
└──────────────────────────────────────────────────────────┘

         User types in OpenCode's TUI normally
                        │
                        ▼
              POST /session/:id/prompt_async
              { parts: [{type:"text", text:"..."}] }
                        │
                        ▼
         ┌─── Rust Proxy intercepts ───┐
         │  1. Parse body, extract text │
         │  2. GET /session/:id/message │
         │     (fetch history from OC)  │
         │  3. Strip tool calls → XML   │
         │     hints, apply sliding     │
         │     window                   │
         │  4. Call router model with    │
         │     structured XML input     │
         │  5. Receive model choice +   │
         │     rationale                │
         │  6. Inject model field into  │
         │     original request body    │
         │  7. POST /tui/show-toast     │
         │     (routing rationale)      │
         │  8. Forward modified POST to │
         │     OpenCode (:4096)         │
         │  9. Return 204 to TUI        │
         └───────────────────────────────┘
                    │
                    ▼
         SSE response streams back through
         GET /event → proxy → TUI (pure passthrough)
```

### Why this works

OpenCode is internally client-server. When you run `opencode`, it starts an HTTP server + a TUI client that talks to that server over HTTP. These are separable:

- `opencode serve --port 4096` → headless server only
- `opencode attach http://localhost:4097` → TUI connects to an existing server (our proxy)

The TUI communicates via the same HTTP/OpenAPI endpoints as the SDK. The proxy is a transparent HTTP middleman that intercepts one endpoint and passes everything else through.

---

## The router model

**Default:** `opencode/mimo-v2.5-free` (configurable per profile via `router_model`).

**Why not the biggest model.** Routing is a one-line classification — "which of these N models fits this message?" — not a reasoning task. The router model need only read the conversation and emit a JSON model ID. Using a large reasoning model here is pure latency overhead with no accuracy benefit.

This was learned the hard way. The original default was `opencode/nemotron-3-ultra-free` (a 550B MoE reasoning model), and **measured end-to-end routing took 31-55 seconds per message** — almost entirely the Nemotron inference call through the free Zen endpoint (oc-route's own Rust overhead is ~40ms total). I tested every free model on the same endpoint doing the identical routing task:

| Router model | Measured latency | Correct JSON? |
|---|---|---|
| `nemotron-3-ultra-free` (old default) | 31-55s | yes |
| `deepseek-v4-flash-free` | 11s | yes |
| `mimo-v2.5-free` (**new default**) | ~8s | yes |
| `north-mini-code-free` | ~2s | yes |

All produced valid, correct routing decisions. `mimo-v2.5-free` was chosen as the default as a balance of speed (~8s, a ~5× improvement) and model capacity for nuanced subjective routing rules. `north-mini-code-free` is faster (~2s) but a small code model; users who want maximum speed and whose routing rules are simple can set it as their profile's `router_model`. Nemotron's `reasoningEffort` variants (low/medium/high) were also tested and made no reliable difference — the variance is provider-side queueing, not thinking budget — so effort tuning is not a lever here.

**Routing tax:** Every user message incurs a router call before the working model starts. With the default (`mimo-v2.5-free`), typically ~8 seconds; with `north-mini-code-free`, ~2s. An animated toast cycles `Routing.` → `Routing..` → `Routing...` for the duration (see below), then a result toast with the chosen model + rationale.

> **Note on why the router can't be skipped or run after forwarding.** OpenCode bakes the model into the user message at creation time (`prompt.ts` `createUserMessage`), and the generation loop reads `lastUser.model`. There is no retroactive model-switch endpoint. So the router's decision must complete *before* the message is forwarded — the routing latency is on the critical path by structural necessity, which is exactly why a fast router model matters.

---

## Routing strategy

**LLM classifier with user-defined natural language rules.**

The router is an LLM (default `opencode/mimo-v2.5-free`, configurable per profile). The user writes a routing prompt in natural language describing when to use each model. The router reads each incoming message (+ conversation context) and returns a model choice.

This is the only architecture that supports arbitrary, subjective, user-defined routing criteria. No rule engine, no trained router, no embedding classifier can capture "if the conversation gets flirty, switch to Claude."

### Existing solutions are NOT relevant

RouteLLM (LMSYS), LiteLLM, Portkey, LLMRouter — all route based on objective optimization (cost, quality, latency). They solve a different problem. None support user fantasies as routing criteria. We build something that doesn't exist: a subjective, user-defined, natural-language LLM router for an interactive coding agent.

---

## Context management

### Working models (model A, model B, etc.)

**Handled entirely by OpenCode.** We do nothing. Verified from source:

- `packages/opencode/src/session/message-v2.ts:443` — all session messages loaded from DB filtered only by `session_id`, no model filter
- `packages/opencode/src/session/prompt.ts:1194` — model for current turn comes from `lastUser.model` (the model ID we inject)
- `packages/opencode/src/session/prompt.ts:1327` — ALL messages sent to `toModelMessagesEffect(msgs, model)`
- `packages/opencode/src/session/message-v2.ts:256` — when `differentModel` is true, provider-specific metadata is stripped, reasoning converted to plain text, but content is fully preserved
- `packages/opencode/src/session/prompt.ts:687` — `SessionEvent.ModelSwitched` is fire-and-forget UI notification, no context truncation

**Result:** Full conversation history transfers across model switches. If message 1 goes to model A and message 2 goes to model B, model B sees message 1 + model A's response. The only adaptation is metadata/reasoning stripping for cross-model compatibility.

### Router model

**Handled by us.** The router model (default `opencode/mimo-v2.5-free`, configurable per profile) is not an OpenCode session — it's a direct API call from the Rust proxy via OpenCode's prompt endpoint.

**Source of history:** We fetch from OpenCode's API (`GET /session/:id/message`), not maintain our own log. This aligns with our philosophy — OpenCode manages the source of truth, we just read from it.

**Sliding window:** We send the last N messages to the router, not the full conversation. This caps token cost and latency. The window size is configurable. The router doesn't need 50-turn history to detect "the conversation shifted from coding to something else" — the last few turns suffice.

**Stripping:** Tool calls, tool results, and reasoning parts are stripped from the history we send to the router. They are replaced with concise XML hints:

```xml
<message role="assistant" model="anthropic/claude-sonnet-4-5">
  <text>I'll read the file first.</text>
  <tool_call name="read" summary="src/main.rs (247 lines)" />
  <text>The function at line 45 has a nested match...</text>
</message>
```

Text content from user and assistant messages is preserved. File parts, agent parts, and subtask parts are summarized concisely.

---

## Router input format (XML-structured)

```xml
<routing_task>
  You are a model router. Read the conversation and the new message,
  then select the most appropriate model from the available pool.
  Follow the user's routing rules exactly.
</routing_task>

<routing_rules>
  {user's natural language routing prompt — their fantasies, their convictions}
</routing_rules>

<available_models>
  <model id="anthropic/claude-sonnet-4-5" />
  <model id="openai/gpt-4o" />
  <model id="opencode/nemotron-3-ultra-free" />
</available_models>

<conversation>
  <message role="user">
    <text>help me refactor this Rust function</text>
  </message>
  <message role="assistant" model="anthropic/claude-sonnet-4-5">
    <text>I'll help you refactor that...</text>
  </message>
  <message role="user">
    <text>actually, what's your take on free will?</text>
  </message>
</conversation>

<new_message>
  {the message the user just typed}
</new_message>

<output_format>
  Respond with a JSON object containing:
  - "model": the model ID from available_models (format: "providerID/modelID")
  - "rationale": a one-line explanation of why this model was chosen
</output_format>
```

## Router output format

The router returns structured JSON:

```json
{
  "model": "anthropic/claude-sonnet-4-5",
  "rationale": "Complex Rust refactoring with trait bounds and async patterns detected"
}
```

- **Model ID:** Must be from the available_models pool. Proxy validates this.
- **Rationale:** One-line explanation, shown to user via toast notification.

The router model itself is NOT shown to the user. The toast shows the target model and the rationale. The user doesn't need to know a router model was consulted.

---

## Rationale display — toast notifications

OpenCode has a toast notification system accessible via HTTP API:

```
POST /tui/show-toast
{
  "title": "Routed to anthropic/claude-sonnet-4-5",
  "message": "Complex Rust refactoring with trait bounds and async patterns detected",
  "variant": "info",
  "duration": 5000
}
```

Renders a bordered box in the top-right of the TUI, auto-dismisses after 5 seconds (configurable). Flows through OpenCode's SSE event stream.

Source confirmed at:
- Event definition: `packages/opencode/src/server/tui-event.ts:36-46`
- TUI handler: `packages/tui/src/app.tsx:957-965`
- HTTP endpoint: `packages/opencode/src/server/routes/instance/httpapi/groups/tui.ts:140-150`

Two phases per routing event:
1. **Animated loading toast** — on intercept, oc-route spawns a background task that reposts the toast every ~400ms, cycling the message through `Routing.` → `Routing..` → `Routing...` → back to `Routing.` for as long as routing takes. This works because the TUI's `toast.show()` replaces the current toast in place and resets its auto-dismiss timer on each call (`packages/tui/src/ui/toast.tsx`), so reposting faster than the 5s duration keeps a single toast alive and visibly progressing. The animation stops the instant routing resolves (the guard is dropped).
2. **Result toast** — once the router returns, oc-route shows the final `"Routed to {model}"` + rationale toast, which replaces the animated one.

The animation matters because the TUI clears the prompt input synchronously on submit and only renders the user-message bubble once the server starts processing — which, since oc-route routes before forwarding, is after the routing window. So the dots are the user's only feedback that something is happening during that gap. See `spawn_routing_toast` / `ToastAnimatorGuard` in `oc_client.rs`.

---

## Wire protocol — exact HTTP format

### Intercepted requests: `POST /session/:sessionID/prompt_async` and `POST /session/:sessionID/message`

Two endpoints carry a new user prompt and are intercepted (model injected) by the proxy:

- `POST /session/:id/prompt_async` — the **async** endpoint the TUI uses. Returns `204 No Content` immediately; AI runs in background, progress flows through the SSE stream.
- `POST /session/:id/message` — the **sync** endpoint (SDK / direct API). Streams the full response back. Same request body shape, so the same interception applies.

Both are routed through `intercept_prompt` (see the dispatch note in the proxy-flow summary). The GET form of `/session/:id/message` is **not** intercepted — it is the history-fetch endpoint and must pass through (see below).

**Request body (PromptPayload = PromptInput without sessionID):**

```json
{
  "parts": [
    { "type": "text", "text": "refactor this function to use async" }
  ]
}
```

Optional fields (all omitted by TUI normally):
- `model`: `{ "providerID": "string", "modelID": "string" }` — **this is what we inject**
- `agent`: string (agent name)
- `noReply`: boolean
- `system`: string (system prompt override)
- `variant`: string (model variant)
- `format`: `{ "type": "text" }` or `{ "type": "json_schema", "schema": {...} }`

**Part types in `parts` array:**
- `{ "type": "text", "text": "..." }` — text message
- `{ "type": "file", "mime": "...", "url": "file:///..." }` — file attachment
- `{ "type": "agent", "name": "build" }` — agent invocation
- `{ "type": "subtask", "prompt": "...", "agent": "general" }` — subtask

**After our injection:**

```json
{
  "parts": [
    { "type": "text", "text": "refactor this function to use async" }
  ],
  "model": {
    "providerID": "anthropic",
    "modelID": "claude-sonnet-4-5"
  }
}
```

**Model field structure** (confirmed from `prompt.ts:1589-1592`):
```json
{ "providerID": "anthropic", "modelID": "claude-sonnet-4-5" }
```
Both are plain strings at the JSON level (branded types internally).

### Response: `204 No Content`

The `prompt_async` endpoint returns `204` immediately. No body. The AI runs in the background.

### SSE stream: `GET /event` — pure passthrough

Standard SSE format:
```
event: message
data: {"id":"evt_...","type":"session.message.part","properties":{...}}

event: message
data: {"id":"evt_...","type":"session.message.updated","properties":{...}}
```

Heartbeat every 10 seconds:
```
event: message
data: {"type":"server.heartbeat","properties":{}}
```

First event:
```
event: message
data: {"type":"server.connected","properties":{}}
```

The proxy does **nothing** with this stream except pipe bytes through. Transparent relay. Flush bytes as they arrive, don't buffer.

### History fetch: `GET /session/:sessionID/message`

Returns array of `SessionV1.WithParts`:

```json
[
  {
    "info": {
      "id": "msg_01J...",
      "sessionID": "ses_01J...",
      "role": "user",
      "model": { "providerID": "anthropic", "modelID": "claude-sonnet-4-5" },
      "time": { "created": 1718700000000, "completed": 1718700005000 },
      "agent": "general",
      "mode": "primary",
      "path": { "cwd": "/home/user/project", "root": "/home/user/project" },
      "cost": 0.0042,
      "tokens": { "total": 1500, "input": 1000, "output": 500 }
    },
    "parts": [
      { "type": "text", "text": "help me refactor this" },
      { "type": "tool", "name": "read", ... },
      { "type": "text", "text": "The function at line 45..." }
    ]
  }
]
```

The proxy parses this, strips tool/reasoning parts, replaces with hints, wraps in XML for the router.

### Toast: `POST /tui/show-toast`

```json
{
  "title": "Routed to anthropic/claude-sonnet-4-5",
  "message": "Complex Rust refactoring detected",
  "variant": "info",
  "duration": 5000
}
```

### Method-split on `/session/:id/message` — the subtle one

This single path serves two unrelated purposes, split by HTTP method:

| Method | Path | Handling | Why |
|--------|------|----------|-----|
| **GET** | `/session/:id/message` | **Passthrough** | The TUI loads session history with this. Blocking it (e.g. via a method-router that 405s the GET) silently breaks old messages for every continued session. |
| **POST** | `/session/:id/message` | **Intercept** (same as `prompt_async`) | The sync prompt endpoint; same body shape as `prompt_async`, so the model is injected the same way. |

This is why `/session/:id/message` is **not** an explicit `post()` route in axum: a method-router on a matched path returns `405 Method Not Allowed` for non-matching methods *instead of* falling to the fallback, which would break the history GET. Both branches are dispatched inside the fallback by method + path (see `passthrough` in `proxy.rs`).

### All other endpoints — transparent passthrough

| Method | Path | Handling |
|--------|------|----------|
| GET | `/event` (SSE) | Passthrough (stream bytes) |
| GET | `/session` | Passthrough |
| GET | `/session/:id` | Passthrough |
| GET | `/session/:id/message` | Passthrough (history fetch; we also call this ourselves for router history) |
| POST | `/session/:id/message` | Intercept (sync prompt; model injected, see above) |
| POST | `/session` | Passthrough |
| DELETE | `/session/:id` | Passthrough |
| PATCH | `/session/:id` | Passthrough |
| POST | `/session/:id/fork` | Passthrough |
| POST | `/session/:id/abort` | Passthrough |
| POST | `/session/:id/command` | Passthrough |
| POST | `/session/:id/shell` | Passthrough |
| POST | `/session/:id/permissions/:permID` | Passthrough |
| All | `/tui/*` | Passthrough (except our own POST /tui/show-toast calls) |
| All | `/provider/*`, `/agent/*`, etc. | Passthrough |

---

## Session management — mostly OpenCode's native behavior

We do minimal session management: OpenCode handles storage, history, and forking. Our only intervention is **resolving "continue last conversation" ourselves** (see below) instead of handing OpenCode a bare `--continue`.

| What | OpenCode provides | Our responsibility |
|---|---|---|
| List existing sessions | `GET /session` | Call it, display in interactive setup |
| Continue a specific session | `--session ses_abc123` flag on `opencode attach` | Pass the flag |
| Continue most recent session | `--continue` flag on `opencode attach` | **Resolve to a concrete id, pass `--session`** (see below) |
| Start new session | Just launch TUI without session flag | Don't pass a flag |
| Fork a session | `--fork` with `--session` or `--continue` | Pass both flags |

Source confirmed at:
- `--continue` flag: `packages/opencode/src/cli/cmd/tui.ts:85-89`, consumed in `packages/tui/src/app.tsx:487-508`
- `--session` flag: `packages/opencode/src/cli/cmd/tui.ts:90-94`, consumed in `packages/tui/src/app.tsx:478-483`
- `opencode attach` supports same flags: `packages/opencode/src/cli/cmd/attach.ts:21-34`
- Session list API: `GET /session` at `groups/session.ts:111`
- Session create API: `POST /session` at `groups/session.ts:203`

### Why we resolve "continue" ourselves (not `--continue`)

OpenCode's `--continue` opens the single most-recently-**updated** session (`app.tsx:494-508`: sorts by `time.updated`, takes the first with no `parentID`). This is fragile in oc-route's presence for two reasons:

1. **Router-session churn.** Every routed message creates a throwaway `"oc-route-router"` session (`prompt_router` in `oc_client.rs`) and deletes it after. While it lives, it is briefly the newest session. A crash or a slow delete can leave it as the newest — and `--continue` would then open an empty router session instead of the user's real conversation.
2. **Wrong "newest".** Even in steady state, the most-recently-*updated* session is not necessarily the one the user thinks of as "last conversation" (a one-off test session can outrank a long working session by timestamp).

So oc-route resolves the choice explicitly: it fetches `GET /session`, **drops its own `"oc-route-router"` sessions** (`is_router_session` in `setup.rs`), sorts the remainder by `time.updated` (falling back to `created`), and passes the resulting id via `--session`. The interactive label reflects the resolved target: `Continue last conversation — <title> (ses_…)`. A bare `--continue` is only passed when there are no sessions to resolve at all.

### Launch flow

```
User runs: oc-route
    │
    ├──► Interactive setup:
    │    Step 1: Profile
    │      → Select existing: [profile names...] + "Create new..."
    │      (if "Create new": prompt for name, model pool, routing prompt)
    │
    │    Step 2: Session (always shown, regardless of step 1)
    │      → Continue last conversation  (resolved to newest non-router id → --session)
    │      → Select from list: [sessions from GET /session, router sessions hidden]
    │      → Start new conversation
    │
    ├──► Start: opencode serve --port 4096
    ├──► Start: Rust proxy on port 4097 (pointing to :4096, using selected profile)
    └──► Launch: opencode attach http://localhost:4097 [--session <id>]
         (a concrete --session is always passed for continue/select;
          no flag is passed for "Start new")
```

---

## Profiles

### Storage

```
~/.config/oc-route/
  profiles.toml
```

### Format

```toml
[[profile]]
name = "coding-personal"
router_model = "opencode/mimo-v2.5-free"
sliding_window = 10

model_pool = [
  "anthropic/claude-sonnet-4-5",
  "openai/gpt-4o",
  "opencode/nemotron-3-ultra-free",
]

routing_prompt = """
You are a model router. Route coding tasks to Claude Sonnet.
Route philosophical questions to Nemotron. Route casual conversation
to GPT-4o. Use your judgment for anything else.
"""
# Note: the router_model is the model that DECIDES routing (should be small/fast,
# e.g. mimo-v2.5-free); the model_pool contains the models it may route TO (any size).
# Nemotron appears in the pool above as a routing TARGET, not as the router.

[[profile]]
name = "creative-writing"
router_model = "opencode/mimo-v2.5-free"
sliding_window = 8

model_pool = [
  "anthropic/claude-sonnet-4-5",
  "opencode/nemotron-3-ultra-free",
]

routing_prompt = """
You are a model router. Route creative writing to Claude.
Route research questions to Nemotron.
"""
```

### Profile ↔ Session relationship

**Orthogonal.** Any profile can be applied to any session. The profile is our routing configuration. The session is OpenCode's conversation history. They don't know about each other.

- User can apply profile A to session X today, and profile B to session X tomorrow.
- We can optionally record which profile was last used with each session (in our own metadata), so the wrapper can default to it next time. Convenience feature, not required.

### Interactive setup UX

**First launch** (no profiles.toml):
1. Create new profile (prompt for name, model pool, routing prompt)
2. Choose session (continue last / pick existing / start new)

**Subsequent launches**:
1. Choose profile: [existing profiles...] + "Create new..."
   - If "Create new": fill out details, save to profiles.toml
2. Choose session: continue last / pick existing / start new
   - If "Pick existing": we list sessions from `GET /session`, user selects

Session selection is always offered regardless of whether the user is creating a new profile or selecting an existing one.

---

## Provider support

OpenCode is fully BYOK (bring your own key). Not limited to OpenCode's Zen/Go subscriptions.

- 75+ providers via AI SDK + Models.dev
- Users add keys via `/connect` command in OpenCode
- Credentials stored in `~/.local/share/opencode/auth.json`
- Supports: Anthropic (API key or OAuth), OpenAI (API key or OAuth), OpenRouter, Google Vertex, AWS Bedrock, Azure OpenAI, and any OpenAI-compatible endpoint
- OpenCode Zen / Go are optional, treated as "just another provider"

**Our router uses whatever models the user has configured in OpenCode.** We query `GET /config/providers` for the list of models each provider **actually serves**, and the user picks from those for their model pool. No extra API keys, no separate provider management.

> **Why `/config/providers` and not `/api/model`.** `GET /api/model` returns the models.dev **catalog** (`catalog.model.available()` in `packages/server/src/handlers/model.ts`) — advertising for every model that *could* be offered, including ones the user's providers don't actually serve (e.g. `opencode/minimax-m3-free` exists in the catalog but no provider serves it). Routing to a catalog-only model fails at runtime with `ProviderModelNotFoundError`. `GET /config/providers` returns each configured provider's real `providers[].models` map, so every model it lists is runnable. oc-route's `list_models` flattens that map into `{providerID, modelID, name}` for the picker. This is also what makes the `opencode` (free tier) and `opencode-go` (subscription) providers distinguishable — same model family, different real IDs (`opencode/minimax-m3-free` is phantom; `opencode-go/minimax-m3` is real).

The default router model (`opencode/mimo-v2.5-free`) is available through OpenCode's free tier, no additional setup needed. Users can set any model as their profile's `router_model`; for maximum speed use a small fast model like `opencode/north-mini-code-free` (~2s). See "The router model" above for the latency measurements that informed this default.

---

## OpenCode source references (verified)

All confirmed against the actual source at `https://github.com/sst/opencode` (v1.17.7):

| What | File | Line(s) |
|---|---|---|
| All session messages loaded (no model filter) | `packages/opencode/src/session/message-v2.ts` | 443-445 |
| Model determined from last user message | `packages/opencode/src/session/prompt.ts` | 1194 |
| All messages sent to toModelMessagesEffect | `packages/opencode/src/session/prompt.ts` | 1327 |
| differentModel detection (metadata strip, not exclusion) | `packages/opencode/src/session/message-v2.ts` | 256 |
| Reasoning converted to text for cross-model | `packages/opencode/src/session/message-v2.ts` | 373-381 |
| ModelSwitched event (fire-and-forget, no context impact) | `packages/opencode/src/session/prompt.ts` | 687-701 |
| ModelRef type ({ providerID, modelID }) | `packages/opencode/src/session/prompt.ts` | 1589-1592 |
| PromptInput schema | `packages/opencode/src/session/prompt.ts` | 1594-1616 |
| --continue flag definition | `packages/opencode/src/cli/cmd/tui.ts` | 85-89 |
| --session flag definition | `packages/opencode/src/cli/cmd/tui.ts` | 90-94 |
| TUI consumes --continue | `packages/tui/src/app.tsx` | 487-508 |
| TUI consumes --session | `packages/tui/src/app.tsx` | 478-483 |
| attach command supports same flags | `packages/opencode/src/cli/cmd/attach.ts` | 21-34 |
| Toast event definition | `packages/opencode/src/server/tui-event.ts` | 36-46 |
| Toast TUI handler | `packages/tui/src/app.tsx` | 957-965 |
| Toast HTTP endpoint | `packages/opencode/src/server/routes/instance/httpapi/groups/tui.ts` | 140-150 |
| POST /session/:id/message route | `packages/opencode/src/server/routes/instance/httpapi/groups/session.ts` | 316 |
| POST /session/:id/prompt_async route | `packages/opencode/src/server/routes/instance/httpapi/groups/session.ts` | 329 |
| GET /session/:id/message route | `packages/opencode/src/server/routes/instance/httpapi/groups/session.ts` | 179 |
| GET /session route (list) | `packages/opencode/src/server/routes/instance/httpapi/groups/session.ts` | 111 |
| POST /session route (create) | `packages/opencode/src/server/routes/instance/httpapi/groups/session.ts` | 203 |
| SSE event handler | `packages/opencode/src/server/routes/instance/httpapi/handlers/event.ts` | 12-73 |
| prompt_async handler (204 No Content) | `packages/opencode/src/server/routes/instance/httpapi/handlers/session.ts` | 309-327 |

---

## Proxy flow summary (every HTTP interaction)

```
┌─────────────────────────────────────────────────────┐
│ PROXY (Rust, port 4097)                              │
│                                                      │
│  1. POST /session/:id/prompt_async  → INTERCEPT      │
│     POST /session/:id/message       → INTERCEPT      │
│     (sync prompt, same body shape; dispatched by     │
│      method+path inside the fallback — NOT via an    │
│      explicit post() route, because that would 405   │
│      the GET /session/:id/message history fetch)     │
│        a. POST /tui/show-toast, cycling "Routing./.."  │
│           every ~400ms until routing resolves (guard)  │
│        b. Parse body, extract text from parts          │
│        c. GET /session/:id/message (fetch history)     │
│        d. Strip tool calls → XML hints                 │
│        e. Apply sliding window (last N messages)       │
│        f. Call router model with structured XML input │
│        g. Receive model choice + rationale             │
│        h. Inject model field into original body        │
│        i. Forward modified POST to OpenCode (:4096)    │
│        j. POST /tui/show-toast (model + rationale)     │
│        k. Return upstream response to TUI              │
│                                                      │
│  2. Everything else → transparent passthrough        │
│     (GET /event SSE, GET /session, GET /session/:id, │
│      GET /session/:id/message [history fetch!],      │
│      session create/delete/fork/permissions,         │
│      command, shell, tui/*, etc.)                    │
└─────────────────────────────────────────────────────┘
```

---

## What is NOT implemented (v1 scope)

- **No caching of routing decisions.** Every message gets a fresh router call.
- **No custom TUI UI.** We use OpenCode's existing toast notification system only.
- **No fork of OpenCode.** Pure external proxy.
- **No JS/TS business logic.** Pure Rust.
- **No session management logic.** We forward flags, OpenCode handles everything.
- **No context management for working models.** OpenCode handles all of it.

---

## Tech stack

- **Language:** Rust
- **HTTP proxy:** hyper or axum (reverse proxy with streaming support)
- **Config:** TOML (profiles.toml)
- **Interactive setup:** Rust TUI prompts (dialoguer or similar)
- **Router API calls:** via OpenCode's server API — the proxy calls OpenCode's prompt endpoint with the chosen router model, so the router uses whatever provider/model the user has configured in OpenCode (default `opencode/mimo-v2.5-free`).

---

## Testing strategy

The proxy can be tested without human interaction:
1. Start `opencode serve` on a port
2. Start the Rust proxy pointing to it
3. Send HTTP requests to the proxy simulating TUI traffic (POST /session/:id/prompt_async)
4. Verify the proxy intercepts, routes, injects model, and forwards correctly
5. Verify SSE passthrough works
6. Verify toast notifications are sent
7. Verify session management flags are forwarded correctly
