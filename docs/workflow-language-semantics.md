# Workflow language semantics

This document defines the executable semantics of the current PaperMachine
Python DSL. It describes what the runtime does, not a future graphical syntax.

## 1. Domain vocabulary

| Term | Identity | Semantics |
|---|---|---|
| `Project` | `ProjectId` | Directory-backed ownership root for Sessions, Workflows, skills, artifacts, and UI overview. |
| `Session` | `SessionId` | Durable multi-turn conversation and workspace. It has `user` or `workflow_agent` origin, never a parent Session. |
| `Turn` | `TurnId` | One user/model interaction inside a Session. |
| `AgentStep` | `StepId` | Inspectable model, tool, workflow, or system step under a Turn. |
| `WorkflowProgram` | `(project_id?, slug, sha256)` | Validated Python source plus literal manifest. A missing Project denotes a built-in. |
| `Workflow` | `WorkflowId` | One execution of an immutable workflow snapshot inside a Project, with one concrete `request`, validated `params`, optional run `instructions`, trigger provenance, and launch context. |
| `WorkflowEffect` | `(WorkflowId, logical path)` | Durable journal entry for one exact Python effect request and its replayable result or error. |
| starting Session | `started_from_session_id?` | Optional Session from which a Workflow was started. It is provenance, not ownership. |
| `Agent instance` | `AgentInstanceId` | One workflow actor backed by exactly one Project-owned Session. |
| `ActionInvocation` | `ActionInvocationId` | Logical call of one declared action on one Agent; it stores the Action `contract`, bound argument data, and optionally the HumanRequest that sourced a verified user Turn. |
| `ActionAttempt` | `ActionAttemptId` | One execution attempt for an invocation; it may be replaced after interruption. |
| `Team` | `TeamId` | Named mutable set of Agent instances. |
| `AgentRelation` | `RelationId` | Directed typed relation used as action context. |
| `TaskScope` | `TaskScopeId` | Durable nested grouping for related action invocations. |
| `WorkflowTimer` | `TimerId` | Periodic trigger state: interval, policy, status, fire count, and deadlines. |
| `Channel` / `Signal` | `ChannelId` / `SignalId` | Named stream and ordered durable values. |
| `HumanRequest` | `HumanRequestId` | Typed question created by Workflow control flow; its answer resumes the suspended workflow. |
| `ControlMessage` | `ControlMessageId` | Pending `guide`, `finish`, or `interrupt` targeted at a Session/action boundary. |

## 2. Ownership and identity invariants

| ID | Invariant |
|---|---|
| I1 | Every Session belongs directly to one Project. |
| I2 | A Workflow belongs directly to one Project; a starting Session is optional and, when present, belongs to that Project. |
| I3 | Every Agent instance in a run owns exactly one Session with origin `workflow_agent`. |
| I4 | Agent Sessions remain navigable after retirement, completion, failure, or cancellation. |
| I5 | An ActionInvocation belongs to one Agent instance and its Session. |
| I6 | Every ActionAttempt belongs to one ActionInvocation and has at most one Turn. |
| I7 | A run may refer only to Sessions, Agents, scopes, channels, and controls in the same Project/run. |
| I8 | A workflow source snapshot and SHA-256 are immutable after run creation. |
| I9 | A Project has at most one editable WorkflowProgram per slug. Saving that slug replaces the program, never existing Workflow snapshots. |
| I10 | Python may request effects but cannot authoritatively mutate domain state. |
| I11 | Within one Workflow, a logical effect path is permanently bound to one exact kind and payload. |
| I12 | Workflow `request`, `params`, `instructions`, trigger, launch configuration, and launch context never mutate after run creation. |

The UI may visually group Agent Sessions beneath a Workflow. This does not
create a Session parent/child relation.

## 3. Definition and run creation

Exactly one async function must be decorated with literal `@workflow(...)`
metadata. Validation produces a manifest and AST summary. A source is runnable
only when there are no error diagnostics.

Starting a run performs these operations atomically enough to expose one
consistent created run:

1. Resolve `slug` in the Project-visible catalog (Project source overrides a built-in of the same slug).
2. Validate reusable run `params` against `params_schema`, including configured model-profile references declared with `format: "model-profile"`.
3. Copy source, manifest, owner, path, and SHA-256 into a WorkflowProgramSnapshot.
4. Validate the selected model, skills, Workflow access ceiling, and Agent class
   overrides. A starting Session must belong to the Project and is a hard outer
   access bound.
5. Enforce the manifest's `request_mode`, then store either the concrete user
   task or no launch task, plus optional run `instructions`, trigger
   provenance, and either a `fresh` launch context or one bounded immutable Project
   snapshot. For context construction, a starting Session supplies focus and
   provenance rather than a copied Session prompt.
6. Create a Project-owned Workflow with `created` status and an optional starting Session.
7. Schedule the run; the worker changes it to `running` before interpreting effects.

The runner exposes these values separately as `ctx.request`, `ctx.params`,
`ctx.trigger`, and `ctx.context`. A WorkflowProgram is generic for a task class:
it must explicitly pass the concrete request or selected context into an Agent
Action. A `request_mode="none"` program receives an empty `ctx.request` and is
expected to obtain user messages through explicit `ask_human` effects. The
runtime never turns either value into instructions automatically.
The current HTTP launcher records `manual` for a Project launch and `user` for
a Session-origin launch. The `workflow` and `timer` trigger kinds are reserved
for internal launch paths; waking an existing timer-backed run does not create
a new Workflow or change its trigger.
The run's output is the value returned by the Python entrypoint and sent through
the `complete` effect.

A terminal Workflow does not terminate, archive, or make its Agent Sessions
read-only. Once no Workflow Action or HumanRequest owns the next Turn, the user
may continue any of those Sessions from the normal composer. That creates an
ordinary `origin=user` Turn with the Session's existing history, model, system
prompt, skills, access, and cache state; it does not resume or mutate the
terminal Workflow. Continuing multi-Agent orchestration requires starting a new
Workflow from the Project or Session.

Closing a Session is a separate, explicit lifecycle operation. It cancels an
active `interactive-agent`, cancels any standalone active Turn, records the
Session as `archived`, and removes it from normal Project listings without
deleting its Turns or provenance. A Session owned by another active Workflow
must finish or cancel that Workflow before it can be closed.
The generic Workflow-cancel endpoint does not cancel `interactive-agent`;
Session close is its sole normal lifecycle control.

## 4. Agent semantics

An `Agent(...)` constructor is local and synchronous. It does not create a
Session immediately. The first action, Team activation, relation, channel send,
human request targeted at that Agent, or explicit retirement calls
`create_agent`. Rust then:

1. checks run status;
2. creates a Session in the run's Project;
3. resolves model and skills from Agent overrides or the Workflow defaults;
4. creates the WorkflowParticipant mapping;
5. emits Session-created, workflow-attached, and participant-created events.

Each Session has one user-facing access profile, expanded into granular runtime
capabilities:

| Profile | Files | Commands | Project network | Model-visible resource tools |
|---|---|---|---|---|
| `model_only` | None | None | None | None. |
| `read_only` | Read Session workspace | None | None | `read_file`. |
| `workspace` | Read/write Session workspace | Sandboxed, network denied | None | `read_file`, `write_file`, `exec_command`. |
| `research` | Read/write Session workspace | Sandboxed, network denied | Hosted web search and controlled public-HTTPS fetch | Workspace tools, `fetch_url`, hosted web search. |
| `full_access` | Read/write host filesystem | Unrestricted | Unrestricted | All registered tools and hosted web search. |

The profile is declared with `access = "research"` on an Agent class or with an
`access=` constructor override. `research` is the default. The launcher's
Workflow profile is the hard run ceiling; for a Session-origin run that ceiling
must be at or below the source Session. A per-run override keyed by Python Agent
class is applied before the class declaration, then the result is clamped to
the Workflow ceiling. Launch-time choices at or below the ceiling are already
authorized and create directly.

`await agent.set_access(profile)` changes an existing Agent. Downgrades apply
without approval; every upgrade within the run ceiling opens a HumanRequest,
while an attempt above the ceiling fails. Access may change only when the
Session has no active Turn. Each Turn captures an immutable access snapshot, so
a later Session change affects only later Turns. The Session UI uses the same
rule and requires an explicit confirmation for `full_access`.

Tool definitions are filtered before a model sample, but that is not the trust
boundary. The registry and each built-in tool recheck the Turn snapshot, paths
are resolved under the matching file policy, and command execution independently
selects the sandbox/network policy. Provider API traffic is runtime transport,
not Agent research-network access.

| Participant status | Meaning | Future actions allowed |
|---|---|---|
| `active` | Session exists and Agent may be scheduled. | Yes. |
| `retired` | Workflow deliberately removed the Agent. | No. |
| `failed` | Agent cannot continue. | No. |

`await agent.retire()` preserves all Session history but rejects later actions.

An Agent class or constructor may set `model`. An empty model inherits the
Workflow Run's default profile; a non-empty value binds that persistent Agent
Session to the named configured profile. Therefore a generator/reviewer split
is ordinary DSL (`Generator(model=...)`, `Reviewer(model=...)`), not a separate
runtime primitive. A Workflow may expose such choices through arbitrary
`params_schema` fields using `format: "model-profile"`.

## 5. Action, attempt, and Turn semantics

An `@action` method declares a prompt and argument signature. The method body is
not executed as agent logic. Calling it creates an awaitable; awaiting that
value requests `invoke_action`. The Agent then follows the Codex-like loop:
sample the model, execute requested tools, append their outputs, and sample
again. The Action ends when the model returns a terminal assistant message,
the user finishes/interrupts/cancels it, or runtime/provider infrastructure
fails.
`reasoning_effort` (`none`, `low`, `medium`,
`high`, `xhigh`, or `max`) overrides the server default for that action. The
value is snapshotted on the Turn and shown in model-step input metadata.
`search_context_size` (`low`, `medium`, or `high`) controls how much retrieved
context each hosted search attaches; use `low` for exploratory routes
and increase it only when the task needs richer page context.
`finalize="after_search"` gives a deliverable-producing action an explicit
completion boundary. If its first Turn used hosted search, the same persistent
Agent Session receives a second Action Turn with tools explicitly disabled. It
must turn the preceding research/progress output into the actual final
deliverable. `finalize="always"` performs that no-tool Turn even when the first
Turn used no hosted search. The finalizer is a separate durable
ActionInvocation and visible Turn, so it is recoverable and inspectable rather
than hidden post-processing. Typed-action JSON repair uses the same internal
no-tool policy.
```text
ActionInvocation
  Attempt 1 -> Turn 1 -> model/tool Steps
  Attempt 2 -> Turn 2 -> model/tool Steps   # only after interruption/retry
```

For an ordinary action, the action docstring/decorator becomes an inspectable
Workflow instruction layer named `Action contract`. Bound arguments are
serialized separately as the input of a workflow-origin Turn. This is the only
way `ctx.request`, `ctx.params`, or `ctx.context` reaches a model: the Workflow
must pass the selected value as an Action argument. A string answer returned by
`ask_human` is instead a `HumanMessage`. When it is passed to an
action parameter annotated as `HumanMessage`, Python sends the request ID and
parameter name. Rust accepts a user-origin Turn only if that direct
HumanRequest is answered, belongs to this Workflow and Agent Session, has a
string answer, and exactly matches the bound argument. The exact human text
becomes the Turn input; the Action contract and any remaining Workflow-provided
context become an inspectable Workflow layer clearly marked as data. The
ActionInvocation retains the source HumanRequest ID.

Every Turn snapshots the exact ordered prompt layers: runtime, Project,
Workflow, Agent/Session, Skills, and runtime control. The Workflow layer may
contain the run `instructions`, Action contract, and relevant directed
relations. It never implicitly contains the run request or launch-context
snapshot. Interruption/retry guidance belongs to the control layer. See
[prompt model](prompt-model.md).

| Invocation/attempt status | Meaning |
|---|---|
| `scheduled` | Durable invocation exists but does not hold execution permits. |
| `running` | Attempt and Turn are executing. |
| `completed` | Turn output was stored and returned to Python. |
| `interrupted` | This attempt ended; runtime starts another attempt for the same invocation. |
| `failed` | Runtime/model/tool failure ended the invocation. |
| `cancelled` | Run or Turn cancellation ended the invocation. |

One completed action returns its assistant output string. Token/cache usage,
Step count, hosted-search count, and timing are added to Workflow telemetry.
Provider-reported usage from incomplete
samples is retained across retries. If every retry fails, the final model Step
is persisted as failed and the consumed usage is still recorded on the run.
When an output limit or a reasoning-only completion produces no message or tool
call, the fixed transient retry lowers reasoning effort and explicitly requests the
original final-answer format.

Completion of an Action is distinct from acceptance of a whole Workflow's
result. For example, the built-in `evidence-loop` exposes `audit_policy`:
`deliver_with_warning` returns an explicit warning result, `fail_run` rejects a
failed evidence/draft gate, and `wait_for_human` durably asks for `/deliver`,
`/fail`, or revision guidance. Free-form guidance is passed as a verified
`HumanMessage` to the persistent Writer Session and is therefore rendered as a
real user-origin Turn.

## 6. Concurrency

`await together(a(), b(), ...)` is the only special concurrency combinator. It
uses `asyncio.gather` and returns a tuple in argument order.

Before starting, `together` examines direct `_ActionCall` operands. If two calls
target the same Agent object, it raises `ValueError`; no action in that group is
started. The Rust runtime independently applies one mutex per Agent instance,
which serializes Turns in its Session, plus the Session runtime's server-wide
concurrent-Turn bound.

Different Agent Sessions can therefore work simultaneously. Calls to the same
Agent outside one `together` group may be created concurrently by ordinary
Python tasks, but they queue at the per-Agent gate.

`background(awaitable)` starts an ordinary asyncio task registered with the
runner. `join()` observes its result; `cancel()` cancels it. All unjoined
background tasks are cancelled when the entrypoint exits.

## 7. Teams and relations

`Team(name, *members)` is local until `activate`, `add`, or `remove`. Team
membership is durable but has no implicit scheduling semantics: it is a
grouping/control primitive, not a hidden loop.

`relate(source, target, kind, instructions)` creates a directed relation. Before
an Agent action, Rust collects every incoming or outgoing relation involving
that Agent and injects readable relationship context. A relation does not
automatically send messages or invoke the target.

Dynamic Agent creation and Team mutation are allowed while the Workflow is
active. Team removal does not retire an Agent; retirement is explicit.

## 8. Task scopes

`async with scope(name, objective)` opens a TaskScope and pushes its ID on the
runner-local scope stack. Actions created in the block record that scope ID.
Nested scopes record the current scope as parent.

Normal exit closes the scope as `completed`; exceptional exit closes it as
`cancelled` and then propagates the Python exception. Scope status does not
cancel actions by itself.

## 9. Human interaction and controls

`await ask_human(...)` is a Workflow control-flow effect. It suspends the
Workflow coroutine and creates a request belonging to the specified Agent
Session or origin Session; no Action Turn is required. Models never receive an
`ask_human` tool definition. An Action may return a typed recommendation such
as `needs_human`, but only Workflow code can turn that recommendation into a
HumanRequest.

A string DSL answer carries its durable HumanRequest ID as `HumanMessage`.
Passing that value into a correspondingly annotated action is the only workflow
path that may create a human-looking `user` Turn. Other values remain ordinary
schema-validated Python values and workflow-dispatched actions.

The response schema is stored with the request. The HTTP API validates an
answer before resolution. While at least one request is open,
`Workflow.attention_required` is true.

`await wait(...)`, `Channel.receive()`, and workflow-level `ask_human(...)` are
replayable suspension points. A branch first receives a suspended protocol
acknowledgement rather than an exception. When every live Python branch is at
such a point, the runner declares quiescence; Rust keeps each effect `started`,
terminates the Python process, and releases the global execution permit. The
supervisor wakes on any ready condition and replays source. Completed branches
and domain mutations replay from their journaled results, so a timer may fire
while another branch still has an open HumanRequest, and a Signal published by
a concurrent branch is consumed exactly once after replay.

`ctx.context` returns the immutable launch snapshot, or `{}` for a fresh run.
`ctx.project.snapshot(...)` separately reads current bounded durable state owned
by the Project. A long-lived Workflow may pass a previous snapshot's
`captured_at` back as `updated_after`; the next snapshot has `mode="delta"` and
contains only Sessions/Turns, Workflows, and Artifacts updated after that
cursor. The capture timestamp is taken before database reads, so concurrent
updates can be repeated but cannot fall between cursors. `publish_artifact(...)`
accepts text, derives its Artifact ID
from the effect path, and is idempotent under replay. These effects let ordinary
user Workflows build Project-level views without direct SQLite or host-file
access.

Control messages are asynchronous:

| Control | Exact semantics |
|---|---|
| `guide` | Queued for a Session/action. At the next Agent checkpoint it becomes a user-history item before the next model sample. It does not invalidate completed work. |
| `finish` | At the next checkpoint, add the instruction and force the current Action's next model sample to be final with no tools. The Workflow continues after that Action returns. |
| `interrupt` | At the next checkpoint, current Turn/Attempt becomes interrupted. The action runtime creates a new Attempt and includes the control text as restart guidance. |
| pause | Changes run to `paused`. Workflow and Agent checkpoints wait; an already in-flight provider response is not rolled back. |
| resume | Changes run to `running`; waiting checkpoints continue. |
| cancel | Changes run to `cancelled` and propagates cancellation to Python, model, and tool work. |
| Stop Turn | Cancels only that active Turn and its model/tool Steps. It does not ask the model to synthesize an answer. If it belongs to a Workflow Action, the effect returns a cancellation error for ordinary Workflow error handling. |

Guide/finish/interrupt delivery is durable and at-most-once at the Store level: pending
messages are marked applied when a checkpoint consumes them.

## 10. Channels and signals

`Channel(name, schema)` creates or reuses a named channel in one run.
`publish(value, sender=...)` appends a Signal with a channel-local monotonically
increasing sequence. `receive()` waits for the first Signal after that Channel
object's local cursor, advances the cursor, and returns its value.

The schema is currently recorded for inspection but Signal values are not yet
validated against it. Publishing does not invoke subscribers; receiving is
explicit.

## 11. Timers

`@every(seconds=..., policy=..., name=...)` creates a TimerHandle and starts a
background loop:

1. register or reuse an active timer by name;
2. wait until `next_fire_at`;
3. persist a fire, advance counts/deadline, and update usage telemetry;
4. await the callback;
5. repeat.

| Policy | Intended scheduling meaning | Current executor behavior |
|---|---|---|
| `coalesce` | Collapse missed ticks into one run. | One callback per returned wait. |
| `skip` | Skip a tick when prior work is still running. | Recorded, not yet behaviorally distinct. |
| `queue` | Preserve every tick as queued work. | Recorded, not yet behaviorally distinct. |

Because the timer loop awaits the callback, one TimerHandle does not overlap its
own callback. A callback action creates a new Turn each time. Active timers are
marked completed when the workflow completes.

## 12. Completion, failure, and observability

| Workflow status | Entry condition | Effects accepted |
|---|---|---|
| `created` | Durable run exists, waiting for scheduler. | Checkpoints may proceed into startup. |
| `running` | Worker is interpreting source. | Yes, subject to validation and permissions. |
| `waiting_for_user` | Workflow requested user input. | Resume follows a validated user response. |
| `waiting_for_timer` | Workflow is waiting for a timer deadline. | Timer wake-up resumes execution. |
| `waiting_for_signal` | Workflow is waiting for a Channel Signal. | Matching Signal resumes execution. |
| `paused` | User paused the run. | Existing calls wait at checkpoints. |
| `completed` | Python submitted output, the process exited successfully, final usage was recorded, and Rust committed the output. | No new domain work. |
| `failed` | Python, action, protocol, sandbox, model, or provider failure. | No. |
| `cancelled` | User/runtime cancellation. | No. |

Agent/action/step/timer/search counts, provider token and cache usage, and
wall-clock time are observational telemetry persisted for inspection. Actions
stop on terminal model output, explicit user control, provider or infrastructure
failure, or context-window limits. The runtime also enforces permissions,
sandbox boundaries, Session serialization, provider request/stream-idle
timeouts, and server-wide concurrency.

An uncaught Python exception exits the runner and fails the run with bounded
stderr. An action failure is returned to Python as an effect exception; if the
workflow does not catch it, the run fails. A normal entrypoint return is
submitted through `complete`. That effect only acknowledges the candidate
output; the scheduler records final wall time and commits the terminal status
after the Python process exits successfully. Exiting without completion is a
protocol error.

## 13. Persistence and replay boundary

All authoritative entities, effect outcomes, and ordered events are durable.
Workflow source is snapshotted. Every non-terminal Workflow is scheduled for
restart recovery. An unfinished standalone Session Turn is instead settled: a
durable terminal candidate is committed, or the Turn becomes `interrupted`
without another provider sample and waits for explicit user direction.

The Python program restarts at its entrypoint rather than serializing a Python
instruction pointer. Each DSL operation has a deterministic logical effect path.
The Store journals that path, kind, exact payload hash, `started/completed/failed`
status, result, error, and timestamps. Reaching a completed path returns its
stored result without repeating the domain mutation. A path left `started` is
redispatched using resource IDs derived from `(WorkflowId, effect path,
resource kind)`, so creation, signal publication, timer firing, and human
requests converge on the original durable object. Reaching one path with a
different request fails closed.

An unfinished Action reuses its ActionInvocation, latest non-terminal Attempt,
and attached Turn. Its Session rollout stores append-or-replace model-context
mutations, cumulative usage, completed-model-step and hosted-search cursors,
plus any terminal candidate message; the Turn's SQLite document does not copy
the cumulative context. Each local Tool Step stores its provider call ID,
effect disposition, and `prepared`/`executing` boundary. Recovery replays the
rollout and reuses the exact output of a completed Tool Step. A prepared call
has not crossed the external-effect boundary and may execute after recovery
marks it executing. For an executing call, `pure` and `idempotent` tools may
replay with the same effect ID, `reconcilable` tools inspect external state
before returning a result or retrying, and `unknown` tools are never replayed
automatically. The latter becomes an explicit `execution_unknown` function
result. A Workflow-level `ask_human` effect is itself journaled and continues
waiting on its deterministic HumanRequest.

Human, timer, and signal waits additionally support process-free suspension.
The Python effect client tracks all pending futures; only when every pending
effect is a replayable wait does it request runtime suspension. This quiescence
rule prevents an early human wait from cancelling a concurrent Agent action or
Signal publisher. On recovery, open direct HumanRequests, active timers, and
started signal waits reconstruct the wake conditions. Dormant time is not a
held scheduler permit; each active replay segment still contributes persisted
wall-time usage.

Pure Python computation between effects may execute again. Authors must keep
the effect sequence deterministic for the same source snapshot and input; wall
clock, randomness, or other non-determinism must not change the request at an
already reserved path.

## 14. Representative trace

For the built-in parallel-discovery workflow with two perspectives:

| Time | Python operation | Durable result |
|---|---|---|
| T0 | Construct two Researchers, one Synthesizer, and a Team. | No effect until first use. |
| T1 | `await team.activate()` | Three Agent instances/Sessions and one Team exist. |
| T2 | Create two `reports_to` relations. | Directed relation records exist. |
| T3 | Enter `scope(...)`. | Open TaskScope exists. |
| T4 | `await together(researcher1.investigate(...), researcher2.investigate(...))` | Two ActionInvocations run in two Sessions concurrently. |
| T5 | Both actions complete. | Two Turns and outputs are durable; scope closes completed. |
| T6 | `await synthesizer.synthesize(...)` | Third ActionInvocation/Turn consumes both output strings. |
| T7 | `return {"summary": ...}` | Runner sends `complete`; run output/status become durable. |

At every point, opening an Agent Session shows the same multi-turn conversation
and folded model/tool execution details as an ordinary user Session.
