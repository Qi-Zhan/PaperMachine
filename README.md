# PaperMachine

PaperMachine is a local-first research workbench built around one durable model:

~~~text
Project
  Session                         one WorkflowProgram execution
    Agents                        independent model identities and rollouts
      ActionInvocation
        ActionAttempt -> Turn -> model/tool Steps
    effects, events, human requests, Agent inputs, artifacts
~~~

A WorkflowProgram is immutable `workflow.pm` source plus its compiled manifest.
Starting one creates a Session that owns its complete lifecycle. The Session may
create one or many Agents; each Agent keeps its own prompt, model, access, skills,
and canonical rollout. Session is the only workflow lifecycle.

> A Project is durable state managed by PaperMachine. A Workspace is the user
> filesystem an Agent may access. Structured runtime APIs connect them, but they
> never share storage or a security boundary.

The built-in Goal, interactive, evidence, discovery, Project summary, and
single-agent research programs use exactly the same compiler, interpreter,
ActionRunner, journal, and APIs as user-authored programs. There are no slug
special cases.

## Storage

`resource_root` contains read-only web assets and built-in Workflows. `data_dir`
defaults to the platform PaperMachine application-data directory:

| Platform | Default |
| --- | --- |
| macOS | `~/Library/Application Support/PaperMachine` |
| Linux | `$XDG_DATA_HOME/papermachine`, or `~/.local/share/papermachine` |
| Windows | `%LOCALAPPDATA%\PaperMachine` |

~~~text
<data_dir>/
  config.toml
  projects/<project-id>/
    state/project.db
    rollouts/<agent-id>.jsonl
    artifacts/
    prompts/
    workflows/<slug>/workflow.pm
    skills/
    runtime/sandboxes/          disposable Agent command scratch
  staging/
  trash/
~~~

Each Project attaches one canonical absolute Workspace, selected by the user or
created under `~/Documents/PaperMachine/`. PaperMachine never writes databases,
prompts, rollouts, skills, or hidden managed metadata into it. Removing a Project
removes only managed state and preserves the Workspace.

## Workflow Language v1

Workflow Language v1 is a Rust-like, dynamically typed, single-file language
implemented in Rust. It has `let`/`var`, immutable collection values, functions,
strict boolean conditions, matching, finite `for`, durable `while`/`loop`,
`await`, and deterministic fixed or keyed parallel branches. It has no imports,
recursion, closures, higher-order functions, reflection, arbitrary I/O, random
numbers, clocks, or exception recovery.

Only trust boundaries carry schemas: launch params, structured Action results,
and HumanRequest responses. Normal Workflow values are dynamic. The compiler
checks declarations, scope, Action/function arity, the non-recursive call graph,
tool membership, access presets, loop durability, parallel keys, schemas, and
the 128 KiB source limit. Each pure interval has 1,000,000 public IR steps;
durable effects reset the budget.

A Session snapshot freezes source, source SHA-256, language version, manifest,
and canonical IR SHA-256. Recovery recompiles the frozen source and requires all
of them to match before an effect runs. Deterministic effect paths plus request
hashes replay completed effects and fail closed when the request changes.

See [Workflow semantics](docs/workflow-language-semantics.md) and the
[durable Workflow contract](docs/workflow-abi.md).

## Runtime boundaries

Before every model Turn the host freezes four independent snapshots:

- `ModelRouteSnapshot`: provider/model route and non-secret config hash;
- `TurnEnvironmentSnapshot`: Workspace revision and effective authorization;
- `ToolSetSnapshot`: exact sorted local tool definitions and hash;
- `PromptSnapshot`: rendered runtime, Project, Session, Agent, Skill, Action,
  and retry-guidance instructions.

ToolRegistry membership, filesystem/process authorization, and hosted provider
tools are separate authority surfaces. Local registry membership controls model
visibility and dispatch. Workspace, credential, network, managed-root, and OS
sandbox checks remain independent enforcement.

Every model Turn belongs to an ActionAttempt. Human messages enter through a
durable HumanRequest and retain exact provenance. Project Home publication
accepts only the exact successfully awaited Action result. Agents collaborate
through the same ActionRunner and durable AgentInput inbox used by Workflow
Actions; there is no second scheduler.

## Repository layout

- `crates/protocol`: canonical IDs, entities, events, and API types.
- `crates/model`: provider profiles, routing, and Responses/Chat Completions transports.
- `crates/tools`: ToolCatalog, per-Turn ToolRegistry, and local tools.
- `crates/execution`: process lifecycle and OS sandbox enforcement.
- `crates/agent`: sampling, tool execution, Agent inputs, and context.
- `crates/session`: ActionAttempt and Turn execution.
- `crates/store`: Project SQLite, rollouts, managed files, and Artifacts.
- `crates/workflow`: Workflow compiler, interpreter, effects, and scheduler.
- `crates/server`: Project-scoped HTTP/SSE API and static serving.
- `apps/web`: Project overview, Session workbench, and Workflow editor.
- `workflows/builtin`: reviewed `workflow.pm` programs.

## Quick start

Rust is pinned by `rust-toolchain.toml`; Node.js and pnpm are required.

~~~sh
pnpm install
pnpm --dir apps/web build
pnpm server:demo
~~~

`--dev` uses `<platform-data-dir>/dev` and, unless overridden, reads
`<resource-root>/papermachine.toml`. Explicit `--data-dir` and `--config` always
win. Open <http://127.0.0.1:4310>; non-loopback Host headers are rejected.

Real providers resolve credentials only from the configured environment
variable. Session `default_model` and Agent `model` values are model-profile IDs,
not upstream model strings. A provider owns credentials and a base URL; each
model profile explicitly selects `open_ai_responses` or
`open_ai_chat_completions`. Optional providers are listed only when their
credential is present and are never contacted merely by loading configuration.

## Verification

~~~sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
pnpm --dir apps/web test
pnpm --dir apps/web build
git diff --check
~~~

See [architecture](docs/architecture.md),
[runtime kernel](docs/runtime-kernel.md),
[Project and Workspace](docs/project-workspace.md),
[prompt and model snapshots](docs/prompt-model.md), and
[security](docs/security.md).
