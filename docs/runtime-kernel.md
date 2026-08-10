# Runtime kernel contract

Status: current clean-break contract, 2026-08-10.

PaperMachine opens only managed schema 21, Agent rollout version 1, and the
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
effects, Agents, Actions, human requests, controls, events, usage, and
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

The ToolCatalog is trusted host configuration. An Action's static tool
declaration supplies candidates; Agent access filters Workspace tools, while
Project tools require explicit declaration. Hosted search is a separate model
capability. Registry membership never bypasses path, network, credential,
managed-root, or sandbox checks.

## Filesystem policy

| Access | Host read | Write | Child network | Hosted research |
| --- | --- | --- | --- | --- |
| model_only | none | none | none | none |
| read_only | ordinary host files | none | none | none |
| workspace | ordinary host files | Workspace | denied | none |
| research | ordinary host files | Workspace | denied | controlled |
| full_access | host except managed state | host except managed state | allowed | controlled |

Relative paths resolve against Workspace. Managed roots are always denied.
Below full_access, common credential files and directories are denied as well.
Direct file tools and command sandboxes materialize the same authorization.
Native Windows is not a current validation target.

## Canonical Agent rollout

Each Agent owns one JSONL with three item kinds:

~~~text
TurnCreated        Turn and required ActionAttempt attachment
ContextCheckpoint context mutation, usage, cursors, terminal candidate
TurnUpdated        Turn boundary and acknowledged controls
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

Rust/Python frames are capped at 16 MiB, with at most 64 in-flight effects and
bounded response channels. Reader, writer, or handler failure ends the Session.

## Store and control

Each loaded Project owns one bounded StoreHandle backed by one blocking thread.
SQLite, managed filesystem work, hashing, and directory scans do not block
Tokio workers.

Session transitions, Action start/finish, HumanRequest resolution, usage, and
terminal cleanup use typed immediate transactions with allowed-from checks.
Human answers use open-status CAS.

Controls transition **pending -> claimed -> applied**. Claim binds a target
Turn. A control becomes applied only in the canonical checkpoint or terminal
transaction that consumes it; a pre-checkpoint crash lets the same Turn reclaim
it.

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
- route, ToolSet, prompt, control, and rollout crash-boundary tests;
- Project creation, interaction, WorkflowProgram launch, and Summary dogfood.
