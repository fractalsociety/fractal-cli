# Corpus v2 structured-patch canary

The scored set contains five authorized tasks: the r13 `board-filters` run and
the four-task r14 run. Every worker used `codex-luna-structured-patch-v1`.
Luna received only allowed seed-file contents, the task manifest, condition-C
graph, and frozen Sol plan. Structured output was validated and applied by the
trusted runner after process exit; the hidden checker was staged after apply.

## Result

2/5 tasks passed the hidden checker (40%). All five patches were accepted and
applied. The three failures were checker `oracle_assertion_failed` results, not
policy, budget, or safety failures.

| Task | Outcome | Sol tokens | Luna tokens | Duration (ms) | Preloaded opens | Patch SHA-256 | Checker |
| --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| `board-filters` | fail | 11,626 | 12,901 | 42,658.332 | 3 | `5903ff3bf39e4a588c4f84cff72efaec8b81a18d681ff7d29cb826cd8aed0b24` | `oracle_assertion_failed` |
| `storage-normal` | pass | 11,675 | 13,067 | 47,696.524 | 3 | `07ebcadb77dec9df1104ba07175ece1ff1a7f82c303014d9c92dbc4c33784ee9` | pass |
| `storage-corrupt` | pass | 11,669 | 14,177 | 70,351.018 | 3 | `4f8722d81a90b10e19a99b4bbe0eee3e53e8416390ba6ec8ff855cc817f61ec1` | pass |
| `graph-valid` | fail | 11,689 | 16,723 | 119,010.323 | 4 | `b32c7c4b0f7320e58cf337a192b4617d4d455b72177ae31a11c9c4698b94bc9e` | `oracle_assertion_failed` |
| `policy-retry` | fail | 11,655 | 13,465 | 60,701.157 | 3 | `d9f09b333b0224865cf7f261c8c1f078d4652ebe8b63de04a5199e4accb10d51` | `oracle_assertion_failed` |

Totals: Sol **58,314** tokens, Luna **70,333** tokens, all agents **128,647**
tokens. Every task stayed below the 90,000-token post-hoc Luna cap. Policy
failure lists, safety violations, network attempts, process-inspection
attempts, and external-side-effect attempts were empty for every task.

## Evidence and scope

The full sanitized aggregate is in [aggregate.json](aggregate.json). It stores
only route/status fields, usage, durations, changed-file hashes, patch hashes,
checker summaries, and source-result hashes; raw model text and temporary
worktrees are not persisted.

Source result SHA-256 values:

- r13 `board-filters.json`: `5abed219e20a3e627e3dd44bf829b8790116c354cd64fb65338019ddc941b001`
- r14 `storage-normal.json`: `b5bfbfde99ce2321e0c7212ad1338ac60c77f567f0640dcc5f37e458ff2d4878`
- r14 `storage-corrupt.json`: `038276363487af212cb83a53219e6c64be93b5b5c92e706bfddca1e72f6bafa3`
- r14 `graph-valid.json`: `37c454099b946cb3fbb03c16e4a67e623f12f5d5f58270d22d53e6cdff91dd81`
- r14 `policy-retry.json`: `cefca332bef0b2e927c8612a1798bc746e10871c7b4f5d26f1fb6dfe24662489`

Earlier infrastructure and legacy-shell/workspace-writable canaries (r4–r12)
are explicitly excluded from this scored result. They remain historical
diagnostics and must not be combined with the structured-patch 2/5 score.
