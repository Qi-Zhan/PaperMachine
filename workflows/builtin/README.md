# Built-in workflows

Built-ins use the same Python Agent DSL as user workflows. They are reviewed
source examples, not privileged Rust handlers.

Built-ins declare access explicitly: evidence-gathering Agents use `research`,
while evaluators, synthesizers, writers, and graders that consume supplied
evidence use `model_only`.

- `parallel-discovery`: independent routes followed by one synthesis Session.
- `interactive-agent`: one persistent Session that waits for a human message
  before every conversational Turn until the user closes the Session; this
  powers the normal New Session action.
- `project-summary`: renders the latest Project progress as a sandboxed HTML
  Artifact once or on a durable refresh timer.
- `evidence-loop`: parallel evidence collection, fixed evaluation, dynamic
  follow-up Sessions, and final cited synthesis.
