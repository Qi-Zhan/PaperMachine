# Workflow language semantics

This document describes the current clean-break Python DSL.

## Runtime model

~~~text
WorkflowProgram definition
  -> Session execution
       -> one or more Agents
            -> ActionInvocation
                 -> ActionAttempt
                      -> Turn
~~~

Project owns WorkflowProgram definitions and Session executions. A Session is
one immutable program snapshot plus its input, configuration, status, effect
journal, Agents, events, human requests, Agent inputs, usage, output, and
Artifacts. Agent is the public name for a model identity inside that Session.
Session directly owns its Agents.

Every Turn is created by an ActionAttempt. Interactive chat uses the same rule:
the **interactive-agent** program asks for a durable HumanRequest and passes
its verified answer to an Action. Provenance lives on the trigger,
HumanRequest, and ActionInvocation rather than a Turn-origin enum.

## Program and launch

Source contains exactly one async entrypoint decorated with **@workflow**. Its
literal manifest defines slug, name, description, request_mode, and
params_schema. Validation also records Agent classes and static Action tool
declarations. A source with error diagnostics cannot launch.

Launching freezes:

- source, manifest, source SHA-256, and Python runtime ABI SHA-256;
- one concrete `request` when `request_mode="required"`;
- validated `params`, optional Session `instructions`, and launch provenance;
- the selected model profile, skills, access ceiling, and Agent overrides.

The runner exposes `ctx.session_id`, `ctx.request`, `ctx.params`, `ctx.instructions`, and
`ctx.trigger`. Workflow code must pass the data an Action needs; the runtime
never silently promotes request or Project data into system instructions.

**request_mode="required"** requires a concrete launch task.
**request_mode="none"** starts without one; interactive programs can obtain
later messages through **ask_human**.

## Public DSL

~~~python
Agent
@action(...)
@workflow(...)
await together(...)
await ask_human(...)
await wait(seconds=... | minutes=..., name=...)
await ctx.project.changes(...)
await publish_artifact(...)
await publish_project_home(action=...)
~~~

Use ordinary Python **if**, **for**, and **while** for control flow. A periodic
Session is a loop containing a durable wait.

Constructing Agent creates only a local descriptor. Its first remote operation
creates one durable Agent row under the current Session. The Agent keeps its
class, name, role, system prompt, model, access, skills, and rollout for the
rest of that Session.

## Action and Turn

An **@action** method is a declaration. Its prompt/docstring, bound arguments,
model options, return type, and tool list describe one model Turn; the Python
method body is not model logic.

Awaiting an Action runs one sample/tool/follow-up loop:

1. create ActionInvocation, ActionAttempt, and immutable Turn;
2. sample the Agent model;
3. execute only model calls present in the Turn ToolRegistry;
4. checkpoint outputs and sample again when needed;
5. finish on a terminal assistant result or runtime control.

An interrupt ends the current attempt and may let program code start a new
attempt for the same invocation. A retry is never represented as a second
logical ActionInvocation.

Typed dict, list, bool, int, and float returns request JSON parsing. With
**finalize="always"**, the normal work Turn remains unconstrained and a second
no-tool, no-search Turn produces the typed result only when the work Turn did
not already return a valid value. JSON repair and
**finalize="after_search"** also use no-tool, no-search model work; none receive
a hidden Registry.

**ask_human** returns a HumanMessage carrying HumanRequest provenance. Only an
Action parameter typed as HumanMessage may turn that verified string answer
into direct user input. Rust verifies Session, Agent, request status, and exact
text.

## Tools and access

Bare **@action** uses collaboration tools plus native tools allowed by access.
**@action(tools=[])** means an empty local Registry; a non-empty static list
selects an exact subset. The host rejects unknown or duplicate names.

- Native tools are `exec_command`, `write_stdin`, and `apply_patch`; access
  filters them.
- Collaboration tools are `list_agents`, `send_message`, `wait_agent`,
  `spawn_agent`, and `interrupt_agent`; child Agents do not receive spawn.
- Hosted web search is separate and requires `search_context_size` plus provider
  capability, not a local access preset.

Tool membership decides visibility and dispatch. Filesystem, command, network,
managed-root, and credential rules remain independent enforcement.

Session access is the hard ceiling. Per-Agent overrides cannot exceed it.
Downgrades apply between Turns; an upgrade within the ceiling requires a typed
human grant. Each existing Turn retains its own authorization snapshot.

## Concurrency

**await together(a(), b(), ...)** is ordinary asyncio gathering and preserves
argument order. Different Agents in one Session may run concurrently. Two
active Actions on the same Agent are rejected because that Agent owns one
canonical rollout and one active Turn.

This is the only required serialization rule; a Session is not globally
single-threaded.

Agent-created tasks use the same durable ActionRunner and per-Agent FIFO as
Workflow Actions. Queue-only messages enter AgentInput without starting a Turn.
`wait_agent` observes Action state and persists no second wait primitive. A
spawn creates a same-Session child plus its first Action atomically; the child
inherits identity configuration and same-or-lower access, cannot spawn again,
and is joined during Session Closing.

## Human input, waits, and Agent input

**ask_human** and **wait** are replayable suspension effects. Wait stores one
journal record, whose started_at plus interval defines its deadline. When all
live futures are at replayable waits, Rust terminates the idle Python process
and releases its permit. An answer or due deadline makes the same Session
runnable; immutable source replay reaches the stored effect result.

AgentInput transitions **pending -> claimed -> applied** and records a Human or
Agent source:

- message and guide enter canonical context before the next sample;
- finish forces the next sample to return without local tools;
- interrupt ends the active attempt;
- pause stops at checkpoints; resume continues; cancel terminates the Session.

A claim becomes applied only in the checkpoint or terminal transaction that
consumes it. A crash before checkpoint lets the same Turn reclaim the input.

## Project APIs

`ctx.project.changes()` returns an opaque cursor and bounded current snapshots
of changed Project, Session, Agent, Turn, Artifact, and Project Home entities.
Passing the cursor back as `after_cursor` returns later committed changes.
Pages are derived from the change log, deduplicate entities, emit tombstones,
chunk large text Artifacts, expose binary metadata only, and filter the calling
Session. `exclude_current_program=True` also skips historical runs of the
caller's WorkflowProgram before snapshots are built. `publish_artifact` writes
deterministic Project-managed content.

Project Home is also Project-managed. A normal Action returns a complete safe
standalone HTML document and passes that exact awaited `_ActionCall` to
`publish_project_home`. Publication verifies Action provenance, validates the
HTML, and atomically updates the canonical page. No Workflow slug or special
Summary Agent is trusted by the kernel.

## Persistence and recovery

Session host effects use deterministic paths and request hashes. Completed
effects replay their stored results. Reusing a path with different input fails
closed. Pure Python between effects may execute again after restart, so effect
order and payloads must be deterministic for the same source and inputs.

Each Agent JSONL is canonical model history:

~~~text
TurnCreated
ContextCheckpoint
TurnUpdated
~~~

A model FunctionCall is synced before dispatch; FunctionCallOutput is synced
before a later sample. On recovery, a call without output gets one stable
**"aborted"** output and is never dispatched again. The same Agent observes
durable reality and decides whether to make a new call. Host-effect replay and
model-tool recovery are deliberately separate.

## Status and completion

Session status is one of **created**, **running**, **waiting_for_input**,
**waiting_for_deadline**, **paused**, **completed**, **failed**, or
**cancelled**. Archival is separate metadata, not another execution status.

The entrypoint return is submitted through the completion effect. The scheduler
commits completed only after the Python process exits successfully and final
usage is recorded. Uncaught Python, model, tool, protocol, or sandbox errors
fail the Session. Archiving cancels an active Session and preserves its history.
