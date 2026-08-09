# Workflow language semantics

This document describes the executable Python DSL in the current clean-break
runtime. It intentionally documents only implemented semantics.

## Domain model

> A Project is a research world persistently managed by PaperMachine; a
> Workspace is the user filesystem an Agent is authorized to operate;
> structured runtime APIs connect them, and they never share storage or a
> security boundary.

- A `Project` owns Sessions, Workflow runs, prompts, Skills, Artifacts, and
  journals. All of that state is under PaperMachine's managed data root.
- A `Workspace` is one user-owned absolute directory attached to a Project. It
  is an Agent cwd and write boundary, never Project storage.
- A `Session` is a durable multi-turn model conversation owned directly by a
  Project.
- A `Workflow` is one run of an immutable Python source snapshot.
- An `Agent` instance is a Workflow participant backed by one ordinary
  Project-owned Session. Its lifetime is the Workflow's lifetime.
- An `ActionInvocation` is one logical Action call. Its `ActionAttempt` owns
  exactly one Turn; interruption may create another Attempt for the same call.
- A `Turn` is one user request or Workflow Action model boundary. PaperMachine
  models normal chat through the built-in `interactive-agent` Workflow, so the
  two cases use the same runtime path.
- A `WorkflowEffect` journals one exact host operation at a deterministic
  logical path.

Every ID referenced by a run must belong to that run's Project. HTTP routes are
Project-scoped; there is no global entity lookup or ownership index.

## Program and launch contract

A source file contains exactly one async `@workflow(...)` entrypoint. Its
literal manifest supplies `slug`, `name`, `description`, `request_mode`, and
`params_schema`. Validation also records Agent classes and each Action's static
tool declaration. A source is runnable only when validation has no errors.

Launching a Workflow freezes:

- source, manifest, source SHA-256, and Python runtime ABI SHA-256;
- one concrete `request` when `request_mode="required"`;
- validated `params`, optional run `instructions`, and launch provenance;
- the selected model profile, skills, access ceiling, and Agent overrides;
- either fresh context or one bounded Project snapshot.

The runner exposes these separately as `ctx.request`, `ctx.params`,
`ctx.instructions`, `ctx.trigger`, and `ctx.context`. Workflow code must pass
the data an Action needs; the runtime never silently promotes request or
Project data into system instructions.

`request_mode="none"` is for persistent interaction that obtains messages with
`ask_human`. New Session uses this path; there is no independent submit-to-
Session kernel.

## Public DSL

The complete public surface is deliberately small:

```python
Agent
@action(...)
@workflow(...)
await together(...)
await ask_human(...)
await wait(seconds=... | minutes=..., name=...)
await ctx.project.snapshot(...)
await publish_artifact(...)
await publish_project_home(action=...)
```

Ordinary Python `if`, `for`, and `while` provide all other control flow.
Repetition is a normal loop containing a durable `wait`.

Constructing `Agent(...)` is local. Its first remote operation creates one
participant Session. `await agent.set_access(profile)` also materializes the
Session first, so an upgrade cannot be hidden as a constructor mutation.
Participants are immutable membership records with no separate lifecycle state.

## Actions and Turns

An `@action` method is declarative: its prompt/docstring, bound arguments, model
options, and tool list describe a model Turn. The Python method body is not
executed as Agent logic.

Awaiting an Action creates an invocation and runs the same sample/tool/follow-up
loop used everywhere else:

1. build one immutable Turn;
2. sample the model;
3. execute model-requested local tools that are in the Turn Registry;
4. append tool outputs and sample again;
5. finish on a terminal assistant message or explicit runtime control.

Typed returns (`dict`, `list`, `bool`, `int`, or `float`) request JSON parsing.
JSON repair and `finalize="after_search"` use a separate no-tool Action Turn;
they do not gain hidden tools.

A string returned by `ask_human` carries its `HumanRequestId` as
`HumanMessage`. Passing it to a parameter annotated `HumanMessage` is the only
Workflow path that creates a user-origin Turn. Rust verifies the request,
answer, Session, and exact text before accepting it.

Every Turn freezes four independent snapshots:

| Snapshot | Meaning |
|---|---|
| `ModelRouteSnapshot` | provider, upstream model, capabilities, context window, reasoning effort, and non-secret configuration hash |
| `TurnEnvironmentSnapshot` | Workspace revision and materialized authorization |
| `ToolSetSnapshot` | exact sorted local tool definitions and SHA-256 |
| `PromptSnapshot` | ordered resolved prompt layers and SHA-256 |

Recovery fails closed when any snapshot cannot be reproduced.

## Tools and access

`@action(tools=[...])` declares the complete local tool request. Bare
`@action` means `tools=[]`. The host rejects unknown names and builds an exact,
immutable Registry for that Turn.

- Workspace tools are filtered by the Agent's materialized access profile.
- Project tools are admitted only when that Workflow Action names them.
- Hosted web search is separate: provider capability, access, and
  `search_context_size` jointly control it.
- A normal interactive Session receives all Workspace tools allowed by access,
  but never Project tools.

Access and tool membership are separate checks. The Registry controls what the
model can see and call; filesystem, command, network, managed-root, and
credential rules remain hard enforcement inside the tools and sandbox.

`model_only`, `read_only`, `workspace`, `research`, and `full_access` form an
ordered ceiling. A Workflow launch fixes the run ceiling. Per-Agent overrides
cannot exceed it. Downgrades apply between Turns; upgrades within the ceiling
open a typed HumanRequest. Every Turn retains the access snapshot it began
with.

## Concurrency, human input, and durable wait

`await together(a(), b(), ...)` uses `asyncio.gather` and returns results in
argument order. Two direct calls on the same Agent are rejected because one
Session admits only one active Turn. Different Agent Sessions may run in
parallel subject to the server-wide permit limit.

`ask_human` and `wait` are replayable suspension effects. `wait` stores only
its journal entry; its deadline is `WorkflowEffect.started_at + interval`.
The effect journal is the only durable state for that wait.

When all live effect futures are at replayable waits, Rust terminates the idle
Python process and releases its execution permit. A validated human answer or
due wait restarts the immutable source. Completed effects return their stored
results, so execution reaches the suspended point without repeating completed
domain mutations.

Control messages use `pending -> claimed -> applied`:

- `guide` enters canonical context before the next sample;
- `finish` forces the next sample to be a no-tool final answer;
- `interrupt` ends the current Attempt and lets the Workflow continue it with
  a new Attempt;
- pause waits at checkpoints; resume continues; cancel terminates the run.

A claim becomes applied only in the canonical checkpoint or terminal
transaction that consumes it. A pre-checkpoint crash lets the same Turn reclaim
the message.

## Project APIs

`ctx.project.snapshot()` returns bounded Project-managed state, not Workspace
files. Passing a prior `cursor` as `after_cursor` returns a committed delta.
`publish_artifact` writes deterministic Project-managed content.

Project Home is also Project-managed. A normal Action explicitly declares
`read_project_home`, `patch_project_home`, and `preview_project_home`, may use
them repeatedly, and then passes that exact awaited `_ActionCall` to
`publish_project_home`. Publication verifies Action provenance and ToolSet
membership and uses the draft revision as a CAS base. No Workflow slug or
special Summary Agent is trusted by the kernel.

## Persistence and recovery

Python host effects and model tool calls intentionally have different recovery
contracts.

Workflow effects use deterministic logical paths plus a request hash. A
completed effect replays its stored result. A path reused with another request
fails closed. Started host effects redispatch only according to their
idempotent domain contract.

Each Session JSONL is canonical model history and contains only:

```text
TurnCreated
ContextCheckpoint
TurnUpdated
```

SQLite Steps and Session events are query/UI projections, not canonical
rollout items. Streaming deltas are transient.

A validated model `FunctionCall` must enter a `ContextCheckpoint` before tool
dispatch. Its `FunctionCallOutput` must be checkpointed before Step completion
or another sample. On recovery:

- a call/output pair repairs a missing Tool Step projection;
- a call without output receives one stable JSON string `"aborted"`;
- the old call is never dispatched again;
- the same Agent continues and observes durable reality before deciding
  whether to issue a new call.

There is no aggregate `ModelSampleCommitted`, effect-disposition enum, or model
tool reconciliation API.

## Statuses and completion

Workflow statuses are `created`, `running`, `waiting_for_user`,
`waiting_for_deadline`, `paused`, `completed`, `failed`, and `cancelled`.
`waiting_for_deadline` means a durable `wait` effect is not yet due.

An entrypoint return is submitted through the `complete` effect. The scheduler
commits `completed` only after the Python process exits successfully and final
usage is recorded. Uncaught Python, model, tool, protocol, or sandbox errors
fail the run. Closing a Session archives it and cancels active Workflows that
own it, without deleting history.

Pure Python between effects may execute again after restart. Workflow authors
must therefore keep effect ordering and payloads deterministic for the same
source snapshot and inputs.
