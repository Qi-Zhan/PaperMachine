# Runtime kernel contract

Status: active clean-break contract, 2026-08-09.

PaperMachine opens only the current schema and current Workflow ABI. User-owned
Workspace files are outside that clean break and are never migrated, deleted,
or rewritten as part of Project-state replacement.

## Product boundary

> Project is a research world persistently managed by PaperMachine; Workspace
> is the user filesystem an Agent is authorized to operate; structured runtime
> APIs connect them, and they never share storage or a security boundary.

- Project owns Sessions, Workflows, prompts, Skills, Artifacts, journals, and
  human/control state. All of it lives below PaperMachine's managed data root.
- Workspace is one user-owned filesystem attachment and the default cwd for
  Agent tools. PaperMachine stores no application metadata there.
- Every Turn belongs to one Workflow ActionAttempt. `interactive-agent`,
  `goal`, and `project-summary` are ordinary built-in Python Workflows; the
  kernel has no slug-specific lifecycle branch.

## Immutable Turn contract

Before creating a Turn, the host materializes three immutable snapshots:

```text
Turn
  ModelRouteSnapshot       exact provider route and non-secret config hash
  TurnEnvironmentSnapshot Workspace revision and authorization hash
  ToolSetSnapshot          sorted exact tool definitions and SHA-256
  PromptSnapshot           ordered resolved instructions and SHA-256
```

`ModelRouteSnapshot` pins the profile, provider, upstream model, context
window, capabilities, final reasoning effort, and a SHA-256 over all relevant
non-secret provider/model configuration. API keys never enter the snapshot.
Every sample and hosted-tool decision uses this route. Recovery fails closed if
the current router cannot reproduce it.

The trusted host ToolCatalog constructs an exact per-Turn ToolRegistry.
Workflow Actions provide a static `tools=[...]` list. Access may remove
Workspace tools; Project tools enter only through the declaring Action.
The built-in interactive Action declares the ordinary Workspace tools and is
filtered by its access preset; it never receives Project tools automatically. Missing executors,
definition drift, invalid hashes, or calls outside Registry membership fail
closed.

Skills are instructions only. A Project may store an editable `SKILL.md`, but
the Turn freezes its fully resolved instructions in PromptSnapshot. Recovery
does not read a live Skill and no scripts/assets package is copied into runtime
state.

## Filesystem and sandbox policy

The five access presets materialize as follows:

| Access | Host reads | Writes | Command child network | Rust-hosted research |
|---|---|---|---|---|
| `model_only` | none | none | none | none |
| `read_only` | allowed | none | none | none |
| `workspace` | allowed | Workspace only | denied | none |
| `research` | allowed | Workspace only | denied | controlled fetch/search |
| `full_access` | allowed | host, except managed state | allowed | controlled fetch/search |

Relative paths resolve against the Workspace cwd. Direct file tools and local
commands consume the same materialized rule. PaperMachine managed roots are
always unreadable and unwritable, including under `full_access`. Below
`full_access`, `.env*`, common credential filenames, and user credential roots
are also unreadable; Workspace `.git`, `.agents`, and `.codex` metadata is
read-only.

Path traversal, symlink following, and replacement races fail closed. One
sandbox manager prepares every untrusted child, clears the environment, applies
filesystem/network rules, bounds output and time, and kills descendants on
cancellation. macOS uses Seatbelt; Linux/WSL2 uses bubblewrap. Native Windows
is not part of the current test or release scope.

## Managed storage and lifecycle

```text
data_dir/
  projects/<project-id>/
    state/project.db
    rollouts/<session-id>.jsonl
    artifacts/
    prompts/
    workflows/
    skills/
    workflow-runtime/        disposable Python scratch
    runtime/sandboxes/       disposable Turn scratch
  staging/
  trash/
```

The Project database row is authoritative; there is no global Project database
or duplicate Workflow-program table. Startup performs a resilient directory
scan: one damaged unrelated Project yields a diagnostic but does not block
other valid Projects. Built-in and Project-local Workflow files are the catalog
truth; each Run still stores immutable source and ABI hashes.

A Project slot is `Open`, `Closing`, or `Retired`. Creating/registering work
holds a read lease. Relocate/remove takes the write lease, enters `Closing`,
rechecks active work, and then mutates. Removal stops and joins that Project's
runtime and Store before moving managed state to trash; the Workspace is never
a deletion target.

Every loaded Project owns one bounded StoreHandle queue (256) and one blocking
thread that holds the synchronous Store core. Server, Session, Workflow, and
tool runtimes call it asynchronously, so SQLite, hashing, and managed directory
scans do not block Tokio workers. Entity ownership is indexed once at Project
open and updated from committed ownership events; request routing never scans
all Projects as a fallback.

Managed text and artifact paths use capability-rooted `ManagedFs`: bounded
nofollow regular-file reads, bounded traversal, atomic replace, directory
fsync, and root-confined deletion. Artifact bytes are synced before metadata
commit; startup removes uncommitted orphans and fails closed on missing or
hash-mismatched durable artifacts.

## Canonical Session rollout

Each Session has one append-only JSONL rollout. It is canonical model context;
SQLite is a query/UI projection and may be rebuilt. One writer assigns sequence
numbers, flushes and syncs each stable record, then advances the SQLite
projection. Replay and final-line repair use streaming `BufRead`, not a whole
rollout allocation.

Streaming deltas and `ModelStepStarted` are transient. After a complete model
response is validated, its stable response items, usage, model-step cursor, and
terminal candidate enter a ContextCheckpoint before a completed Step or tool
dispatch is projected. A `FunctionCall` therefore becomes canonical before any
executor sees it. After a tool returns, its `FunctionCallOutput` becomes
canonical before Step/UI completion and before another sample.

## Crash recovery

On restart, PaperMachine replays canonical records and scans function
call/output pairs:

- a call with an output repairs only missing Step/UI projection;
- a call without an output receives exactly one JSON string output
  `"aborted"`;
- a running Tool Step for that call becomes `aborted`;
- no old model tool call is ever sent to an executor.

The same Agent then continues the same Turn and sees `aborted` in context. It
must inspect durable Workspace or external state before choosing whether a new
call is appropriate. This is deliberately Codex-style at-most-once recovery;
there is no aggregate `ModelSampleCommitted`, tool effect disposition,
reconciliation interface, or automatic replay of uncertain model tools.

Workflow host effects are a different layer. Python restarts from its immutable
entrypoint and uses deterministic logical effect IDs plus request-hash CAS.
Completed effects replay their results; started effects redispatch according to
their deterministic domain contract. Model tool calls never use that effect
journal.

The real-process SIGKILL matrix verifies:

| Crash boundary | Required result |
|---|---|
| before call checkpoint | sample again; uncommitted call never dispatches |
| call checkpointed, no output | append `aborted`; dispatch count unchanged |
| effect may exist, output absent | append `aborted`; Agent observes reality |
| output checkpointed, projection absent | recover real output; do not replay |
| terminal checkpointed, Turn commit absent | complete without another sample |

It also verifies rollout/projection convergence and in-flight sampling. Fault
hooks exist only in debug builds. The matrix runs on macOS and Linux.

## Control, store, and protocol reliability

Workflow lifecycle, Action start/finish, timer fire, usage, and terminal
transitions use typed `BEGIN IMMEDIATE` transactions with explicit allowed-from
states. Human answers use an `id + open-status` CAS.

Control messages move `pending -> claimed -> applied`. Claim records the target
Turn. IDs become applied only in the same canonical context/terminal
transaction that consumes them. A crash before checkpoint lets the same Turn
claim them again; an interrupt is acknowledged with its terminal Turn update.

Rust and Python cap each Workflow JSONL frame at 16 MiB. At most 64 effects may
be in flight, the response channel is bounded to 64, and reader, writer, or
handler failure immediately fails the runtime. The scheduler drops terminal
in-memory handles; a late `wait()` reads the persistent terminal result.

## Adaptation boundary and completion gates

PaperMachine adapts selected Codex patterns—Responses streaming, the
sample/tool/follow-up loop, process sandboxing, canonical rollout ordering, and
aborted missing-output normalization—but owns its Project, Workflow, Session,
prompt, provider, and Artifact domain model. Codex is source material, not a
runtime dependency.

The kernel is release-ready only when direct tools and command sandboxes agree
on authorization, managed state remains unreachable, route/tool/prompt
snapshots rebuild exactly, all five crash boundaries pass, and complete Rust,
Python, Web, production-build, and real-provider validation succeeds.
