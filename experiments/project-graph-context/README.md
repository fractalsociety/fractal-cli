# Project-graph-context benchmark

This directory is a self-contained, offline-oriented benchmark harness for
the A/B/C/D project-graph context hypothesis.  It is deliberately separate
from Fractal's execution graph and never reads or mutates legacy
`graph-state*.json` files.

## Reproducible commands

All worker commands are argv arrays (`shell=False`); no shell interpolation or
network fallback is permitted.

```sh
cd /workspace/fractal-cli
python3 experiments/project-graph-context/cli.py calibrate \
  --output /tmp/pgc-calibration
python3 experiments/project-graph-context/cli.py scripted-pilot \
  --replicates 2 --output /tmp/pgc-scripted-pilot
python3 experiments/project-graph-context/cli.py analyze \
  /tmp/pgc-scripted-pilot --output /tmp/pgc-scripted-pilot/analysis-rerun.json
python3 experiments/project-graph-context/cli.py live-pilot \
  --replicates 10
```

`calibrate` and `scripted-pilot` run only the deterministic adapter in
`scripted_worker.py`.  Their receipts are synthetic adapter receipts, not LLM
usage claims.  `live-pilot` prints the smallest proposed Sol-high/Luna plan;
it never starts a live run.  A root-approved live command must provide an
explicit argv list and a frozen clean commit to `runner.py`.

## Preregistered protocol

The complete protocol is in [PROTOCOL.md](PROTOCOL.md).  In brief, the unit is
one task/arm/replicate episode.  The planner is Sol-high and the worker is Luna
and both are held fixed across arms.  Tasks are independently randomized by a
seeded, balanced schedule; related task pairs are retained for paired
calibration and pilot checks.  The learning estimand (whether prior evidence
changes routing and repeated failures) is separate from the primary context
estimand (the effect of the context arm on correctness, intent, opens, tokens,
and repairs).

* **A — search-only:** task/search snippets and no curated graph.
* **B — behavior → source handbook:** behavior notes, source pointers, and a
  scope handbook; no execution graph or prior outcomes.
* **C — three-layer graph:** behavior, source, and execution nodes plus typed
  edges.
* **D — C + time-sliced prior:** C plus dated prior evidence, outcomes, and
  lessons.  Priors are frozen before a run and cannot include current episode
  events.

The primary comparisons are C versus A and D versus C.  A result is never
called a win when a required metric is absent.  The analysis reports a
conservative `inconclusive`/no-go decision for missing telemetry or fewer than
10 complete task/replicate pairs (the scripted pilot intentionally has small
`n`).

## Layout and contracts

* `runner.py` — detached worktree execution, read-only context mount, offline
  environment, timeout/output/repair/token budgets, event ledger, and evidence
  hashes.
* `scorer.py` — ledger validation and oracle/intent/open/failure metric
  normalization.
* `analysis.py` — raw arm metrics, paired deltas/ratios, deterministic
  bootstrap intervals, and exact threshold decisions.
* `corpus.py` and `fixtures/` — four tiny deterministic tasks (two related
  pairs) and private checkers copied outside episode worktrees.
* `schemas/` and `examples/` — versioned JSON contracts and representative
  manifests/contexts/ledgers.
* `tests/` — unit/integration checks for isolation, scope, telemetry,
  determinism, timeout/checker behavior, and threshold/no-go analysis.

## Corpus v2

The sanitized v2 corpus is documented in [CORPUS_V2.md](CORPUS_V2.md).  It is
kept separate from the preregistered four-task v1 pilot: use `corpus-v2` to
inspect public/holdout metadata and `quality-v2` to run the local baseline,
gold, mutation, determinism, leakage, and scope gate.  A failing task is
quarantined rather than silently included, and holdout checker contents remain
external to episode contexts/results.

The runner records token/cost fields as unavailable unless the worker writes a
valid `project-graph-context.usage-receipt.v1` receipt with non-negative input,
output, total (`input + output`) and cost values.  A checker or script cannot
make an unavailable metric appear to be zero.
