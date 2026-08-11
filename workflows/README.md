# Workflows

PaperMachine Workflows are `workflow.pm` programs compiled and interpreted by
the Rust workflow crate.

- `builtin/<slug>/workflow.pm`: reviewed programs shipped with PaperMachine.
- `<data-dir>/projects/<project-id>/workflows/<slug>/workflow.pm`: user programs
  owned by one Project.

Both locations use the same v1 compiler, canonical IR, interpreter, durable
effect journal, and ActionRunner. The HTTP API generates, validates, edits, and
saves source at runtime; a server restart is not required. There is no import or
module search path.

See [Workflow Language semantics](../docs/workflow-language-semantics.md).
