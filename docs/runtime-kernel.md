# Runtime kernel contract

Status: current clean-break contract, 2026-08-10.

PaperMachine opens only schema 20, Session rollout version 3, and the current
Workflow ABI. It does not migrate earlier managed state or Workflow source.
User Workspace files are outside this clean break and are never rewritten or
deleted with Project state.

## Product boundary

> Project is a research world persistently managed by PaperMachine; Workspace
> is the user filesystem an Agent is authorized to operate; structured runtime
> APIs connect them, and they never share storage or a security boundary.

Project owns every Session, Workflow, prompt, Skill, Artifact, journal,
HumanRequest, and control message. Workspace is one user-owned filesystem
attachment and the default Agent cwd. `interactive-agent`, `goal`, and
`project-summary` are ordinary built-in Workflows; the kernel contains no
program-specific execution branch.

## Minimal durable model

The Workflow layer persists:

```text
Workflow source snapshot
WorkflowEffect(key, request hash, started/completed/failed, result)
Participant -> Session
ActionInvocation -> ActionAttempt -> Turn
HumanRequest / ControlMessage / Artifact
```

Python `if/for/while` and `together` are the control language. A durable `wait`
is one WorkflowEffect whose deadline is derived from its `started_at` and
interval.

Participant membership is immutable and ends with its Workflow.

## Immutable Turn

The host resolves and atomically stores:

```text
ModelRouteSnapshot
TurnEnvironmentSnapshot
ToolSetSnapshot
PromptSnapshot
```

The route pins provider behavior and a non-secret configuration hash. The
environment pins Workspace revision and authorization. The ToolSet is the exact
sorted Registry visible to the model. The prompt freezes fully resolved
instructions, including Skill text. Recovery fails closed on drift.

Workflow Actions request local tools statically. Access may remove Workspace
tools; only explicit Action declaration admits Project tools. Hosted search is
separate provider capability. Registry membership is never a permission bypass:
tool path checks and command sandboxing enforce the same authorization again.

## Filesystem policy

| Access | Host read | Write | Child network | Hosted research |
|---|---|---|---|---|
| `model_only` | none | none | none | none |
| `read_only` | ordinary host files | none | none | none |
| `workspace` | ordinary host files | Workspace | denied | none |
| `research` | ordinary host files | Workspace | denied | controlled fetch/search |
| `full_access` | host except managed state | host except managed state | allowed | controlled fetch/search |

Relative paths resolve against Workspace. Managed roots are never readable or
writable, even under `full_access`. Below `full_access`, credential roots and
common credential files are also unreadable. Symlink traversal and replacement
races fail closed. macOS Seatbelt and Linux bubblewrap apply the same policy to
commands. Native Windows is outside current release testing.

## Project lifecycle and Store

The Project catalog is a resilient scan of independent managed directories;
there is no global database. A loaded Project has one `ProjectHandle` with a
bounded `StoreHandle` and lazy runtime. Normal work reads the Project map.
Relocate/remove take its write lock, recheck active work, stop and join the
runtime/Store, then change the catalog. There is no separate slot/lease state
machine.

One bounded Store queue (256) feeds one blocking thread per loaded Project.
SQLite, managed filesystem work, and hashing do not run on Tokio workers.
Managed paths use capability-rooted nofollow operations, atomic replace,
directory fsync, and root-confined deletion.

## Canonical Session rollout

Session JSONL is canonical model history. It has only:

```text
TurnCreated       includes the required ActionAttempt attachment
ContextCheckpoint model context mutation, usage, cursors, terminal candidate
TurnUpdated       Turn boundary and acknowledged controls
```

AgentSteps, Session events, and streaming deltas are projection or live UI
state. They never become canonical rollout items.

The writer syncs JSONL before SQLite projection. A validated FunctionCall is
checkpointed before dispatch. Its FunctionCallOutput is checkpointed before
Step completion or another sample. Replay streams records and repairs only an
incomplete final line.

Recovery uses Codex-style at-most-once model-tool semantics:

- canonical output repairs missing Tool Step projection;
- a call without output gets exactly one `"aborted"` output;
- old calls never dispatch again;
- the same Agent resumes and observes durable reality.

There is no `ModelSampleCommitted`, model-tool effect ID, disposition,
reconciliation method, or automatic uncertain-effect replay.

Workflow host effects remain deterministic and replayable at their own layer.
They do not share recovery machinery with model tools.

## Transactions and control

Workflow/Action transitions, usage, HumanRequest CAS, and terminal cleanup use
typed immediate transactions. Controls transition `pending -> claimed ->
applied`; application occurs in the canonical checkpoint or terminal
transaction that consumes the message.

Frames in the Rust/Python protocol are capped at 16 MiB. In-flight effects and
response channels are capped at 64. Reader, writer, or handler failure
terminates the run. Idle human/deadline waits release both Python process and
scheduler permit; terminal scheduler handles are removed.

## Release gates

The kernel is releasable only when:

- Rust format, tests, and Clippy pass for the full workspace;
- Python DSL and built-in tests pass;
- Web tests and production build pass;
- direct tools and command sandboxes agree on permissions;
- Project tools cannot enter normal Sessions;
- route, ToolSet, prompt, and canonical rollout recovery fail closed on drift;
- real-provider dogfood confirms complete model/tool traces and no replay of old
  calls.
