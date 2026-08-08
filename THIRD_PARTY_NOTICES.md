# Third-party notices

## OpenAI Codex

Selected implementation patterns and adapted source fragments originate from
OpenAI Codex at commit `b2dc8b3e4be4fe3a453d50e13835f707b258f15b`.

- Source: https://github.com/openai/codex
- License: Apache License 2.0
- Copyright: 2025 OpenAI

Adapted files carry a source note where the relationship is direct. PaperMachine
changes the original coding-agent concepts into research-specific protocols and
does not claim API or behavior compatibility with Codex.

The accepted runtime-kernel target maps the following upstream implementation
areas for source adaptation. A mapping records provenance; it does not make the
Codex configuration schema or product domain a PaperMachine compatibility
surface.

- `codex-rs/protocol/src/permissions.rs`
- `codex-rs/sandboxing/src/manager.rs` and its platform backends
- `codex-rs/core/src/exec_env.rs`
- `codex-rs/thread-store/src/local/live_writer.rs`
- `codex-rs/core/src/session/rollout_reconstruction.rs`

On native Windows, PaperMachine directly compiles `codex-protocol`,
`codex-utils-absolute-path`, and `codex-windows-sandbox` from that exact commit.
The Git revision and upstream transport forks are pinned in Cargo metadata;
PaperMachine does not launch or depend on the Codex CLI or app-server.

## codex-mobile

The web interface uses interaction and layout ideas from codex-mobile at commit
`fac2291b0e606c869d4760f56c0f49172214cb79`.

- Source: https://github.com/friuns2/codex-mobile
- License: MIT

No codex-mobile source has been copied at this stage.

## Microsoft LiveDRBench

The benchmark adapter and `live-dr-grader` workflow adapt claim-matching
semantics and evaluation rubrics from LiveDRBench at commit
`6ff85b67b35fa303907f6f275417622338acd1f6`.

- Source: https://github.com/microsoft/LiveDRBench
- License: MIT (evaluation code); CDLA v2 (dataset)

PaperMachine keeps the released benchmark answers encrypted at rest and uses
them only in isolated post-write grading Sessions.
