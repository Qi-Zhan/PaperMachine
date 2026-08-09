# Kernel validation — 2026-08-09

This release validates the clean-break PaperMachine kernel on macOS. Native
Windows was intentionally out of scope.

## Released changes

The implementation was split into independent commits:

1. `020f54c` — canonical FunctionCall persistence and aborted recovery;
2. `b436d73` — atomic Workflow transitions and reliable controls;
3. `a04cc9b` — immutable model routes, Project lifecycle leases, and unified
   host-read/Workspace-write authorization;
4. `6971c7e` — filesystem catalog truth, ManagedFs, instructions-only Skills,
   and exact Project-home Action provenance;
5. `2863e3c` — bounded Workflow protocol, per-Project StoreHandle, streaming
   rollout replay, ownership index, and terminal scheduler cleanup;
6. the release commit — crash-matrix completion, documentation/test cleanup,
   and current real-provider evidence.

The resulting recovery rule is deliberately small: a validated model
FunctionCall is canonical before dispatch; a returned FunctionCallOutput is
canonical before the next sample. Recovery never dispatches an old call. A
call without output gets exactly one `"aborted"` output, and the same Agent must
observe durable reality before choosing whether to create a new call. Workflow
host effects retain their separate deterministic effect journal.

## Process crash matrix

`crates/server/tests/process_recovery.rs` launches the real server binary,
pauses a debug build at one named boundary, sends SIGKILL, and restarts over the
same data directory. All cases passed:

| Boundary | Observed result |
|---|---|
| FunctionCall received, before canonical checkpoint | model sampled again; call absent from recovered context and never dispatched |
| FunctionCall checkpointed, before dispatch | one aborted Tool Step; command never ran |
| tool effect completed, before output checkpoint | effect remained; old call became aborted; Agent made a new observation call; effect was not repeated |
| FunctionCallOutput checkpointed, before Step projection | real output repaired the Step; command was not repeated |
| terminal candidate checkpointed, before Turn commit | Turn completed with no new model request |

The same suite also passed rollout-ahead-of-projection replay, usage recovery,
and in-flight model resampling.

## Real DeepSeek dogfood

A fresh data directory was created and `deepseek-flash` routed to provider
`deepseek`, upstream model `deepseek-v4-flash`. The pinned route fingerprint was
`e6844be18c07864cec661db074bf3c45f0d47f322ce4abf16ac87554920a58e5`;
no credential entered the snapshot, evidence, logs, or Git.

### Side effect followed by process loss

- The first server process executed one append to `recovery-proof.log` and was
  SIGKILLed after the effect but before canonical output.
- Restart recovered the same Workflow, ActionInvocation, ActionAttempt, Turn,
  and Session.
- Old call `call_00_jmNgwgYT7Mg6wq609rNE0406` became `aborted` with exactly one
  canonical aborted output.
- The mutating command was dispatched exactly once; the file contained exactly
  one line after recovery.
- DeepSeek made a new read observation call before completing.
- Canonical/projected rollout sequences converged at `27/27`; Workflow status
  was `completed`, with no Action retry or terminal failure.
- The Turn ToolSet was exactly `exec_command`, `read_file`, hash
  `16f2e9aa06d1283485fb5d4c0fd8870166406f3e0dc49a5d2a8ed4fd20d2a454`.

### Project Summary first publish and refresh

Both Summary runs used DeepSeek and exactly these Project tools:
`patch_project_home`, `preview_project_home`, `read_project_home`. Their shared
ToolSet hash was
`785c8cc1064f4db53f99614a5f706f183e28547e0a63e6c61ab3657abc4c0ef5`;
neither Turn received a Workspace tool.

- First publish began from no existing page, used six model samples, and made
  two failed patch attempts before correcting itself. Its final preview had
  zero diagnostics and publication completed normally.
- A separate Project Artifact then added the exact verified marker
  `KERNEL-DOGFOOD-MILESTONE-2026-08-09`.
- Refresh used four model samples, began from the first page Artifact, preserved
  supported content, included the new marker, and ended with zero diagnostics.
- Each page/source Artifact stored the exact ActionInvocation ID of its awaited
  `_ActionCall`; the Project canonical pointer ended at the refreshed page.
- Each Summary had one ActionInvocation and one ActionAttempt, with no failed
  model Step or terminal failure.

The first Summary's two visible tool errors are expected and useful evidence:
the Summary Agent was allowed to inspect a bad patch result, correct it, preview
again, and finish only after the materialized page was valid.

## Complete release checks

All commands passed:

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

Python results were 37 DSL/built-in tests, 12 deep-research mini tests, 4
BrowseComp mini tests, 13 LiveDR mini tests, and 8 shared benchmark-runtime
tests. Web results were 9 tests plus a successful production build.
