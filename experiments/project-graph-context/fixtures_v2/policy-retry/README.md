# Transient recovery fixture

Implement `policy.retry.run_plan` using stdlib-only code.  `plan` is an ordered
list of step names and `outcomes` maps each step to a deterministic list of
outcomes.  `ok` completes a step and writes a checkpoint; `transient` retries
the same step up to `max_retries`; any other outcome is terminal.  A rerun with
the same checkpoint skips completed steps.  Return a JSON-safe dictionary with
`status` (`completed`, `denied`, or `budget_exhausted`), `completed`, `attempts`,
and `checkpoint`.  `hard_budget` caps all attempts, including retries, and the
budget check happens before another attempt.  Never execute callbacks or use
network/process APIs.  `legacy_policy.py` is a decoy that violates the retry
bound.
