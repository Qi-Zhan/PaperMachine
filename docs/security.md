# Security boundaries

PaperMachine is local-first software, not a hardened multi-tenant service. It
has no authentication layer, binds to `127.0.0.1`, and rejects non-loopback
Host headers.

## Three authority surfaces

The runtime keeps three surfaces independent:

- Workflow code operates on PaperMachine-managed Project state only through
  typed host effects.
- Agent local tools operate on the user Workspace and, where allowed, ordinary
  host files. Every access preset denies PaperMachine managed roots.
- Hosted web search is a provider capability. It is not a local tool and does
  not expand filesystem or child-process authority.

Prompt text cannot widen any of these boundaries.

## Workflow isolation

WorkflowPrograms are executable Python, but never run in the server process.
Before registration, the AST validator limits source size, permits only
`from papermachine import ...`, requires one literal `@workflow` manifest, and
rejects direct filesystem, network, subprocess, environment, reflection, and
dynamic-code APIs.

Validation is defense in depth. The real boundary is the OS sandbox: Workflow
Python receives only a disposable materialized runtime directory, synthetic
home/temp directories, a small cleared environment, and the bounded JSONL
effect protocol. Rust validates effect shape, identity, ownership, status, and
schema before any durable mutation. Missing platform isolation fails closed.

Every Session freezes its Workflow source and Python ABI hashes. A mismatch on
start or recovery fails before Python or an effect runs.

## Project data and publication

`ctx.project.changes()` is fixed to the calling Session's Project. It derives
current entity snapshots from the durable change log, filters the caller's own
records, deduplicates a page, emits tombstones for deleted entities, chunks
large text Artifacts, and returns binary metadata without bytes. Cursors are
opaque, bound to the exact query, and fail closed when malformed or out of
range. A page is bounded to about 1 MiB. Workflows may set
`exclude_current_program=True`; matching historical runs are skipped before
snapshot materialization, preventing a derived Workflow from consuming its own
outputs.

Project data is not injected into ordinary Agents. Workflow code must pass the
returned snapshots as Action data. `publish_artifact` and
`publish_project_home` are typed host effects; model tools cannot access their
managed files.

Project Home publication accepts one exact awaited, completed Action, verifies
its Session/Agent/Action provenance, extracts the complete HTML document, writes
immutable source and page Artifacts, then atomically
updates the canonical pointer. Identical content reuses the current revision.
No Workflow slug or Agent class is privileged.

Generated HTML remains untrusted. Raw Artifact delivery uses `nosniff` and a
restrictive sandbox CSP. The Web client sanitizes again with DOMPurify:
scripts, styles, forms, interactive controls, frames, objects, external media,
SVG/MathML, unsafe attributes, and non-Web links are removed. Raster images are
accepted only as bounded `data:image/...;base64` URLs.

## ToolRegistry and access

The host builds one immutable ToolRegistry for every Turn and persists its
sorted definitions and SHA-256. Missing executors, changed definitions, forged
hashes, and calls outside membership fail closed. A child Agent's Registry does
not contain `spawn_agent`.

| Access | Default local tools | Filesystem | Child network |
| --- | --- | --- | --- |
| `model_only` | collaboration | none | denied |
| `read_only` | collaboration, command/process | ordinary host read; no write | denied |
| `workspace` | collaboration, command/process/patch | ordinary host read; Workspace write | denied |
| `full_access` | collaboration, command/process/patch | host read/write except managed roots | allowed |

Bare `@action` uses that default. `tools=[]` exposes no local tool; a non-empty
list selects an exact subset and never grants authority beyond access. Hosted
search appears only when `search_context_size` is set and the frozen provider
route declares support, regardless of the local access preset.

Session access is the ceiling. Agent overrides and child Agents cannot exceed
it; children also cannot exceed their parent. In-Session upgrades require a
typed HumanRequest grant and occur between Turns. Each Turn retains the exact
authorization snapshot with which it was created.

## Native tools and processes

The native surface is deliberately small:

- `exec_command` starts a sandboxed command and returns directly or yields a
  bounded `process_id`;
- `write_stdin` writes to or polls a process owned by the same Session, Agent,
  and authorization fingerprint;
- `apply_patch` applies the Codex patch grammar through nofollow,
  authorization-checked file operations.

Relative paths resolve from Workspace cwd. Managed roots are denied before all
other rules, including under `full_access`. Below `full_access`, common
credential files/directories are unreadable and Workspace `.git`, `.agents`,
and `.codex` metadata is read-only. Commands start with a filtered environment
and synthetic home/temp paths. Interrupt, access change, Turn completion,
Session Closing, Project close, and server shutdown terminate owned process
trees. Process handles are intentionally not recoverable after restart.

macOS Seatbelt behavior is covered by real filesystem and process tests. Linux
retains compile and policy coverage. Native Windows is outside the current
release test scope.

## Agent collaboration

Collaboration never bypasses Project or Session ownership. `list_agents`
returns identity, tree, derived activity, and last outcome, not transcripts or
managed files. `send_message` may target only a live Agent in the same Project.
`spawn_agent` creates a same-Session child and first Action atomically, with
depth one and a bounded child count. `interrupt_agent` may target only caller
descendants.

Human and Agent messages share the durable AgentInput inbox:
`pending -> claimed -> applied`. A claim binds one Turn and becomes applied
only with the canonical context checkpoint or terminal transaction that
consumes it. A pre-checkpoint crash lets the same Turn reclaim it. Agent-created
tasks are ordinary ActionInvocations in the same per-Agent FIFO as Workflow
Actions.

## Credentials and provider transport

Provider configuration stores only an `api_key_env` name. Credentials stay in
the server process memory and are not copied into snapshots, SQLite, Workflow
source, Artifacts, child environments, or logs. Optional providers are omitted
when their named credential is absent; required providers fail startup.

Use HTTPS endpoints. Rust provider requests run outside Workflow and command
sandboxes. Hosted search results enter canonical model context as provider
response items; capability declarations must match the concrete endpoint.

## Recovery and remaining limitations

- A canonical model FunctionCall is synced before dispatch. If recovery finds
  no output, it appends one stable `"aborted"` output and never replays the old
  call. An external side effect may already exist; the next model sample must
  observe reality.
- Workflow host effects use deterministic IDs and request hashes. Reusing an ID
  with another request fails closed.
- The macOS sandbox API is deprecated, and filesystem/process sandboxing is not
  a VM.
- Protocol frames and in-flight effects are bounded, but these limits do not
  replace OS isolation or input validation.
- Model output can still be factually wrong. Persistence and provenance provide
  inspectability, not truth.
