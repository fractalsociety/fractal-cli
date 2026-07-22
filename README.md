# Fractal Execution Graph

This local dashboard compiles `FRACTAL_MAC_RUNTIME_PRD.md` checkboxes into a
live `fractal.execution_graph_view.v1` projection. Its `graph` member is the
backend-neutral `fractal.execution_graph.v1` contract; status and presentation
metadata deliberately remain outside the signed compiled graph.

- Green: checked in the PRD.
- Yellow: explicitly active in `graph-state.json`.
- Red: unchecked and not active.

Run:

```bash
python3 execution-graph/server.py --port 8090
```

Open <http://127.0.0.1:8090/>. The API is available at `/api/graph` and refreshes
from the PRD on every request.
