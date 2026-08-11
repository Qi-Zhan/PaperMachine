# Runtime kernel

PaperMachine has one execution chain:

~~~text
WorkflowInterpreter
  -> durable Session effect
  -> ActionInvocation
  -> ActionRunner
  -> ActionAttempt
  -> TurnRuntime
  -> Agent sample/tool/follow-up loop
~~~

Workflow-created Actions and Agent-created tasks enter the same ActionRunner.
AgentInput is the single durable Agent inbox. Session is the only workflow
lifecycle; Turn is the only model/tool lifecycle.

## Workflow execution

Starting or recovering a Session recompiles its frozen `workflow.pm`. The source
hash, manifest language version, and canonical IR hash are checked before
execution. The interpreter begins at root with a fresh local environment.

Expressions and statements consume a public 1,000,000-step fuel budget. Durable
effects reset it. Compile-time control-flow analysis separately requires every
unbounded loop back edge to cross a durable await.

Stable effect paths are formed from IR Node IDs, function call sites, loop
iterations, and parallel branch identity. The Store journals the request hash
before dispatch and the result/error after dispatch. Completed effects replay;
changed requests and prior failures fail closed.

Human and deadline waits remain `Started` while suspended. A resumed Session
restarts from root and reaches the same effect path. Parallel branches are all
allowed to reach a stable completion or suspension point; human input wins the
aggregate status, otherwise the earliest deadline wins.

## Agent and Action execution

Agent UUID is derived from Session, template, and canonical key. Durable Agent
creation freezes its initial configuration. Per-template launch overrides are
applied before the Session ceiling.

Awaiting an Action writes an ActionInvocation and waits for the shared
ActionRunner. The runner admits at most one non-terminal Action per Agent and may
run different Agents concurrently. Each attempt owns one Turn and records retry,
interruption, terminal output, and usage.

Structured Action finalization and repair are ordinary durable ActionInvocations
with deterministic suffixes. They use no local tools or search, and their exact
results replay after restart.

## Turn transaction ordering

Before sampling, TurnRuntime freezes ModelRoute, environment, ToolSet, and Prompt
snapshots. Model output is appended to the Agent rollout before being projected
to Steps or events. A validated FunctionCall is durable before dispatch; its
FunctionCallOutput is durable before the next sample.

On recovery, a canonical FunctionCall without output receives one stable aborted
output. Model tool calls are not automatically rerun. This differs intentionally
from host effects, whose APIs are designed for deterministic replay.

## Cancellation and shutdown

One application CancellationToken flows through Project runtimes, schedulers,
Actions, Turns, model streams, process groups, and tools. Parallel Workflow
failure drops and joins sibling evaluation. Session closure cancels outstanding
work before the scheduler commits terminal status and final usage.

SSE streams use the same application lifecycle and keep-alive wrapper. There is
no connection registry or second shutdown state model.
