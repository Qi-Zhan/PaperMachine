# Security boundaries

PaperMachine is local-first software, not a hardened multi-tenant service. The
server binds to loopback by default and currently has no authentication layer.

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
Python. It is fixed to the current Workflow's Project, excludes summary runs to
avoid recursive ingestion, caps collection counts and per-Turn text, and omits
provider reasoning and credentials. `publish_artifact(...)` accepts bounded
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
with, so later edits cannot change execution history.

Generated project-summary HTML is untrusted model output. It is served with
`nosniff` and a restrictive CSP (`sandbox`, no default network/source access,
inline style and data images only), and the Project Page embeds it in an iframe
with an empty sandbox permission set. It is never injected into the parent Vue
DOM.

## Agent tools

Every Session selects one of five access presets. `model_only` has no resource
tools; `read_only` can only read its Session Workspace; `workspace` adds
Workspace writes and sandboxed commands; `research` adds hosted web search and
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
capabilities, and network capabilities. The Turn persists that policy and its
SHA-256. Tool schemas are filtered before model sampling, and registry dispatch
plus each built-in implementation rechecks the same materialized context.
Omitting a schema is therefore not the enforcement boundary.

For every preset below `full_access`, `read_file` and `write_file` resolve paths
and symlinks against the snapshotted Workspace roots. Direct file operations
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

Outside explicit demo mode, startup fails unless a valid PaperMachine provider
configuration is present and every provider's named credential variable is
non-empty.

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
  If the process died while an arbitrary command/tool was running, PaperMachine
  can only report execution as unknown: it cannot prove whether an external side
  effect happened before the process disappeared. Workflow authors should make
  destructive or non-idempotent tool operations require an explicit checkpoint.
- An Action continues until the model returns a terminal answer, the user
  finishes/interrupts/cancels it, or an infrastructure/provider error occurs.
  Provider request and stream-idle timeouts protect broken connections, and
  server-wide concurrency limits protect the process. Token/cache/search/time
  usage is persisted for inspection.
- Generated protocols and model output can still be wrong. Inspectability and
  provenance do not establish factual correctness.
