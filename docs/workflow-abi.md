# Python Workflow ABI

PaperMachine Workflows are isolated async Python programs. Python describes
control flow; Rust owns every durable mutation, model Turn, permission check,
and Project resource.

## Minimal source

```python
from papermachine import Agent, action, together, workflow


class Researcher(Agent):
    access = "research"

    @action(
        search_context_size="low",
        reasoning_effort="high",
        tools=["read_file", "write_file", "exec_command", "fetch_url"],
    )
    async def investigate(self, question: str, perspective: str):
        """Find evidence, counterevidence, provenance, and uncertainty."""


class Synthesizer(Agent):
    access = "model_only"

    @action(tools=[])
    async def synthesize(self, question: str, findings: list[str]):
        """Compare the findings and return a bounded conclusion."""


@workflow(
    slug="parallel-review",
    name="Parallel review",
    description="Research in parallel, then synthesize.",
    params_schema={"type": "object", "additionalProperties": False},
)
async def main(ctx):
    support = Researcher(name="Support")
    limits = Researcher(name="Limits")
    findings = await together(
        support.investigate(ctx.request, "support"),
        limits.investigate(ctx.request, "limitations"),
    )
    return {
        "summary": await Synthesizer(name="Synthesis").synthesize(
            ctx.request, list(findings)
        )
    }
```

An Action body is declarative. Awaiting it asks Rust to create the
ActionInvocation, ActionAttempt, and Turn in that Agent's persistent Session.

## Manifest and context

Exactly one async entrypoint has literal `@workflow(...)` metadata:

| Field | Contract |
|---|---|
| `slug` | lowercase kebab-case catalog key |
| `name` | user-facing name |
| `description` | short purpose |
| `request_mode` | `"required"` by default; `"none"` for interaction driven by `ask_human` |
| `params_schema` | supported JSON Schema subset; `format: "model-profile"` selects configured profiles |

The runtime supplies:

| Value | Meaning |
|---|---|
| `ctx.request` | immutable launch task, or empty for `request_mode="none"` |
| `ctx.instructions` | optional run-wide guidance; also a Workflow prompt layer |
| `ctx.params` | launch parameters validated before scheduling |
| `ctx.trigger` | `manual`, `user`, or `workflow` provenance |
| `ctx.context` | fresh `{}` or one immutable bounded Project snapshot |
| `ctx.workflow_id` | current run ID |
| `ctx.project` | structured Project snapshot API |

Request and context become model data only when Workflow code passes them to
an Action.

## Public surface

| Primitive | Meaning |
|---|---|
| `Agent(...)` | local declaration; first remote use creates one participant Session |
| `await agent.set_access(...)` | change access between Turns; upgrades within the run ceiling require human approval |
| `@action(...)` | declare a model Action, prompt, options, typed result, and complete local tool list |
| `await together(...)` | explicit concurrency; direct same-Agent duplicates are rejected |
| `await ask_human(...)` | durable, schema-validated human input |
| `await wait(...)` | durable deadline derived from the effect start time |
| `await ctx.project.snapshot(...)` | bounded current Project state or cursor-based delta |
| `await publish_artifact(...)` | deterministic Project-managed text Artifact |
| `await publish_project_home(action=call)` | publish the draft created by that exact completed Action |

The only exports are `Agent`, `ArtifactRef`, `HumanMessage`, `ProjectContext`,
`WorkflowContext`, `action`, `ask_human`, `publish_artifact`,
`publish_project_home`, `together`, `wait`, and `workflow`.

Ordinary `if`, `for`, `while`, functions, collections, and exceptions are the
control language. Arbitrary imports, filesystem/network access, subprocesses,
environment access, reflection, and dynamic code are outside the ABI.

## Action options and tools

`@action` accepts a prompt string or docstring plus:

- `tools=[...]`: complete static local-tool request; default `[]`;
- `search_context_size`: hosted search retrieval size;
- `reasoning_effort`: per-Action model compute override;
- `finalize`: optional `after_search` or `always` no-tool finalization Turn.

Tool names must be static, non-empty, and unique. Rust rejects unknown tools,
filters Workspace tools by access, admits Project tools only for explicitly
declaring Workflow Actions, then stores the exact definitions and hash in the
Turn. Hosted web search remains provider-controlled and is not a local tool
name.

Return annotations `dict`, `list`, `bool`, `int`, and `float` request typed JSON
parsing. Repair uses an empty Registry.

An Agent declares one of `model_only`, `read_only`, `workspace`, `research`, or
`full_access`. The launch access is a hard ceiling; a Session-origin launch is
also bounded by that Session. Agent class overrides cannot widen it. Each Turn
keeps the access snapshot captured at creation.

Agent `model=""` inherits the explicitly selected run profile. A non-empty
value selects another configured profile. Route resolution and all non-secret
provider settings are frozen in `ModelRouteSnapshot` before the Turn exists.

## Effect wire protocol

The isolated runner reserves stdout for newline-delimited JSON:

```json
{"id":"root/together:0/branch:0/effect:0/invoke_action","kind":"invoke_action","payload":{"agent_instance_id":"...","action_name":"investigate","arguments":{"question":"..."},"tools":["read_file"]}}
{"id":"root/together:0/branch:0/effect:0/invoke_action","ok":true,"result":{"output":"...","turn_id":"..."}}
```

Frames are limited to 16 MiB in both directions. Python permits at most 64
in-flight effects; Rust uses a bounded response channel and propagates reader,
writer, or handler failure immediately.

The request ID is both response correlation and durable idempotency identity.
Sequential effects reserve stable paths; `together` gives each branch a stable
subpath, so completion order cannot change identity. Rust journals the exact
kind and payload hash. Reusing a path with another request is a protocol error.

The complete effect set is:

```text
create_agent       set_agent_access   invoke_action
wait               ask_human          project_snapshot
publish_artifact   publish_project_home
complete
```

Unknown effects and malformed or cross-run IDs fail closed.

## Replay and suspension

On restart, the immutable source runs again from its entrypoint. Completed
effects return their stored results. A started host effect redispatches only
under its deterministic, idempotent domain contract. Source or runtime ABI hash
drift fails closed.

Model tool calls are not Workflow effects. A FunctionCall enters canonical
Session context before dispatch; output enters canonical context before another
sample. Recovery never executes an old call. A missing output becomes
`"aborted"`, after which the same Agent observes reality and decides whether a
new call is needed.

When all live effect futures are `ask_human` or `wait` suspensions, Rust stops
the idle Python process and releases the run permit. An answer or deadline
restarts source replay. A wait deadline is computed from the journaled effect's
`started_at` and interval.

## Catalog

The catalog truth is the filesystem:

```text
workflows/builtin/<slug>/workflow.py
<data-dir>/projects/<project-id>/workflows/<slug>/workflow.py
```

Both roots use the same AST validator. Saving a Project Workflow replaces that
editable slug; existing runs retain their immutable source and ABI snapshots.
Validation returns only manifest, Agent/Action declarations, tool names, and
diagnostics. It does not manufacture a second feature summary.
