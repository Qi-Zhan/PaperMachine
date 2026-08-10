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

`ctx.project.changes()` is a Rust-owned effect fixed to the current Session's
Project. It returns only a durable cursor and changed `pm://` resource URIs,
excluding the calling Session's own records and outputs. Project contents are
never injected into a Workflow or model prompt. An Action must explicitly
declare `read_resource` and choose which current-Project resource to read;
ownership and response-size bounds are enforced by Rust.

`publish_artifact(...)` accepts bounded text only, derives a deterministic
Artifact ID from the Session/effect path, and verifies optional Agent
ownership.

Python also cannot label arbitrary action text as a human message. A
direct-user Action must name an answered HumanRequest and its annotated
`HumanMessage` argument. Rust verifies Session/Agent ownership,
request status, response type, and exact text before the Store accepts the
Turn; the ActionInvocation retains the source HumanRequest ID for inspection.

Saving the same Project-local slug replaces the editable program source. Every
Session stores an immutable snapshot of the exact source and SHA-256 it started
with, together with the SHA-256 of the Python DSL runtime used to validate it.
Before every initial execution or recovery, Rust re-hashes both source and DSL
runtime; a mismatch fails before Python starts or any effect is dispatched.

All PaperMachine-managed text paths go through a capability-rooted ManagedFs.
It accepts only bounded root-relative operations, never follows symlinks,
requires regular files, atomically replaces and syncs content, and confines
traversal/deletion beneath the opened Project root.

The ordinary `project-summary` Agent declares only `read_resource`. It reads
relevant Project state on demand and returns a complete HTML fragment as its
normal Action result. The awaited `_ActionCall` retains its exact first
ActionInvocation ID; `publish_project_home(action=call)` accepts only that
completed Action, validates the HTML, atomically commits source, page, and the
canonical pointer, and reuses the current revision for identical content.
Generic Artifact metadata cannot claim the reserved Project-home roles.

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
tools. `read_only` can read ordinary host files but cannot write or run commands.
`workspace` and `research` keep host reads and add writes only inside the
Workspace plus sandboxed commands; `research` also authorizes controlled URL
fetching and capability-gated hosted search. `full_access` allows ordinary host
writes and child-process network after explicit confirmation. Every profile,
including `full_access`, is denied PaperMachine managed state. `ask_human` is
not model-visible and exists only as an explicit Workflow DSL effect.

Session creation applies access bounds before Python starts. Session access is
the hard ceiling; a Session-origin launch cannot choose a profile above its
source Session, and an Agent class override cannot exceed the new Session.
Agent class declarations above the ceiling are clamped. The Python
`set_access(...)` effect cannot cross that ceiling, and any upgrade within it
still requires a typed boolean HumanRequest. The Store and Workflow runtime
both enforce these rules; hiding unavailable choices in the web UI is not the
security boundary.

Turn creation materializes the Agent preset, bounded by Session access, with the exact Workspace
attachment and revision, cwd, managed-state deny, filesystem scopes, tool
capabilities, and network capabilities. Independently, the host ToolCatalog
constructs one exact ToolRegistry. An Action starts from its static
`@action(tools=[...])` declaration and filters Workspace tools through that
materialized access policy. Project tools also require explicit Action
declaration. The Turn atomically persists the sorted tool definitions and their
SHA-256 beside the authorization snapshot.

Model exposure, dispatch, pause/resume, and crash recovery all rebuild from
that ToolSet. An absent executor, changed definition, invalid hash, or forged
call fails closed. The registry has no permission bypass and cannot execute a
tool outside its membership. File, network, managed-state, and sandbox checks
remain inside each implementation as defense in depth. Thus declaring a
Workspace tool never expands the Agent's filesystem or network policy, while
declaring a Project tool never grants access to its managed files.

Provider tool-call IDs are opaque call identities, but they must be non-empty,
bounded, and unique across the whole Turn. The Agent validates the complete
batch against prior history before emitting a tool-call event or beginning any
execution; duplicates fail the model Step and cannot alias a durable effect.

Before creating that Turn, the Store verifies that the attached path still
exists as a real directory at the canonical path recorded by the attachment.
A removed directory or a path replaced by a symlink fails before model sampling.
Relocation is an explicit Project operation that creates a later attachment
revision; it never mutates the immutable environment of an earlier Turn.

`read_file` and `write_file` first resolve and authorize the requested target,
then reopen every component through directory handles without following
symlinks. A rename or symlink swap after authorization therefore fails instead
of redirecting the operation. Below `full_access`, direct file operations make
credential-bearing files such as `.env*` and user credential directories
unreadable; Workspace-root `.git`, `.agents`, and `.codex` metadata is
read-only. `exec_command` consumes the
same materialized filesystem, metadata, credential, managed-root, environment,
and child-network policy. It starts with an empty environment, redirects
home/temp paths into a Turn sandbox, denies writes outside the Workspace,
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

The macOS policy is exercised by real filesystem and process tests; Linux keeps
compile and policy coverage. Native Windows is outside the current release and
test scope.

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
- The AST policy is intentionally small and should not be treated as a proof
  that Python code is harmless; OS isolation remains mandatory.
- Workflow recovery relies on deterministic effect paths. Replaying the same
  path with a different kind or payload fails closed instead of guessing which
  side effect the author intended. Workflow source should therefore keep the
  sequence and shape of effects deterministic for the same snapshotted input;
  arbitrary external I/O remains outside the Python sandbox and effect model.
- Model tools use at-most-once crash recovery. A canonical FunctionCall without
  a canonical output receives `"aborted"` and is never automatically replayed,
  even if its external effect may already exist. The next model sample must
  inspect durable reality. This avoids duplicate writes but deliberately cannot
  prove whether an interrupted external effect happened.
- Every Turn is owned by one Agent and ActionAttempt and is recovered only
  through its Session. There is no direct Turn-submit or manual Turn-resume
  path.
- An Action continues until the model returns a terminal answer, the user
  finishes/interrupts/cancels it, or an infrastructure/provider error occurs.
  Provider request and stream-idle timeouts protect broken connections, and
  server-wide concurrency limits protect the process. Token/cache/search/time
  usage is persisted for inspection.
- Workflow protocol frames are capped at 16 MiB, with at most 64 in-flight
  effects and a bounded response channel. These are resource limits, not a
  substitute for OS isolation or input validation.
- Generated protocols and model output can still be wrong. Inspectability and
  provenance do not establish factual correctness.
