# Security model

PaperMachine separates four boundaries: managed Project state, user Workspace,
model-visible tools, and hosted provider tools. A capability on one boundary
does not imply authority on another.

## Workflow source

`workflow.pm` is parsed, checked, and interpreted inside the Rust server. The
language has no imports, arbitrary filesystem or network access, subprocesses,
environment access, reflection, eval, clocks, randomness, native extensions, or
exception recovery. Workflow code can change durable state only through named
host effects.

Compilation fails closed on malformed syntax or schemas, undefined and duplicate
names, Action/function arity, recursive calls, illegal control flow, non-durable
loop back edges, invalid tool membership/access presets, invalid parallel keys,
and source larger than 128 KiB. Dynamic type errors, missing fields, out-of-range
indexes, and invalid operations stop the Session. There is no truthiness or
implicit coercion.

Pure execution receives 1,000,000 IR steps between durable effects. This bounds
CPU-only loops even if a compiler invariant regresses. Dynamic parallelism is
bounded, keyed, and joined; branch failure cancels the remaining evaluation.

## Durable integrity

A Session freezes source, source hash, language version, manifest, and canonical
IR hash. Recovery recompiles the frozen source and compares every value before
executing. Each host effect is journaled under a deterministic path and request
hash. Reusing a path with different input is an invariant failure; completed
results replay without duplicating resources.

Agent identity is derived from `(Session, template, key)`. The first durable
configuration is frozen by the same request-hash rule. Access overrides match
the template name and remain clamped by the Session ceiling.

## Project and Workspace

Project databases, prompts, rollouts, programs, skills, and Artifacts live only
under the managed Project root. Workflow code cannot read that root directly.
Agent file tools operate against the attached Workspace and reject traversal,
managed-state paths, protected metadata, and known credential locations.

Removing or resetting managed Project data never authorizes deletion of the
Workspace. A missing Workspace leaves managed history inspectable.

## Turns and tools

ToolRegistry membership determines which local definitions a model sees and
which names can dispatch. Filesystem/process authorization separately enforces
the Agent access preset. Hosted search is a provider capability outside the
local registry. Model-generated arguments are validated again at dispatch.

Every Turn freezes its model route, environment, tool set, and prompt. A model
FunctionCall is synced before dispatch, and its FunctionCallOutput is synced
before another sample. After a crash, an incomplete canonical tool call receives
one stable aborted output rather than being guessed or replayed.

HumanRequest answers are validated by the same controlled schema validator used
for params and Action results. Workflow source names a declared Human response
schema; the compiler expands it, so runtime-computed raw JSON Schema is not an
authority surface. Direct human Turns require an opaque provenance value tied to
the exact answered request. Project Home publication similarly requires the
exact completed Action handle and validates a full standalone HTML document
before storing it.

## Network and UI

The HTTP server binds loopback and rejects non-loopback Host headers. HTML
Artifacts are served with `nosniff`, a sandboxing CSP, and no script permission.
Secrets remain provider configuration references and are never embedded in
Workflow source or durable non-secret snapshots.
