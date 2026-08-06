# BrowseComp Mini

This directory contains a deterministic six-question development slice of
OpenAI's [BrowseComp](https://openai.com/index/browsecomp/). BrowseComp contains
1,266 deliberately difficult short-answer browsing problems whose answers are
usually easy to verify once found but hard to locate.

The sample is exactly `random.Random(0).sample(rows, 6)` from the pinned official
CSV. Both questions and reference answers remain encrypted in `tasks.json` and
are decrypted only in memory. Research Workflows receive the question but
never the answer. After research completes, a separate no-tool
`short-answer-grader` Session receives the question, final response, and answer
and applies the upstream correctness criteria.

The default matrix compares single-agent research, one-round coverage research,
and two-round coverage research, with two repeats and at most two concurrent
Workflows:

```bash
python3 benchmarks/browsecomp-mini/run_matrix.py
```

Use `--prepare-only` to create the pinned matrix without calling the API. Ctrl-C
requests cancellation for every in-flight run before exiting. The report keeps
grader tokens separate from research cost and records raw tokens, effective
uncached tokens, cache reads, hosted-search calls, continuation hits, source
hashes, final responses, and raw grader judgments.

This slice is for runtime and workflow diagnosis, not leaderboard comparison.
