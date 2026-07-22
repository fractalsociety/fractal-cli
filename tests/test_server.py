import json
import tempfile
import threading
import unittest
from datetime import datetime, timezone
from http import HTTPStatus
from pathlib import Path
from http.server import ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from execution_graph_server import GraphHandler, TaskStateError, mutate_task_state, parse_prd


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

    def test_checkout_conflict_release_and_completion_attribution(self):
        prd = """### M0 — Foundations
- [ ] M0.1 Audit system.
"""
        first_time = datetime(2026, 7, 22, 10, 0, tzinfo=timezone.utc)
        second_time = datetime(2026, 7, 22, 11, 0, tzinfo=timezone.utc)
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            prd_path = root / "PRD.md"
            state_path = root / "state.json"
            prd_path.write_text(prd, encoding="utf-8")
            state_path.write_text('{"active": []}\n', encoding="utf-8")

            assignment = mutate_task_state(
                "checkout",
                "M0.1",
                "codex/root",
                "Codex · root",
                prd_path=prd_path,
                state_path=state_path,
                now=first_time,
            )
            self.assertEqual(assignment["state"], "checked_out")
            graph = parse_prd(prd_path, state_path)
            task = graph["groups"][0]["tasks"][0]
            self.assertEqual(task["status"], "active")
            self.assertEqual(task["assignment"]["agent_label"], "Codex · root")

            with self.assertRaises(TaskStateError) as conflict:
                mutate_task_state(
                    "checkout",
                    "M0.1",
                    "claude/worker-1",
                    prd_path=prd_path,
                    state_path=state_path,
                )
            self.assertEqual(conflict.exception.status, HTTPStatus.CONFLICT)

            mutate_task_state(
                "release",
                "M0.1",
                "codex/root",
                prd_path=prd_path,
                state_path=state_path,
                now=second_time,
            )
            state = json.loads(state_path.read_text(encoding="utf-8"))
            self.assertNotIn("M0.1", state["active"])
            self.assertEqual(state["assignments"]["M0.1"]["state"], "released")

            mutate_task_state(
                "checkout",
                "M0.1",
                "claude/worker-1",
                "Claude 1",
                prd_path=prd_path,
                state_path=state_path,
                now=second_time,
            )
            with self.assertRaises(TaskStateError):
                mutate_task_state(
                    "complete",
                    "M0.1",
                    "claude/worker-1",
                    prd_path=prd_path,
                    state_path=state_path,
                )

            prd_path.write_text(prd.replace("[ ]", "[x]"), encoding="utf-8")
            completed = mutate_task_state(
                "complete",
                "M0.1",
                "claude/worker-1",
                prd_path=prd_path,
                state_path=state_path,
                now=second_time,
            )
            self.assertEqual(completed["state"], "completed")
            task = parse_prd(prd_path, state_path)["groups"][0]["tasks"][0]
            self.assertEqual(task["status"], "complete")
            self.assertEqual(task["assignment"]["agent_id"], "claude/worker-1")

    def test_http_checkout_returns_assignment_and_conflict(self):
        prd = """### M0 — Foundations
- [ ] M0.1 Audit system.
"""
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            prd_path = root / "PRD.md"
            state_path = root / "state.json"
            prd_path.write_text(prd, encoding="utf-8")
            state_path.write_text('{"active": []}\n', encoding="utf-8")

            class TemporaryGraphHandler(GraphHandler):
                def log_message(self, format, *args):  # noqa: A002
                    pass

            TemporaryGraphHandler.prd_path = prd_path
            TemporaryGraphHandler.state_path = state_path
            server = ThreadingHTTPServer(("127.0.0.1", 0), TemporaryGraphHandler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            endpoint = f"http://127.0.0.1:{server.server_port}/api/tasks/M0.1/checkout"
            try:
                request = Request(
                    endpoint,
                    data=json.dumps(
                        {"agent_id": "codex/root", "agent_label": "Codex · root"}
                    ).encode("utf-8"),
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                with urlopen(request) as response:
                    payload = json.load(response)
                self.assertTrue(payload["ok"])
                self.assertEqual(payload["assignment"]["agent_id"], "codex/root")

                conflict = Request(
                    endpoint,
                    data=b'{"agent_id":"claude/worker-1"}',
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                with self.assertRaises(HTTPError) as error:
                    urlopen(conflict)
                self.assertEqual(error.exception.code, HTTPStatus.CONFLICT)
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)


if __name__ == "__main__":
    unittest.main()
