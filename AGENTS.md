# Engineering principles

- Prefer structural simplification to additive layering. Reuse existing ownership and lifecycle primitives before adding a manager, registry, state machine, or compatibility path.
- Treat substantial code growth for a small change as a design warning. Seek one general rule, merge duplicate paths, and delete obsolete machinery.
- Keep one source of truth per invariant. Derived projections may exist, but must never become independent state models.
- Prefer standard Rust, Python, and Web primitives with ordinary control flow. Add an abstraction only when it removes more complexity than it introduces.
- Fail closed at security, persistence, schema, and recovery boundaries. Do not mask invariant mismatches with fallback, retry, or guessed reconstruction.
- Tests must protect real behavior, security, concurrency, or crash boundaries. Do not keep tests that only prove removed features remain absent.
- Validate proportionally: focused regression first, affected suites next, and real dogfood for model/tool/persistence paths.
- Make clean breaks unless migration or compatibility is explicitly requested.
- Preserve the core boundary: Project is PaperMachine-managed durable state; Workspace is the user filesystem Agents may access. Never merge their storage or security boundaries.
- Keep Workflow Actions and Agent-created tasks on the same ActionRunner; use AgentInput as the single durable inbox instead of adding a message scheduler or status model.
- Keep ToolRegistry membership, filesystem/process authorization, and hosted provider tools as independent authority surfaces.
- Keep benchmark harnesses, datasets, runs, and reports outside this repository.
