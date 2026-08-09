# Security boundaries

PaperMachine is local-first software, not a hardened multi-tenant service. The
server has no authentication layer, so the binary binds only to `127.0.0.1`
and rejects Host headers other than literal loopback addresses or `localhost`.

## Workflow source

Workflow definitions are executable Python, but they do not execute in the
server process. Before catalog registration, the AST validator:

- limits source to 128 KiB;
- accepts only `from papermachine import ...` imports;
- requires exactly one literal `@workflow(...)` manifest;
- rejects file, dynamic-code, reflection, dunder, process, socket, environment,
  and other explicitly forbidden names/calls;
- validates slug and schema metadata;
- extracts Agent/action and coordination summaries for inspection.

Validation is a usability and defense-in-depth layer, not the isolation
boundary. The validated source runs in a separate Python process prepared by
the same sandbox manager used for Agent commands. Workflow Python gets only its
materialized run directory, synthetic home and temporary directories, a small
core environment, no provider credentials, and no child-process network. The
runtime fails closed when the current platform cannot enforce that policy.

The Python process cannot create authoritative domain state directly. It can
only request typed effects over JSONL. Rust validates IDs, ownership, schemas,
statuses, permissions, and Session serialization before applying them.

`ctx.project.snapshot()` is a Rust-owned read effect, not database access from
Python. It is fixed to the current Workflow's Project and excludes only the
calling Workflow's own Sessions and outputs, so published Project state remains
available to other Agents without recursive self-ingestion. It caps collection
counts and per-Turn text and omits provider reasoning and credentials.
`publish_artifact(...)` accepts bounded
text only, derives a deterministic Artifact ID from the Workflow/effect path,
and verifies optional Agent ownership.

The optional launch-time Project snapshot uses the same bounded builder. It is
persisted once on the Workflow, exposed read-only as `ctx.context`, and rendered
inside an explicit untrusted-data delimiter for Agent Turns. Starting from a
Session only changes focus/provenance; it does not copy that Session's system
prompt or permissions into prompt text. A live Project read remains an explicit
`ctx.project.snapshot()` effect, so unrelated Project updates cannot silently
alter an active run's context.

Python also cannot label arbitrary action text as a human message. A
user-origin workflow Action must name an answered direct HumanRequest and its
annotated `HumanMessage` argument. Rust verifies Workflow/Session ownership,
request status, response type, and exact text before the Store accepts the
Turn; the ActionInvocation retains the source HumanRequest ID for inspection.

Saving the same Project-local slug replaces the editable program source. Every
Workflow stores an immutable snapshot of the exact source and SHA-256 it started
with, together with the SHA-256 of the Python DSL package used to validate it.
Before every initial execution or recovery, Rust re-hashes both source and DSL
runtime; a mismatch fails before Python starts or any effect is dispatched.

The `project-summary` Agent does not receive PaperMachine's managed directory as
a Workspace. Its Action declares exactly `read_project_home`,
`patch_project_home`, and `preview_project_home`; the host materializes those
Project tools into that Action Turn's immutable ToolSet. They operate on a
bounded managed draft keyed to that exact Workflow Action. Semantic patches use revision checks and stable block IDs;
unsafe active tags, inline event/style attributes, script URLs, oversized
blocks, stale revisions, and no-op edits fail as tool results that the same
Agent can inspect and correct. The publication effect accepts only the latest
completed Action belonging to that Workflow and Agent. It compares the draft's
base to the canonical Project-home revision, atomically commits the source,
page, and canonical pointer, and reuses the current revision for a no-op. Generic
Artifact metadata cannot claim the reserved Project-home roles.

Generated project-summary HTML remains untrusted model output. It is served
with `nosniff` and a restrictive CSP (`sandbox`, no default network/source
access, inline style and data images only) for safe raw access. Before the
canonical Artifact becomes the Project home page, the Web client parses its body
and sanitizes it with DOMPurify. Scripts, inline styles, forms, controls,
frames, embedded objects, external media, SVG/MathML, unsafe attributes, and
non-Web links are removed. Only that sanitized semantic fragment is rendered
into the parent Vue DOM.

## Agent tools

Every Session selects one of five access presets. `model_only` has no resource
tools; `read_only` can only read the attached Workspace authorized for the
Turn; `workspace` adds writes there and sandboxed commands; `research` adds hosted web search and
controlled URL fetching; `full_access` allows host files and child-process
network after explicit human grant. Even `full_access` commands remain inside a
platform sandbox so PaperMachine-managed state can stay unreadable and
unwritable. `ask_human` is not a model-visible tool and is available only as an
explicit Workflow DSL effect.

Run creation applies access bounds before Python starts. The Workflow profile
is a hard ceiling; a Session-origin Workflow cannot choose a profile above the
source Session, and an Agent class override cannot exceed the Workflow. Agent
class declarations above the run ceiling are clamped. The Python
`set_access(...)` effect cannot cross that ceiling, and any upgrade within it
still requires a typed boolean HumanRequest. The Store and Workflow runtime
both enforce these rules; hiding unavailable choices in the web UI is not the
security boundary.

Turn creation materializes the Session preset with the exact Workspace
attachment and revision, cwd, managed-state deny, filesystem scopes, tool
capabilities, and network capabilities. Independently, the host ToolCatalog
constructs one exact ToolRegistry. A Workflow Action starts from its static
`@action(tools=[...])` declaration and filters Workspace tools through that
materialized access policy. Project tools also require explicit Action
declaration. The Turn atomically persists the sorted tool definitions and their
SHA-256 beside the authorization snapshot.

Model exposure, dispatch, pause/resume, and crash recovery all rebuild from
that ToolSet. An absent executor, changed definition, invalid hash, or forged
call fails closed. The registry has no permission bypass and cannot execute a
tool outside its membership. File, network, managed-state, and sandbox checks
remain inside each implementation as defense in depth. Thus declaring a
Workspace tool never expands the Session's filesystem or network policy, while
declaring a Project tool never grants access to its managed files.

Provider tool-call IDs are opaque effect identities, but they must be non-empty,
bounded, and unique across the whole Turn. The Agent validates the complete
batch against prior history before emitting a tool-call event or beginning any
execution; duplicates fail the model Step and cannot alias a durable effect.

Before creating that Turn, the Store verifies that every attached root still
exists as a real directory at the canonical path recorded by the attachment.
A removed root or a path replaced by a symlink fails before model sampling.
Relocation is an explicit Project operation that creates a later attachment
revision; it never mutates the immutable environment of an earlier Turn.

`read_file` and `write_file` first resolve and authorize the requested target,
then reopen every component through directory handles without following
symlinks. A rename or symlink swap after authorization therefore fails instead
of redirecting the operation. Direct file operations
make credential-bearing files such as `.env` unreadable and Workspace-root
`.git`, `.agents`, and `.codex` metadata read-only. `exec_command` consumes the
same materialized filesystem, metadata, credential, managed-root, environment,
and child-network policy. It starts with an empty environment, redirects
home/temp paths into a Session sandbox, denies writes outside the Workspace,
and denies child-process network below `full_access`.

One manager performs request validation, policy resolution, environment
construction, platform transformation, output limiting, timeout/cancellation,
and descendant cleanup for both Agent commands and Workflow Python:

- macOS uses Seatbelt;
- Linux and WSL2 use bubblewrap with user, PID, mount, and optional network
  namespaces;
- native Windows uses the elevated restricted-token backend from the pinned
  Codex source; and
- unsupported platforms, WSL1, or a missing required backend fail closed.

The macOS and Linux paths are build-validated in this repository. The Windows
source integration targets Rust's MSVC ABI and retains Codex's on-demand
elevated provisioning/ACL refresh behavior; it was not exercised during the
2026-08-08 cutover because no Windows host was in scope.

`fetch_url` is the only built-in research tool with outbound network access. It
accepts public HTTPS destinations only, rejects credentials and nonstandard
ports, resolves and pins a public IP, revalidates redirects, limits responses to
text-like content, caps the body at two MiB, and uses fixed redirect/time limits.
These controls reduce SSRF and unbounded-download risk; host-level egress policy
is still required for a hostile deployment.

Model-provider requests are made by the Rust server outside tool and workflow
sandboxes. They require ordinary host network access.

## Credentials and transport

PaperMachine provider configuration stores only an `api_key_env` name. The
credential must be present in the server process environment; it is read into
memory but is not copied into the provider configuration, SQLite, workflow source,
artifacts, or logs. Configuration debug output omits/redacts credentials, and
child workflow or tool processes receive a cleared environment.

Outside explicit demo mode, startup requires a valid configuration and an
available `default_model`. A provider marked `optional = true` and its profiles
are omitted when its named credential is absent; all other provider credentials
remain mandatory.

Use an HTTPS provider endpoint. With an explicitly configured plain HTTP base
URL, the bearer credential, prompts, tool results, and research outputs cross
the network without transport encryption. PaperMachine logs a warning but does
not silently rewrite the endpoint.

## Remaining limitations

- `sandbox-exec` is a deprecated macOS interface.
- Read confinement is not a VM: the platform runtime and explicitly mounted
  executable/library roots remain readable to sandboxed commands.
- Windows elevated-sandbox behavior still requires validation on a real MSVC
  Windows host before Windows is treated as a release-tested platform.
- The AST policy is intentionally small and should not be treated as a proof
  that Python code is harmless; OS isolation remains mandatory.
- Workflow recovery relies on deterministic effect paths. Replaying the same
  path with a different kind or payload fails closed instead of guessing which
  side effect the author intended. Workflow source should therefore keep the
  sequence and shape of effects deterministic for the same snapshotted input;
  arbitrary external I/O remains outside the Python sandbox and effect model.
- A local tool Step completed before a crash is replayed from its durable output.
  Every call also persists whether it was merely prepared or may have started,
  plus a `pure`, `idempotent`, `reconcilable`, or `unknown` disposition. Only
  pure/idempotent effects replay automatically after the boundary; reconcilable
  tools must inspect external state first. An arbitrary command is `unknown`,
  because PaperMachine cannot prove whether its external effect happened before
  the process disappeared, and is therefore surfaced without automatic replay.
- Every Turn is owned by one ActionAttempt and is recovered only by its
  Workflow runtime. There is no separate Session submit or manual Turn-resume
  capability.
- An Action continues until the model returns a terminal answer, the user
  finishes/interrupts/cancels it, or an infrastructure/provider error occurs.
  Provider request and stream-idle timeouts protect broken connections, and
  server-wide concurrency limits protect the process. Token/cache/search/time
  usage is persisted for inspection.
- Generated protocols and model output can still be wrong. Inspectability and
  provenance do not establish factual correctness.
