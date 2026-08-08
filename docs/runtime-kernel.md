# Runtime kernel target

Status: accepted clean-break target, 2026-08-08.

This document fixes the security and durability contracts for the next
PaperMachine runtime kernel. It is intentionally not a compatibility contract:
the new server opens only fresh state created with the new schema, and the HTTP
API, stored documents, and internal Rust types may change without adapters.
User-owned Workspace files remain outside that clean break and must never be
deleted or rewritten as part of Project-state replacement.

## Product boundary

> Project is a research world persistently managed by PaperMachine; Workspace
> is the user filesystem an Agent is authorized to operate; structured runtime
> APIs connect them, and they never share storage or a security boundary.

- Project is the sole ownership root for Sessions, Workflows, Artifacts,
  prompts, skills, human requests, and runtime journals.
- Project state lives only below PaperMachine's managed data directory.
- Workspace is an attachment containing one or more canonical filesystem
  roots. PaperMachine creates no hidden state inside those roots.
- Removing Project state never removes Workspace files. A missing Workspace
  makes execution unavailable but does not make the Project disappear.
- `goal` is an ordinary built-in Python Workflow. The Rust server, scheduler,
  Store, Session runtime, and Agent runtime must not branch on its slug or give
  it privileged lifecycle semantics.

## Turn environment and authorization

The five user-facing access choices remain presets, not enforcement objects.
At Turn creation, Rust materializes one immutable authorization context from
the selected preset, Workspace attachment, Project-managed roots, and provider
capabilities. The Turn persists the environment and its policy hash.

```text
TurnEnvironmentSnapshot
  workspace attachment ID and revision
  canonical roots and cwd
  materialized filesystem policy
  local-process network policy
  model-visible and server-hosted tool capabilities
  immutable protected roots
  environment-variable policy
```

Every enforcement boundary consumes that same materialized context:

1. model-visible tool schema filtering;
2. ToolRegistry dispatch authorization;
3. each direct file and network tool;
4. local command sandbox construction; and
5. sandboxed Workflow Python execution.

For every preset, PaperMachine-managed Project state is unreadable and
unwritable by Agent tools. Below `full_access`, writes to `.git`, `.agents`, and
`.codex` are denied, and credential-bearing Workspace files such as `.env` are
not readable. `research` authorizes controlled server tools such as
`fetch_url`; it does not give child processes unrestricted network access.

Path authorization must be anchored to a Workspace root and remain valid at
the file operation, not only during an earlier string or canonical-path check.
Absolute-path escape, parent traversal, symlink escape, and replacement races
must fail closed.

## Sandbox boundary

One sandbox manager prepares every untrusted child process. Command tools and
Workflow Python do not maintain separate policy builders.

- macOS uses Seatbelt.
- Linux and WSL2 use the adapted Codex bubblewrap path; WSL1 fails closed.
- Native Windows uses the pinned Codex elevated restricted-token backend. The
  source path is implemented, but Windows is not a release-tested platform
  until it passes the same policy matrix on a real MSVC host.
- A requested restricted profile fails closed when no backend is available.
- The child environment starts empty. Rust constructs a small explicit
  environment, provides synthetic home and temporary directories, and never
  forwards provider credentials.
- Timeout, output limits, process-group cancellation, and descendant cleanup
  are mandatory properties of every backend.

`full_access` deliberately grants ordinary host authority after explicit user
authorization, but the PaperMachine-managed root remains protected.

## Project storage

There is one authoritative Project record: the row in that Project's managed
database. The target topology contains no global `library.db`.

```text
data_dir/
  projects/<project-id>/
    state/project.db
    rollouts/
    artifacts/
    runtime/
  staging/
  trash/
```

Startup scans managed Project directories and builds an in-memory catalog.
Project creation initializes state under `staging/` and atomically renames it
into `projects/`. Project removal atomically renames managed state into
`trash/` before asynchronous deletion. Workspace roots are never targets of
those operations.

The Store has one current schema and rejects older databases. There is no
migration, backfill, dual read, dual write, or legacy-document fallback.
Integrity checks and backups protect current-format state; they are recovery
mechanisms, not compatibility mechanisms.

## Session journal and projection

Each Session owns one append-only rollout below the Project-managed root. The
rollout is the canonical model-context history. SQLite keeps query projections,
ownership, lifecycle status, usage summaries, and PaperMachine domain state;
it does not persist a cumulative copy of model history in every Turn.

Stable rollout items cover Turn input, model items, hosted calls, function
calls, tool lifecycle, usage, compaction replacement, terminal candidates, and
Turn terminal state. Streaming deltas remain ephemeral until they form a
stable context item.

The write rule is journal first, projection second:

1. append the next monotonically sequenced item;
2. flush at the item's durability boundary;
3. apply the SQLite projection with that sequence; and
4. replay journal items after the last projected sequence on restart.

One live writer serializes each Session. Compaction is an explicit replacement
item and never mutates prior journal entries.

## Recovery and external effects

Recovery reconstructs the Turn from the rollout rather than trusting a stored
history snapshot. A completed model item, tool result, or terminal candidate is
not sampled or executed again.

Tools declare an effect disposition:

- `pure`: safe to replay;
- `idempotent`: replay with the same call/effect identity;
- `reconcilable`: inspect durable external state before deciding; or
- `unknown`: never replay automatically after an execution-unknown crash.

Workflow-owned Turns resume automatically because the Workflow is the durable
control-flow owner. This includes the ordinary built-in `goal` Workflow.
Standalone user Turns interrupted by process loss become explicitly
`interrupted` and wait for a user resume decision. An execution-unknown command
is surfaced to the model and user; it is never silently treated as either
failed or completed.

## Adaptation boundary

PaperMachine owns its Project, Workflow, Session, Run, Evaluation, Artifact,
prompt, and provider models. It does not run Codex CLI or app-server as a
dependency. Proven implementation is adapted from the pinned OpenAI Codex
source where the semantics match this document:

| Concern | Pinned Codex source |
| --- | --- |
| filesystem and network policy | `codex-rs/protocol/src/permissions.rs` |
| sandbox selection and request transformation | `codex-rs/sandboxing/src/manager.rs` |
| macOS and Linux/WSL backends | `codex-rs/sandboxing/src/{seatbelt,bwrap}.rs` |
| native Windows backend | `codex-rs/windows-sandbox-rs` |
| child environment construction | `codex-rs/core/src/exec_env.rs` |
| append-only live writer | `codex-rs/thread-store/src/local/live_writer.rs` |
| context reconstruction | `codex-rs/core/src/session/rollout_reconstruction.rs` |

Direct adaptations retain source notes and Apache-2.0 attribution. Codex
configuration schemas and product-domain objects are not compatibility goals.

## Completion gates

The kernel cutover is complete only when:

- no runtime branch recognizes `goal`;
- no production path creates or opens `library.db`;
- direct tools and child processes agree on filesystem authorization;
- all restricted child processes use the unified sandbox manager;
- Session context is reconstructable from append-only rollouts;
- completed effects are not repeated and unknown effects are not auto-replayed;
- process-level crash injection covers every durability boundary;
- Rust, Python, and Web tests and the Web production build pass; and
- a real provider run survives a forced server restart with inspectable proof.
