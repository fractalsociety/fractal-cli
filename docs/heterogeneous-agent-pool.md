# Heterogeneous agent pool

Opt-in worker capacity for the in-process graph executor. When unset, scheduling is unchanged: `detect_agents` still auto-detects or honors `$FRACTAL_AGENTS`, and Codex still exposes a distinct lead planner plus `codex-luna` implementation route.

## Configuration

Set `$FRACTAL_AGENT_POOL` to an explicit `provider=count` list. The original four provider keys are required; OpenCode is optional. Set a temporarily unavailable provider to zero and replace its capacity with another provider:

```text
FRACTAL_AGENT_POOL=codex=6,cursor=6,claude=6,hermes=6
```

```text
FRACTAL_AGENT_POOL=codex=5,cursor=5,claude=5,hermes=5,opencode=4
```

```text
FRACTAL_AGENT_POOL=codex=5,cursor=5,claude=0,hermes=5,opencode=9
```

Rules:

* Total worker slots must be **20–42**. The example above is 24.
* Counts expand into stable identities such as `codex-luna:1`…`codex-luna:6`, `cursor:1`…`cursor:6`, `claude:1`…`claude:6`, `hermes:1`…`hermes:6`, and, when configured, `opencode:1`…`opencode:N`.
* The Codex **lead** planner is a separate roster entry (`codex` by default, or `$FRACTAL_LEAD_AGENT`). It is **not** counted as worker capacity and does not consume a pool slot.
* Codex implementation workers use the existing `codex-luna` command adapter (`gpt-5.6-luna`). The lead keeps the Sol High planner route.
* Cursor, Claude, and Hermes workers use the existing `cursor-agent` / `claude` / `hermes` adapters.
* OpenCode workers run `opencode run --format json --model <model> --dir <worktree>`. The default model is `zai-coding-plan/glm-5.3`; `$FRACTAL_OPENCODE_MODEL` overrides it.
* Duplicates, unknown providers, overflow counts, totals outside 20–42, missing core provider keys, and unavailable providers with nonzero counts are hard errors. A zero count is an explicit disable, never a silent fallback.

## Readiness

Before enabling the pool, every provider with a nonzero count must have its physical binary on `PATH`:

| Provider | Binary        | Worker kind  |
|----------|---------------|--------------|
| codex    | `codex`       | `codex-luna` |
| cursor   | `cursor-agent`| `cursor`     |
| claude   | `claude`      | `claude`     |
| hermes   | `hermes`      | `hermes`     |
| opencode (optional) | `opencode` | `opencode` |

A mixed PATH (any configured provider missing) rejects the whole configuration.

## Scheduling

* Only dependency-ready nodes are assigned.
* Each provider’s concurrent leases cannot exceed its configured count.
* Each slot holds at most one active lease.
* If one provider is slow, stalled, or returns a worker failure, the other provider slots keep pulling ready work.
* Failures requeue through the existing bounded `reopen_for_retry` path (three attempts, matching in-process repair). Nodes are not double-owned or double-completed.

## Metrics

Deterministic injected-runner tests compare identical seeded 48-node workloads against the current one-slot-per-provider baseline:

* **Logical makespan** — last completion time in work units. The 24-slot pool must be at least 40% lower.
* **Queue work units** — ready-but-unassigned nodes integrated over logical time.
* **Throughput** — completed nodes / makespan; must not regress.
* Safety: zero drops, starvation, duplicate leases, duplicate completions, or dependency violations. All four providers complete work on a healthy pool.

## Rollback

Unset the variable:

```sh
unset FRACTAL_AGENT_POOL
```

Default one-slot-per-detected-provider behavior returns immediately. No graph or controller rewrite is required.

## Opt-in 24-worker smoke

Confirm the four binaries, then run a local graph with the 24-slot roster. This launches real workers; it is not used by unit tests.

```sh
command -v codex >/dev/null
command -v cursor-agent >/dev/null
command -v claude >/dev/null
command -v hermes >/dev/null
export FRACTAL_AGENT_POOL=codex=6,cursor=6,claude=6,hermes=6
fractal run --local --graph-file path/to/graph.json
```

To stop the pool, `unset FRACTAL_AGENT_POOL` and rerun.
