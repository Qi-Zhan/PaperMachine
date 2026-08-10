# Built-in workflows

Built-ins use the same Python Agent DSL as user workflows. They are reviewed
source examples, not privileged Rust handlers.

Built-ins declare access explicitly: evidence-gathering Agents use `research`,
while evaluators, synthesizers, and writers that consume supplied
evidence use `model_only`. Each Action also declares its complete local tool
set: research Actions request the Workspace tools and `read_resource`,
reasoning-only Actions use `tools=[]`, and Project Summary requests only
`read_resource`.

- `parallel-discovery`: independent routes followed by one synthesis Session.
- `single-agent-research`: one persistent research Session produces a report.
- `goal`: one persistent Agent performs each tool-capable Turn, stops when that
  Turn marks the objective complete or genuinely blocked, and otherwise starts
  another Turn without waiting for a human.
- `interactive-agent`: one persistent Session that waits for a human message
  before every conversational Turn until the user closes the Session; this
  powers the normal New Session action.
- `project-summary`: one persistent Agent reads changed Project resources on
  demand and returns the complete home page for publication, once or in a loop
  separated by durable waits.
- `evidence-loop`: parallel evidence collection, evaluator-directed follow-ups
  in the same route Sessions, and iterative review of the final draft.
