# Python workflow ABI

PaperMachine workflows are async Python programs written against the small
`papermachine` DSL. Python expresses the collaboration protocol; the Rust
runtime interprets every stateful operation as a typed effect.

## Minimal source

```python
from papermachine import Agent, action, scope, together, workflow


class Researcher(Agent):
    access = "research"
    role = "independent evidence route"
    system_prompt = "Prefer primary evidence and preserve uncertainty."

    @action(
        max_search_calls=16,
        search_context_size="low",
        reasoning_effort="high",
        max_output_tokens=16_384,
    )
    async def investigate(self, question: str, perspective: str):
        """Find evidence, counterevidence, provenance, and uncertainty."""


class Synthesizer(Agent):
    access = "model_only"

    @action(max_steps=1, reasoning_effort="medium", max_output_tokens=8_192)
    async def synthesize(self, question: str, findings: list[str]):
        """Compare findings and return the strongest bounded conclusion."""


@workflow(
    slug="parallel-review",
    name="Parallel review",
    description="Run independent Sessions, then synthesize them.",
    input_schema={
        "type": "object",
        "properties": {
            "perspectives": {
                "type": "array",
                "items": {"type": "string"},
            }
        },
    },
    output_schema={
        "type": "object",
        "properties": {"summary": {"type": "string"}},
    },
    budget={
        "max_agents": 8,
        "max_concurrent_actions": 4,
        "max_action_steps": 24,
        "max_total_tokens": 1500000,
        "max_uncached_tokens": 400000,
        "max_hosted_search_calls": 64,
        "max_wall_time_seconds": 7200,
    },
)
async def main(ctx):
    perspectives = ctx.input.get("perspectives") or ["support", "limitations"]
    agents = [Researcher(name=f"Route {index + 1}") for index, _ in enumerate(perspectives)]
    async with scope("Independent evidence", ctx.objective):
        findings = await together(*(
            agent.investigate(ctx.objective, perspective)
            for agent, perspective in zip(agents, perspectives)
        ))
    summary = await Synthesizer(name="Synthesis").synthesize(ctx.objective, list(findings))
    return {"summary": summary}
```

The body of an `@action` method is declarative. Its signature binds arguments;
its decorator string or docstring becomes the action prompt. Awaiting the method
asks Rust to create an ActionInvocation, an ActionAttempt, and a Turn in that
Agent's Session.

## Manifest

`@workflow(...)` must appear exactly once on an async entrypoint and all values
must be Python literals.

| Field | Meaning |
|---|---|
| `slug` | Lowercase kebab-case catalog key. |
| `name` | Human-readable name. |
| `description` | Protocol purpose shown on the Workflow page. |
| `input_schema` | Supported JSON Schema subset checked before scheduling. |
| `output_schema` | Declared result contract; currently descriptive at completion. |
| `budget` | Agent, action concurrency/steps, hosted-search, raw and uncached token, wall-time, and optional cost limits. |

The runtime provides `ctx.objective`, `ctx.input`, `ctx.workflow_id`, and
`ctx.project`. `await ctx.project.snapshot(...)` returns a bounded, structured
view of the Project's durable Sessions, Turns, Workflow results, and Artifact
metadata; it excludes summary runs themselves to avoid recursive summaries.

## DSL surface

| Primitive | Effect |
|---|---|
| `Agent(...)` | Local declaration; first use creates an Agent instance and Session. |
| `Agent(system_prompt=...)` | Overrides the class system prompt for this persistent Agent Session. |
| `Agent(access=...)` | Overrides the class access profile for this instance. |
| `await agent.set_access(...)` | Downgrades immediately between Turns; an upgrade suspends for explicit human approval. |
| `@action` | Declares a model-backed action. Optional `max_steps`, `max_search_calls`, `search_context_size`, `reasoning_effort`, and `max_output_tokens` give each role its own sample, search, retrieval-context, compute, and output policy. |
| `await together(...)` | Runs awaitables concurrently; duplicate same-Agent actions fail before starting. |
| `Team(name, *agents)` | Creates a named, dynamically mutable membership set. |
| `await team.add/remove(...)` | Changes Team membership. |
| `await agent.retire()` | Prevents future actions while preserving its Session. |
| `await relate(a, b, kind=..., instructions=...)` | Records a directed relation and injects relevant context into actions. |
| `async with scope(name, objective)` | Opens/closes a durable task scope; scopes may nest. |
| `Channel(name, schema=...)` | Creates a durable channel; publish emits ordered Signals, receive waits for the next one. |
| `await ask_human(...)` | Suspends until a schema-validated answer is supplied. A string answer is a `HumanMessage` carrying its durable request ID. |
| `action(message: HumanMessage)` | Creates a true user-origin Turn only after Rust verifies that the text exactly matches the referenced answered HumanRequest; the action prompt becomes a Workflow prompt layer. |
| `background(awaitable)` | Starts concurrent workflow work and returns a joinable handle. |
| `@every(seconds=..., policy=...)` | Starts a periodic callback backed by a durable timer record. |
| `await wait(seconds=... / minutes=...)` | Suspends one branch until a named durable timer fires. |
| `await ctx.project.snapshot(...)` | Reads bounded Project-owned research state through Rust; Python never opens the database directly. |
| `await publish_artifact(...)` | Publishes a deterministic text Artifact, optionally associated with an Agent Session. |

Ordinary Python `if`, `for`, `while`, functions, collections, and exceptions
remain the workflow control language. Arbitrary imports, filesystem/network
access, subprocesses, environment access, dynamic code, and reflection are not
part of the ABI.

Every Agent class should declare `access` as one of `model_only`, `read_only`,
`workspace`, `research`, or `full_access`. `research` is the default. The
origin Session's profile is the initial ceiling: creating an Agent above it
opens a boolean HumanRequest before the first action. Creating one at or below
the ceiling does not. A later `set_access` upgrade always opens a HumanRequest;
a downgrade does not. A Turn keeps the profile snapshot captured at creation.

Agent classes may declare `system_prompt`; a constructor override takes
precedence. Project, Workflow, Agent/Session, skill, and control layers are
snapshotted on every Turn. See [prompt model](prompt-model.md).

The built-in `interactive-agent` is the reference conversational program. It
uses an ordinary `while` loop: `ask_human(..., agent=agent)`, then
`await agent.respond(message)`. The New Session UI starts this Workflow through
the same Project Workflow API used for every other program.

The built-in `project-summary` is the reference background program. It reads
`ctx.project.snapshot()`, asks one persistent summary Agent to render a
self-contained HTML report, publishes it as an Artifact, and optionally calls
`wait(minutes=...)` before repeating. Its reviewed Agent prompt lives in source;
the Project Page exposes the run's user-controlled Workflow system prompt and
timer interval. A scheduled summary is therefore an ordinary durable Workflow,
not a second "instance" entity or a hidden Project daemon.

## Effect protocol

The isolated runner reserves stdout for newline-delimited JSON:

```json
{"id":"root/together:2/branch:0/effect:0/invoke_action","kind":"invoke_action","payload":{"agent_instance_id":"..."}}
{"id":"root/together:2/branch:0/effect:0/invoke_action","ok":true,"result":{"output":"...","turn_id":"..."}}
```

The request ID is both the concurrent response correlation ID and a durable
logical idempotency key. Sequential operations reserve monotonically numbered
paths; `together(...)` and `background(...)` give each child a stable branch
path, so completion order does not change identity. Rust journals the exact
kind and payload hash before dispatch and records the terminal result or error.
Reusing one path with a changed request is a hard protocol error. Supported
effect kinds are:

```text
create_agent      set_agent_access   retire_agent       invoke_action
create_team       set_team_members    set_relation
open_scope        close_scope         register_timer
wait_timer        create_channel      publish_signal
wait_signal       ask_human           project_snapshot
publish_artifact  complete
```

Rust rejects unknown effects and malformed or cross-run IDs. On process
restart, the snapshotted Python program starts at its entrypoint: completed
effects replay their stored results, while a journal entry left `started` is
redispatched to deterministic domain-resource IDs. Unfinished Agent actions
resume the same checkpointed Turn rather than sampling their first model step
again.

When every live Python branch is waiting on replayable effects such as
`ask_human`, `wait_timer`, or `wait_signal`, the runner reports a quiescent
suspension. Rust leaves those effects `started`, terminates the Python process,
and releases the global run permit. A human answer, due timer, or matching
Signal makes the Workflow runnable; source replay then completes whichever
effect became ready. This allows many long-lived Workflows without reserving a
Python process or execution permit for each idle wait.

## Concurrency and timers

`together` is explicit concurrency. A run-level semaphore enforces
`max_concurrent_actions`; a per-Agent mutex ensures that one Session never runs
two Turns concurrently. The output tuple preserves argument order, independent
of completion order.

Timers use `coalesce`, `skip`, or `queue` policy metadata and persist fire count,
next fire time, and last fire time. Dormant timer waits are scheduler wakeups,
not sleeping Python processes. One callback runs per `wait_timer` response;
complete backlog semantics for all three policies are not yet distinct.
Periodic work is cancelled when the workflow entrypoint exits, and active timer
records become completed.

## Catalog and publication

The catalog scans:

```text
workflows/builtin/<slug>/workflow.py
<project-root>/.papermachine/workflows/<slug>/workflow.py
```

Both roots pass through the same AST validator. Saving a user WorkflowProgram
writes validated source to its Project directory. Saving the same slug replaces
the editable source; already-created Workflows keep their original source snapshot.

The Workflow page uses the validator's AST summary to show Agent classes,
actions, parallel blocks, Teams, relations, scopes, channels, timers, background
tasks, human checkpoints, Project snapshots, Artifact publication, and
diagnostics. Source remains available under
Advanced source for precise review and edits.
