# Live C-vs-D comparison

The authorized run used [cd_live_run.py](cd_live_run.py) at harness commit
`04db2c3f86c269917ae16be38c0e3c8efa555370`: four frozen corpus tasks, arms C
and D, three independent repetitions (24 Luna cells and 12 paired
observations), and the same balanced A/D order remapped A→C. The four
arm-blind Sol plans and task versions were verified and copied byte-for-byte
from the completed A-vs-D run; this comparison made zero new Sol calls.

Each cell used a fresh detached worktree, fresh temporary Codex home, fixed
`gpt-5.6-luna`/high reasoning, offline environment, read-only mounted context,
and at most four concurrent workers. Raw prompts, JSONL traces, usage receipts,
hidden checkers, and worktrees remain under `/tmp`, not in the repository.

Run command (the output directory must be a new temporary directory):

```sh
python3 experiments/project-graph-context/cd_live_run.py \
  --output /tmp/pgc-cd-live.XXXXXX \
  --source-run /tmp/pgc-ad-live.mTXYpP \
  --summary-path experiments/project-graph-context/results/cd-live-summary.json
```

## Observed outcome

All 24 cells completed with no infrastructure, leakage, safety, or git
mutation flags. Actual Luna usage was **1,346,126 tokens**; the reused Sol
plan receipts add **44,100**, for **1,390,226 all-agent tokens**, below both
hard ceilings. C passed 11/12 and D passed 12/12; the paired D−C correctness
delta was +0.0833 (bootstrap 95% CI [0, 0.25]). One C
`text-slugify-edge` cell failed the hidden oracle; all other cells passed.

The token proxy was D/C = 1.0128 and wall-time proxy D/C = 1.0162, so neither
met the ≤0.90 execution-cost criterion. Dollar cost, repeated-failure ratio
(C's count was zero), routing quality, and tool-selection quality were
unavailable or had zero denominators and are explicitly **untestable**, never
imputed as passes. The overall learning decision is therefore
`inconclusive`; production go remains false. Compact paired metrics, CIs,
cell ledger hashes, context/plan hashes, safety evidence, and limitations are
in [results/cd-live-summary.json](results/cd-live-summary.json).
