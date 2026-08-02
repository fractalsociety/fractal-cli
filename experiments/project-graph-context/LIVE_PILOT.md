# Live Sol/Luna pilot status

The committed adapter is [live_adapter.py](live_adapter.py). It passes only
argv arrays to `codex exec`, disables web search and proxy/network variables,
uses `--ephemeral` with fixed `gpt-5.6-sol`/`gpt-5.6-luna` and high reasoning,
and writes JSONL event evidence outside the episode worktree. Usage receipts are
accepted only from numeric `turn.completed.usage` fields; a missing cost field
stays `null`.

The smallest configured pilot is one related pair (`calculator-add` and
`calculator-subtract`), four arms, one replicate (eight Luna cells) with two
arm-blind Sol plans. Run calibration first:

```sh
python3 experiments/project-graph-context/live_pilot.py \
  --output /tmp/pgc-live-pilot \
  --calibrate-only
```

Inspect the calibration report before resuming the remaining cells. The driver
now refuses to start more cells when the actual receipt is missing, timed out,
or exceeds the 20,000-token cell ceiling. The observed calibration is recorded
in [results/live-calibration-report.json](results/live-calibration-report.json):
the hidden checker passed and only the target file changed, but worker totals
were 65,647 and 53,148 tokens, so the pilot is an honest budget no-go and no
A/B/C/D cells were started.

Raw prompts, JSONL traces, receipts, and detached worktrees are intentionally
kept in the ignored `/tmp` run directory and are not committed.
