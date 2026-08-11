# Workflow Language v1 semantics

Workflow Language v1 is a small Rust-like orchestration language. It is dynamic
inside the program and explicit at external schema boundaries.

## Declarations

~~~rust
version 1;

schema Decision = object {
    message: string,
    status: enum["active", "complete", "blocked"],
};

agent Worker {
    access = workspace;
    role = "persistent worker";
    system = "Own and verify the objective.";
    action work(objective) {
        search_context = high;
        finalize = if_needed;
        result = Decision;
        prompt = "Work now and inspect the result.";
    }
}

workflow goal {
    name = "Goal";
    description = "Work until verified complete.";
    request = required;
    params {
        title?: string(default = "Goal", title = "Session title");
        model?: model_profile(title = "Agent model");
    }
    run(ctx) {
        let worker = Worker(key = "main", name = ctx.params.title, model = get(ctx.params, "model", ""));
        loop {
            let decision = await worker.work(objective = ctx.request);
            match decision.status {
                "active" => continue,
                "complete" | "blocked" => return decision,
                _ => fail("unreachable status"),
            }
        }
    }
}
~~~

There is exactly one Workflow. Schemas must be declared before use. Agent and
function declarations may appear in any top-level order. Source is UTF-8 and at
most 128 KiB. v1 comments begin with `//`; strings are ordinary escaped literals
or multiline triple-quoted literals.

Workflow params use the same field syntax as object schemas. `name: schema` is
required when it has no default; `name?: schema` is optional. Defaults are
applied before validation and `run`, so Workflow code should read validated
fields directly instead of repeating schema bounds or defaults.

## Dynamic values

Normal variables and function parameters have no declared type. `let` bindings
cannot be rebound; `var` bindings can. Lists and objects are immutable values.
`append`, `extend`, and `update` return new values. Missing fields, invalid
indexes, arithmetic errors, and incompatible operands fail immediately.

Conditions require bool. There is no truthiness and no implicit conversion.
Explicit `string`, `int`, and `number` conversions are available. Equality is
structural for JSON values. Logical operators short-circuit.

Opaque AgentHandle, Action result, HumanMessage, and ArtifactRef values cannot be
forged or serialized as ordinary input. Action results transparently expose
their underlying text/JSON while preserving invocation provenance.

## Control flow and functions

v1 provides `if/else`, exhaustive `match` with `_`, finite `for`, `while`,
`loop`, `break`, `continue`, `return`, and `await`. Top-level functions use
positional or named arguments and may await Actions/effects. They cannot recurse,
capture local bindings, accept or return callable values, or be imported.

Every `while`/`loop` back edge must be proven to cross a durable `await`.
Finite `for` needs no effect. Independently, each pure interval receives
1,000,000 IR steps and resets after a durable effect.

Pure builtins are `len`, `range`, `enumerate`, `zip`, `min`, `max`, `clamp`,
`get`, `append`, `extend`, `update`, `slice`, `trim`, explicit conversions,
`is_*`, `assert`, and `fail`.

## Agents and Actions

Constructors accept named `key`, `name`, `role`, `system`, `model`, `skills`,
and `access`. The default key is `"main"`. Identity is
`(Session, template, canonical key)`; first configuration is frozen. Session
access overrides match the template name and remain below the Session ceiling.

Action parameters are dynamic values serialized as one object. An Action with no
result schema returns text. A result schema returns validated JSON. `if_needed`
is designed for persistent work Actions that produce a normal report plus a
typed decision; it is not a requirement to answer with only a status token.

## Parallel worlds

~~~rust
let fixed = parallel {
    primary => {
        let worker = Researcher(key = "primary", name = "Primary");
        await worker.research(question = ctx.request, objective = "primary evidence")
    },
    challenge => {
        let worker = Researcher(key = "challenge", name = "Challenge");
        await worker.research(question = ctx.request, objective = "counterevidence")
    },
};

let reports = parallel for route in plan.routes key route.key {
    let researcher = Researcher(key = route.key, name = route.name);
    await researcher.research(question = ctx.request, objective = route.objective)
};
~~~

Fixed `parallel` returns an object keyed by declared branch name. Dynamic
parallelism returns input order, requires unique scalar keys, and places the
branch index plus canonical key hash in durable effect paths. Branches receive
local environment copies and merge only by return value. They still share the
Project Workspace. The built-in `parallel-universe` is the complete dynamic-key
example; the fixed form above is also exercised by durable runtime tests.

## Context and durable builtins

`ctx` exposes `request`, `instructions`, `params`, `trigger`, `session_id`, and
`project`. Durable calls are:

- Agent Actions and `Agent.set_access`;
- `ask_human(question, response = DeclaredSchema, agent)`;
- `wait(seconds|minutes, name)`;
- `ctx.project.changes(after_cursor, exclude_current_program)`;
- `publish_artifact(name, content, kind, media_type, metadata, agent)`;
- `publish_home(action, metadata)`.

No time, random, filesystem, process, or network primitive exists in the
language. Those capabilities belong to explicitly authorized Agent tools.
The optional Human response schema is a top-level language schema reference,
expanded and validated at compile time; raw JSON Schema is not part of the
Workflow source interface.
