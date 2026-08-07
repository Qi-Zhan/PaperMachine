# LiveDRBench Mini

This directory contains an eight-task development slice of Microsoft's
[LiveDRBench](https://github.com/microsoft/LiveDRBench). It tests structured
claim discovery rather than long-form prose quality:

- `0`: exhaustive entity discovery across IMO participation and country-rank data;
- `20`: identify a multilingual product-search dataset and extract model results;
- `22`: identify and verify one scientific dataset plus publication metadata;
- `23`: identify a dataset/paper and extract five framework-specific findings;
- `40`: reconstruct a seven-attempt aviation incident timeline;
- `47`: find a paper using a specific combination of geographic datasets;
- `66`: identify a material and paper satisfying several measured properties;
- `83`: find prior art for a proposed contrastive activation-attribution method.

The source Parquet is pinned by revision and SHA-256 in `tasks.json`. Questions
are plaintext because they are model inputs. Ground truths and evaluator
metadata stay in the upstream encrypted representation and are decrypted only
after research completes. A separate `model_only` grader Session receives the
reference and submitted structured answer; research Sessions never receive it.

The default matrix compares one persistent researcher with the coverage-ledger
workflow at one and two evaluator rounds, with two repeats per task. At most two
Workflows execute at once so cache warming and wall-time measurements are not
dominated by a burst of competing runs:

```bash
python3 benchmarks/live-dr-mini/run_matrix.py
```

Each research and grader job starts directly from the Project with a fresh
launch context, keeping tasks isolated from prior benchmark results.

Ctrl-C asks every in-flight Workflow to cancel before the runner exits. The
runner records model tokens, cached input, hosted web-search actions,
search-query counts, Responses continuation hits, workflow and runtime source
hashes, raw deliverables, grader cost, and precision/recall/F1. The primary
score ports the complete upstream semantic matching rubric into an isolated
Responses-based grader and computes metrics deterministically. A deliberately
strict local matcher is also recorded as a cheap regression diagnostic. This is
a development slice, not an official leaderboard score: the port batches each
task into one judge action instead of reproducing the upstream Chat Completions
call sequence byte for byte.
