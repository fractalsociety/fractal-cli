# Direct A-vs-D live pilot

The authorized run used [ad_live_run.py](ad_live_run.py) at harness commit
`571e65c`: four corpus tasks, arms A and D, three independent repetitions
(24 Luna cells, 12 paired observations), and four arm-blind Sol-high plans
(one per task). A/D order was seeded with `20260802` and balanced 6/6. Every
cell used a fresh detached worktree, fresh Codex home, `--ephemeral`, fixed
`gpt-5.6-luna`/high reasoning, offline environment, and at most four concurrent
workers. Arm D received only frozen time-sliced paired-task examples.

Run command:

```sh
python3 experiments/project-graph-context/ad_live_run.py \
  --output /tmp/pgc-ad-live \
  --summary-path experiments/project-graph-context/results/ad-live-summary.json
```

Outcome: all 24 cells completed, with 1,352,354 worker tokens plus 44,100
planner tokens (1,396,454 total), below the 2.25M hard ceiling. There were no
infrastructure failures, context/git mutations, or leakage flags. A had 11/12
successful tasks (0.9167); D had 12/12 (1.0). The direct paired success delta
was +0.0833 (bootstrap 95% CI [0, 0.25]); aggregate D/A rate ratio was 1.0909.
Mean D-minus-A worker tokens were +2,165.7 (ratio 1.0514; bootstrap ratio CI
[0.9798, 1.1164]). One A `text-slugify-edge` cell failed the hidden oracle;
all other cells passed. Intent violations and evidenced irrelevant opens were
zero in both arms.

Cost, repair iterations, routing, and tool-selection telemetry were absent and
remain unavailable; no zeros were imputed. A-vs-D is descriptive only and
cannot separate three-layer graph exposure from prior/learning exposure. The
results support a controlled C-vs-A / D-vs-C decomposition follow-up, not a
production go decision. Compact metrics, paired CIs, cell ledger hashes, and
limitations are in [results/ad-live-summary.json](results/ad-live-summary.json).
Raw prompts, JSONL traces, receipts, and detached worktrees remain outside the
repository.
