# PaperMachine

PaperMachine is a local-first auto-research workbench. A **Project** owns all
of its durable, Codex-like **Sessions** and **Workflows**. A Session is the
main workbench: each user message creates a Turn, and model samples, tool calls,
retries, context trims, usage, and output remain inspectable under that Turn.

A WorkflowProgram is a Python collaboration protocol. Starting it creates a
durable Workflow that snapshots the exact program source. It creates Agent
instances, and every Agent instance is backed by an ordinary Project-owned
Session. The workflow combines those Sessions; it does not create a separate
Session hierarchy.

```text
Project
  Sessions
    Turn -> model/tool Steps
  Workflows
    optional starting Session
    Agent instance <-> Session
      Action -> Attempt -> Turn -> Steps
    Teams, relations, scopes, timers, channels, human requests
```

There is no legacy compatibility layer. Server state lives under
`.papermachine/state/`; every Project is anchored to its own absolute directory.

## Codex relationship

PaperMachine follows selected Codex implementation patterns and carries adapted
code for:

- Responses API request construction, WebSocket continuation, and SSE fallback;
- the sample/tool/follow-up agent loop and parallel tool calls;
- cancellation, retry visibility, and context-window accounting;
- process-group lifecycle and fail-closed sandbox execution;
- a conversation-first UI with execution details folded under each Turn.

Codex is source material, not PaperMachine's runtime dependency. PaperMachine
does not launch or embed the Codex CLI. It deliberately omits Codex app-server,
CLI/TUI protocols, approvals compatibility, MCP, plugins, apps, connectors,
telemetry, and the global skill marketplace. Skills are small packages owned by
one Project.

## Repository layout

- `crates/protocol`: canonical IDs, entities, events, and API data types.
- `crates/model`: provider profiles, model routing, Responses API streaming,
  and deterministic model clients.
- `crates/tools`: model-visible file, shell, fetch, and human-input tools.
- `crates/execution`: process lifecycle and OS sandbox enforcement.
- `crates/agent`: sampling, tool execution, retry, control checkpoints, context.
- `crates/session`: durable multi-turn Session and workflow-action runtime.
- `crates/store`: SQLite documents/events and content-addressed artifacts.
- `crates/workflow`: Python DSL validation, effect interpretation, scheduling.
- `crates/server`: HTTP/SSE API and static web serving.
- `apps/web`: Project overview, Session workbench, and Workflow page.
- `python/papermachine`: user-facing DSL and the isolated effect client.
- `workflows/builtin`: reviewed workflows shipped with PaperMachine.
- `<project>/.papermachine/workflows`: Project-owned user WorkflowPrograms.

## Current capabilities

- Run multiple Projects, Sessions, Turns, and Workflows concurrently.
- Configure multiple AI providers in PaperMachine's own TOML file and select a
  model profile per Session or workflow Agent. Provider model IDs, credentials,
  transport policy, and context windows do not come from Codex.
- Preserve multi-turn model history without provider-side response storage,
  reuse provider-managed prompt caches with stable prompt-prefix keys shared by
  matching Agents, continue local tool loops and later Turns with per-Session
  WebSocket state, distinguish cache writes from cache reads, and compact long
  histories at 90% of the available context budget.
- Inspect live text, model steps, tool calls, retries, trims, errors, and usage.
- Enable Project-local skills per Session and snapshot them per Turn.
- Assign each Session/Workflow Agent one of five access profiles. Profiles are
  snapshotted per Turn and enforced in model tool exposure, registry dispatch,
  built-in tools, path resolution, and command sandboxing; upgrades requested
  by workflows require a typed human grant.
- Define Agents and actions in Python; use ordinary `if`, `for`, and `while`.
- Compose explicit concurrency with `together(...)` and serialize each Agent's
  own Session Turns.
- Add Agents dynamically; define Teams, directed relations, task scopes,
  channels/signals, background tasks, and durable timer records.
- Pause, resume, or cancel a Workflow; guide an Agent at the next safe boundary;
  interrupt an attempt; or let workflow/model code request typed human input.
- Recover every non-terminal Workflow after a server restart by replaying its
  immutable Python source against deterministic effect IDs and a durable result
  journal; unfinished Agent actions resume the same checkpointed Turn.
- Generate, inspect, validate, and save workflow source from the Workflow
  page. Advanced source editing is available but is not the primary UI.
- Use Responses API hosted web search for normal research and retain every
  hosted call as an inspectable Tool Step. `fetch_url` remains available for
  bounded, readable extraction of a known HTTPS source.

## Quick start

Rust is pinned by `rust-toolchain.toml`; Node.js and pnpm are required.

```sh
pnpm install
pnpm --dir apps/web build
cargo run -p papermachine-server -- --root . --demo
```

Open <http://127.0.0.1:4310>. Demo mode exercises the full runtime and UI but
does not perform substantive research.

For real models, PaperMachine loads `papermachine.toml` from the workspace root
by default (or the file passed to `--config`). The committed development config
uses DeepSeek V4 Flash and resolves its credential only from the environment:

```sh
DEEPSEEK_API_KEY=... cargo run -p papermachine-server -- --root .
```

The configuration shape supports several providers and model profiles in one
server process:

```toml
default_model = "deepseek-flash"

[providers.deepseek]
kind = "open_ai_responses"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
responses_websockets = false
prompt_cache_mode = "implicit"

[providers.openai]
kind = "open_ai_responses"
base_url = "https://api.openai.com/v1"
api_key_env = "PAPERMACHINE_OPENAI_API_KEY"

[models.deepseek-flash]
provider = "deepseek"
model = "deepseek-v4-flash"
context_window = 1000000

[models.openai-main]
provider = "openai"
model = "gpt-5.6-sol"
context_window = 1000000
```

The Session `model` field and `Agent.model` select profile IDs such as
`deepseek-flash`, not raw provider model names. Model-step metadata records the
selected profile, provider, and concrete upstream model. The current transport
adapter supports providers that implement the OpenAI Responses shape; additional
wire protocols can be added behind the same router.

Reusing an existing Codex OpenAI configuration remains an explicit fallback
import path when no PaperMachine config file is present:

```sh
cargo run -p papermachine-server -- \
  --root . \
  --codex-home /path/to/.codex
```

The importer reads `model`, `openai_base_url`, `model_reasoning_effort`,
`disable_response_storage`, `model_context_window`, optional
`prompt_cache_mode`, and `OPENAI_API_KEY`.
`network_access` and `review_model` are intentionally not imported: PaperMachine
owns its tool policy and has no special review-model path.

Legacy single-provider environment configuration is also supported when no
PaperMachine config file is present:

```sh
OPENAI_API_KEY=... \
OPENAI_BASE_URL=https://api.openai.com/v1 \
OPENAI_REASONING_EFFORT=medium \
PAPERMACHINE_MODEL=gpt-5.6-sol \
PAPERMACHINE_MODEL_CONTEXT_WINDOW=1000000 \
  cargo run -p papermachine-server -- --root .
```

`OPENAI_RESPONSES_ENDPOINT` takes precedence over `OPENAI_BASE_URL`; otherwise
`/responses` is appended. Provider request and stream-idle deadlines default to
15 and 5 minutes and can be changed with
`OPENAI_REQUEST_TIMEOUT_SECONDS` and `OPENAI_STREAM_IDLE_TIMEOUT_SECONDS`.
`PAPERMACHINE_PROMPT_CACHE_MODE` accepts `auto` (default), `implicit`, or
`explicit`. Auto mode probes explicit-breakpoint support once per model and
falls back to provider-managed implicit caching when an OpenAI-compatible
endpoint rejects the breakpoint field.
PaperMachine first attempts Responses WebSocket mode for each model-backed
Session. If
the configured provider rejects the handshake, that server process falls back
to ordinary HTTP SSE for that Session while prompt caching remains enabled.
Set `PAPERMACHINE_RESPONSES_WEBSOCKETS=false` when a compatible provider is
known not to implement Responses WebSocket mode.

## Verification

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
pnpm --dir apps/web test
pnpm --dir apps/web build
```

See [architecture](docs/architecture.md), [prompt model](docs/prompt-model.md),
[workflow ABI](docs/workflow-abi.md),
[workflow semantics](docs/workflow-language-semantics.md), and
[security boundaries](docs/security.md).
