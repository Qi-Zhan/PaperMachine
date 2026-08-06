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
| `Workflow` | `WorkflowId` | One execution of an immutable workflow snapshot inside a Project. |
| `WorkflowEffect` | `(WorkflowId, logical path)` | Durable journal entry for one exact Python effect request and its replayable result or error. |
| starting Session | `started_from_session_id?` | Optional Session from which a Workflow was started. It is provenance, not ownership. |
| `Agent instance` | `AgentInstanceId` | One workflow actor backed by exactly one Project-owned Session. |
| `ActionInvocation` | `ActionInvocationId` | Logical call of one declared action on one Agent; it optionally records the HumanRequest that sourced a verified user Turn. |
| `ActionAttempt` | `ActionAttemptId` | One execution attempt for an invocation; it may be replaced after interruption. |
| `Team` | `TeamId` | Named mutable set of Agent instances. |
| `AgentRelation` | `RelationId` | Directed typed relation used as action context. |
| `TaskScope` | `TaskScopeId` | Durable nested grouping for related action invocations. |
| `WorkflowTimer` | `TimerId` | Periodic trigger state: interval, policy, status, fire count, and deadlines. |
| `Channel` / `Signal` | `ChannelId` / `SignalId` | Named stream and ordered durable values. |
| `HumanRequest` | `HumanRequestId` | Typed question whose answer resumes a suspended workflow or tool call. |
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
| I12 | Workflow launch configuration and launch context never mutate after run creation. |

The UI may visually group Agent Sessions beneath a Workflow. This does not
create a Session parent/child relation.

## 3. Definition and run creation

Exactly one async function must be decorated with literal `@workflow(...)`
metadata. Validation produces a manifest and AST summary. A source is runnable
only when there are no error diagnostics.

Starting a run performs these operations atomically enough to expose one
consistent created run:

1. Resolve `slug` in the Project-visible catalog (Project source overrides a built-in of the same slug).
2. Validate the user input against `input_schema`.
3. Copy source, manifest, owner, path, and SHA-256 into a WorkflowProgramSnapshot.
4. Validate the selected model, skills, Workflow access ceiling, and Agent class
   overrides. A starting Session must belong to the Project and is a hard outer
   access bound.
5. Store either a `fresh` launch context or one bounded immutable Project
   snapshot. For context construction, a starting Session supplies focus and
   provenance rather than a copied Session prompt.
6. Create a Project-owned Workflow with `created` status and an optional starting Session.
7. Schedule the run; the worker changes it to `running` before interpreting effects.

The run's output is the value returned by the Python entrypoint. The runner
sends it through the `complete` effect.

## 4. Agent semantics

An `Agent(...)` constructor is local and synchronous. It does not create a
Session immediately. The first action, Team activation, relation, channel send,
human request targeted at that Agent, or explicit retirement calls
`create_agent`. Rust then:

1. checks run status and `max_agents`;
2. creates a Session in the run's Project;
3. resolves model and skills from Agent overrides or the Workflow defaults;
4. creates the WorkflowParticipant mapping;
5. emits Session-created, workflow-attached, and participant-created events.

Each Session has one user-facing access profile, expanded into granular runtime
capabilities:

| Profile | Files | Commands | Project network | Model-visible resource tools |
|---|---|---|---|---|
| `model_only` | None | None | None | `ask_human` only. |
| `read_only` | Read Session workspace | None | None | `read_file`, `ask_human`. |
| `workspace` | Read/write Session workspace | Sandboxed, network denied | None | `read_file`, `write_file`, `exec_command`, `ask_human`. |
| `research` | Read/write Session workspace | Sandboxed, network denied | Hosted web search and controlled public-HTTPS fetch | Workspace tools, `fetch_url`, hosted web search, `ask_human`. |
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
| `waiting_for_human` | Reserved participant-level attention state. | Runtime-dependent. |
| `retired` | Workflow deliberately removed the Agent. | No. |
| `failed` | Agent cannot continue. | No. |

`await agent.retire()` preserves all Session history but rejects later actions.

## 5. Action, attempt, and Turn semantics

An `@action` method declares a prompt and argument signature. The method body is
not executed as agent logic. Calling it creates an awaitable; awaiting that
value requests `invoke_action`. `@action(max_steps=N)` may set a smaller
per-action model-sample limit; the Workflow's `max_action_steps` remains the
hard ceiling. Setting `max_steps=1` also disables tools for that action because
the first sample must be the final response. `@action(max_search_calls=N)` sets
a hosted web-search allowance across the Turn; zero disables hosted search
without changing the Agent's other access permissions. On endpoints that accept
the Responses API `max_tool_calls` field the allowance is provider-enforced. If
a proxy rejects that field, PaperMachine records `runtime_fallback` in each
model step and enforces the remaining allowance between samples, so one response
may overshoot it. Each response receives at most four calls from the remaining
allowance and a stable matching control instruction; this keeps one response
from consuming an entire Turn on endpoints that honor either mechanism without
changing continuation identity. `reasoning_effort` (`none`, `low`, `medium`,
`high`, `xhigh`, or `max`) overrides the server default for that action, while
`max_output_tokens` sets its per-model-response output ceiling. Both values are
snapshotted on the Turn and shown in model-step input metadata.
`search_context_size` (`low`, `medium`, or `high`) controls how much retrieved
context each hosted search attaches; use `low` for bounded exploratory routes
and increase it only when the task needs richer page context.
Because the Responses WebSocket beta is not consistent across compatible
endpoints for `max_output_tokens`, an action with an explicit output ceiling
uses HTTP SSE and records `max_output_tokens_requires_http`. Keep output ceilings
on one-sample planning, evaluation, and writing actions; omit them on multi-step
research actions that benefit from incremental WebSocket continuation.

```text
ActionInvocation
  Attempt 1 -> Turn 1 -> model/tool Steps
  Attempt 2 -> Turn 2 -> model/tool Steps   # only after interruption/retry
```

For an ordinary action, the runtime formats the action docstring/decorator
prompt and bound arguments as a workflow-origin Turn objective. A string answer
returned by `ask_human` is instead a `HumanMessage`. When it is passed to an
action parameter annotated as `HumanMessage`, Python sends the request ID and
parameter name. Rust accepts a user-origin Turn only if that direct
HumanRequest is answered, belongs to this Workflow and Agent Session, has a
string answer, and exactly matches the bound argument. The human text becomes
the Turn input; the action prompt and remaining arguments become an inspectable
Workflow prompt layer. The ActionInvocation retains the source HumanRequest ID.

Every Turn snapshots the exact ordered prompt layers: runtime, Project,
Workflow, Agent/Session, Skills, and runtime control. The Workflow layer
includes the immutable launch context when configured. Relevant directed
relations belong to that layer; interruption/retry guidance belongs to the
control layer. See [prompt model](prompt-model.md).

| Invocation/attempt status | Meaning |
|---|---|
| `scheduled` | Durable invocation exists but does not hold execution permits. |
| `running` | Attempt and Turn are executing. |
| `waiting_for_human` | Model tool call is suspended for an answer. |
| `completed` | Turn output was stored and returned to Python. |
| `interrupted` | This attempt ended; runtime starts another attempt for the same invocation. |
| `failed` | Runtime/model/tool failure ended the invocation. |
| `cancelled` | Run or Turn cancellation ended the invocation. |

One completed action returns its assistant output string. Usage and Step count
are added to Workflow budget usage. Provider-reported usage from incomplete
samples is retained across retries. If every retry fails, the final model Step
is persisted as failed and the consumed usage is still charged to the run.
When an output limit or a reasoning-only completion produces no message or tool
call, the bounded retry lowers reasoning effort and explicitly requests the
original final-answer format.

## 6. Concurrency

`await together(a(), b(), ...)` is the only special concurrency combinator. It
uses `asyncio.gather` and returns a tuple in argument order.

Before starting, `together` examines direct `_ActionCall` operands. If two calls
target the same Agent object, it raises `ValueError`; no action in that group is
started. The Rust runtime independently applies:

- a run semaphore bounded by `max_concurrent_actions`;
- one mutex per Agent instance, which serializes Turns in its Session;
- the Session runtime's global concurrent-Turn bound.

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

Dynamic Agent creation and Team mutation are allowed until the Agent budget is
exhausted. Team removal does not retire an Agent; retirement is explicit.

## 8. Task scopes

`async with scope(name, objective)` opens a TaskScope and pushes its ID on the
runner-local scope stack. Actions created in the block record that scope ID.
Nested scopes record the current scope as parent.

Normal exit closes the scope as `completed`; exceptional exit closes it as
`cancelled` and then propagates the Python exception. Scope status does not
cancel actions by itself.

## 9. Human interaction and controls

There are two request paths:

| Path | Suspension point | Session/Turn behavior |
|---|---|---|
| DSL `await ask_human(...)` | Workflow coroutine | Request belongs to the specified Agent Session or origin Session; no Turn is required. |
| model tool `ask_human` | Tool call inside an action Turn | Turn and Session become `waiting_for_human`; answer is the tool output. |

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
by the Project. `publish_artifact(...)` accepts text, derives its Artifact ID
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

Guide/interrupt delivery is durable and at-most-once at the Store level: pending
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
3. persist a fire, advance counts/deadline, and update budget usage;
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

## 12. Completion, failure, and budgets

| Workflow status | Entry condition | Effects accepted |
|---|---|---|
| `created` | Durable run exists, waiting for scheduler. | Checkpoints may proceed into startup. |
| `running` | Worker is interpreting source. | Yes, subject to validation/budgets. |
| `waiting_for_user` | Workflow requested user input. | Resume follows a validated user response. |
| `waiting_for_timer` | Workflow is waiting for a timer deadline. | Timer wake-up resumes execution. |
| `waiting_for_signal` | Workflow is waiting for a Channel Signal. | Matching Signal resumes execution. |
| `paused` | User paused the run. | Existing calls wait at checkpoints. |
| `completed` | Python submitted output, the process exited successfully, final usage was recorded, and Rust committed the output. | No new domain work. |
| `failed` | Python, action, protocol, sandbox, or budget failure. | No. |
| `cancelled` | User/runtime cancellation. | No. |

Budget fields are `max_agents`, `max_concurrent_actions`, `max_action_steps`,
`max_total_tokens`, `max_uncached_tokens`, `max_hosted_search_calls`,
`max_wall_time_seconds`, and optional `max_cost_usd`. `max_total_tokens` is the
raw provider input-plus-output safety limit. `max_uncached_tokens` is the
economic limit and counts
`input_tokens - cached_input_tokens + output_tokens`; prompt-cache reads therefore
remain visible but do not consume the uncached allowance.
`max_action_steps` is the run-wide limit for persisted model, local-tool,
hosted-tool, and compaction Steps. A Step is charged when it is created, including
for actions that later fail, and the runtime checks the limit before each model
sample. Concurrent provider responses can still produce a small bounded overshoot
when several tool Steps arrive from the same in-flight response.
`max_hosted_search_calls` bounds the run-wide sum of provider-hosted web search,
open-page, and find-in-page actions. Each action should also declare
`max_search_calls` so a single provider response cannot consume the whole run
budget. Agent, action, step, timer, hosted-search, token, and wall-time usage are
persisted. Some limits are checked only at effect/model boundaries; concurrent
in-flight responses can overshoot the run-wide search limit, while each action's
provider-side limit remains hard when the endpoint supports `max_tool_calls`.
With a proxy that rejects that request property, the action limit is enforced
between model samples and one response can still overshoot the four-call soft
batch size. Cost enforcement
requires a provider cost estimate.

An uncaught Python exception exits the runner and fails the run with bounded
stderr. An action failure is returned to Python as an effect exception; if the
workflow does not catch it, the run fails. A normal entrypoint return is
submitted through `complete`. That effect only acknowledges the candidate
output; the scheduler records final wall time and commits the terminal status
after the Python process exits successfully. Exiting without completion is a
protocol error.

## 13. Persistence and replay boundary

All authoritative entities, effect outcomes, and ordered events are durable.
Workflow source is snapshotted. Standalone Session Turns and every non-terminal
Workflow are scheduled for restart recovery.

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
and attached Turn. The Turn checkpoint stores model history, cumulative usage,
completed-model-step and hosted-search cursors, plus any terminal candidate
message. Each local Tool Step also stores its provider call ID. Recovery reuses
the exact output of a completed Tool Step; a Step still running when the process
disappeared becomes an explicit execution-unknown restart output. It also
cancels a stale open human-tool request and resumes at the next sample. A direct
workflow-level `ask_human` effect is itself journaled and continues waiting on
its deterministic HumanRequest.

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
