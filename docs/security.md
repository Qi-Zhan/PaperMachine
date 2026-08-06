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
- validates slug, schemas, and budget metadata;
- extracts Agent/action and coordination summaries for inspection.

Validation is a usability and defense-in-depth layer, not the isolation
boundary. The validated source runs in a separate Python process. On macOS,
Seatbelt denies network access, denies writes outside that run's runtime
workspace, and denies reads from the user's home and common temporary/user-data
roots. The environment is cleared and `HOME`/`TMPDIR` point inside the workspace.
The runtime fails closed on platforms without an implemented Python sandbox.

The Python process cannot create authoritative domain state directly. It can
only request typed effects over JSONL. Rust validates IDs, ownership, schemas,
statuses, budgets, and Session serialization before applying them.

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

Every Session selects one of five access profiles. `model_only` has no resource
tools; `read_only` can only read its Session workspace; `workspace` adds
workspace writes and sandboxed commands; `research` adds hosted web search and
controlled URL fetching; `full_access` allows host files and unrestricted
commands/network after explicit human grant. `ask_human` remains available as a
control primitive in every profile.

Run creation applies access bounds before Python starts. The Workflow profile
is a hard ceiling; a Session-origin Workflow cannot choose a profile above the
source Session, and an Agent class override cannot exceed the Workflow. Agent
class declarations above the run ceiling are clamped. The Python
`set_access(...)` effect cannot cross that ceiling, and any upgrade within it
still requires a typed boolean HumanRequest. The Store and Workflow runtime
both enforce these rules; hiding unavailable choices in the web UI is not the
security boundary.

The Turn snapshots the Session profile. Tool schemas are filtered before model
sampling, and registry dispatch plus each built-in implementation rechecks that
snapshot. Omitting a schema is therefore not the enforcement boundary.

For every profile below `full_access`, `read_file` and `write_file` resolve paths
and symlinks against one Session workspace. Sandboxed `exec_command` clears the
host environment, redirects home/temp paths into that workspace, denies network
access, denies writes outside the workspace, and blocks reads from common
user-data roots with macOS Seatbelt. It fails closed when no sandbox backend
exists. `full_access` deliberately bypasses this filesystem/command sandbox and
must be treated as equivalent to granting the Agent the server user's authority.

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
memory but is not copied into `papermachine.toml`, SQLite, workflow source,
artifacts, or logs. Configuration debug output omits/redacts credentials, and
child workflow or tool processes receive a cleared environment.

Codex credential reuse is retained only as an opt-in fallback importer through
`--codex-home` when no PaperMachine provider config is loaded. It is not the
primary credential or provider registry.

Use an HTTPS provider endpoint. With an explicitly configured plain HTTP base
URL, the bearer credential, prompts, tool results, and research outputs cross
the network without transport encryption. PaperMachine logs a warning but does
not silently rewrite the endpoint.

## Remaining limitations

- `sandbox-exec` is a deprecated macOS interface. Linux and a future macOS
  backend require separate implementations.
- Read confinement is not a VM: system files and installed programs remain
  readable to sandboxed commands unless denied by the profile.
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
- Raw, uncached-token, and run-wide hosted-search budgets are checked at
  effect/model boundaries. One in-flight response can exceed a remaining token,
  search, or wall-time allowance before usage is reported. Per-action
  `max_search_calls` is forwarded as the Responses API `max_tool_calls` hard
  response limit when the endpoint supports it. PaperMachine probes field
  acceptance once per model and also counts the actual hosted calls in every
  response. If a proxy rejects the field, it is omitted. If a proxy accepts but
  exceeds the requested limit, that response is recorded as
  `provider_violated` and all later requests for the model use
  `runtime_fallback`. In either fallback case the runtime stops further hosted
  search between model samples, but the response that reveals a violation can
  already have overshot its allowance. PaperMachine also batches the remaining
  allowance at four hosted calls per response and adds a stable matching
  instruction; non-enforcing proxies can still exceed this soft batch size
  before the runtime regains control.
- An explicit `max_output_tokens` ceiling selects HTTP SSE because compatible
  Responses WebSocket beta endpoints do not consistently accept that property.
  Multi-step research actions should omit the ceiling to retain incremental
  WebSocket continuation; one-step orchestration actions can use it safely.
- `max_cost_usd` is metadata until a provider client supplies pricing estimates.
- Generated protocols and model output can still be wrong. Inspectability and
  provenance do not establish factual correctness.
