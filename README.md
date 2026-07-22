# Fractal Execution Graph

This local dashboard compiles `FRACTAL_MAC_RUNTIME_PRD.md` checkboxes into a
live `fractal.execution_graph_view.v1` projection. Its `graph` member is the
backend-neutral `fractal.execution_graph.v1` contract; status and presentation
metadata deliberately remain outside the signed compiled graph.

- Green: checked in the PRD.
- Yellow: explicitly active in `graph-state.json`.
- Red: unchecked and not active.
- Agent badge: the agent that checked out or completed the task. Amber means
  checked out, green means completed, and muted means released.

Run:

```bash
python3 execution-graph/server.py --port 8090
```

Open <http://127.0.0.1:8090/>. The API is available at `/api/graph` and refreshes
from the PRD on every request.

## Agent task checkout

Agents should claim a task before editing it. Checkout is atomic and rejects a
second agent while the first checkout is active:

```bash
python3 execution-graph/task-state.py checkout M3.13 \
  --agent-id codex/root --agent-label "Codex · root"
```

After verification, check the task in the PRD and retain its attribution:

```bash
python3 execution-graph/task-state.py complete M3.13 --agent-id codex/root
```

Use `release` to abandon a claim without marking the PRD complete, and `status`
to inspect a task. `FRACTAL_AGENT_ID` and `FRACTAL_AGENT_LABEL` may replace the
agent flags. Completed and released records stay in `graph-state.json`, so the
dashboard keeps showing who worked on the task.

The same lifecycle is available while the dashboard server is running:

```text
POST /api/tasks/M3.13/checkout
POST /api/tasks/M3.13/complete
POST /api/tasks/M3.13/release
{"agent_id":"codex/root","agent_label":"Codex · root"}
```

`complete` fails unless the same agent owns the checkout and the PRD checkbox is
already checked. State writes use a process lock and atomic replacement.
