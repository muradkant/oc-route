# Architecture

`oc-route` adds one decision between OpenCode's TUI and server: which configured
model should own this message?

```text
oc-route
  ├─ opencode serve --port 4096
  ├─ reverse proxy on 4097
  └─ opencode attach http://127.0.0.1:4097

TUI prompt
  → proxy extracts text and requests recent history
  → router model applies the selected profile
  → proxy validates and injects {providerID, modelID}
  → OpenCode handles the message, tools, and response
  → proxy returns bytes unchanged
```

Preferred ports are opportunistic: the wrapper acquires free alternatives when
4096 or 4097 is occupied.

## Invariants

1. OpenCode remains the source of sessions, history, configured providers,
   context assembly, compaction, tools, and generation.
2. Only prompt-submission POSTs are routed; every other request and SSE byte is
   transparent.
3. A target must belong to the profile's model pool.
4. Every user message receives a fresh routing decision.
5. Router history is current, bounded, and read from OpenCode—not cached by the
   proxy.
6. A throwaway router session is used once, then released.
7. Proxy failure never manufactures a successful response.

## Request dispatch

Two endpoints submit prompts:

```text
POST /session/:id/prompt_async
POST /session/:id/message
```

Both carry `parts` and accept an optional model:

```json
{
  "parts": [{"type":"text","text":"review this function"}],
  "model": {"providerID":"anthropic","modelID":"claude-sonnet-4-5"}
}
```

The second path is method-split. Its GET form loads history. Axum's matched
method router returns 405 for unsupported methods instead of falling through,
so the proxy dispatches by method and path inside its fallback:

| Request | Behavior |
| --- | --- |
| `POST …/prompt_async` | Route, inject, forward |
| `POST …/message` | Route, inject, forward |
| `GET …/message` | Transparent history |
| `GET /event` | Byte-stream passthrough |
| everything else | Transparent passthrough |

An unparseable or textless prompt is forwarded without routing rather than
destroyed.

## Decision input

The proxy asks OpenCode for:

```text
GET /session/:id/message?limit=N
```

OpenCode selects the newest N records and returns them chronologically. The
proxy therefore avoids fetch-all-then-slice growth while leaving history
ownership upstream. A regression test proves this window equivalent to local
slicing.

Router input is XML because the boundary must distinguish policy, allowed
models, prior messages, tool summaries, and the new prompt without allowing one
to impersonate another:

```xml
<routing_task>Choose one allowed model and return JSON.</routing_task>
<routing_rules>…user-authored policy…</routing_rules>
<available_models><model id="provider/model" /></available_models>
<conversation>
  <message role="assistant" model="provider/model">
    <text>…</text>
    <tool_call name="read" summary="src/main.rs" />
  </message>
</conversation>
<new_message>…</new_message>
```

User and assistant text survives. Tool calls, results, reasoning, files, agents,
and subtasks become bounded hints; XML-sensitive content is escaped.

The router returns:

```json
{"model":"anthropic/claude-sonnet-4-5","rationale":"The task is a code review."}
```

Parsing tolerates fenced or surrounding prose, but validation requires a pool
member and a splittable `provider/model` identifier.

## Critical path and concurrency

The selected model must be present when OpenCode creates the user message, so
routing cannot move behind forwarding. The proxy instead removes work around
the inference:

- persistent HTTP clients reuse upstream connections;
- server-side history limiting bounds transfer and parsing;
- `RouterSessionSlot` prefetches the next session in the background;
- the used session is deleted asynchronously after the attempt;
- one failure retries with a distinct fresh session;
- timeouts bound each inference.

Fresh sessions preserve decision independence and prevent router context from
becoming an accidental second conversation. Prefetch changes timing, not
identity: consecutive `take()` calls must return distinct IDs.

## Toast lifecycle

Submitting a prompt clears the TUI input before OpenCode can render the user
message. During the router delay, a guard reposts one toast every second:

```text
Routing. → Routing.. → Routing... → Routing.
```

Dropping the guard sets an atomic stop flag and wakes its sleep before another
POST can occur. That ordering prevents a late animation frame from overwriting
the result. The rationale toast is posted only after the working response has
forwarded, lasts eight seconds, and is fire-and-forget so toast delivery cannot
delay response bytes.

## Session selection

The proxy's short-lived `oc-route-router` sessions can temporarily become the
most recently updated session. Passing bare `--continue` could therefore open a
router artifact. Setup instead:

1. fetches `GET /session`;
2. removes router sessions;
3. orders the remainder by update time, then creation time;
4. passes the chosen concrete ID through `--session`.

Starting new passes no session flag. A bare `--continue` is used only when
there is no session to resolve.

## Model discovery and context transfer

`/config/providers` supplies the picker because it describes models connected
providers actually serve. `/api/model` is a broader catalogue and can advertise
unavailable targets.

The router sees only its bounded summary. Working models are different:
OpenCode loads the complete session regardless of the model used on prior
turns. When providers change, OpenCode strips incompatible metadata and turns
reasoning into portable text; content remains.

## Failure and ownership

- A router attempt owns its session lease. Success, parse failure, HTTP failure,
  or timeout releases it; retry acquires another.
- The wrapper monitors the `opencode serve` child. Premature server death tears
  down proxy and TUI cleanly.
- The loading guard owns its animator and stops on every return path.
- Router failure falls back through the proxy's declared behavior and reports a
  warning; it does not silently inject an unvalidated target.
- Standalone proxy mode owns no OpenCode server or TUI child.

## Verification map

Unit tests cover:

- XML escaping and message rendering;
- decision extraction and pool validation;
- server-side window equivalence;
- router-session identity and URL limiting;
- prompt-path method dispatch;
- configuration validation and defaults.

The live harness exercises a real server, proxy, router inference, working
model, toast endpoints, session cleanup, history parity, and continued-session
behavior. Its network/model dependency makes it explicit rather than part of
the default test command.
