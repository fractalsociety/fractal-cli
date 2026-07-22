#!/usr/bin/env python3
"""Local execution-graph viewer backed directly by the Fractal Mac Runtime PRD."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import re
import tempfile
import threading
from datetime import datetime, timezone
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


APP_DIR = Path(__file__).resolve().parent
PROJECT_DIR = APP_DIR.parent
DEFAULT_PRD = PROJECT_DIR / "FRACTAL_MAC_RUNTIME_PRD.md"
DEFAULT_STATE = APP_DIR / "graph-state.json"

MILESTONE_RE = re.compile(r"^###\s+(M\d+)\s+—\s+(.+?)\s*$")
GATE_RE = re.compile(r"^Gate\s+(M\d+)\s+—\s+`?([^`]+)`?:\s*$")
CHECK_RE = re.compile(r"^- \[([ xX])\]\s+(.+?)\s*$")
TASK_ID_RE = re.compile(r"^(M\d+\.\d+)\s+(.+)$")
TASK_NODE_ID_RE = re.compile(r"^M\d+\.\d+$")
TASK_ACTION_RE = re.compile(r"^/api/tasks/(M\d+\.\d+)/(checkout|complete|release)$")
AGENT_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:/-]{0,79}$")
STATE_WRITE_LOCK = threading.Lock()

MILESTONE_DEPS: dict[str, list[str]] = {
    "M0": [],
    "M1": ["M0"],
    "M2": ["M0"],
    "M3": ["M0"],
    "M4": ["M1", "M2", "M3"],
    "M5": ["M4"],
    "M6": ["M5"],
    "M7": ["M5", "M6"],
    "M8": ["M7"],
    "M9": ["M7"],
    "M10": ["M8", "M9"],
}


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError):
        return {}
    return value if isinstance(value, dict) else {}


class TaskStateError(ValueError):
    """A safe task-state mutation failure with an HTTP-compatible status."""

    def __init__(self, status: HTTPStatus, message: str) -> None:
        super().__init__(message)
        self.status = status


def _agent_payload(agent_id: str, agent_label: str | None) -> tuple[str, str]:
    agent_id = agent_id.strip()
    if not AGENT_ID_RE.fullmatch(agent_id):
        raise TaskStateError(
            HTTPStatus.BAD_REQUEST,
            "agent_id must be 1-80 safe identifier characters",
        )
    label = (agent_label or agent_id).strip()
    if not label or len(label) > 80 or any(
        ord(character) < 32 for character in label
    ):
        raise TaskStateError(
            HTTPStatus.BAD_REQUEST,
            "agent_label must be 1-80 printable characters",
        )
    return agent_id, label


def _task_from_prd(task_id: str, prd_path: Path, state_path: Path) -> dict[str, Any]:
    graph = parse_prd(prd_path, state_path)
    for group in graph["groups"]:
        for task in group["tasks"]:
            if task["id"] == task_id and task["kind"] == "task":
                return task
    raise TaskStateError(HTTPStatus.NOT_FOUND, f"unknown PRD task: {task_id}")


def mutate_task_state(
    action: str,
    task_id: str,
    agent_id: str,
    agent_label: str | None = None,
    *,
    prd_path: Path = DEFAULT_PRD,
    state_path: Path = DEFAULT_STATE,
    now: datetime | None = None,
) -> dict[str, Any]:
    """Atomically check out, complete, or release one PRD task for an agent."""
    if action not in {"checkout", "complete", "release"}:
        raise TaskStateError(HTTPStatus.BAD_REQUEST, f"unsupported task action: {action}")
    if not TASK_NODE_ID_RE.fullmatch(task_id):
        raise TaskStateError(HTTPStatus.BAD_REQUEST, f"invalid task id: {task_id}")
    agent_id, label = _agent_payload(agent_id, agent_label)
    timestamp = (now or datetime.now(timezone.utc)).astimezone(timezone.utc).isoformat()
    state_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = state_path.with_suffix(f"{state_path.suffix}.lock")

    with STATE_WRITE_LOCK, lock_path.open("a+", encoding="utf-8") as lock_file:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        task = _task_from_prd(task_id, prd_path, state_path)
        state = _read_json(state_path)
        assignments = state.setdefault("assignments", {})
        if not isinstance(assignments, dict):
            raise TaskStateError(HTTPStatus.CONFLICT, "graph assignments state is malformed")
        active = state.setdefault("active", [])
        if not isinstance(active, list):
            raise TaskStateError(HTTPStatus.CONFLICT, "graph active state is malformed")
        active[:] = [str(item) for item in active]
        current = assignments.get(task_id)
        current = current if isinstance(current, dict) else None

        if action == "checkout":
            if task["checked"]:
                raise TaskStateError(HTTPStatus.CONFLICT, f"task {task_id} is already complete")
            if (
                current
                and current.get("state") == "checked_out"
                and current.get("agent_id") != agent_id
            ):
                raise TaskStateError(
                    HTTPStatus.CONFLICT,
                    f"task {task_id} is checked out by {current.get('agent_label', current.get('agent_id'))}",
                )
            checked_out_at = (
                current.get("checked_out_at", timestamp)
                if current and current.get("agent_id") == agent_id
                else timestamp
            )
            assignment = {
                "agent_id": agent_id,
                "agent_label": label,
                "state": "checked_out",
                "checked_out_at": checked_out_at,
            }
            assignments[task_id] = assignment
            if task_id not in active:
                active.append(task_id)
        else:
            if not current or current.get("agent_id") != agent_id:
                raise TaskStateError(
                    HTTPStatus.CONFLICT,
                    f"task {task_id} is not owned by {agent_id}",
                )
            if action == "complete":
                if not task["checked"]:
                    raise TaskStateError(
                        HTTPStatus.CONFLICT,
                        f"task {task_id} must be checked in the PRD before attribution is completed",
                    )
                current["state"] = "completed"
                current["completed_at"] = timestamp
            else:
                current["state"] = "released"
                current["released_at"] = timestamp
            assignment = current
            active[:] = [item for item in active if item != task_id]

        file_descriptor, temporary_name = tempfile.mkstemp(
            prefix=f".{state_path.name}.", suffix=".tmp", dir=state_path.parent
        )
        try:
            with os.fdopen(file_descriptor, "w", encoding="utf-8") as temporary_file:
                json.dump(state, temporary_file, indent=2)
                temporary_file.write("\n")
                temporary_file.flush()
                os.fsync(temporary_file.fileno())
            if state_path.exists():
                os.chmod(temporary_name, state_path.stat().st_mode & 0o777)
            os.replace(temporary_name, state_path)
        finally:
            if os.path.exists(temporary_name):
                os.unlink(temporary_name)
        return dict(assignment)


def parse_prd(prd_path: Path = DEFAULT_PRD, state_path: Path = DEFAULT_STATE) -> dict[str, Any]:
    """Compile Markdown milestones and checkboxes into a browser-ready graph."""
    state = _read_json(state_path)
    prd_bytes = prd_path.read_bytes()
    active = {str(item) for item in state.get("active", [])}
    assignments = state.get("assignments", {})
    assignments = assignments if isinstance(assignments, dict) else {}
    groups: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    gate_mode = False
    gate_index = 0

    for line_number, raw_line in enumerate(prd_bytes.decode("utf-8").splitlines(), 1):
        milestone = MILESTONE_RE.match(raw_line)
        if milestone:
            current = {
                "id": milestone.group(1),
                "title": milestone.group(2),
                "line": line_number,
                "tasks": [],
            }
            groups.append(current)
            gate_mode = False
            gate_index = 0
            continue

        if current is None:
            continue

        gate = GATE_RE.match(raw_line)
        if gate and gate.group(1) == current["id"]:
            gate_mode = True
            current["gate"] = gate.group(2)
            continue

        check = CHECK_RE.match(raw_line)
        if not check:
            continue

        checked = check.group(1).lower() == "x"
        body = check.group(2)
        explicit_id = TASK_ID_RE.match(body)
        if explicit_id:
            node_id, title = explicit_id.groups()
            kind = "task"
        else:
            gate_index += 1
            node_id = f"{current['id']}.G{gate_index}"
            title = body
            kind = "gate" if gate_mode else "criterion"

        assignment = assignments.get(node_id)
        assignment = dict(assignment) if isinstance(assignment, dict) else None
        checked_out = assignment is not None and assignment.get("state") == "checked_out"
        status = (
            "complete"
            if checked
            else "active"
            if node_id in active or checked_out
            else "incomplete"
        )
        if assignment is not None and checked:
            assignment["state"] = "completed"
        current["tasks"].append(
            {
                "id": node_id,
                "title": title.rstrip("."),
                "status": status,
                "checked": checked,
                "kind": kind,
                "line": line_number,
                "assignment": assignment,
            }
        )

    overview_nodes: list[dict[str, Any]] = []
    for group in groups:
        tasks = group["tasks"]
        completed = sum(task["status"] == "complete" for task in tasks)
        has_active = any(task["status"] == "active" for task in tasks)
        status = "active" if has_active else "complete" if tasks and completed == len(tasks) else "incomplete"
        overview_nodes.append(
            {
                "id": group["id"],
                "title": group["title"],
                "status": status,
                "completed": completed,
                "total": len(tasks),
                "progress": round((completed / len(tasks)) * 100) if tasks else 0,
                "line": group["line"],
                "gate": group.get("gate", ""),
            }
        )

    overview_edges = [
        {"from": dependency, "to": milestone}
        for milestone, dependencies in MILESTONE_DEPS.items()
        for dependency in dependencies
        if any(node["id"] == milestone for node in overview_nodes)
    ]

    for group in groups:
        group["edges"] = [
            {"from": group["tasks"][index - 1]["id"], "to": task["id"]}
            for index, task in enumerate(group["tasks"])
            if index > 0
        ]

    all_tasks = [task for group in groups for task in group["tasks"]]
    totals = {
        "complete": sum(task["status"] == "complete" for task in all_tasks),
        "active": sum(task["status"] == "active" for task in all_tasks),
        "incomplete": sum(task["status"] == "incomplete" for task in all_tasks),
        "all": len(all_tasks),
    }
    totals["percent"] = round((totals["complete"] / totals["all"]) * 100) if totals["all"] else 0

    compiled_nodes = [
        {
            "id": task["id"],
            "kind": "verification" if task["kind"] == "gate" else "human",
            "capability": "runtime.verify" if task["kind"] == "gate" else "runtime.implement",
            "memory_scopes": ["project:fractal-runtime"],
            "route_candidates": ["capability:human-local"],
            "budget": {"timeout_ms": 86400000},
        }
        for group in groups
        for task in group["tasks"]
    ]
    compiled_edges = [
        {"from": edge["from"], "to": edge["to"], "condition": "predecessor_complete"}
        for group in groups
        for edge in group["edges"]
    ]
    group_by_id = {group["id"]: group for group in groups}
    for milestone, dependencies in MILESTONE_DEPS.items():
        target = group_by_id.get(milestone)
        if not target or not target["tasks"]:
            continue
        for dependency in dependencies:
            source = group_by_id.get(dependency)
            if source and source["tasks"]:
                compiled_edges.append(
                    {
                        "from": source["tasks"][-1]["id"],
                        "to": target["tasks"][0]["id"],
                        "condition": "milestone_gate_passed",
                    }
                )

    compiled_graph = {
        "schema": "fractal.execution_graph.v1",
        "graph_id": "fractal-mac-runtime-prd",
        "work_hash": f"sha256:{hashlib.sha256(prd_bytes).hexdigest()}",
        "harness_hash": f"sha256:{hashlib.sha256(b'prd-implementation-harness-v1').hexdigest()}",
        "compiler_version": "fractal-prd-graphc/0.1.0",
        "target": "darwin-arm64",
        "nodes": compiled_nodes,
        "edges": compiled_edges,
    }
    compiled_hash_payload = json.dumps(
        compiled_graph, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    compiled_graph["graph_hash"] = f"sha256:{hashlib.sha256(compiled_hash_payload).hexdigest()}"

    view = {
        "schema": "fractal.execution_graph_view.v1",
        "graph": compiled_graph,
        "title": state.get("title", "Build the Fractal Mac Runtime"),
        "work_id": state.get("work_id", "fractal-mac-runtime-prd"),
        "source": prd_path.name,
        "source_mtime": datetime.fromtimestamp(prd_path.stat().st_mtime, timezone.utc).isoformat(),
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "note": state.get("note", ""),
        "totals": totals,
        "overview": {"nodes": overview_nodes, "edges": overview_edges},
        "groups": groups,
    }
    view_hash_payload = {
        key: view[key]
        for key in ("schema", "graph", "work_id", "source", "overview", "groups")
    }
    serialized_view = json.dumps(
        view_hash_payload, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    view["view_hash"] = f"sha256:{hashlib.sha256(serialized_view).hexdigest()}"
    return view


class GraphHandler(SimpleHTTPRequestHandler):
    prd_path = DEFAULT_PRD
    state_path = DEFAULT_STATE

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, directory=str(APP_DIR), **kwargs)

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        route = self.path.split("?", 1)[0]
        if route == "/api/graph":
            self._send_json(parse_prd(self.prd_path, self.state_path))
            return
        if route == "/api/health":
            self._send_json({"ok": True, "service": "fractal-execution-graph"})
            return
        super().do_GET()

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        route = self.path.split("?", 1)[0]
        match = TASK_ACTION_RE.fullmatch(route)
        if not match:
            self._send_json({"error": "not found"}, HTTPStatus.NOT_FOUND)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > 4096:
                raise TaskStateError(
                    HTTPStatus.BAD_REQUEST,
                    "request body must be 1-4096 bytes",
                )
            payload = json.loads(self.rfile.read(length))
            if not isinstance(payload, dict):
                raise TaskStateError(
                    HTTPStatus.BAD_REQUEST,
                    "request body must be a JSON object",
                )
            agent_id = payload.get("agent_id")
            if not isinstance(agent_id, str):
                raise TaskStateError(HTTPStatus.BAD_REQUEST, "agent_id is required")
            agent_label = payload.get("agent_label")
            if agent_label is not None and not isinstance(agent_label, str):
                raise TaskStateError(HTTPStatus.BAD_REQUEST, "agent_label must be a string")
            assignment = mutate_task_state(
                match.group(2),
                match.group(1),
                agent_id,
                agent_label,
                prd_path=self.prd_path,
                state_path=self.state_path,
            )
            self._send_json(
                {"ok": True, "task_id": match.group(1), "assignment": assignment}
            )
        except TaskStateError as error:
            self._send_json({"error": str(error)}, error.status)
        except (json.JSONDecodeError, ValueError):
            self._send_json({"error": "request body must be valid JSON"}, HTTPStatus.BAD_REQUEST)

    def end_headers(self) -> None:
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Security-Policy", "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; connect-src 'self'")
        super().end_headers()

    def _send_json(self, payload: dict[str, Any], status: HTTPStatus = HTTPStatus.OK) -> None:
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8090)
    parser.add_argument("--prd", type=Path, default=DEFAULT_PRD)
    args = parser.parse_args()
    GraphHandler.prd_path = args.prd.resolve()
    server = ThreadingHTTPServer((args.host, args.port), GraphHandler)
    print(f"Fractal execution graph: http://{args.host}:{args.port}/", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
