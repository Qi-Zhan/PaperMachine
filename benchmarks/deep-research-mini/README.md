# Deep Research Mini

This directory contains a small, reproducible development slice of
[DeepResearch Bench](https://github.com/Ayanami0730/deep_research_bench). It is
for runtime smoke tests and workflow iteration, not for claiming an official
benchmark score.

The upstream benchmark contains 100 English and Chinese PhD-level research
tasks with dimension-specific rubrics. The default matrix uses five research
questions chosen to exercise different failure modes, including:

- current protocol comparison and primary-source synthesis (`69`);
- product/plugin capability and maintenance verification (`66`);
- architecture and operational recommendation research (`68`).

`tasks.json` keeps the original prompt and a compact routing/rubric projection
for PaperMachine workflows. The authoritative prompts and full rubrics remain
in the upstream `data/prompt_data/query.jsonl` and
`data/criteria_data/criteria.jsonl` files.

Source license: Apache-2.0. Always record the workflow source hash, model,
timestamp, report, token usage, cached input tokens, and observed failures when
adding a run result.

Development run metadata lives under `results/`. These files preserve measured
behavior and known defects; they are not official DeepResearch Bench scores.

## Controlled matrix runner

`run_matrix.py` compares selectable conditions using a deterministic, resumable job
order:

- `single_agent`: one persistent Session researches and writes the report;
- `evidence_r1`: at least two evidence routes, one internal evaluator pass, then writer;
- `evidence_r2`: the same workflow with at most one evaluator-directed follow-up
  round;
- `evidence_r3`: at least three parallel routes and up to three evaluator passes,
  with a larger workflow budget for difficult tasks;
- `evidence_r4`: four forced-independent routes and up to four evaluator passes,
  with four directed follow-ups per failed pass. This is the intentionally
  expensive stress tier, not the normal default recommendation.

Every final report is then sent to the separate `report-grader` workflow. The
grader sees the original question and every upstream criterion and explanation,
but not the research condition or its internal evaluation. Python applies the
upstream criterion and dimension weights after validating that the grader
returned every criterion exactly once. Research and grading usage are recorded
separately, including cache reads, uncached input tokens, and failed or invalid
attempts that incurred cost before a successful retry.

The default experiment is five tasks, five conditions, and two repeats. Use
`--conditions` to run a controlled subset such as the three-condition baseline
or only the expensive difficulty tier:

```bash
python3 benchmarks/deep-research-mini/run_matrix.py
python3 benchmarks/deep-research-mini/run_matrix.py --conditions evidence_r4
```

The generated run directory contains a commit-pinned upstream snapshot,
resumable state, raw reports, raw grader outputs, and a Markdown summary. The
point-wise post-write score is useful for controlled comparisons but is not the
upstream reference-normalized RACE score and does not include FACT citation
scraping.
