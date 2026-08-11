# Built-in Workflows

Each child directory contains one reviewed `workflow.pm`. Built-ins have no
privileged execution path and may be shadowed by a valid Project-owned program
with the same slug.

- `goal`: persistent Agent loop with a structured completion decision.
- `interactive-agent`: durable direct-human Turns with provenance.
- `evidence-loop`: effectful helpers, keyed route Agents, parallel follow-up,
  immutable evidence ledger, evaluation, and draft revision.
- `parallel-universe`: keyed parallel research universes followed by
  cross-universe synthesis.
- `project-summary`: change cursor, exact-Action Project Home publication, and
  optional durable refresh wait.
- `single-agent-research`: text Action with post-search finalization.

Tests compile and execute these through the same public path used by user source.
