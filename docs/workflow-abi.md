# Python Workflow ABI

PaperMachine WorkflowPrograms are isolated async Python programs. Python describes
control flow; Rust owns every durable mutation, model Turn, permission check,
and Project resource.

## Minimal source

```python
from papermachine import Agent, action, together, workflow


class Researcher(Agent):
    access = "model_only"

    @action(
        search_context_size="low",
        reasoning_effort="high",
        tools=[],
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
ActionInvocation, ActionAttempt, and Turn for that Agent in the current
Session.

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
| `ctx.instructions` | optional Session-wide guidance; also a Session prompt layer |
| `ctx.params` | launch parameters validated before scheduling |
| `ctx.trigger` | `manual` or source-Session `user` provenance |
| `ctx.session_id` | current Session ID |
| `ctx.project` | paged Project entity-snapshot API |

Request and Project snapshots become model data only when Workflow code passes
them to an Action. No Project content is injected into ordinary Agents.

`await ctx.project.changes(after_cursor=cursor, exclude_current_program=False)`
returns:

```json
{
  "cursor": "opaque-host-cursor",
  "changed": true,
  "has_more": false,
  "resources": [{
    "kind": "turn",
    "id": "...",
    "session_id": "...",
    "deleted": false,
    "data": {}
  }]
}
```

Pages are derived from the change log rather than stored as another projection.
Large text Artifacts span pages; binary Artifacts expose metadata only. Setting
`exclude_current_program=True` skips historical runs owned by the caller's
WorkflowProgram before materializing snapshots; its cursor cannot be reused
with another query.

## Public surface

| Primitive | Meaning |
|---|---|
| `Agent(...)` | local declaration; first remote use creates one durable Agent under the current Session |
| `await agent.set_access(...)` | change access between Turns; upgrades within the Session ceiling require human approval |
| `@action(...)` | declare a model Action, prompt, options, typed result, and complete local tool list |
| `await together(...)` | explicit concurrency; direct same-Agent duplicates are rejected |
| `await ask_human(...)` | durable, schema-validated human input |
| `await wait(...)` | durable deadline derived from the effect start time |
| `await ctx.project.changes(...)` | opaque cursor and bounded current Project entity snapshots |
| `await publish_artifact(...)` | deterministic Project-managed text Artifact |
| `await publish_project_home(action=call)` | publish that exact completed Action's HTML result |

The only exports are `Agent`, `ArtifactRef`, `HumanMessage`, `ProjectContext`,
`SessionContext`, `action`, `ask_human`, `publish_artifact`,
`publish_project_home`, `together`, `wait`, and `workflow`.

Ordinary `if`, `for`, `while`, functions, collections, and exceptions are the
control language. Arbitrary imports, filesystem/network access, subprocesses,
environment access, reflection, and dynamic code are outside the ABI.

## Action options and tools

`@action` accepts a prompt string or docstring plus:

- `tools=None` (the omitted default): access-allowed native and collaboration tools;
- `tools=[]`: empty local Registry;
- `tools=[...]`: exact static subset request;
- `search_context_size`: hosted search retrieval size;
- `reasoning_effort`: per-Action model compute override;
- `finalize`: optional `after_search` or `always` no-tool finalization Turn.

Tool names must be static, non-empty, and unique. Rust rejects unknown tools,
filters native tools by access, removes `spawn_agent` from child Agents, then
stores the exact definitions and hash in the Turn. The native tools are
`exec_command`, `write_stdin`, and `apply_patch`; collaboration tools are
`list_agents`, `send_message`, `wait_agent`, `spawn_agent`, and
`interrupt_agent`. Hosted web search remains provider-controlled and is not a
local tool name.

Return annotations `dict`, `list`, `bool`, `int`, and `float` request typed JSON
parsing. Repair uses an empty Registry.

An Agent declares one of `model_only`, `read_only`, `workspace`, or
`full_access`; the default is `workspace`. The launch access is a hard ceiling; a Session-origin launch is
also bounded by that Session. Agent class overrides cannot widen it. Each Turn
keeps the access snapshot captured at creation.

Agent `model=""` inherits the Session's default model profile. A non-empty
value selects another configured profile. Route resolution and all non-secret
provider settings are frozen in `ModelRouteSnapshot` before the Turn exists.

Collaboration calls are model tools, not Python Workflow functions:

```text
list_agents()
send_message(agent_id, message, start_turn=false)
wait_agent(action_invocation_ids, timeout_ms?)
spawn_agent(task, name?, access?)
interrupt_agent(agent_id)
```

Queue-only messages enter durable AgentInput. A started message or spawn creates
an ordinary `agent_task` Action in the same ActionRunner used by Workflow
Actions. `wait_agent` stores no second wait record. A child inherits its
parent's model, prompt, role, class, skills, and same-or-lower access; spawn
depth is one.

## Effect wire protocol

The isolated runner reserves stdout for newline-delimited JSON:

```json
{"id":"root/together:0/branch:0/effect:0/invoke_action","kind":"invoke_action","payload":{"agent_id":"...","action_name":"investigate","arguments":{"question":"..."},"tool_policy":[],"web_search_context_size":"low"}}
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
wait               ask_human          project_changes
publish_artifact   publish_project_home
complete
```

Unknown effects and malformed or cross-Session IDs fail closed.

## Replay and suspension

On restart, the immutable source runs again from its entrypoint. Completed
effects return their stored results. A started host effect redispatches only
under its deterministic, idempotent domain contract. Source or runtime ABI hash
drift fails closed.

Model tool calls are not Workflow effects. A FunctionCall enters canonical
Agent context before dispatch; output enters canonical context before another
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
editable slug; existing Sessions retain their immutable source and ABI snapshots.
Validation returns only manifest, Agent/Action declarations, tool names, and
diagnostics. It does not manufacture a second feature summary.
