# Durable Workflow contract

This document defines the v1 boundary between Workflow Language source and the
PaperMachine host.

## Program snapshot

Saving and starting compile source immediately. A `WorkflowProgramManifest`
contains stable ID, slug, name, description, language version, request mode, and
params JSON Schema. A Session freezes:

- owner and source kind;
- managed definition path ending in `workflow.pm`;
- source and source SHA-256;
- manifest and language version;
- canonical serialized IR SHA-256.

Recovery recompiles from the frozen source. Source hash, language version,
manifest, and IR hash must all match. v1 intentionally has no legacy document
decoder or migration execution path.

## Compilation

The compiler consists of a UTF-8 lexer, recursive-descent statement parser,
Pratt expression parser, span-carrying AST, semantic checker, and canonical IR
serializer. Diagnostics report one-based line and column. Node IDs follow parsed
semantic order; source spans are excluded from the canonical hash, so formatting
alone does not alter IR identity.

Top-level declarations are schemas, Agent templates, local functions, and one
Workflow. Functions may perform effects but cannot recurse or escape as values.
The call graph is closed in one file.

## Values and boundaries

Runtime values are `null`, `bool`, `int`, `number`, `string`, immutable list and
object values, plus unforgeable Agent, Action-result, HumanMessage, and Artifact
handles. Opaque provenance survives member/index access and collection joins but
is stripped only when an ordinary JSON result crosses the Session output
boundary.

Schemas are limited to `any`, scalar types, list, map, object, and scalar enum,
with optional/default fields and length/numeric constraints. The same validator
serves params, structured Action output, and HumanRequest answers.
Params use object-field required/optional syntax. HumanRequest response schemas
are named top-level schemas expanded during compilation rather than runtime JSON
Schema values embedded in Workflow code.

## Effects and replay

The interpreter restarts from root. Local environments are not persisted.
Durable effects use a path composed from IR Node IDs, function call sites, loop
iterations, and parallel branch identity. Dynamic branch keys are canonical
scalars and hashed into paths; completion order never changes identity.

`session_effects` stores effect kind, payload, request hash, status, result, and
error. A completed effect replays its result. A failed effect replays its failure.
A started human/deadline wait suspends. Same path plus changed request fails
closed.

The host effects are Agent creation/access, Action invocation, human request,
deadline wait, Project changes, Artifact publication, and Project Home
publication. Workflow completion is the interpreter return value, not an effect.

## Action contract

An Action declaration fixes prompt, parameters, tool policy, search context,
reasoning effort, finalization policy, and optional result schema. Awaiting an
Action creates a durable ActionInvocation on the shared ActionRunner.

- no result schema: return terminal text;
- ordinary structured result: use the schema as model response format and
  validate the returned JSON;
- `if_needed`: run the normal tool-capable work Turn with a generated typed
  trailer, parse whole/fenced/first structured JSON, then at most one no-tool
  finalizer and two low-reasoning repairs;
- `after_search`: run one no-tool final deliverable Turn only when hosted search
  was actually used.

All structured paths use the identical schema. Exhausting repairs fails the
Action. Await returns the dynamic value while retaining its exact invocation ID,
which is required by Project Home publication.

An opaque HumanMessage may be the sole argument of a direct-human Action. The
host verifies Session, Agent, answered request, argument name, and exact answer
before creating the Turn.

## Concurrency and suspension

Fixed parallel branches return a name-keyed object. `parallel for` requires
unique scalar keys and returns a list in input order. Branches clone local
environments and merge only through results. Different Agents may run
concurrently; one Agent cannot run two Actions concurrently. The shared
Workspace is not filesystem-isolated.

If branches suspend, the host waits until all runnable branches settle, then
selects human input over deadlines and otherwise the earliest deadline. A hard
branch failure cancels and joins siblings. Cancellation propagates through the
Session, ActionRunner, model loop, and tools.
