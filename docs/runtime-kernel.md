# Runtime kernel contract

Status: current clean-break contract, 2026-08-10.

PaperMachine opens only managed schema 24, Agent rollout version 1, and the
current Workflow ABI. It does not migrate older managed state or Workflow
source. Workspace files remain outside the clean break.

## One runtime lifecycle

~~~text
WorkflowProgram                    immutable definition
  -> Session                       one durable execution
       -> Agent                    model identity + canonical rollout
            -> ActionInvocation
                 -> ActionAttempt
                      -> Turn
~~~

The Session owns source snapshot, inputs, configuration, status, output,
effects, Agents, Actions, human requests, Agent inputs, events, usage, and
Artifacts. There is no parallel runtime Workflow record. An Agent has no
second Session; several Agents may belong to one Session.

Every Turn is an ActionAttempt. Interactive input is an answered HumanRequest
passed to an Action, not another Turn origin. Provenance remains on the Session
trigger, HumanRequest, and ActionInvocation.

## Immutable Turn

Before Turn insertion, the host resolves and atomically stores:

| Snapshot | Content |
| --- | --- |
| ModelRouteSnapshot | provider route, capabilities, reasoning, context limit, non-secret config hash |
| TurnEnvironmentSnapshot | Workspace attachment revision and materialized access |
| ToolSetSnapshot | exact sorted local definitions and SHA-256 |
| PromptSnapshot | exact resolved instruction layers and SHA-256 |

One Session may have concurrent Turns on different Agents. One Agent admits
only one active Turn, which gives its rollout a single writer.

The ToolCatalog is trusted host configuration. Bare Actions receive
collaboration tools plus native tools allowed by Agent access; `tools=[]`
means none, and a non-empty declaration selects an exact subset. Child Agents
do not receive `spawn_agent`. Hosted search is a separate model capability.
Registry membership never bypasses path, network, credential, managed-root, or
sandbox checks.

## Filesystem policy

| Access | Default local tools | Host read | Write | Child network |
| --- | --- | --- | --- | --- |
| model_only | collaboration | none | none | denied |
| read_only | collaboration + command/process | ordinary host files | none | denied |
| workspace | collaboration + command/process/patch | ordinary host files | Workspace | denied |
| full_access | collaboration + command/process/patch | ordinary host files | host except managed state | allowed |

Relative paths resolve against Workspace. Managed roots are always denied.
Below full_access, common credential files and directories are denied as well.
`exec_command`, `write_stdin`, and `apply_patch` consume the same materialized
authorization. Hosted web search is independent of these presets and appears
only when the Action requests it and the provider supports it. Native Windows
is not a current validation target.

## Canonical Agent rollout

Each Agent owns one JSONL with three item kinds:

~~~text
TurnCreated        Turn and required ActionAttempt attachment
ContextCheckpoint context mutation, usage, cursors, terminal candidate
TurnUpdated        Turn boundary and acknowledged Agent inputs
~~~

The writer assigns monotonic sequence numbers and syncs JSONL before advancing
SQLite projection. AgentSteps, Session events, and streaming deltas are
projection or live state.

A validated FunctionCall is checkpointed before dispatch. Its
FunctionCallOutput is checkpointed before Step completion or a later model
sample. Recovery scans canonical pairs:

- call plus output repairs a missing projection;
- call without output receives one stable JSON string **"aborted"**;
- a running projected Tool Step without canonical output becomes aborted;
- no old call is sent to an executor.

The same Agent resumes the same Turn and observes durable reality. There is no
aggregate model-sample transaction, model-tool effect ID, disposition, or
reconciliation API.

## Workflow effects and scheduling

Python host effects use a separate Session journal. Immutable source restarts
at its entrypoint. A deterministic logical path and request hash replay a
completed effect; a reused path with different input fails closed. Started
effects may converge only through their explicit idempotent host contract.
This journal never replays model tool calls.

Human and deadline waits are journaled effects. When all live futures are at
replayable waits, Rust terminates the idle Python process and releases its
permit. An answer or due deadline makes the Session runnable again. Terminal
in-memory scheduler handles are removed; late waiters read persistent Session
state.

One ActionRunner consumes Workflow Actions and collaboration-created
`agent_task` Actions. It preserves one FIFO per Agent and permits concurrency
across Agents. Sampling alone holds a model permit, so `wait_agent` cannot
deadlock its child. `waiting_for_input` and `waiting_for_deadline` describe the
Workflow process and do not freeze Agent tasks; explicit pause does.

Rust/Python frames are capped at 16 MiB, with at most 64 in-flight effects and
bounded response channels. Reader, writer, or handler failure ends the Session.

## Store and Agent input

Each loaded Project owns one bounded StoreHandle backed by one blocking thread.
SQLite, managed filesystem work, hashing, and directory scans do not block
Tokio workers.

Session transitions, Action start/finish, HumanRequest resolution, usage, and
terminal cleanup use typed immediate transactions with allowed-from checks.
Human answers use open-status CAS.

AgentInput transitions **pending -> claimed -> applied**, records whether the
source is Human or Agent, and may target one Action. It becomes applied only in
the canonical checkpoint or terminal transaction that consumes it; a
pre-checkpoint crash lets the same Turn reclaim it.

## Project lifecycle

The Project catalog is a resilient scan of independent managed directories.
Ordinary work uses the loaded Project handle. Relocate/remove take the Project
map write lock, recheck active Sessions, stop and join runtime and Store, then
mutate the catalog. Removal moves only managed state to trash; Workspace is
never a deletion target.

Managed files use capability-rooted nofollow reads, bounded traversal, atomic
replace, directory fsync, and root-confined deletion. Artifact bytes are synced
before metadata commit; startup removes uncommitted orphans and fails closed on
missing or modified durable Artifacts.

## Release gates

- Rust format, workspace tests, and Clippy;
- Python DSL and built-in tests;
- Web tests and production build;
- permission agreement between direct tools and process sandbox;
- per-Agent concurrency and cross-Session ownership tests;
- route, ToolSet, prompt, AgentInput, collaboration, and rollout crash-boundary tests;
- Project creation, interaction, WorkflowProgram launch, and Summary dogfood.
