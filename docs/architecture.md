# Architecture

## Product invariants

1. **Project is the ownership root.** It owns every Session, Workflow,
   Project skill, artifact, and human request in that research effort.
2. **There is no separate Research entity.** Project is the only ownership
   concept and there is no compatibility alias.
3. **Session is the main workbench.** It is a durable multi-turn conversation;
   its Turns may be verified human messages or workflow-dispatched work.
4. **Turn is the model-execution boundary.** Model samples and tool calls are
   Steps folded under the Turn in the UI.
5. **Workflow belongs to Project.** `started_from_session_id` optionally records
   where it was launched; Project-level/background Workflows need no starting Session.
6. **Agent instance maps to one Session.** An action invocation executes as one
   or more attempts, and every attempt uses a Turn in that Session.
7. **One Agent Session is serial.** Different Agent Sessions may execute actions
   concurrently; two actions for the same Agent cannot be passed to one
   `together(...)` call.
8. **Python owns control flow; Rust owns effects.** Workflow code may decide what
   to do next, but only Rust creates Sessions, calls models/tools, persists
   state, applies controls, and enforces budgets.
9. **A run snapshots its program.** Source, manifest, path, source owner, and
   SHA-256 are copied into the Workflow. Later files cannot change history.
10. **Access is a Turn snapshot.** A Session chooses one of five profiles; a
    Turn captures it at creation, and model/tool/execution layers all enforce it.
11. **Prompt context is a Turn snapshot.** Runtime, Project, Workflow,
    Agent/Session, skill, and control layers retain content, source, and hashes;
    later edits affect only later Turns.
12. **New Session is a built-in Workflow.** The UI starts
    `interactive-agent`; its persistent Agent Session waits for a verified human
    message before each Turn. No privileged standalone creation route exists.

## Ownership graph

```text
Project
  +-- Session [origin=user]
  |     +-- Turn
  |           +-- AgentStep [model | tool | workflow | system]
  +-- Session [origin=workflow_agent]
  |     +-- Turn ...
  +-- Workflow
        +-- started_from_session_id -> Session (optional)
        +-- WorkflowParticipant -> Session
        +-- ActionInvocation -> ActionAttempt -> Turn
        +-- Team / AgentRelation / TaskScope
        +-- WorkflowTimer / Channel / Signal
        +-- HumanRequest / ControlMessage / Artifact
```

Sessions do not have parent IDs. The Project overview groups participant
Sessions by Workflow for navigation, but that grouping is a view over
WorkflowParticipant records, not a storage hierarchy.

For an interactive action, Rust resolves the referenced answered HumanRequest,
checks Workflow and Session ownership, verifies the bound string byte-for-byte,
and only then creates a `user` Turn. The action prompt and non-message arguments
become a Workflow prompt layer. Ordinary workflow-dispatched actions retain a
`workflow` Turn and show their generated objective explicitly.

## Runtime layers

```text
Vue client
  Project overview | Session thread + inspector | Workflow page
                               |
                          HTTP + SSE
                               |
PaperMachine server ------------+
  | ModelRouter -> profile -> provider client
  | SessionRuntime                       | WorkflowScheduler
  |                                      | PythonWorkflowRuntime
  +-> AgentRuntime                       +-> sandboxed Python runner
       | Responses WebSocket                  | JSONL effect requests
       | (HTTP SSE fallback)
       | sample/tool loop                     v
       +-> ToolRegistry <-------------- RunEffectContext
                 |                         |
                 +-> execution sandbox     +-> SessionRuntime actions
                               |
                 SQLite store + events + artifacts + Project directories
```

The Python runner loads the snapshotted `workflow.py`, executes its async
entrypoint, and sends JSONL effect requests over stdin/stdout. Requests may be
handled concurrently, while per-Agent gates serialize actions in the same
Session. Python stdout is reserved for the protocol; ordinary prints are sent
to stderr and captured with a size limit.

## Session execution

A user Turn or workflow action follows the same core path:

1. Persist the Turn with immutable access, Project-skill, and ordered prompt
   snapshots. `Turn.origin` records whether input is a direct/verified human
   message or program-generated Workflow work.
2. Rebuild history from completed prior Turns in that Session.
3. Render runtime, Project, Workflow, Agent/Session, enabled-skill, and control
   prompt layers into the exact provider instructions.
4. Stream a Responses API sample and persist model-step events. Each Session
   retains one sequential WebSocket continuation chain; unsupported providers
   fall back to HTTP SSE.
5. Execute requested tools, persist inputs/outputs, and sample again.
6. Finish the Turn with output, usage, history, and terminal Step states. If a
   provider reports an incomplete response after consuming tokens, retry usage
   is accumulated; a terminal failure persists a failed model Step and charges
   those tokens before the Turn is failed. Output-limit and reasoning-only empty
   completions are retried with low reasoning and an explicit final-answer
   instruction so a provider cannot repeatedly spend the completion on hidden
   reasoning without producing the action result.

## Providers and model profiles

`papermachine.toml` is the authoritative model configuration. A provider owns
the endpoint, credential environment-variable name, transport policy, cache
mode, and default reasoning policy. A model profile maps a stable user-facing
ID to one provider's concrete model ID and context window. Sessions and workflow
Agents store the profile ID; `ModelRouter` resolves it immediately before the
request and records profile, provider, and upstream model in Step metadata.

This keeps workflow source portable: an Agent can inherit the Workflow's
profile or explicitly select another configured profile, while the same server
can route planner, researcher, evaluator, and grader Sessions to different
providers. The current provider client implements the OpenAI Responses wire
shape, including compatible implementations such as DeepSeek's Responses
endpoint. Provider formats are an adapter boundary, not a Workflow DSL concern.

`--codex-home` is only an opt-in single-provider importer used when a
PaperMachine config is absent. Codex configuration is not the model registry.

History is stored locally, so `disable_response_storage=true` is supported.
Before every model sample the agent estimates instruction, tool-schema, output,
and history tokens. At 90% of the available history budget, it runs a no-tool
semantic compaction sample and replaces early model-visible history with a
handoff summary. Durable Turns, Steps, tool calls, and outputs remain unchanged
for inspection. Deterministic middle-history trimming remains the final safety
bound if the compacted history is still too large.

Every normal and compaction request derives `prompt_cache_key` from the durable
Session ID and model. It is a routing-affinity key rather than a prompt-content
digest, so all actions, response schemas, tool sets, samples, compactions, and
later Turns in one Session retain the same key. The provider still decides
which tokens are reusable by matching the actual prompt prefix. Different
Sessions are deliberately isolated even when they render the same instructions:
some Responses-compatible gateways appear to interpret the routing hint as a
wider response-cache namespace, making cross-Session sharing unsafe under
concurrency.

Prompt-cache mode is provider-aware. In `auto` mode the client performs one
small capability probe per model. Providers that accept GPT-5.6 explicit cache
breakpoints receive `prompt_cache_options.mode=explicit` and a breakpoint at
the end of the stable developer instructions. Providers that reject that field
use ordinary implicit caching instead. `PAPERMACHINE_PROMPT_CACHE_MODE` may pin
`implicit` or `explicit` when provider behavior is already known.

The final model sample no longer rewrites instructions or removes tool
definitions. It appends a final-answer message and sets `tool_choice=none`, so
the previously cached prefix and WebSocket response chain remain valid. Usage
records distinguish cache reads (`cached_input_tokens`) from first-time cache
writes (`cache_write_input_tokens`). Model-step output also records the actual
transport, cache mode/key, breakpoint use, continuation decision, and fallback
reason.

A routing key does not itself guarantee a cache read: the provider still
requires an exact prefix and may impose a minimum cacheable length. A fresh
one-sample Agent can only benefit from a prefix written by an earlier matching
request, so low read ratios remain possible when every Agent has unique or
short instructions.

Within one Session, the model client retains the most recent response on the
same WebSocket connection across local tool loops and later user/workflow
Turns. A later sample sends `previous_response_id` and only the strict input
suffix when its model, instructions, tools, cache settings, output schema, and
prior input/output prefix still match. Response-only controls such as
`tool_choice` may change without breaking that chain. Any input property or
history mismatch starts a full request instead and records the reason. Failed,
interrupted, cancelled, expired, and evicted connections restart from locally
persisted full history. This continuation path works with `store=false`; it
avoids replaying history over the wire but is distinct from the provider's
billed prompt-cache read/write counters.

## Human control

Pause is a Workflow state. Runtime and Agent checkpoints wait while paused;
an in-flight provider request is not forcibly rewound. Resume releases the next
checkpoint. Cancel propagates a cancellation token to the workflow, actions,
tools, and model streams.

`guide` is consumed at the next Agent checkpoint and appended as a user-history
item for the running action. `interrupt` terminates the current ActionAttempt;
the workflow runtime starts a new attempt for the same ActionInvocation with the
interruption text in an inspectable `control` prompt layer.

Both workflow code and the model-visible `ask_human` tool can create a typed
HumanRequest. The request marks the run as requiring attention; an action-level
request also moves its Turn and Session to `waiting_for_human`. The validated
answer is returned to the suspended call as its result.

Workflow Agents declare `model_only`, `read_only`, `workspace`, `research`, or
`full_access`. The origin Session profile is the initial creation ceiling.
Creation above that ceiling and every later upgrade suspend on a boolean
HumanRequest; downgrades do not. Direct Session changes are accepted only
between Turns, and the UI separately confirms `full_access`.

## Persistence and recovery

SQLite stores JSON documents plus indexed ownership/status columns and ordered
append-only Session and Workflow event streams. Artifacts are stored on disk
under content-hashed metadata records. The web client uses SSE for live deltas
and refreshes durable views for lifecycle changes.

Unfinished standalone Session Turns and every non-terminal Workflow are
recovered at startup. A recovered Workflow reruns its immutable Python source
from the entrypoint. DSL operations use deterministic logical effect paths;
SQLite journals each path with the exact request hash, status, result, and
error. A completed effect returns its stored result, while an effect that was
started when the process disappeared is safely redispatched against
deterministic resource IDs.

Action Turns checkpoint model history, usage, completed-model-step and hosted
search cursors, and a terminal candidate message. Recovery keeps the same
ActionInvocation, ActionAttempt, and Turn, cancels only orphaned in-flight
Steps/human-tool waiters, reconstructs a completed local-tool result from the
Step's durable call ID, gives an execution-unknown tool an explicit restart
result, and resumes at the next model sample. This avoids repeating a
checkpointed completed sample or charging the Action-start budget twice. The
effect journal is returned in Workflow views and is visible in the Session
inspector.

## Skills and workflow roots

Project-local skills live at:

```text
<project-root>/.papermachine/skills/<slug>/SKILL.md
```

The user-editable Project system prompt lives at:

```text
<project-root>/.papermachine/prompts/system.md
```

Workflow source lives at:

```text
workflows/builtin/<slug>/workflow.py
<project-root>/.papermachine/workflows/<slug>/workflow.py
```

Built-in and user workflows use the same validator and runtime. The directory
split expresses ownership and review status, not extra privileges.

See [prompt model](prompt-model.md) for exact layering, editing, provenance,
message-origin, and cache semantics.

## Explicit non-goals

- Codex app-server, CLI, TUI, rollout, wire-protocol, or IDE compatibility.
- Treating the Codex CLI as a subprocess or library kernel.
- MCP, plugins, apps, connectors, approvals compatibility, or a marketplace.
- A second workflow engine based on a separate graph or node abstraction.
- A browser Python IDE. The Workflow page is a catalog, generator, structural
  inspector, validator, and saver with an advanced source escape hatch.
- A distributed, authenticated multi-tenant scheduler in the local-first release.
