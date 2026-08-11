# Prompt and model snapshots

PaperMachine renders model input at the Action/Turn boundary, not at Workflow
parse time. Each Turn freezes the exact route, environment, tools, and prompt so
recovery and audit refer to durable facts.

## Prompt layers

PromptSnapshot records ordered layers for runtime policy, Project context,
Session instructions, Agent system/role, enabled Skills, Action contract, and
interruption/retry guidance. Layers have stable identity and kind. The rendered
prompt is a projection of these layers rather than a second source of truth.

Workflow Action arguments enter as explicitly labeled Workflow-provided data.
They are not silently promoted to system instructions. A direct HumanMessage is
different: after provenance validation, the exact answered string becomes the
human Turn input while the remaining Action contract stays in prompt layers.

## Project context

Project contents are not injected wholesale. `ctx.project.changes()` returns a
bounded current snapshot and opaque cursor to Workflow code. Content reaches a
model only when the Workflow passes selected resources into an Action. This
keeps prompt growth and evidence provenance explicit.

Session `instructions` are immutable launch-time, high-priority guidance shared
by its Agents. They remain available as `ctx.instructions` and as a frozen prompt
layer. Workflow code may pass them as data, but cannot mutate their durable
meaning.

## Model routes

Session default model and Agent model refer to configured profile IDs. Before a
Turn, the router resolves provider, upstream model, context window, capabilities,
reasoning default, and non-secret config hash into ModelRouteSnapshot. A changed
route configuration cannot masquerade as the old snapshot on recovery.

Hosted search is requested only by Action search context and only when the route
declares capability. It is separate from local ToolRegistry membership.

## Structured outputs

Action result schemas are generated from Workflow boundary declarations. For
ordinary structured Actions the schema becomes the model response format. For
`if_needed`, the work prompt instead receives a typed trailer so the Agent can
use tools and give a normal report before submitting a machine-readable result.

Finalizer and repair Turns receive the same schema, the immediately preceding
response through canonical Agent history, no local tools, and no hosted search.
The host never reconstructs missing fields or changes the schema between stages.

## Context durability

Agent rollout JSONL is the canonical model conversation. SQLite Turns, Steps,
usage, and events are durable projections. Compaction and retry guidance are
recorded explicitly. A crash cannot cause an unseen tool result to be assumed;
missing canonical outputs are normalized to one aborted result before sampling
continues.
