import json
import os
import tempfile
import threading
import unittest
from datetime import datetime, timezone
from http import HTTPStatus
from pathlib import Path
from http.server import ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from execution_graph_server import (
    GraphHandler,
    TaskStateError,
    mutate_graph_node_state,
    mutate_task_state,
    parse_graph,
    parse_prd,
)


class ParsePrdTests(unittest.TestCase):
    def test_committed_graph_maps_to_one_board_group(self):
        committed = {
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_sample",
            "work_hash": "sha256:" + "1" * 64,
            "harness_hash": "sha256:" + "2" * 64,
            "compiler_version": "test/1",
            "target": "darwin-arm64",
            "nodes": [
                {
                    "id": "analyze",
                    "kind": "model",
                    "capability": "reason",
                    "memory_scopes": ["work"],
                    "budget": {"timeout_ms": 1000},
                },
                {
                    "id": "verify",
                    "kind": "tool",
                    "capability": "test",
                    "memory_scopes": ["work"],
                    "budget": {"timeout_ms": 1000},
                },
            ],
            "edges": [
                {
                    "from": "analyze",
                    "to": "verify",
                    "condition": "predecessor_complete",
                }
            ],
            "graph_hash": "sha256:" + "3" * 64,
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            graph_path = Path(temp_dir) / "sample.json"
            graph_path.write_text(json.dumps(committed), encoding="utf-8")
            state_path = Path(temp_dir) / "sample-state.json"

            # Planning phase: with the planner (root) not yet complete, the board
            # reveals only the planner — tasks are not displayed before they are
            # planned.
            planning = parse_graph(graph_path)
            self.assertEqual(planning["phase"], "planning")
            self.assertEqual(
                planning["groups"][0]["tasks"],
                [
                    {
                        "id": "analyze",
                        "title": "🧠 planning the task breakdown…",
                        "kind": "task",
                        "status": "incomplete",
                        "checked": False,
                        "assignment": None,
                    }
                ],
            )
            self.assertEqual(planning["groups"][0]["edges"], [])

            # Once the planner completes, the board reveals the full planned graph.
            state_path.write_text(
                json.dumps(
                    {
                        "graph_id": "fg_sample",
                        "assignments": {
                            "analyze": {
                                "state": "completed",
                                "agent_id": "a",
                                "completed_at": 1,
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )
            view = parse_graph(graph_path, state_path)

        expected_keys = {
            "schema",
            "graph",
            "title",
            "work_id",
            "groups",
            "overview",
            "totals",
            "view_hash",
        }
        self.assertTrue(expected_keys.issubset(view))
        self.assertEqual(view["graph"], committed)
        self.assertEqual(view["phase"], "executing")
        self.assertEqual(len(view["groups"]), 1)
        self.assertEqual(
            view["groups"][0]["tasks"],
            [
                {
                    "id": "analyze",
                    "title": "model: reason",
                    "kind": "task",
                    "status": "complete",
                    "checked": True,
                    "assignment": {
                        "state": "completed",
                        "agent_id": "a",
                        "completed_at": 1,
                    },
                },
                {
                    "id": "verify",
                    "title": "tool: test",
                    "kind": "task",
                    "status": "incomplete",
                    "checked": False,
                    "assignment": None,
                },
            ],
        )
        self.assertEqual(view["groups"][0]["edges"], committed["edges"])

    def test_graph_view_preserves_dependency_derived_execution_labels(self):
        committed = {
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_waves",
            "nodes": [
                {
                    "id": "plan",
                    "kind": "control",
                    "capability": "control.plan",
                    "title": "Plan",
                    "instruction": "Plan the work.",
                    "execution": {"mode": "sequential", "wave": 1, "parallel_group": None},
                },
                {
                    "id": "app",
                    "kind": "tool",
                    "capability": "code.generate",
                    "title": "Build app",
                    "instruction": "Build the app.",
                    "execution": {"mode": "parallel", "wave": 2, "parallel_group": "wave-2"},
                },
                {
                    "id": "tests",
                    "kind": "tool",
                    "capability": "code.generate",
                    "title": "Write tests",
                    "instruction": "Write tests.",
                    "execution": {"mode": "parallel", "wave": 2, "parallel_group": "wave-2"},
                },
            ],
            "edges": [
                {"from": "plan", "to": "app", "condition": "success"},
                {"from": "plan", "to": "tests", "condition": "success"},
            ],
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            graph_path = root / "graph.json"
            state_path = root / "state.json"
            graph_path.write_text(json.dumps(committed), encoding="utf-8")
            state_path.write_text(
                json.dumps({
                    "graph_id": "fg_waves",
                    "assignments": {
                        "plan": {"state": "completed", "agent_id": "claude"},
                    },
                }),
                encoding="utf-8",
            )
            tasks = parse_graph(graph_path, state_path)["groups"][0]["tasks"]

        self.assertEqual(tasks[0]["title"], "Plan")
        self.assertEqual(tasks[0]["instruction"], "Plan the work.")
        self.assertEqual(tasks[1]["execution"]["mode"], "parallel")
        self.assertEqual(tasks[1]["execution"]["wave"], 2)
        self.assertEqual(tasks[2]["execution"]["parallel_group"], "wave-2")

    def test_graph_node_lifecycle_is_persistent_attributed_and_conflict_safe(self):
        committed = {
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_lifecycle",
            "nodes": [
                {"id": "analyze", "kind": "model", "capability": "reason"},
                {"id": "implement", "kind": "tool", "capability": "code.edit"},
            ],
            "edges": [],
        }
        first_time = datetime(2026, 7, 23, 12, 0, tzinfo=timezone.utc)
        second_time = datetime(2026, 7, 23, 12, 5, tzinfo=timezone.utc)
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            graph_path = root / "graph.json"
            state_path = root / "graph-state.json"
            graph_path.write_text(json.dumps(committed), encoding="utf-8")

            checked_out = mutate_graph_node_state(
                "checkout",
                "analyze",
                "codex-1",
                "Coordinate · codex-1",
                graph_path=graph_path,
                state_path=state_path,
                now=first_time,
            )
            self.assertEqual(checked_out["state"], "checked_out")
            active = parse_graph(graph_path, state_path)
            self.assertEqual(active["totals"], {
                "complete": 0,
                "active": 1,
                "incomplete": 1,
                "all": 2,
                "percent": 0,
            })
            self.assertEqual(
                active["groups"][0]["tasks"][0]["assignment"]["agent_id"],
                "codex-1",
            )

            with self.assertRaises(TaskStateError) as conflict:
                mutate_graph_node_state(
                    "checkout",
                    "analyze",
                    "cursor-1",
                    graph_path=graph_path,
                    state_path=state_path,
                )
            self.assertEqual(conflict.exception.status, HTTPStatus.CONFLICT)

            completed = mutate_graph_node_state(
                "complete",
                "analyze",
                "codex-1",
                graph_path=graph_path,
                state_path=state_path,
                now=second_time,
            )
            self.assertEqual(completed["state"], "completed")
            view = parse_graph(graph_path, state_path)
            self.assertEqual(view["groups"][0]["tasks"][0]["status"], "complete")
            self.assertTrue(view["groups"][0]["tasks"][0]["checked"])
            self.assertEqual(view["totals"]["complete"], 1)
            self.assertEqual(view["totals"]["percent"], 50)

            with self.assertRaises(TaskStateError) as completed_conflict:
                mutate_graph_node_state(
                    "checkout",
                    "analyze",
                    "codex-1",
                    graph_path=graph_path,
                    state_path=state_path,
                )
            self.assertEqual(completed_conflict.exception.status, HTTPStatus.CONFLICT)

    def test_graph_node_release_allows_a_new_agent_to_claim(self):
        committed = {
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_release",
            "nodes": [{"id": "verify-related", "kind": "tool", "capability": "test"}],
            "edges": [],
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            graph_path = root / "graph.json"
            state_path = root / "graph-state.json"
            graph_path.write_text(json.dumps(committed), encoding="utf-8")

            mutate_graph_node_state(
                "checkout",
                "verify-related",
                "codex-1",
                graph_path=graph_path,
                state_path=state_path,
            )
            released = mutate_graph_node_state(
                "release",
                "verify-related",
                "codex-1",
                graph_path=graph_path,
                state_path=state_path,
            )
            self.assertEqual(released["state"], "released")
            reclaimed = mutate_graph_node_state(
                "checkout",
                "verify-related",
                "cursor-1",
                graph_path=graph_path,
                state_path=state_path,
            )
            self.assertEqual(reclaimed["agent_id"], "cursor-1")
            self.assertEqual(
                parse_graph(graph_path, state_path)["groups"][0]["tasks"][0]["status"],
                "active",
            )

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

    def test_http_graph_mode_accepts_slug_checkout_and_completion(self):
        committed = {
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_http",
            "nodes": [{"id": "implement", "kind": "model", "capability": "code.edit"}],
            "edges": [],
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            graph_path = root / "graph.json"
            state_path = root / "graph-state.json"
            graph_path.write_text(json.dumps(committed), encoding="utf-8")

            class TemporaryGraphHandler(GraphHandler):
                def log_message(self, format, *args):  # noqa: A002
                    pass

            TemporaryGraphHandler.graph_path = graph_path
            TemporaryGraphHandler.state_path = state_path
            server = ThreadingHTTPServer(("127.0.0.1", 0), TemporaryGraphHandler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            base = f"http://127.0.0.1:{server.server_port}"

            def post(action, agent_id="codex-reverse-1"):
                return Request(
                    f"{base}/api/tasks/implement/{action}",
                    data=json.dumps({
                        "agent_id": agent_id,
                        "agent_label": f"Coordinate · {agent_id}",
                    }).encode("utf-8"),
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )

            try:
                with urlopen(post("checkout")) as response:
                    checkout = json.load(response)
                self.assertEqual(checkout["task_id"], "implement")
                self.assertEqual(checkout["assignment"]["state"], "checked_out")

                with urlopen(f"{base}/api/graph") as response:
                    active = json.load(response)
                self.assertEqual(active["groups"][0]["tasks"][0]["status"], "active")
                self.assertEqual(active["totals"]["active"], 1)

                with urlopen(post("complete")) as response:
                    completed = json.load(response)
                self.assertEqual(completed["assignment"]["state"], "completed")
                with urlopen(f"{base}/api/graph") as response:
                    view = json.load(response)
                self.assertEqual(view["groups"][0]["tasks"][0]["status"], "complete")
                self.assertEqual(view["totals"]["percent"], 100)

                unknown = Request(
                    f"{base}/api/tasks/not-in-graph/checkout",
                    data=b'{"agent_id":"codex-reverse-1"}',
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                with self.assertRaises(HTTPError) as error:
                    urlopen(unknown)
                self.assertEqual(error.exception.code, HTTPStatus.NOT_FOUND)

                develop = Request(
                    f"{base}/api/development",
                    data=json.dumps({
                        "step_id": "grow-verify-edge",
                        "operation": "grow",
                        "scale": "graph",
                        "subject": "graph:fixture",
                        "motivating_outcome": "sha256:" + ("a" * 64),
                        "produced_outcome": "sha256:" + ("b" * 64),
                        "anchored": True,
                        "visible_node_id": "verify.edgecase",
                    }).encode("utf-8"),
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                with urlopen(develop) as response:
                    recorded = json.load(response)
                self.assertTrue(recorded["ok"])
                self.assertEqual(recorded["step"]["operation"], "grow")
                with urlopen(f"{base}/api/graph") as response:
                    developed = json.load(response)
                self.assertTrue(developed["development"]["visible"])
                self.assertTrue(developed["development"]["grew_or_repaired"])
                self.assertEqual(developed["development"]["reshaping_count"], 1)
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)

    def test_http_pause_requires_token_and_invokes_narrow_fractal_stop(self):
        committed = {
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_pause",
            "nodes": [{"id": "build", "kind": "tool", "capability": "code.edit"}],
            "edges": [],
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            graph_path = root / "graph.json"
            state_path = root / "graph-state.json"
            workspace = root / "trusted-workspace"
            run_state = workspace / ".fractal" / "run-state.json"
            run_state.parent.mkdir(parents=True)
            run_state.write_text('{"status":"running"}\n', encoding="utf-8")
            graph_path.write_text(json.dumps(committed), encoding="utf-8")
            arguments = root / "arguments.txt"
            executable = root / "fractal-test"
            executable.write_text(
                "#!/bin/sh\n"
                f"printf '%s\\n' \"$@\" > '{arguments}'\n"
                f"printf '%s\\n' '{{\"status\":\"halted\"}}' > '{run_state}'\n",
                encoding="utf-8",
            )
            os.chmod(executable, 0o700)

            class TemporaryGraphHandler(GraphHandler):
                def log_message(self, format, *args):  # noqa: A002
                    pass

            TemporaryGraphHandler.graph_path = graph_path
            TemporaryGraphHandler.state_path = state_path
            TemporaryGraphHandler.fractal_bin = executable
            TemporaryGraphHandler.workspace = workspace
            TemporaryGraphHandler.control_token = "test-control-token"
            server = ThreadingHTTPServer(("127.0.0.1", 0), TemporaryGraphHandler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            base = f"http://127.0.0.1:{server.server_port}"
            try:
                with urlopen(f"{base}/api/graph") as response:
                    graph = json.load(response)
                self.assertEqual(graph["run_control"]["phase"], "running")
                self.assertEqual(graph["run_control"]["token"], "test-control-token")

                unauthorized = Request(
                    f"{base}/api/run/pause",
                    data=b"",
                    method="POST",
                )
                with self.assertRaises(HTTPError) as error:
                    urlopen(unauthorized)
                self.assertEqual(error.exception.code, HTTPStatus.FORBIDDEN)

                pause = Request(
                    f"{base}/api/run/pause",
                    data=b"",
                    headers={"X-Fractal-Control-Token": "test-control-token"},
                    method="POST",
                )
                with urlopen(pause) as response:
                    result = json.load(response)
                self.assertTrue(result["ok"])
                self.assertEqual(
                    arguments.read_text(encoding="utf-8").splitlines(),
                    ["stop", "--project", str(workspace)],
                )
                with urlopen(f"{base}/api/graph") as response:
                    halted = json.load(response)
                self.assertEqual(halted["run_control"]["phase"], "halted")
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)

    def test_gate_checkout_and_completion_are_attributed(self):
        prd = """### M3 — Isolation
- [x] M3.1 Build backend.

Gate M3 — `ISOLATION_READY`:
- [ ] Disposable execution passes.
"""
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            prd_path = root / "PRD.md"
            state_path = root / "state.json"
            prd_path.write_text(prd, encoding="utf-8")
            state_path.write_text('{"active": []}\n', encoding="utf-8")

            assignment = mutate_task_state(
                "checkout",
                "M3.G1",
                "codex/root",
                "Codex · root",
                prd_path=prd_path,
                state_path=state_path,
            )
            self.assertEqual(assignment["state"], "checked_out")
            gate = parse_prd(prd_path, state_path)["groups"][0]["tasks"][1]
            self.assertEqual(gate["kind"], "gate")
            self.assertEqual(gate["status"], "active")

            with self.assertRaises(TaskStateError):
                mutate_task_state(
                    "complete",
                    "M3.G1",
                    "codex/root",
                    prd_path=prd_path,
                    state_path=state_path,
                )
            prd_path.write_text(prd.replace("[ ] Disposable", "[x] Disposable"), encoding="utf-8")
            completed = mutate_task_state(
                "complete",
                "M3.G1",
                "codex/root",
                prd_path=prd_path,
                state_path=state_path,
            )
            self.assertEqual(completed["state"], "completed")


if __name__ == "__main__":
    unittest.main()
