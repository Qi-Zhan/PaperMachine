# Architecture

PaperMachine is a local-first research runtime built around one boundary:

> Project is a research world persistently managed by PaperMachine; Workspace
> is the user filesystem an Agent is authorized to operate; structured runtime
> APIs connect them, and they never share storage or a security boundary.

## Domain graph

```text
Project
  WorkspaceAttachment              user-owned filesystem authority
  Sessions                         durable model conversations
    Turns                          one ActionAttempt boundary
      AgentSteps                   disposable query/UI projection
  Workflows                        immutable source snapshot + effect journal
    Participants <-> Sessions      one Agent instance, one Session
    ActionInvocations
      ActionAttempts -> Turns
    HumanRequests / ControlMessages
    Artifacts
  Prompts / Skills / Workflow source / Project Home
```

Every entity route starts with `/api/projects/{project_id}`. The selected
Project store resolves the rest. There is no global entity scan, ownership
table, ownership event bus, or fallback lookup.

Every Turn belongs to an ActionAttempt. A normal user message is modeled by the
ordinary `interactive-agent` Workflow, so user chat and custom Workflow Actions
share one execution and recovery path. `goal` and `project-summary` are also
ordinary built-in Python programs; the Rust kernel has no slug-specific branch.

## Process and crate boundaries

```text
Web UI
  -> server        Project-scoped HTTP/SSE, catalog, lifecycle
      -> workflow  Python validation, effects, scheduler, replay
          -> session  Turn snapshots and persistent Agent Sessions
              -> agent  sample/tool/follow-up loop
                  -> model      provider transport and route snapshots
                  -> tools      host catalog and exact Turn Registry
                  -> execution  sandboxed processes and filesystem policy
      -> store     Project SQLite, canonical Session JSONL, managed files
      -> skills    Project-managed instruction resolution
      -> protocol  shared IDs and wire/domain data
```

Rust is the trusted host. Python cannot access SQLite, managed files, the
Workspace, network, environment, or subprocesses directly; it sends bounded
typed effects over newline-delimited JSON. Model tools execute through a
host-owned Registry and sandbox.

## Storage

Application resources, Project state, and Workspace files are separate:

```text
resource_root/                     read-only shipped assets and built-ins

data_dir/
  projects/<project-id>/
    state/project.db
    rollouts/<session-id>.jsonl
    artifacts/
    prompts/
    workflows/
    skills/
    workflow-runtime/              disposable Python scratch
    runtime/sandboxes/             disposable Turn scratch
  staging/
  trash/

workspace/                         arbitrary user-owned directory
```

There is no global Project database. Startup scans each Project directory
independently; one damaged entry produces a diagnostic without hiding healthy
Projects. Creation publishes a fresh staging directory atomically. Removal
stops and joins that Project runtime and Store, then moves only managed state to
trash. Relocation changes the Workspace attachment record and never moves
managed state or user files.

`ManagedFs` is capability-rooted and provides bounded nofollow reads, atomic
replacement, directory fsync, bounded traversal, and root-confined deletion.
Artifact bytes are synced before metadata commit; startup removes uncommitted
orphans and fails closed on missing or modified durable artifacts.

Each loaded Project has one `ProjectHandle` containing a bounded asynchronous
`StoreHandle` and a lazily initialized `ProjectRuntime`. The StoreHandle owns a
256-entry queue and one blocking thread, keeping SQLite, hashing, and directory
work off Tokio workers. Project map reads admit ordinary work; relocate/remove
take the map write lock, recheck active work, stop the runtime, and mutate the
catalog. Project lifecycle needs no separate state machine.

## Turn creation

Before inserting a Turn, the host resolves four immutable snapshots:

| Snapshot | Frozen content |
|---|---|
| `ModelRouteSnapshot` | profile, provider, upstream model, capabilities, context window, effective reasoning, non-secret configuration SHA-256 |
| `TurnEnvironmentSnapshot` | Workspace ID/revision/path and materialized authorization hash |
| `ToolSetSnapshot` | exact sorted local definitions and SHA-256 |
| `PromptSnapshot` | ordered runtime, Project, Workflow, Agent, Skill, and control instructions |

The Turn and its required ActionAttempt attachment enter the canonical Session
rollout before execution. Access, route, ToolSet, or prompt drift fails closed
on resume.

The ToolCatalog is trusted host configuration. For a Workflow Action, its
static `tools=[...]` declaration is the requested set. Workspace tools are
filtered by access; Project tools require explicit Action declaration. Hosted
web search is a provider capability outside the local Registry. Registry
membership controls visibility and dispatch, while file/network/sandbox rules
remain independent enforcement.

## Agent loop and canonical history

The Agent runtime follows the Codex-shaped loop:

```text
checkpoint inputs and controls
  -> sample model
  -> validate stable response items
  -> append ContextCheckpoint and sync JSONL
  -> project Model/Tool UI state
  -> dispatch declared local calls
  -> append each FunctionCallOutput and sync
  -> project completion
  -> sample again or finish Turn
```

Each Session JSONL is the canonical model history. Its schema has only three
item kinds:

```text
TurnCreated        Turn + required ActionAttempt
ContextCheckpoint append/replace context, usage, cursors, terminal candidate
TurnUpdated        Turn boundary and acknowledged controls
```

SQLite Turns are the query projection. AgentSteps and Session events are also
SQLite/UI projections and never enter canonical JSONL. Streaming deltas and
`ModelStepStarted` exist only on the live event stream.

The JSONL writer serializes each Session, assigns monotonic sequence numbers,
flushes and syncs before advancing the SQLite projection, repairs an incomplete
final line, and streams replay with `BufRead`.

## Recovery

Canonical `FunctionCall` durability is the dispatch gate. Recovery scans call
and output pairs:

- call plus output repairs missing Tool Step projection;
- call without output gets one stable JSON string `"aborted"`;
- a running Tool Step without canonical output becomes `aborted`;
- no old call is ever passed to an executor.

The same Agent resumes the same Turn with `aborted` in context and observes
durable Workspace or external state before deciding whether to issue a new
call. PaperMachine does not aggregate a model sample transaction and does not
use effect dispositions or model-tool reconciliation.

Workflow host effects form a separate journal. Python restarts at the immutable
entrypoint. Deterministic logical paths and request hashes replay completed
results and make idempotent started host effects converge. This journal never
replays model tool calls.

## Workflow scheduler

The public DSL intentionally exposes only Agent/Action, normal Python control
flow, `together`, `ask_human`, `wait`, Project snapshots, and Artifact/Home
publication.

`wait` is one durable Workflow effect. Its wake time is derived from the
effect's persisted `started_at` plus the requested interval. There is no Timer
table, ID, fire count, callback state, or periodic policy. A periodic Workflow
is simply a Python loop that performs work and awaits `wait`.

If every pending Python future is a replayable human/deadline wait, the runner
reports quiescence. Rust terminates the idle process and releases the global run
permit. An answer or due deadline marks the run runnable and source replay
reaches the waiting effect through stored results.

The scheduler removes terminal in-memory handles. A late waiter reads the
persistent terminal Workflow. Rust/Python frames are limited to 16 MiB, at most
64 effects may be in flight, response channels are bounded, and any protocol
reader/writer/handler failure ends the run.

## Transactions and control

Workflow transitions, Action start/finish, HumanRequest resolution, usage, and
terminal cleanup use typed `BEGIN IMMEDIATE` transactions with allowed-from
checks. Human answers use `id + open status` CAS.

Control messages transition `pending -> claimed -> applied`. Claim binds a
target Turn. Application occurs only in the canonical checkpoint or terminal
transaction that consumes the message; a crash before checkpoint lets that same
Turn reclaim it.

## Project Home

Project Home is Project-managed structured source plus immutable HTML/source
Artifacts. It is not in the Workspace. A Summary Action is a normal Agent
Action whose ToolSet explicitly contains the three Home tools. The Agent may
read, patch, preview, and correct repeatedly. Publication accepts the exact
awaited Action call, verifies provenance and ToolSet membership, and commits
with revision CAS. No Summary slug, Agent class, call count, or fixed tool order
is hard-coded.

## Adaptation boundary

PaperMachine adapts selected Codex implementation patterns: Responses
streaming, the model/tool/follow-up loop, process sandboxing, prompt/context
handling, durable-write-before-projection rollout ordering, and aborted
missing-output normalization.

PaperMachine owns its Project, Workspace, Workflow, provider, prompt, Skill,
Artifact, and HTTP domain model. Codex is source material, not a runtime
dependency.
