#!/usr/bin/env python3
"""Local execution-graph viewer backed directly by the Fractal Mac Runtime PRD."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
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


def parse_prd(prd_path: Path = DEFAULT_PRD, state_path: Path = DEFAULT_STATE) -> dict[str, Any]:
    """Compile Markdown milestones and checkboxes into a browser-ready graph."""
    state = _read_json(state_path)
    prd_bytes = prd_path.read_bytes()
    active = {str(item) for item in state.get("active", [])}
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

        status = "active" if node_id in active else "complete" if checked else "incomplete"
        current["tasks"].append(
            {
                "id": node_id,
                "title": title.rstrip("."),
                "status": status,
                "checked": checked,
                "kind": kind,
                "line": line_number,
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

    def end_headers(self) -> None:
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Security-Policy", "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; connect-src 'self'")
        super().end_headers()

    def _send_json(self, payload: dict[str, Any]) -> None:
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(HTTPStatus.OK)
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
