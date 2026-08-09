# Architecture

> A Project is a research world persistently managed by PaperMachine; a
> Workspace is the user filesystem an Agent is authorized to operate;
> structured runtime APIs connect them, and they never share storage or a
> security boundary.

## Product invariants

1. **Project is the ownership root.** It owns every Session, Workflow,
   Project skill, artifact, and human request in that research effort.
2. **Project is the research entity.** It is the sole ownership concept for a
   research effort.
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
   state, applies controls, permissions, and sandbox boundaries.
9. **A run snapshots its program.** Source, manifest, path, source owner, and
   SHA-256 are copied into the Workflow. Later files cannot change history.
10. **The execution environment is a Turn snapshot.** A Session chooses one of
    five access presets. Turn creation materializes that preset with the
    Workspace attachment, cwd, managed roots, tool capabilities, and network
    policy; model/tool/execution layers consume the resulting immutable policy.
11. **Prompt context is a Turn snapshot.** Runtime, Project, Workflow,
    Agent/Session, skill, and control layers retain content, source, and hashes;
    later edits affect only later Turns.
12. **New Session is a built-in Workflow.** The UI starts
    `interactive-agent`; its persistent Agent Session waits for a verified human
    message before each Turn and has no chat command that ends the loop. Closing
    the Session archives it and cancels this Workflow. No privileged standalone
    creation route exists.
13. **A Workflow launch is explicit and immutable.** The run snapshots its
    concrete request, validated params, optional run instructions, trigger,
    selected model profile, skills, permission ceiling, per-Agent class overrides, and
    either fresh or bounded Project context.
14. **Instructions and task data are separate.** Runtime, Project, run, Agent,
    Action contract, skill, and control instructions form the prompt snapshot.
    The run request, params, and captured context reach a model only when Python
    explicitly passes them as Action arguments.
15. **Per-Agent model choice is ordinary DSL.** One persistent Agent Session
    binds one configured model profile. Different roles use different models by
    constructing different Agents with `model=...`; there is no model-slot entity.

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
        +-- trigger -> user | workflow | timer | manual provenance
        +-- launch_context -> immutable Project snapshot (optional)
        +-- access ceiling / Agent class overrides
        +-- WorkflowParticipant -> Session
        +-- ActionInvocation -> ActionAttempt -> Turn
        +-- Team / AgentRelation / TaskScope
        +-- WorkflowTimer / Channel / Signal
        +-- HumanRequest / ControlMessage / Artifact
```

Sessions do not have parent IDs. The Project overview groups participant
Sessions by Workflow for navigation, but that grouping is a view over
WorkflowParticipant records, not a storage hierarchy.

For a `HumanMessage` interactive action, Rust resolves the referenced answered HumanRequest,
checks Workflow and Session ownership, verifies the bound string byte-for-byte,
and only then creates a `user` Turn. The action prompt and non-message arguments
become a Workflow prompt layer. Ordinary workflow-dispatched actions retain a
`workflow` Turn: the Action contract is an instruction layer and the bound
arguments are shown explicitly as Turn input data.

## Storage topology and Project lifecycle

PaperMachine has three independent path roles:

```text
resource_root/                  read-only shipped resources
  apps/web/dist/
  python/
  workflows/builtin/

data_dir/                       application-global state
  config.toml                   default provider configuration
  projects/<project-id>/        one Project's PaperMachine-owned state
    state/project.db
    rollouts/<session-id>.jsonl
    artifacts/
    workflow-runtime/
    runtime/
    prompts/
    workflows/
    skills/
  staging/                      unpublished Project construction
  trash/                        atomically retired managed state

workspace/                      user-owned files attached to a Project
```

`resource_root` is required server configuration; no current-working-directory
default or compile-time source-tree fallback is used. Startup fails before
opening application data if the built-in Workflow directory or Python DSL
validator is missing. `PAPERMACHINE_PYTHON` may select the Python executable;
otherwise PaperMachine resolves `python3` or `python` from `PATH` and verifies
Python 3.11 or newer. It does not probe installation-specific absolute paths.

The platform default `data_dir` is
`~/Library/Application Support/PaperMachine` on macOS,
`$XDG_DATA_HOME/papermachine` (or `~/.local/share/papermachine`) on Linux, and
`%LOCALAPPDATA%\PaperMachine` on Windows. `--data-dir` selects another managed
root, and `--config` selects a provider file independently. No path is
derived from the selected Project, and application data is never inferred from
the current repository.

Creating a Project allocates an ID, initializes a fresh current-schema Store in
`data_dir/staging/`, and atomically renames it to
`data_dir/projects/<project-id>` before it becomes visible. It attaches a
user-selected absolute Workspace; the Workspace and the entire managed root
must not overlap. PaperMachine creates no hidden files inside the Workspace.
Relocation changes only the Workspace attachment. Removal atomically renames
managed state into `trash/` before asynchronous deletion and never deletes
Workspace files. Relocation or removal is rejected while that Project has
resumable work.

At startup the server scans `projects/`. Every directory name must be a
ProjectId, every Store must use the one current schema, and its database must
contain exactly one matching Project row. That row is authoritative; there is
no second Project document or global SQLite catalog. The server builds an
in-memory runtime catalog and independently checks whether each attached
Workspace is available. A missing Workspace does not make the managed Project
disappear; it can be reattached. Each Project has its own Store, Workflow
catalog, Session runtime, and Workflow scheduler; process-wide semaphores still
bound concurrent work across Projects.

The HTTP representation keeps this boundary structural. Project creation and
relocation accept `workspace: { roots, primary_root }`; Project listings expose
that attachment plus `workspace_available`. There is no flattened path field.
An unavailable attachment affects execution, not Project identity or access to
managed history. Relocation increments the attachment revision, and every later
Turn snapshots the new revision.

## Workflow launch

The Project page and Session header open the same Run Workflow surface. The
Project is always the owner. `started_from_session_id` records the optional
source Session and focuses its recent Turns when a Project snapshot is captured;
the source profile also bounds the run's access, but does not create a nested
Workflow or Session hierarchy. A Project launch
navigates to the first participant Session once one exists. A Session-origin
launch keeps the current Session in focus so the new run can be inspected from
the same workbench.

Launch context has two modes:

- `fresh` carries no prior research state. The Project system prompt still
  applies because it is an instruction layer, not research data.
- `project_snapshot` captures one bounded Rust-produced view of existing
  Sessions, Turns, Workflow results, and text Artifacts. It is stored on the
  Workflow and exposed to Python as `ctx.context`. It is never automatically
  rendered into an Agent Turn; Workflow code explicitly passes only relevant
  data. It never changes during the run. Code that intentionally needs current
  state calls
  `await ctx.project.snapshot()` as a separate durable effect.

The immutable snapshot is important for reproducibility, while explicit data
flow is important for caching: unrelated Project data cannot silently enter or
change the instruction prefix of an already-running Agent Session. A
Session-origin launch prioritizes that
Session's history in the snapshot, but does not copy its mutable Session system
prompt into the new Agents. The instruction stack remains Runtime -> Project ->
Run instructions/Action contract -> Agent/Session -> Skills -> Control; request
and Action arguments remain Turn data.

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
       +-> exact ToolRegistry <--------- RunEffectContext
                 ^                         |
                 |                         +-> SessionRuntime actions
            host ToolCatalog               |
                 +-> execution sandbox ----+
                               |
                 in-memory catalog + per-Project Store/artifacts
```

The Python runner loads the snapshotted `workflow.py`, executes its async
entrypoint, and sends JSONL effect requests over stdin/stdout. Requests may be
handled concurrently, while per-Agent gates serialize actions in the same
Session. Python stdout is reserved for the protocol; ordinary prints are sent
to stderr and captured with a size limit.

## Session execution

A user Turn or workflow action follows the same core path:

1. Verify that every attached Workspace root is still a real directory at its
   recorded canonical path. If not, fail before creating a Turn.
2. Resolve the local tools before the Turn exists. A standalone user Turn gets
   all Workspace tools allowed by access; a Workflow Action starts from its
   static `tools=[...]` declaration, filters Workspace tools by access, and may
   receive declared Project tools. Atomically append the Turn with the sorted
   definitions and SHA-256 ToolSetSnapshot, immutable Workspace/authorization
   environment, Project-skill snapshot, and ordered prompt snapshot. `Turn.origin` records
   whether input is a direct/verified human message or program-generated
   Workflow work.
3. Reconstruct canonical model context by replaying that append-only rollout.
4. Render runtime, Project, Workflow, Agent/Session, enabled-skill, and control
   prompt layers into the exact provider instructions.
5. Stream a Responses API sample. Deltas remain ephemeral; completed model
   items and cursor checkpoints cross the rollout durability barrier before
   their SQLite projections. Each Session
   retains one sequential WebSocket continuation chain; unsupported providers
   fall back to HTTP SSE.
6. Execute requested tools, append stable call/result and Step lifecycle facts,
   project them, and sample again.
7. Finish the Turn by journaling its output, usage, and terminal state. SQLite
   never stores a cumulative context copy in the Turn document. If a
   provider reports an incomplete response after consuming tokens, retry usage
   is accumulated; a terminal failure persists a failed model Step and charges
   those tokens before the Turn is failed. Output-limit and reasoning-only empty
   completions are retried with low reasoning and an explicit final-answer
   instruction so a provider cannot repeatedly spend the completion on hidden
   reasoning without producing the action result.

A deliverable-producing DSL action may opt into `finalize="after_search"`. Rust
returns the durable Turn's hosted-search count to the Python DSL; after a
tool-using Turn, Python schedules one separate model-only Action on the same
Session. This preserves the full research history and cache affinity while
preventing a provider's progress narration from being accepted as the final
deliverable. The extra Action and Turn remain visible in the Session workbench.

## Providers and model profiles

The selected PaperMachine provider configuration is authoritative. By default
it is `<data_dir>/config.toml`; development may pass the repository's
`papermachine.toml` explicitly. A provider owns
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

Outside explicit demo mode, the server must load a valid provider configuration,
and all Session/Agent model selection uses its profile IDs. Workflow launch
always names a non-empty model profile and access ceiling; the API does not use
an omitted or blank value as an implicit profile selection.

History is stored locally, so providers may set `store_responses = false`.
Before every model sample the agent estimates instruction, tool-schema, output,
and history tokens. At 90% of the available history capacity, it runs a no-tool
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
use ordinary implicit caching instead. A provider's `prompt_cache_mode` setting
may pin `implicit` or `explicit` when provider behavior is already known.

Hosted tools are provider capabilities, not properties inferred from the wire
protocol. Every provider explicitly declares `hosted_web_search`; the Agent
filters hosted definitions for the selected model profile before sampling.
Local function tools such as `fetch_url` remain governed by the Turn access
snapshot independently of that provider capability.

The final model sample keeps the same instructions and history prefix, appends
a final-answer message, removes local and hosted tool definitions, and sets
`tool_choice=none`. This gives compatible providers no tool surface even when
they ignore the choice field; the input-history prefix remains cacheable. Usage
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
tools, and model streams. Stop Turn is narrower: it cancels that Turn's active
model or tool work and closes its running Steps. It does not synthesize a final
answer and is not implemented as a prompt.

`guide` is consumed at the next Agent checkpoint and appended as a user-history
item for the running action. `interrupt` terminates the current ActionAttempt;
the workflow runtime starts a new attempt for the same ActionInvocation with the
interruption text in an inspectable `control` prompt layer.
`finish` asks for one last synthesis sample; the runtime removes local and
hosted tool definitions from that request instead of relying only on
`tool_choice=none`.

Only explicit Workflow code can create a typed HumanRequest through the
`ask_human` DSL effect. Models do not receive an `ask_human` tool. An Action may
recommend escalation in its typed result, but the Workflow decides whether to
open a request. The request marks the run as requiring attention, and the
validated answer is returned to the suspended Workflow call.

Workflow-level human, timer, and signal waits are durable suspension points.
Once all Python branches are waiting on replayable effects, the isolated Python
process exits and the scheduler releases its global run permit. The Workflow
supervisor remains lightweight: it wakes on an answered direct HumanRequest, a
due active timer, or a matching durable Signal, sets the run runnable, and
replays the immutable program against its effect journal. Parallel branches can
therefore combine `ask_human`, background timers, and Channels without keeping
one idle process per long-lived Workflow.

Workflow Agents declare `model_only`, `read_only`, `workspace`, `research`, or
`full_access`. The user-selected Workflow profile is a hard ceiling for the
entire run. If launched from a Session it must also be at or below that source
Session's profile. Per-run Agent class overrides are validated at launch and
may only narrow the run; a class declaration above the run ceiling is clamped.
These launch-time choices are already authorized and create no HumanRequest.
A later `set_access` upgrade within the fixed Workflow ceiling suspends on a
boolean HumanRequest; an attempt above the ceiling fails. Downgrades do not
require approval. Direct Session changes are accepted only between Turns, and
the UI separately confirms `full_access`.

## Persistence and recovery

Each Project's SQLite database stores its authoritative Project row, JSON domain
documents, indexed ownership/status columns, ordered Session and Workflow
events, and the last projected sequence for each Session. Each Session's JSONL
rollout is the canonical model-context and execution history. One writer per
Session flushes each stable record before applying its SQLite projection;
startup repairs only an incomplete final line and replays any records newer
than the projection cursor. Its Artifacts are stored under that same Project's
managed directory using content-hashed metadata records. The web client uses
SSE for live deltas and refreshes durable views for lifecycle changes.

At startup, unfinished standalone Session Turns are settled and every
non-terminal Workflow is recovered. A standalone Turn with a durable terminal
candidate is committed without another provider sample. Otherwise it becomes
`interrupted`; its partial rollout context, recovered tool outputs, and an
explicit process-restart marker are committed for the next user-directed Turn.
It is never automatically sampled again. The Session view exposes the canonical
rollout version/sequence, its SQLite projection sequence, and the IDs of
standalone interrupted Turns that may be resumed. Explicit Resume creates a
new user Turn in the same Session over the committed context; the interrupted
Turn remains terminal and is never reopened. Workflow-owned Turns are recovered
only by their Workflow runtime and are not user-resumable through this endpoint.
A recovered Workflow reruns its
immutable Python source from the entrypoint. DSL operations use deterministic
logical effect paths; SQLite journals each path with the exact request hash,
status, result, and error. A completed effect returns its stored result, while
an effect that was started when the process disappeared is redispatched against
deterministic resource IDs defined by that effect's contract.

Action Turns append context mutations, usage, completed-model-step and hosted
search cursors, and a terminal candidate message to the Session rollout.
Ordinary additions are append records; compaction and trimming are explicit
replacement records that do not mutate prior entries. Every local Tool Step
persists its provider call ID, effect disposition, and the durability boundary
between `prepared` and `executing`. Recovery keeps the same ActionInvocation,
ActionAttempt, and Turn. It reuses completed results; executes a still-prepared
call after durably marking it `executing`; replays an executing `pure` or
`idempotent` call with the same effect ID; asks a `reconcilable` tool to inspect
external state first; and never automatically replays an executing `unknown`
call. The last case becomes `execution_unknown` and is supplied to the next
model sample as a real function-call output. This avoids repeating a
checkpointed completed sample or counting the Action start twice. The effect
journal is returned in Workflow views and is visible in the Session inspector.

The Project Page is itself backed by ordinary Workflow data. A built-in
`project-summary` run reads a bounded Rust-produced Project snapshot and
starts one ordinary tool-capable Action on a persistent summary Agent. Project
page access is a structured host API, not filesystem access: the Action
declares read, semantic block patch, and materialized preview tools. Its Turn
stores exactly those definitions and no Workspace tools, without granting
access to PaperMachine managed files or expanding its Workspace preset. The Agent can edit and inspect the
draft for as many model/tool steps as it needs and ends naturally when it is
satisfied; no Workflow-level review state machine or evaluator Action decides
for it.

After that Action completes, one replay-safe publication effect stores the
block source and exposes an immutable semantic HTML Artifact. The UI fetches
the newest page Artifact, removes active content, inline styles, forms, and
external media, and renders the remaining semantic markup directly as the
Project home page. There is no fixed Project dashboard and no iframe boundary.
Manual refreshes are one-shot runs, while an active refresh policy is simply a
non-terminal scheduled Workflow. Its first refresh is full; later timer firings
request only changes since the prior `captured_at` cursor and skip model work
when that delta is empty. The same summary Agent Session retains prior Turns and
the next draft starts from the last published block source. There is no separate
summary-instance table or privileged summary daemon.

## Skills and workflow roots

Project-local skills live at:

```text
<data-dir>/projects/<project-id>/skills/<slug>/SKILL.md
```

The user-editable Project system prompt lives at:

```text
<data-dir>/projects/<project-id>/prompts/system.md
```

Workflow source lives at:

```text
workflows/builtin/<slug>/workflow.py
<data-dir>/projects/<project-id>/workflows/<slug>/workflow.py
```

Built-in and user workflows use the same validator and runtime. The directory
split expresses ownership and review status, not extra privileges.

See [prompt model](prompt-model.md) for exact layering, editing, provenance,
message-origin, and cache semantics.

## Deployment model

PaperMachine is a local-first, single-user application. It uses one Python
workflow engine and exposes a catalog, generator, structural inspector,
validator, and source editor in the Workflow page.
