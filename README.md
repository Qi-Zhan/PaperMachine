# PaperMachine

PaperMachine is a local-first research workbench. Its runtime model is small:

~~~text
Project
  Session                         one durable WorkflowProgram execution
    Agents                        independent model identities and rollouts
      ActionInvocation
        ActionAttempt -> Turn -> model/tool Steps
    effects, events, human requests, Agent inputs, artifacts
~~~

A WorkflowProgram is immutable Python source plus a manifest. Starting one
creates a Session that owns its complete lifecycle. The Session may create one
or many Agents; every Agent keeps its own prompt, model, access, skills, and
canonical rollout. Session is the only runtime lifecycle.

> A Project is a research world persistently managed by PaperMachine; a
> Workspace is the user filesystem an Agent is authorized to operate;
> structured runtime APIs connect them, and they never share storage or a
> security boundary.

Every model Turn belongs to an ActionAttempt. A user message is not a separate
execution primitive: the built-in **interactive-agent** program obtains a
durable HumanRequest answer and passes it to an ordinary Action. **goal** and
**project-summary** are ordinary built-in programs as well; the Rust kernel has
no slug-specific execution path.

One Session may run several Agents concurrently, while each Agent admits one
active Turn at a time. Agents collaborate through durable `AgentInput` and
ordinary `agent_task` Actions: `list_agents`, `send_message`, `wait_agent`,
`spawn_agent`, and `interrupt_agent` never create a second scheduler or
recovery protocol. A Session-origin launch records provenance but still creates
an independent Session with one immutable program snapshot.

Project Home is the output of the ordinary **project-summary** Session.
`ctx.project.changes()` supplies bounded Project entity snapshots directly to
its no-tool Action. The Agent returns one complete HTML fragment; the host
validates and publishes that exact Action result as immutable source and HTML
Artifacts. The page is Project-managed state, not a Workspace file or embedded
dashboard.

## Storage

The **Project Page** can run the reviewed `project-summary` Workflow once or in
a normal loop separated by durable waits. Snapshot pages are derived from the
Project change log and bounded to about 1 MiB. The built-in excludes prior runs
of its own WorkflowProgram, so the derived Home never becomes its own evidence.
The Project stores one canonical Artifact/source/revision reference;
unchanged refreshes reuse it. The sanitized fragment is the Project home page
itself; there is no fixed dashboard, embedded frame, hidden summary daemon, or
special Summary runtime.

**resource_root** contains read-only application resources: web assets, the
Python DSL runtime, and built-in WorkflowPrograms. The server requires it via
**--resource-root** or **PAPERMACHINE_RESOURCE_ROOT**; it never treats the
current repository as an implicit data store.

**data_dir** defaults to:

| Platform | Default |
| --- | --- |
| macOS | ~/Library/Application Support/PaperMachine |
| Linux | $XDG_DATA_HOME/papermachine, or ~/.local/share/papermachine |
| Windows | %LOCALAPPDATA%\\PaperMachine |

PaperMachine-managed state is isolated by Project:

~~~text
<data_dir>/
  config.toml
  projects/<project-id>/
    state/project.db
    rollouts/<agent-id>.jsonl
    artifacts/
    prompts/
    workflows/
    skills/
    workflow-runtime/           disposable Python process scratch
    runtime/sandboxes/          disposable Turn scratch
  staging/
  trash/
~~~

There is no global Project database. Startup scans each Project independently;
one damaged entry produces a diagnostic without hiding healthy Projects.
Creation publishes a staged directory atomically. Removal stops that Project's
runtime and Store, moves only managed state to trash, and never deletes its
Workspace.

Each Project attaches one canonical absolute Workspace. It may be selected by
the user or created under **~/Documents/PaperMachine/**. PaperMachine writes no
database, prompt, Skill, rollout, or hidden metadata there. A missing Workspace
leaves Project history inspectable and can be reattached.

## Runtime

Before every Turn the host freezes four independent snapshots:

- **ModelRouteSnapshot**: exact provider/model route and non-secret config hash;
- **TurnEnvironmentSnapshot**: Workspace revision and materialized access;
- **ToolSetSnapshot**: exact sorted local tool definitions and hash;
- **PromptSnapshot**: rendered runtime, Project, Session, Agent, Skill, Action,
  and retry-guidance instructions.

Bare **@action** uses collaboration tools plus the native tools allowed by the
Agent's access. **@action(tools=[])** creates an empty local Registry; a
non-empty list selects an exact subset. The native surface is
`exec_command`, `write_stdin`, and `apply_patch`. Hosted web search is outside
the local Registry and depends only on `search_context_size` plus provider
capability. Registry membership controls visibility and dispatch; filesystem,
network, managed-root, credential, and sandbox checks remain independent hard
enforcement.

Each Agent JSONL is canonical model history. A validated model FunctionCall is
synced before dispatch, and its FunctionCallOutput is synced before another
sample. SQLite Steps and events are projections. After a crash, a canonical
call without output receives one stable **"aborted"** output and is never
automatically replayed; the same Agent continues from persisted context and
observes durable reality.

Python host effects use a separate deterministic journal. Replaying immutable
program source returns completed results and suspends again at durable human or
deadline waits. Model tool calls never use that replay contract.

## Codex relationship

PaperMachine adapts selected Codex implementation patterns:

- Responses request construction, streaming, and WebSocket/SSE fallback;
- the sample/tool/follow-up loop and parallel tool calls;
- cancellation, retry visibility, context accounting, and compaction;
- process-group lifecycle and fail-closed sandbox execution;
- durable-write-before-projection rollout ordering;
- missing tool-output normalization to **"aborted"**;
- a conversation-first UI with execution details beneath each Turn.

PaperMachine owns its Project, Workspace, WorkflowProgram, Session, Agent,
provider, Skill, Artifact, and HTTP model. Codex is source material, not a
runtime dependency.

## Repository layout

- **crates/protocol**: canonical IDs, entities, events, and API types.
- **crates/model**: provider profiles, routing, and Responses transports.
- **crates/tools**: host ToolCatalog, per-Turn ToolRegistry, and local tools.
- **crates/execution**: process lifecycle and OS sandbox enforcement.
- **crates/agent**: sampling, tool execution, Agent inputs, and context.
- **crates/session**: ActionAttempt and Turn execution for durable Agents.
- **crates/store**: Project SQLite, Agent rollouts, managed files, and Artifacts.
- **crates/workflow**: Python validation, effect interpretation, and scheduling.
- **crates/server**: Project-scoped HTTP/SSE API and static web serving.
- **apps/web**: Project overview, unified Session workbench, and program editor.
- **python/papermachine**: public DSL and isolated effect client.
- **workflows/builtin**: reviewed WorkflowPrograms shipped with PaperMachine.

## Quick start

Rust is pinned by **rust-toolchain.toml**; Node.js and pnpm are required.

~~~sh
pnpm install
pnpm --dir apps/web build
pnpm server:demo
~~~

Open http://127.0.0.1:4310. Demo mode exercises the runtime and UI without
substantive research. The loopback-only server rejects non-loopback Host
headers.

Real models use **<data_dir>/config.toml** or **--config**. Credentials are
resolved only from the environment variable named by each provider:

~~~sh
AEROIDES_API_KEY=... DEEPSEEK_API_KEY=... pnpm server:dev
~~~

A minimal multi-provider configuration looks like:

~~~toml
default_model = "glm-5-2"

[providers.aeroides]
kind = "open_ai_responses"
base_url = "https://private.aeroides.dev/v1"
api_key_env = "AEROIDES_API_KEY"
responses_websockets = false
prompt_cache_mode = "implicit"

[providers.deepseek]
kind = "open_ai_responses"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
optional = true
responses_websockets = false
prompt_cache_mode = "implicit"

[models.glm-5-2]
provider = "aeroides"
model = "glm-5.2"
context_window = 1048576
capabilities = []

[models.deepseek-flash]
provider = "deepseek"
model = "deepseek-v4-flash"
context_window = 1000000
capabilities = ["hosted_web_search"]
~~~

Session **default_model** and **Agent.model** name model profile IDs, not
upstream model strings. Provider transport, timeouts, caching, reasoning
defaults, and capabilities remain provider/model configuration. A capability
such as **hosted_web_search** should be declared only when the concrete
endpoint returns auditable Responses tool items.

## Verification

~~~sh
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
PYTHONPATH=python python3 -m unittest discover -s python/tests -p 'test_*.py'
pnpm --dir apps/web test
pnpm --dir apps/web build
git diff --check
~~~

Native Windows is outside the current release test scope.

See [architecture](docs/architecture.md),
[runtime kernel](docs/runtime-kernel.md),
[Project and Workspace](docs/project-workspace.md),
[prompt and model snapshots](docs/prompt-model.md),
[Workflow ABI](docs/workflow-abi.md),
[Workflow semantics](docs/workflow-language-semantics.md), and
[security](docs/security.md).
