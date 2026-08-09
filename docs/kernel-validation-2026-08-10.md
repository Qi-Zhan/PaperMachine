# Simplified kernel validation — 2026-08-10

This release validates the clean-break PaperMachine kernel on macOS. Native
Windows remains outside the current release scope.

## Structural result

The work deliberately removed parallel state models instead of adding another
abstraction layer:

| Commit | Structural change |
|---|---|
| `622decc` | Scope Session, Turn, Workflow, HumanRequest, and Artifact routing directly through Project; delete global ownership tables, triggers, index, event bus, and scan fallback. |
| `e30d3a2` | Collapse Project ownership into one ProjectHandle with map locking and explicit runtime shutdown. |
| `f045424` | Remove Workflow metadata state machines; orchestration is Agent/Action, normal Python control flow, `together`, `ask_human`, and one durable `wait` effect. |
| `3f3e2c0` | Restrict canonical Session JSONL to TurnCreated, ContextCheckpoint, and TurnUpdated; make Steps/events disposable projections and remove Participant lifecycle state. |
| release commit | Rename the wait state to `waiting_for_deadline`, replace obsolete documentation with the smaller current contract, and record final validation. |

Relative to `59a4e35`, the final diff is:

| Area | Added | Deleted | Net |
|---|---:|---:|---:|
| Production | 597 | 3,289 | **-2,692** |
| Tests | 289 | 487 | **-198** |
| Documentation | 997 | 1,931 | **-934** |
| Total | 1,883 | 5,707 | **-3,824** |

The current managed-state contract is schema 20 and Session rollout version 3.
There is no migration or compatibility reader for older managed state.

## Canonical recovery result

Session JSONL now contains only:

```text
TurnCreated
ContextCheckpoint
TurnUpdated
```

A validated FunctionCall is synced before dispatch. A returned
FunctionCallOutput is synced before Step completion and before another sample.
AgentSteps and Session events remain query/UI projections.

The real server SIGKILL matrix passed every boundary:

| Boundary | Result |
|---|---|
| call received before checkpoint | uncommitted call is absent and never dispatched |
| call checkpointed before dispatch | one aborted projection; old call dispatch count unchanged |
| side effect may exist before output checkpoint | old call becomes aborted; Agent observes durable reality |
| output checkpointed before projection | real output repairs projection; old call is not replayed |
| terminal candidate checkpointed before Turn commit | Turn completes without a new model sample |

Rollout-ahead-of-SQLite replay, usage recovery, in-flight resampling, route
drift, ToolSet drift, CAS, managed-root, symlink, and sandbox tests also passed.

## Real DeepSeek dogfood

A fresh temporary data directory and Workspace used profile
`deepseek-flash`, provider `deepseek`, and upstream model
`deepseek-v4-flash`. The temporary state was deleted after the run. Credentials
were loaded only from ignored `.env` process state and did not enter snapshots,
output, logs, fixtures, or Git.

### Workspace tools

One custom ordinary Workflow Action wrote the exact marker
`PM-SIMPLIFY-2026-08-10-A` to `dogfood-proof.txt`, read it back, and published
its result as a Project Artifact.

- status: completed;
- one ActionInvocation and one ActionAttempt;
- three completed model steps;
- ToolSet: `read_file`, `write_file`;
- ToolSet SHA-256:
  `90085d06c4ebbe5293146181ce381aa09bfef74722fa320b5f42eab6d5d1ca71`;
- one completed `write_file` Step and one completed `read_file` Step;
- zero failed or aborted Steps;
- the user Workspace file matched the marker byte-for-byte.

A second run replaced and verified the marker
`PM-SIMPLIFY-2026-08-10-B`, providing new Project evidence for Summary refresh.

### Project Summary first publish and refresh

Both real Summary runs completed with:

- one ActionInvocation and one ActionAttempt;
- four completed model steps;
- two Artifacts: structured source and sanitized HTML;
- zero failed or aborted Steps;
- exact ToolSet `patch_project_home`, `preview_project_home`,
  `read_project_home`;
- ToolSet SHA-256
  `785c8cc1064f4db53f99614a5f706f183e28547e0a63e6c61ab3657abc4c0ef5`.

No Workspace tool entered either Summary Turn. The first Home revision was
`138b972971100c4d29731bb8ffab3c15f8af3593ca650f4474279aa121a7f2a4`.
After new evidence, refresh produced
`274741e472b17270067be4dfddc725f047e4f532c4e9b4fdcab1c5567c35b7be`.
Raw Artifact HTML contained the required latest marker, proving that the
existing-page path committed a meaningful CAS update.

An earlier probe with no Summary instruction also completed and reused the
existing revision when the Agent found no meaningful page change. That is the
intended no-op diff suppression, not a failed refresh.

## Complete release gates

All commands passed after the final code changes:

```text
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
PYTHONPATH=python:benchmarks python3 -m unittest discover -s python/tests -p 'test_*.py'
PYTHONPATH=python:benchmarks python3 -m unittest discover -s benchmarks/deep-research-mini -p 'test_*.py'
PYTHONPATH=python:benchmarks python3 -m unittest discover -s benchmarks/browsecomp-mini -p 'test_*.py'
PYTHONPATH=python:benchmarks python3 -m unittest discover -s benchmarks/live-dr-mini -p 'test_*.py'
PYTHONPATH=python:benchmarks python3 benchmarks/test_benchmark_runtime.py
pnpm --dir apps/web test
pnpm --dir apps/web build
git diff --check
```

Observed Python counts were 37 DSL/built-in tests, 12 DeepResearch mini tests,
4 BrowseComp mini tests, 13 LiveDR mini tests, and 8 shared benchmark-runtime
tests. Web validation was 9 tests plus a successful production build.
