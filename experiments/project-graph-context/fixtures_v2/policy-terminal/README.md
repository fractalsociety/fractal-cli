# Terminal policy fixture

Implement `policy.retry.run_plan` using stdlib-only code.  It has the sibling
fixture's transient retry and checkpoint semantics, but this task emphasizes
negative paths: `denied` (and any unknown outcome) is terminal and must never
be retried, while `hard_budget` stops before an attempt that would exceed the
budget.  Return `status`, ordered `completed`, per-step `attempts`, and the
checkpoint list.  A checkpoint is written only after `ok`; reruns skip those
steps.  Do not execute callbacks, spawn processes, or use network APIs.

`legacy_policy.py` is a decoy that would bypass the terminal policy.
