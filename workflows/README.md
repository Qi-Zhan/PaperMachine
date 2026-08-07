# Workflows

PaperMachine workflows are validated Python programs. The directory split is
ownership only; built-in and user workflows execute through the same Rust
effect runtime.

- `builtin/<slug>/workflow.py`: reviewed workflows shipped with PaperMachine.
- `<project-root>/.papermachine/workflows/<slug>/workflow.py`: workflows authored
  or generated for one Project in the Workflow page.

Each source defines Agent classes with an explicit `access` profile and
`@action` methods, plus exactly one async
function decorated with `@workflow(...)`. Python owns ordinary control flow;
Rust owns Sessions, Turns, tools, sandboxing, permissions, timers, human requests,
events, and persistence. A Workflow stores its exact source and SHA-256, so
later edits cannot change the meaning of an existing run.
