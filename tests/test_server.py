import json
import tempfile
import unittest
from pathlib import Path

from execution_graph_server import parse_prd


class ParsePrdTests(unittest.TestCase):
    def test_statuses_edges_and_totals(self):
        prd = """### M0 — Foundations
- [x] M0.1 Audit system.
- [ ] M0.2 Build contracts.

Gate M0 — `READY`:
- [ ] Tests pass.

### M1 — Runtime
- [ ] M1.1 Start daemon.
"""
        state = {"active": ["M0.2"], "title": "Test graph"}
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            prd_path = root / "PRD.md"
            state_path = root / "state.json"
            prd_path.write_text(prd, encoding="utf-8")
            state_path.write_text(json.dumps(state), encoding="utf-8")
            graph = parse_prd(prd_path, state_path)

        self.assertEqual(graph["schema"], "fractal.execution_graph_view.v1")
        self.assertEqual(graph["graph"]["schema"], "fractal.execution_graph.v1")
        self.assertEqual(len(graph["graph"]["graph_hash"]), 71)
        self.assertEqual(len(graph["view_hash"]), 71)
        self.assertEqual(graph["totals"], {"complete": 1, "active": 1, "incomplete": 2, "all": 4, "percent": 25})
        self.assertEqual(graph["overview"]["nodes"][0]["status"], "active")
        self.assertEqual(graph["groups"][0]["tasks"][2]["id"], "M0.G1")
        self.assertEqual(graph["overview"]["edges"], [{"from": "M0", "to": "M1"}])

    def test_hash_is_deterministic_across_generation_times(self):
        prd = """### M0 — Foundations
- [x] M0.1 Audit system.
"""
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            prd_path = root / "PRD.md"
            state_path = root / "state.json"
            prd_path.write_text(prd, encoding="utf-8")
            state_path.write_text('{"active": []}', encoding="utf-8")
            first = parse_prd(prd_path, state_path)
            second = parse_prd(prd_path, state_path)
        self.assertEqual(first["graph"]["graph_hash"], second["graph"]["graph_hash"])
        self.assertEqual(first["view_hash"], second["view_hash"])


if __name__ == "__main__":
    unittest.main()
