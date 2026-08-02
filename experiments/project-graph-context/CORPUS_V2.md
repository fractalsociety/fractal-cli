# Sanitized corpus v2

Corpus v2 is an offline benchmark extension with eight deterministic,
multi-file tasks arranged into four behaviorally related pairs:

| Pair | Public development task | Sealed holdout task |
| --- | --- | --- |
| state | `storage-normal` | `storage-corrupt` |
| board | `board-filters` | `board-rollback` |
| graph | `graph-valid` | `graph-diagnostics` |
| policy | `policy-retry` | `policy-terminal` |

Each seed contains three to eight files, a target module, plausible sibling
modules, and at least three dependent behavior steps.  Seeds are original
sanitized fixtures; local repositories were used only as high-level design
references.  No original prompts, answers, hidden evaluators, learning
records, credentials, or legacy `graph-state*.json` files are read or copied.
Fixtures use Python/Node/Rust standard libraries only and have no network or
external side effects.

## Quality gate

`task_quality.py` runs the private checker from a directory outside each audit
worktree.  It checks prompt/checker clause counts, a failing baseline, a
passing reference implementation, five named mutants (no-op, wrong-file,
happy-path, overbroad, policy-bypass), three identical checker runs, source
leakage/network scans, scope discrimination, and dependency/localization
non-triviality.  Mutation detection must be at least 80%; otherwise a task is
quarantined and omitted from the included corpus split.

The checker emits only `passed`, a stable failure code, and clause counts.
Holdout metadata exposes seed/dependency/behavior hashes but marks checker
contents as `sealed-external`; checker source and reference patches never enter
episode contexts or result ledgers.  A quality report is versioned and carries
`quality_report_hash`; split hashes are derived from dependency shape, fixture
seed hash, and behavior fingerprint.  Dedupe must use those structural fields,
not titles.

## Telemetry contract

The intent manifest carries `project-graph-context.telemetry-requirements.v1`.
Command argv, exit code, duration, open/read/write attempts, network attempts,
repair and routing/tool records, and usage receipts are distinct fields.  A
missing signal remains JSON `null`; it is never imputed as zero or success.

Run the audit without paid episodes:

```sh
python3 experiments/project-graph-context/cli.py quality-v2 \
  --output /tmp/project-graph-context-v2-quality.json
```

The command performs only local deterministic checks and exits non-zero if any
task is quarantined.  `corpus-v2` prints split metadata without checker
contents.

