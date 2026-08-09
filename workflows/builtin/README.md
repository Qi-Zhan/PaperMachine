# Built-in workflows

Built-ins use the same Python Agent DSL as user workflows. They are reviewed
source examples, not privileged Rust handlers.

Built-ins declare access explicitly: evidence-gathering Agents use `research`,
while evaluators, synthesizers, writers, and graders that consume supplied
evidence use `model_only`. Each Action also declares its complete local tool
set: research Actions request the four Workspace tools, model-only Actions use
`tools=[]`, and Project Summary requests only its three Project-home tools.

- `parallel-discovery`: independent routes followed by one synthesis Session.
- `goal`: one persistent Agent performs each tool-capable Turn, stops when that
  Turn marks the objective complete or genuinely blocked, and otherwise starts
  another Turn without waiting for a human.
- `interactive-agent`: one persistent Session that waits for a human message
  before every conversational Turn until the user closes the Session; this
  powers the normal New Session action.
- `project-summary`: one persistent Agent reads, incrementally edits, and
  previews the Project home page within its normal tool loop, then publishes
  the validated semantic page once or on a durable refresh timer.
- `evidence-loop`: parallel evidence collection, evaluator-directed follow-ups
  in the same route Sessions, and iterative review of the final draft.
