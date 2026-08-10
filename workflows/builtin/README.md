# Built-in workflows

Built-ins use the same Python Agent DSL as user workflows. They are reviewed
source examples, not privileged Rust handlers.

Built-ins declare access explicitly. Hosted-search researchers, evaluators,
synthesizers, and writers use `model_only` with `tools=[]`. Goal, Interactive
Agent, and Project Summary use `workspace` with bare Actions, so they receive
collaboration plus access-allowed native tools. Hosted search remains a
provider capability selected by `search_context_size`.

- `parallel-discovery`: independent routes followed by one synthesis Session.
- `single-agent-research`: one persistent research Session produces a report.
- `goal`: one persistent Agent performs each tool-capable Turn and returns a
  typed active, complete, or blocked decision. Invalid output uses the bounded
  no-tool finalization path. Active starts another Turn without waiting for a
  human.
- `interactive-agent`: one persistent Session that waits for a human message
  before every conversational Turn until the user closes the Session; this
  powers the normal New Session action.
- `project-summary`: one ordinary Agent receives bounded changed Project
  snapshots, may inspect the Workspace or collaborate with other Agents, and
  returns a complete standalone home page, once or across durable waits.
- `evidence-loop`: parallel evidence collection, evaluator-directed follow-ups
  in the same route Sessions, and iterative review of the final draft.
