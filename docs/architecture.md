# Architecture

PaperMachine is a local-first research runtime built around one boundary:

> Project is a research world persistently managed by PaperMachine; Workspace
> is the user filesystem an Agent is authorized to operate; structured runtime
> APIs connect them, and they never share storage or a security boundary.

## Domain graph

```text
Project
  WorkspaceAttachment              user-owned filesystem authority
  WorkflowPrograms                 immutable executable definitions
  Sessions                         one durable WorkflowProgram execution
    Agents                         independent model identities and rollouts
    ActionInvocations
      ActionAttempts -> Turns
        AgentSteps                 disposable query/UI projection
    HumanRequests / AgentInputs
    Effect journal
    Artifacts
  Prompts / Skills / Workflow source / Project Home
```

Every entity route starts with `/api/projects/{project_id}`. The selected
Project store resolves the rest. There is no global entity scan, ownership
table, ownership event bus, or fallback lookup.

WorkflowProgram is a definition; Session is its only runtime instance. Every
Turn belongs to an ActionAttempt owned by one Session Agent. A normal user
message is modeled by the ordinary `interactive-agent` program, so user chat
and custom Actions share one execution and recovery path. `goal` and
`project-summary` are ordinary programs too; the Rust kernel has no
slug-specific branch.

## Process and crate boundaries

```text
Web UI
  -> server        Project-scoped HTTP/SSE, catalog, lifecycle
      -> workflow  Python validation, effects, scheduler, replay
          -> session  ActionAttempt and Turn execution for durable Agents
              -> agent  sample/tool/follow-up loop
                  -> model      provider transport and route snapshots
                  -> tools      host catalog and exact Turn Registry
                  -> execution  sandboxed processes and filesystem policy
      -> store     Project SQLite, canonical Agent JSONL, managed files
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
    rollouts/<agent-id>.jsonl
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
| `PromptSnapshot` | ordered runtime, Project, Session, Agent, Skill, Action, and retry guidance |

The Turn and its required ActionAttempt attachment enter the canonical Agent
rollout before execution. Access, route, ToolSet, or prompt drift fails closed
on resume.

The ToolCatalog is trusted host configuration. Bare Actions receive all
collaboration tools plus native tools allowed by access; `tools=[]` means an
empty Registry, and a non-empty declaration selects an exact subset. Child
Agents never receive `spawn_agent`. Hosted web search is a provider capability
outside the local Registry. Registry membership controls visibility and
dispatch, while file/network/sandbox rules remain independent enforcement.

## Agent loop and canonical history

The Agent runtime follows the Codex-shaped loop:

```text
checkpoint Agent inputs
  -> sample model
  -> validate stable response items
  -> append ContextCheckpoint and sync JSONL
  -> project Model/Tool UI state
  -> dispatch declared local calls
  -> append each FunctionCallOutput and sync
  -> project completion
  -> sample again or finish Turn
```

Each Agent JSONL is the canonical model history. Its schema has only three
item kinds:

```text
TurnCreated        Turn + required ActionAttempt
ContextCheckpoint append/replace context, usage, cursors, terminal candidate
TurnUpdated        Turn boundary and acknowledged Agent inputs
```

SQLite Turns are the query projection. AgentSteps and Session events are also
SQLite/UI projections and never enter canonical JSONL. Streaming deltas and
`ModelStepStarted` exist only on the live event stream.

The JSONL writer serializes each Agent, assigns monotonic sequence numbers,
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

Session host effects form a separate journal. Python restarts at the immutable
entrypoint. Deterministic logical paths and request hashes replay completed
results and make idempotent started host effects converge. This journal never
replays model tool calls.

## Session scheduler

The public DSL intentionally exposes only Agent/Action, normal Python control
flow, `together`, `ask_human`, `wait`, Project change cursors, and Artifact/Home
publication.

`wait` is one durable Workflow effect. Its wake time is derived from the
effect's persisted `started_at` plus the requested interval. There is no Timer
table, ID, fire count, callback state, or periodic policy. A periodic Session
is simply a Python loop that performs work and awaits `wait`.

If every pending Python future is a replayable human/deadline wait, the runner
reports quiescence. Rust terminates the idle process and releases the global run
permit. An answer or due deadline marks the run runnable and source replay
reaches the waiting effect through stored results.

The scheduler removes terminal in-memory handles. A late waiter reads the
persistent terminal Session. Rust/Python frames are limited to 16 MiB, at most
64 effects may be in flight, response channels are bounded, and any protocol
reader/writer/handler failure ends the run.

The ActionRunner is the only path from a durable ActionInvocation to a Turn.
Workflow Actions and collaboration-created `agent_task` Actions share its
per-Agent FIFO. Different Agents may run concurrently; one Agent has one active
Turn. Model permits cover sampling only, so a parent waiting in `wait_agent`
does not block its child from sampling. Workflow waits do not freeze other
Agents in that Session; explicit pause, Closing, and terminal states do.

Collaboration tools operate on this existing lifecycle. Queue-only
`send_message` writes one durable AgentInput. `start_turn=true` and
`spawn_agent` create ordinary Actions. `wait_agent` observes Action status and
persists nothing. Interrupt records durable intent and cancels the live Action;
only descendants in the caller Session may be interrupted. Session Closing
stops admission, cancels and joins unfinished child work, then commits the
terminal status.

## Transactions and Agent input

Session transitions, Action start/finish, HumanRequest resolution, usage, and
terminal cleanup use typed `BEGIN IMMEDIATE` transactions with allowed-from
checks. Human answers use `id + open status` CAS.

AgentInput transitions `pending -> claimed -> applied` and records a Human or
Agent source. Claim binds a target Turn. Application occurs only in the
canonical checkpoint or terminal transaction that consumes the input; a crash
before checkpoint lets that same Turn reclaim it.

## Project Home

Project Home is Project-managed structured source plus immutable HTML/source
Artifacts. It is not in the Workspace. The Workflow runtime derives bounded
current entity snapshots from the Project change log and passes them to an
ordinary Summary Action with an empty ToolSet. Publication accepts the exact
awaited Action call, verifies provenance, validates the fragment, and commits
it atomically. No Summary slug or Agent class is hard-coded.

## Adaptation boundary

PaperMachine adapts selected Codex implementation patterns: Responses
streaming, the model/tool/follow-up loop, process sandboxing, prompt/context
handling, durable-write-before-projection rollout ordering, and aborted
missing-output normalization.

PaperMachine owns its Project, Workspace, WorkflowProgram, Session, Agent,
provider, prompt, Skill,
Artifact, and HTTP domain model. Codex is source material, not a runtime
dependency.
