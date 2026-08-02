# Preregistered protocol (v1.0)

## Question and estimands

The question is whether progressively richer project context improves agent
task execution, and whether time-sliced prior evidence improves routing and
failure recovery without reducing correctness.

The **primary context estimand** is the paired arm effect on a fixed
Sol-high-planner/Luna-worker stack over a frozen task and commit: correctness,
weighted intent violations, irrelevant opens, worker tokens, and repair
iterations.  The **learning estimand** is the D-versus-C effect on repeated
failure codes, cost, routing/tool selection, and correctness noninferiority.
The learning estimand is reported separately and cannot be used to relabel the
primary estimand after seeing outcomes.

## Arms and fixed factors

* A (`search_only`) exposes search snippets/queries only.
* B (`behavior_source_handbook`) exposes a behavior-to-source handbook with
  allowed scope, but no execution graph or outcomes.
* C (`three_layer_graph`) exposes behavior, source, and execution layers with
  typed edges.
* D (`three_layer_graph_prior`) exposes C plus a frozen, UTC time-sliced prior
  snapshot containing prior evidence, outcomes, and lessons.

The planner model is Sol-high and the worker model is Luna, fixed across all
arms.  Prompts, worker budgets, frozen commit, task corpus, checker, and
replicate count are fixed.  No current-episode output may enter C or D
context.  D priors are hashed and frozen before execution.

## Unit, randomization, and pilot size

An episode is `(experiment_id, task_id, arm_id, replicate)`.  The schedule is
balanced across arms and task pairs using the manifest seed.  Pairing is by
exact `(task_id, replicate)`; a missing arm is never imputed.  The minimum
decision sample is 10 complete paired episodes per required metric.  A pilot
below that floor is useful for harness feasibility but is underpowered and
must be reported `inconclusive`.

The corpus has two related pairs (`calculator-add`/`calculator-subtract` and
`text-slugify`/`text-slugify-edge`) and no network dependency.  Hidden oracles
are outside episode worktrees and are invoked only after worker execution.

## Isolation and execution

Each episode resolves a commit object, creates a fresh detached git worktree,
and mounts arm context and task intent in an outside, read-only directory.
The child receives `FRACTAL_OFFLINE=1`, `NO_NETWORK=1`, disabled proxy values,
`PIP_NO_INDEX=1`, and `GIT_TERMINAL_PROMPT=0`.  Commands are argv lists and
run with `shell=False`.  The runner enforces wall timeout, output-byte,
max-repair, and (when a valid receipt exists) token budgets.  It captures
stdout/stderr, timing, exit/timeout, sorted changed paths, trace availability,
and SHA-256 evidence hashes.  A worktree is removed after scoring unless
explicitly retained for debugging; the source repo and frozen commit are not
modified.

## Outcomes and scoring

Correctness is the hidden-checker pass bit.  Intent violations are severe
scope violations plus weighted violations for forbidden/traversal paths and
out-of-scope edits.  Irrelevant opens are counted only when the worker emits a
valid open trace; otherwise the metric is unavailable.  Repair iterations,
failure-code repetitions, routing, and tool selection follow the same
availability rule.  Token and cost metrics are available only from a valid
worker usage receipt; the runner never infers them from wall time or output
length.

## Decision thresholds (exact preregistration)

For C versus A, paired relative ratios must meet all of:

* success `C/A >= 1.20` (+20%);
* weighted intent violations `C/A <= 0.75` (-25%);
* irrelevant opens `C/A <= 0.80` (-20%);
* tokens `C/A <= 0.85` (-15%);
* repair iterations `C/A <= 0.80` (-20%).

For D versus C, all of the following are required:

* repeated failure codes `D/C <= 0.85` (-15%);
* cost `D/C <= 0.90` (-10%);
* routing and tool-selection quality strictly improves (paired mean delta
  `> 0`); and
* correctness is noninferior (`D - C >= 0`).

Zero denominators, absent metrics, and incomplete pairs produce an explicit
`inconclusive` no-go decision.  The analysis emits deterministic percentile
bootstrap intervals for `n >= 2`; for smaller samples it emits
`method: small_n` with no fabricated interval.

## Leakage, contamination, and stopping rules

Context paths are outside the worktree and read-only.  The hidden checker is
copied to a separate private directory and is not in worker environment.
Tests assert that a worker cannot modify mounted context, that changed paths
are scope-scored, that arm order does not alter hashes, and that malformed
telemetry remains unavailable.  Any cross-arm context hash mismatch, source
commit mutation, checker tampering, or network attempt is a protocol failure;
the affected cell is excluded and the run is stopped for audit.  No live
high-cost run starts until scripted calibration passes and the root approves
the proposed cell count and budget.
