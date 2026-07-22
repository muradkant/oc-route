# oc-route

[![CI](https://github.com/muradkant/oc-route/actions/workflows/ci.yml/badge.svg)](https://github.com/muradkant/oc-route/actions/workflows/ci.yml)

Choose a different OpenCode model for each message using rules written in plain
language.

```text
Use Sonnet for code review. Use DeepSeek when a conversation turns
philosophical. Use a fast model for casual writing.
```

`oc-route` is a Rust reverse proxy, not an OpenCode fork. It launches the real
OpenCode server and TUI, intercepts only prompt submission, asks the configured
router model to apply your policy, validates the answer, injects the selected
model, and forwards the request. OpenCode still owns sessions, context,
compaction, tools, files, LSP, providers, and generation.

## Requirements

- Linux x86-64;
- OpenCode 1.17.7 or newer on `PATH`;
- at least one connected OpenCode provider.

## Install

Download `oc-route-x86_64-linux` from the latest prerelease. It is one executable
Linux ELF with no shared-library dependencies:

```sh
install -Dm755 oc-route-x86_64-linux "$HOME/.local/bin/oc-route"
```

Or build from a clean checkout with stable Rust:

```sh
git clone https://github.com/muradkant/oc-route.git
cd oc-route
cargo build --locked --release
install -Dm755 target/release/oc-route "$HOME/.local/bin/oc-route"
```

## Use

Run from the project whose OpenCode session you want:

```sh
oc-route
```

Choose or create a routing profile, then continue a real session or start a new
one. You enter OpenCode's ordinary TUI. Each text prompt shows a routing progress
toast; the result toast names the chosen model and gives the router's short
rationale. Attachment-only prompts remain untouched.

Profiles live at `~/.config/oc-route/profiles.toml`:

```toml
[[profile]]
name = "coding-personal"
router_model = "opencode/north-mini-code-free"
sliding_window = 10
router_timeout_secs = 90

model_pool = [
  "anthropic/claude-sonnet-4-5",
  "openai/gpt-4o",
]

routing_prompt = """
Use Claude for code review and difficult implementation. Use GPT-4o for casual
conversation and prose. Apply my intent when a message crosses categories.
"""
```

- `routing_prompt` is the user's entire subjective routing policy.
- `model_pool` contains the only valid destinations.
- `router_model` makes the decision and does not need to be a destination.
- `sliding_window` is 1–50 recent OpenCode messages.
- `router_timeout_secs` is 1–600 seconds per attempt.

Unknown fields, duplicate models, malformed IDs, empty policies, and out-of-range
limits are rejected with profile-specific diagnostics. The interactive picker
uses `/config/providers`, so it offers models connected providers actually serve,
not unavailable entries from the broader catalogue.

## Dedicated router agent

The user's policy and the machine protocol have separate jobs. The profile says
*how to choose*. A small internal protocol says to choose exactly one pool member,
treat conversation text as untrusted data, and return a two-field JSON object.

OpenCode normally prepends a full coding-agent prompt even when a request supplies
its own system text. `oc-route` therefore adds a hidden, tool-free `oc-route` agent
to the child server's in-memory configuration. It does not write OpenCode config,
appear in the agent picker, or use the unrelated hidden `summary` agent.

On the same live routing sample, OpenCode's default coding agent consumed 7,194
input tokens, the earlier summary-agent experiment consumed 266, and the dedicated
agent consumed 220. The configured `router_model` is still the model doing the
work; an agent is only OpenCode's prompt/tool persona.

## Correctness and failure behavior

- Both `POST /session/:id/prompt_async` and `POST /session/:id/message` route.
- `GET /session/:id/message`, query strings, authentication, directory context,
  SSE bodies, and all unrelated endpoints pass through.
- Current OpenCode message, model, tool, file, agent, and subtask schemas are
  converted into bounded routing hints. Private reasoning and bulky tool output
  are omitted.
- Every attempt gets a fresh router session. A second fresh attempt covers
  transport failures, malformed JSON, and choices outside the pool.
- A routing failure preserves the complete original request, including the model
  OpenCode or its client already selected. It never silently substitutes the first
  pool entry.
- Internal sessions are tracked, retried on deletion failure, filtered from the
  session picker, and drained during shutdown.

## Performance

The routing inference must finish before OpenCode creates the user message; the
model cannot be changed retroactively. `oc-route` minimizes the surrounding work:

- OpenCode returns only the newest `sliding_window` messages;
- router input text, tool hints, and rationale are bounded;
- HTTP connections are pooled;
- router sessions are created on demand—measured locally at roughly 18 ms—and
  deleted off the critical path;
- the compact dedicated agent avoids thousands of irrelevant prompt tokens.

No decision is cached: every text message receives a new judgment.

## Standalone proxy

To proxy an existing HTTP OpenCode server:

```sh
oc-route proxy \
  --upstream http://127.0.0.1:4096 \
  --profile coding-personal \
  --directory "$PWD" \
  --bind 127.0.0.1:4097
```

If that server already exposes the exact dedicated agent, it is reused. Otherwise
`oc-route` starts one private OpenCode router sidecar with an isolated temporary database.
The sidecar shares local provider configuration/authentication but cannot use
credentials that exist only on a remote machine. Measured cold readiness ranged
from roughly 1.7–3.7 seconds; its OpenCode runtime used about 300 MiB RSS before
provider initialization and about 440 MiB afterward, while the Rust proxy used
about 6 MiB. It persists for the proxy lifetime to avoid adding startup latency to
every message, then exits with the proxy and removes its private database.

Set `OPENCODE_SERVER_PASSWORD` for Basic authentication; pass `--username` only
when the server does not use OpenCode's default username. Passwords are not accepted
as command-line arguments because process arguments are commonly visible to other
local users.

## Verify

The deterministic suite is quota-free:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

The live harness builds the release binary, creates isolated temporary profiles,
selects a connected free model, drives a real asynchronous OpenCode prompt, checks
model injection and cleanup, proves upstream config/provider state is unchanged,
and deletes its disposable session:

```sh
./tests/integration.sh
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for protocol and ownership details.

MIT licensed.
