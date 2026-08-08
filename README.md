# PaperMachine

PaperMachine is a local-first auto-research workbench. A **Project** owns all
of its durable, Codex-like **Sessions** and **Workflows**. A Session is the
main workbench: each user message creates a Turn, and model samples, tool calls,
retries, context trims, usage, and output remain inspectable under that Turn.

> A Project is a research world persistently managed by PaperMachine; a
> Workspace is the user filesystem an Agent is authorized to operate;
> structured runtime APIs connect them, and they never share storage or a
> security boundary.

A WorkflowProgram is a Python collaboration protocol. Starting it creates a
durable Workflow that snapshots the exact program source. It creates Agent
instances, and every Agent instance is backed by an ordinary Project-owned
Session. The workflow combines those Sessions; it does not create a separate
Session hierarchy.

Both the Project page and a Session header use the same **Run Workflow**
launcher. A Project-level launch starts new work from the Project as a whole;
a Session-origin launch records that Session as provenance and can prioritize
its recent Turns in the captured context. The Workflow's concrete request,
reusable params, optional run instructions, explicitly selected model profile, skills, permission
ceiling, per-Agent access overrides, trigger, and launch context are explicit
run configuration rather than hidden global state. Workflow code decides which
Agents receive the request or captured context; the runtime never promotes
either into system instructions.

The ordinary **New Session** command starts the reviewed `interactive-agent`
Workflow. That program creates one persistent Agent Session, waits for a human
message before every Turn, and normally remains `waiting_for_user` for the
Session's lifetime. Closing the Session archives its history and cancels that
interactive Workflow. There is no separate standalone-Session creation path.

The reviewed `goal` Workflow is the minimal autonomous loop: one persistent
Agent performs a normal tool-capable Turn and leaves the Goal `active`, or marks
that same Turn `complete` or genuinely `blocked`. Only `active` starts another
Turn; it never waits for a user message between Turns. Model, access, persistent
Agent prompt, Project context, and user guidance all use the ordinary Workflow
and Session mechanisms; pause, cancellation, provider failure, and completion
all stop the loop without a separate evaluator Agent.

The **Project Page** can run the reviewed `project-summary` Workflow once or on
a configurable durable timer. Its summary Agent reads a bounded snapshot of
existing Project Sessions, Workflow results, and Artifact metadata, then
publishes an immutable HTML progress report that the UI embeds in a sandboxed
frame. The visible summary policy is the run's ordinary `instructions` field;
there is no hidden summary daemon or extra instance model.

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

PaperMachine separates shipped resources, PaperMachine-managed Project worlds,
and user-owned Workspace directories. The repository is never used as an
implicit data store.

## Storage model

`resource_root` is read-only application material: web assets, the Python DSL
runtime, and built-in Workflows. The server requires it explicitly through
`--resource-root` or `PAPERMACHINE_RESOURCE_ROOT`; it never infers resources
from the current working directory. `data_dir` contains only application-global state:

| Platform | Default `data_dir` |
| --- | --- |
| macOS | `~/Library/Application Support/PaperMachine` |
| Linux | `$XDG_DATA_HOME/papermachine`, or `~/.local/share/papermachine` |
| Windows | `%LOCALAPPDATA%\PaperMachine` |

The default provider configuration is `<data_dir>/config.toml`; `--config`
selects another file without changing Project storage. There is no global
Project database. PaperMachine owns all Project state below `data_dir`:

```text
<data_dir>/
  projects/<project-id>/
    state/project.db
    rollouts/<session-id>.jsonl
    artifacts/
    workflow-runtime/
    runtime/
    prompts/
    workflows/
    skills/
  staging/
  trash/
```

At startup, PaperMachine scans `projects/<project-id>/state/project.db`; the
directory ID and that database's single Project row must agree. Creation builds
fresh state in `staging/` and atomically publishes it into `projects/`. Removal
atomically moves managed state into `trash/` before asynchronous deletion.

Each Project references one user-selected absolute Workspace. Agents start in
that Workspace, but PaperMachine never writes application metadata there and
rejects a Workspace that overlaps any managed state. Relocating a Project only
changes this attachment. Removing a Project leaves the Workspace untouched. A
missing Workspace remains visible and can be reattached. The web client loads
only the selected Project's full overview.

## Codex relationship

PaperMachine follows selected Codex implementation patterns and carries adapted
code for:

- Responses API request construction, WebSocket continuation, and SSE fallback;
- the sample/tool/follow-up agent loop and parallel tool calls;
- cancellation, retry visibility, and context-window accounting;
- process-group lifecycle and fail-closed sandbox execution;
- one-live-writer Session rollouts with durable-write-before-projection ordering;
- a conversation-first UI with execution details folded under each Turn.

Codex is source material, not PaperMachine's runtime dependency. Skills are
small packages owned by one Project.

## Repository layout

- `crates/protocol`: canonical IDs, entities, events, and API data types.
- `crates/model`: provider profiles, model routing, Responses API streaming,
  and deterministic model clients.
- `crates/tools`: model-visible file, shell, and fetch tools.
- `crates/execution`: process lifecycle and OS sandbox enforcement.
- `crates/agent`: sampling, tool execution, retry, control checkpoints, context.
- `crates/session`: durable multi-turn Session and workflow-action runtime.
- `crates/store`: SQLite documents/events and content-addressed artifacts.
- `crates/workflow`: Python DSL validation, effect interpretation, scheduling.
- `crates/server`: HTTP/SSE API and static web serving.
- `apps/web`: Project overview, Session workbench, and Workflow page.
- `python/papermachine`: user-facing DSL and the isolated effect client.
- `workflows/builtin`: reviewed workflows shipped with PaperMachine.
- `<data_dir>/projects/<project-id>/workflows`: Project-owned user WorkflowPrograms.

## Current capabilities

- Run multiple Projects, Sessions, Turns, and Workflows concurrently.
- Configure multiple AI providers in PaperMachine's own TOML file and select a
  model profile per Session or workflow Agent. Provider model IDs, credentials,
  transport policy, and context windows do not come from Codex.
- Preserve multi-turn model history without provider-side response storage,
  reuse provider-managed prompt caches with stable prompt-prefix keys shared by
  matching Agents, continue local tool loops and later Turns with per-Session
  WebSocket state, distinguish cache writes from cache reads, and compact long
  histories at 90% of the available context capacity.
- Inspect live text, model steps, tool calls, retries, trims, errors, and usage.
- Enable Project-local skills per Session and snapshot them per Turn.
- Assign each Session/Workflow Agent one of five access presets. A Workflow
  launch establishes a hard run ceiling, a Session-origin launch cannot exceed
  the source Session, and per-Agent class overrides cannot exceed the run.
  Profiles are snapshotted per Turn and enforced in model tool exposure,
  registry dispatch, built-in tools, path resolution, and command sandboxing;
  later in-run upgrades within the established ceiling require a typed human
  grant.
- Define Agents and actions in Python; use ordinary `if`, `for`, and `while`.
- Preserve exact human-message provenance in interactive actions: a string
  HumanRequest answer becomes the visible/model-facing user Turn, while its
  action contract remains an inspectable prompt layer.
- Compose explicit concurrency with `together(...)` and serialize each Agent's
  own Session Turns.
- Add Agents dynamically; define Teams, directed relations, task scopes,
  channels/signals, background tasks, and durable timer records.
- Suspend quiescent human/timer/signal waits without retaining an idle Python
  process or global execution permit; replay wakes on an answer, due timer, or
  durable Signal, including concurrent background-timer plus human-wait flows.
- Read bounded Project state from Workflow code with `ctx.project.snapshot()`;
  long-lived Workflows can pass `captured_at` back as `updated_after` to receive
  only later changes. Publish deterministic text/HTML Artifacts with
  `publish_artifact(...)`.
- Launch with either a fresh context or one immutable, bounded Project snapshot.
  The latter is exposed as `ctx.context`; the Workflow explicitly routes raw or
  summarized portions to the Agents that need them. A Session-origin snapshot
  focuses that Session without copying its mutable system prompt. Workflows may
  still request live state explicitly with `ctx.project.snapshot()`.
- Generate a Project progress webpage manually or with the built-in scheduled
  `project-summary` Workflow, with an explicit user-editable Workflow prompt.
- Pause, resume, or cancel a Workflow; guide an Agent at the next safe boundary;
  interrupt an attempt; or let explicit Workflow code request typed human input.
- Recover every non-terminal Workflow after a server restart by replaying its
  immutable Python source against deterministic effect IDs and a durable result
  journal; unfinished Agent actions resume the same checkpointed Turn. Local
  tool calls persist a `prepared`/`executing` boundary plus a `pure`,
  `idempotent`, `reconcilable`, or `unknown` effect disposition, so recovery
  replays only safe work. Standalone user Turns are never sampled again merely
  because the server restarted: a durable terminal candidate is committed,
  otherwise the Turn becomes `interrupted` with any uncertain effect exposed.
  Explicit Resume creates a new user-directed Turn over the committed context;
  it never reopens or resamples the interrupted Turn.
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
pnpm server:demo
```

Open <http://127.0.0.1:4310>. Demo mode exercises the full runtime and UI but
does not perform substantive research. The development launcher uses the
platform `data_dir` above with a dedicated `dev` suffix, so it cannot populate
the normal Project catalog. It uses `CARGO` when explicitly set, otherwise
`cargo` from `PATH`; it does not guess a Rust installation directory.

For real models, PaperMachine loads `config.toml` from its platform user-data
directory by default, or the file passed to `--config`. The committed
development config contains ordinary Responses-compatible profiles for GLM 5.2
and DeepSeek V4 Flash. It currently selects GLM and resolves credentials only
from the environment:

```sh
AEROIDES_API_KEY=... DEEPSEEK_API_KEY=... pnpm server:dev
```

`pnpm server:dev --config /absolute/path/config.toml` selects a different
development provider file. A packaged or direct server invocation may omit
`--data-dir` to use the normal platform location; development and benchmark
commands always provide an isolated directory explicitly.

The configuration shape supports several providers and model profiles in one
server process:

```toml
default_model = "glm-5-2"

[providers.aeroides]
kind = "open_ai_responses"
base_url = "https://private.aeroides.dev/v1"
api_key_env = "AEROIDES_API_KEY"
request_timeout_seconds = 900
stream_idle_timeout_seconds = 300
responses_websockets = false
hosted_web_search = false
prompt_cache_mode = "implicit"

[providers.deepseek]
kind = "open_ai_responses"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
request_timeout_seconds = 900
stream_idle_timeout_seconds = 300
responses_websockets = false
hosted_web_search = true
prompt_cache_mode = "implicit"

[providers.openai]
kind = "open_ai_responses"
base_url = "https://api.openai.com/v1"
api_key_env = "PAPERMACHINE_OPENAI_API_KEY"
hosted_web_search = true

[models.deepseek-flash]
provider = "deepseek"
model = "deepseek-v4-flash"
context_window = 1000000

[models.glm-5-2]
provider = "aeroides"
model = "glm-5.2"
context_window = 1048576

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

Real-model mode requires a PaperMachine provider file; demo mode must be
selected explicitly. Only the credential variable named by each provider's
`api_key_env` is read from the environment.

Provider request deadlines, stream-idle deadlines, cache mode, response
storage, reasoning defaults, hosted-tool capability, and transport policy belong
in that provider's TOML table. `hosted_web_search` is required: set it only when
the endpoint returns auditable Responses `web_search_call` items, rather than
merely accepting the tool schema. `prompt_cache_mode` accepts `auto` (default), `implicit`, or
`explicit`. Auto mode probes explicit-breakpoint support once per model and
uses provider-managed implicit caching when an OpenAI-compatible endpoint
rejects the breakpoint field.
PaperMachine first attempts Responses WebSocket mode for each model-backed
Session. If
the configured provider rejects the handshake, that server process falls back
to ordinary HTTP SSE for that Session while prompt caching remains enabled.
Set `responses_websockets = false` in the provider table when a compatible
provider is known not to implement Responses WebSocket mode.

## Verification

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
pnpm --dir apps/web test
pnpm --dir apps/web build
```

On macOS and Linux, the Rust workspace run includes the real-process `SIGKILL`
matrix in `crates/server/tests/process_recovery.rs`. Native Windows is not in
the current release test scope.

See the accepted [runtime kernel target](docs/runtime-kernel.md),
[architecture](docs/architecture.md), [Project/Workspace semantics](docs/project-workspace.md),
[prompt model](docs/prompt-model.md),
[workflow ABI](docs/workflow-abi.md),
[workflow semantics](docs/workflow-language-semantics.md), and
[security boundaries](docs/security.md). The final clean-break test and real
DeepSeek restart evidence are summarized in
[the 2026-08-08 validation report](docs/clean-break-report-2026-08-08.md).
