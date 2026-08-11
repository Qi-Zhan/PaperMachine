# Architecture

PaperMachine is a local-first Rust server plus Web client. Domain ownership is
deliberately narrow:

~~~text
HTTP / SSE
  -> ProjectRuntime
       StoreHandle
       WorkflowProgramCatalog
       SessionScheduler
       WorkflowInterpreter
       ActionRunner
       TurnRuntime
       ToolCatalog / ProjectSkillCatalog
~~~

No Workflow runtime manager, message scheduler, or connection registry exists.
Existing Store, Session, ActionRunner, TurnRuntime, and application cancellation
primitives own their respective lifecycles.

## Domain model

- Project: PaperMachine-managed durable research state.
- Workspace: external user filesystem authorized to Agents.
- WorkflowProgram: immutable v1 source, manifest, and canonical IR identity.
- Session: one durable Workflow execution and ownership boundary.
- Agent: one model identity and canonical rollout inside a Session.
- ActionInvocation/Attempt: durable unit admitted by ActionRunner.
- Turn: one model/tool loop attempt.
- Artifact, HumanRequest, AgentInput, effect, and event: durable Session resources.

Project and Workspace never merge storage or authorization. Project operations
refer to Workspace paths but never install managed metadata there.

## Compiler and catalog

The workflow crate owns a handwritten UTF-8 lexer, recursive-descent and Pratt
parser, AST spans, semantic validation, canonical serialization, and interpreter.
The catalog scans built-in `workflow.pm` files and Project-managed user files.
The HTTP API may generate, validate, edit, and save user source at runtime.

Both built-in and user programs compile through the same API. Catalog entries
carry validation diagnostics and canonical IR hash. A Project-owned slug shadows
the built-in slug for that Project only.

## Execution and persistence

The interpreter implements SessionExecutor directly. It restarts from root,
while `session_effects` provides idempotent durable boundaries. Workflow return
becomes Session output. Action work always goes through ActionRunner; the
interpreter never samples a model itself.

The Store is Project-scoped SQLite plus canonical Agent rollout JSONL and managed
Artifact files. There is no global Project database. Startup discovers Projects
independently so one damaged Project does not hide healthy ones. Staging and
trash make creation/removal recoverable without touching Workspace contents.

## Authority surfaces

ToolCatalog membership, operating-system authorization, and hosted provider
tools are independent:

1. a Workflow Action declares a local tool policy;
2. the Agent access preset filters native capability;
3. the Turn freezes exact Tool definitions;
4. dispatch rechecks arguments, Workspace paths, credentials, and sandbox rules;
5. hosted search depends on provider capability and Action search context.

Skills add prompt/tool guidance but do not bypass any authority boundary.

## Web application

The Web app uses the Project-scoped HTTP/SSE protocol. Workflow Library exposes
source generation, validation diagnostics with line and column, manifest
language/request metadata, Agent/Action declarations, and the source textarea.
Session pages derive state from durable entities and events; UI projections are
never independent runtime state.

## Application lifecycle

The server owns one cancellation token. Project runtimes receive children of
that token. Graceful shutdown cancels work, allows the server a bounded drain,
and leaves durable state recoverable. The `--dev` switch changes default data and
config paths only; it does not select a different runtime.
