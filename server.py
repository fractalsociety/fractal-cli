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

# Milestone/task ids use a single uppercase-letter prefix (M for the Mac Runtime
# PRD, P for the pipeline PRD, …) + digits, so one board tool serves many PRDs.
MILESTONE_RE = re.compile(r"^###\s+([A-Z]\d+)\s+—\s+(.+?)\s*$")
GATE_RE = re.compile(r"^Gate\s+([A-Z]\d+)\s+—\s+`?([^`]+)`?:\s*$")
CHECK_RE = re.compile(r"^- \[([ xX])\]\s+(.+?)\s*$")
TASK_ID_RE = re.compile(r"^([A-Z]\d+\.\d+)\s+(.+)$")
TASK_NODE_ID_RE = re.compile(r"^[A-Z]\d+\.(?:\d+|G\d+)$")
GRAPH_NODE_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
TASK_ACTION_RE = re.compile(r"^/api/tasks/([^/]+)/(checkout|complete|release)$")
DEVELOPMENT_RE = re.compile(r"^/api/development$")
AGENT_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:/-]{0,79}$")
STATE_WRITE_LOCK = threading.Lock()
RESHAPING_OPS = frozenset({"grow", "repair"})

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
            if task["id"] == task_id and task["kind"] in {"task", "gate"}:
                return task
    raise TaskStateError(HTTPStatus.NOT_FOUND, f"unknown PRD task: {task_id}")


def _write_state_atomic(state_path: Path, state: dict[str, Any]) -> None:
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

        _write_state_atomic(state_path, state)
        return dict(assignment)


def _graph_identity(graph_path: Path) -> tuple[str, set[str]]:
    graph = json.loads(graph_path.read_text(encoding="utf-8"))
    if not isinstance(graph, dict) or graph.get("schema") != "fractal.execution_graph.v1":
        raise TaskStateError(
            HTTPStatus.CONFLICT,
            f"{graph_path} is not a fractal.execution_graph.v1 graph",
        )
    graph_id = graph.get("graph_id")
    if not isinstance(graph_id, str) or not graph_id.strip():
        raise TaskStateError(HTTPStatus.CONFLICT, "execution graph id is malformed")
    nodes = graph.get("nodes")
    if not isinstance(nodes, list):
        raise TaskStateError(HTTPStatus.CONFLICT, "execution graph nodes are malformed")
    node_ids: set[str] = set()
    for node in nodes:
        node_id = node.get("id") if isinstance(node, dict) else None
        if not isinstance(node_id, str) or not GRAPH_NODE_ID_RE.fullmatch(node_id):
            raise TaskStateError(HTTPStatus.CONFLICT, "execution graph has an invalid node id")
        if node_id in node_ids:
            raise TaskStateError(
                HTTPStatus.CONFLICT,
                f"execution graph has duplicate node id: {node_id}",
            )
        node_ids.add(node_id)
    return graph_id, node_ids


def mutate_graph_node_state(
    action: str,
    node_id: str,
    agent_id: str,
    agent_label: str | None = None,
    *,
    graph_path: Path,
    state_path: Path,
    now: datetime | None = None,
) -> dict[str, Any]:
    """Atomically check out, complete, or release one committed graph node."""
    if action not in {"checkout", "complete", "release"}:
        raise TaskStateError(HTTPStatus.BAD_REQUEST, f"unsupported task action: {action}")
    if not GRAPH_NODE_ID_RE.fullmatch(node_id):
        raise TaskStateError(HTTPStatus.BAD_REQUEST, f"invalid graph node id: {node_id}")
    agent_id, label = _agent_payload(agent_id, agent_label)
    timestamp = (now or datetime.now(timezone.utc)).astimezone(timezone.utc).isoformat()
    state_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = state_path.with_suffix(f"{state_path.suffix}.lock")

    with STATE_WRITE_LOCK, lock_path.open("a+", encoding="utf-8") as lock_file:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        graph_id, node_ids = _graph_identity(graph_path)
        if node_id not in node_ids:
            raise TaskStateError(HTTPStatus.NOT_FOUND, f"unknown graph node: {node_id}")
        state = _read_json(state_path)
        state_graph_id = state.get("graph_id")
        if state_graph_id is not None and state_graph_id != graph_id:
            raise TaskStateError(
                HTTPStatus.CONFLICT,
                f"state belongs to execution graph {state_graph_id}, not {graph_id}",
            )
        assignments = state.setdefault("assignments", {})
        if not isinstance(assignments, dict):
            raise TaskStateError(HTTPStatus.CONFLICT, "graph assignments state is malformed")
        current = assignments.get(node_id)
        current = current if isinstance(current, dict) else None

        if action == "checkout":
            if current and current.get("state") == "completed":
                raise TaskStateError(
                    HTTPStatus.CONFLICT,
                    f"graph node {node_id} is already complete",
                )
            if (
                current
                and current.get("state") == "checked_out"
                and current.get("agent_id") != agent_id
            ):
                raise TaskStateError(
                    HTTPStatus.CONFLICT,
                    f"graph node {node_id} is checked out by "
                    f"{current.get('agent_label', current.get('agent_id'))}",
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
            assignments[node_id] = assignment
        else:
            if (
                not current
                or current.get("state") != "checked_out"
                or current.get("agent_id") != agent_id
            ):
                raise TaskStateError(
                    HTTPStatus.CONFLICT,
                    f"graph node {node_id} is not checked out by {agent_id}",
                )
            if action == "complete":
                current["state"] = "completed"
                current["completed_at"] = timestamp
            else:
                current["state"] = "released"
                current["released_at"] = timestamp
            assignment = current

        state["graph_id"] = graph_id
        _write_state_atomic(state_path, state)
        return dict(assignment)


def _normalize_development_step(raw: dict[str, Any]) -> dict[str, Any]:
    """Validate and normalize one developmental step for board visibility."""
    required = (
        "step_id",
        "operation",
        "scale",
        "subject",
        "motivating_outcome",
        "produced_outcome",
    )
    for field in required:
        value = raw.get(field)
        if not isinstance(value, str) or not value.strip():
            raise TaskStateError(HTTPStatus.BAD_REQUEST, f"development.{field} is required")
    operation = str(raw["operation"]).strip().lower()
    if operation not in {"grow", "repair", "differentiate"}:
        raise TaskStateError(
            HTTPStatus.BAD_REQUEST,
            "development.operation must be grow|repair|differentiate",
        )
    step_id = str(raw["step_id"]).strip()
    if not GRAPH_NODE_ID_RE.fullmatch(step_id):
        raise TaskStateError(HTTPStatus.BAD_REQUEST, f"invalid development.step_id: {step_id}")
    visible = raw.get("visible_node_id")
    if visible is not None:
        if not isinstance(visible, str) or not GRAPH_NODE_ID_RE.fullmatch(visible):
            raise TaskStateError(HTTPStatus.BAD_REQUEST, "invalid development.visible_node_id")
    anchored = raw.get("anchored", True)
    if not isinstance(anchored, bool):
        raise TaskStateError(HTTPStatus.BAD_REQUEST, "development.anchored must be a boolean")
    return {
        "step_id": step_id,
        "operation": operation,
        "scale": str(raw["scale"]).strip(),
        "subject": str(raw["subject"]).strip(),
        "motivating_outcome": str(raw["motivating_outcome"]).strip(),
        "produced_outcome": str(raw["produced_outcome"]).strip(),
        "anchored": anchored,
        "visible_node_id": visible.strip() if isinstance(visible, str) else None,
    }


def record_development_step(
    step: dict[str, Any],
    *,
    graph_path: Path,
    state_path: Path,
    now: datetime | None = None,
) -> dict[str, Any]:
    """Append a developmental step to the graph board state (lineage-visible)."""
    normalized = _normalize_development_step(step)
    timestamp = (now or datetime.now(timezone.utc)).astimezone(timezone.utc).isoformat()
    state_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = state_path.with_suffix(f"{state_path.suffix}.lock")

    with STATE_WRITE_LOCK, lock_path.open("a+", encoding="utf-8") as lock_file:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        graph_id, node_ids = _graph_identity(graph_path)
        visible = normalized.get("visible_node_id")
        if visible is not None and visible not in node_ids:
            # Grown nodes may not be in the compiled harness yet — allow, but
            # keep the step visible on the board with lineage metadata.
            pass
        state = _read_json(state_path)
        state_graph_id = state.get("graph_id")
        if state_graph_id is not None and state_graph_id != graph_id:
            raise TaskStateError(
                HTTPStatus.CONFLICT,
                f"state belongs to execution graph {state_graph_id}, not {graph_id}",
            )
        development = state.setdefault("development", {"schema": "fractal.board_development.v1", "steps": []})
        if not isinstance(development, dict):
            raise TaskStateError(HTTPStatus.CONFLICT, "graph development state is malformed")
        steps = development.setdefault("steps", [])
        if not isinstance(steps, list):
            raise TaskStateError(HTTPStatus.CONFLICT, "graph development.steps is malformed")
        for existing in steps:
            if isinstance(existing, dict) and existing.get("step_id") == normalized["step_id"]:
                raise TaskStateError(
                    HTTPStatus.CONFLICT,
                    f"development step {normalized['step_id']} already recorded",
                )
        recorded = {
            **normalized,
            "recorded_at": timestamp,
        }
        steps.append(recorded)
        state["graph_id"] = graph_id
        state["development"] = development
        _write_state_atomic(state_path, state)
        return recorded


def development_summary(state: dict[str, Any]) -> dict[str, Any]:
    """Board-visible developmental lineage summary for P5.4."""
    development = state.get("development")
    if not isinstance(development, dict):
        return {
            "schema": "fractal.board_development.v1",
            "steps": [],
            "grew_or_repaired": False,
            "visible": False,
        }
    steps = development.get("steps", [])
    steps = steps if isinstance(steps, list) else []
    normalized = [step for step in steps if isinstance(step, dict)]
    reshaping = [
        step
        for step in normalized
        if str(step.get("operation", "")).lower() in RESHAPING_OPS
    ]
    return {
        "schema": "fractal.board_development.v1",
        "steps": normalized,
        "grew_or_repaired": bool(reshaping),
        "visible": bool(normalized),
        "reshaping_count": len(reshaping),
    }


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


def parse_graph(graph_path: Path, state_path: Path | None = None) -> dict[str, Any]:
    """Adapt a committed execution graph to the browser-ready board shape."""
    graph = json.loads(graph_path.read_text(encoding="utf-8"))
    if not isinstance(graph, dict) or graph.get("schema") != "fractal.execution_graph.v1":
        raise ValueError(f"{graph_path} is not a fractal.execution_graph.v1 graph")

    state = _read_json(state_path) if state_path is not None else {}
    state_graph_id = state.get("graph_id")
    if state_graph_id is not None and state_graph_id != graph.get("graph_id"):
        raise ValueError(
            f"{state_path} belongs to execution graph {state_graph_id}, "
            f"not {graph.get('graph_id')}"
        )
    assignments = state.get("assignments", {})
    assignments = assignments if isinstance(assignments, dict) else {}
    graph_id = graph["graph_id"]
    nodes = graph["nodes"]
    edges = graph["edges"]

    def node_status(node_id):
        assignment = assignments.get(node_id)
        assignment = dict(assignment) if isinstance(assignment, dict) else None
        assignment_state = assignment.get("state") if assignment else None
        status = (
            "complete"
            if assignment_state == "completed"
            else "active"
            if assignment_state == "checked_out"
            else "incomplete"
        )
        return status, assignment

    # Planning phase: the lead planner is the root node(s) (no incoming edges).
    # Until it completes, reveal ONLY the planner ("planning the task breakdown…"),
    # so the board doesn't display the tasks before they're planned. Once the
    # plan node is complete, reveal the full set of tasks it planned. The frontend
    # re-polls, so the board expands automatically.
    incoming = {node["id"]: 0 for node in nodes}
    for edge in edges:
        target = edge.get("to")
        if target in incoming:
            incoming[target] += 1
    roots = [node["id"] for node in nodes if incoming.get(node["id"], 0) == 0]
    # Only a *single* root is the lead planner. A graph with several independent
    # roots has no single planning step, so it reveals everything immediately.
    planner = roots[0] if len(roots) == 1 else None
    planning = planner is not None and node_status(planner)[0] != "complete"
    visible_ids = {planner} if planning else {node["id"] for node in nodes}

    tasks = []
    for node in nodes:
        if node["id"] not in visible_ids:
            continue
        status, assignment = node_status(node["id"])
        title = f"{node['kind']}: {node['capability']}"
        if planning and node["id"] == planner:
            title = "🧠 planning the task breakdown…"
        tasks.append(
            {
                "id": node["id"],
                "title": title,
                "kind": "task",
                "status": status,
                "checked": status == "complete",
                "assignment": assignment,
            }
        )
    group_edges = [
        {
            key: edge[key]
            for key in ("from", "to", "condition")
            if key in edge
        }
        for edge in edges
        if edge.get("from") in visible_ids and edge.get("to") in visible_ids
    ]
    group = {
        "id": "G0",
        "title": "Execution graph",
        "gate": "",
        "tasks": tasks,
        "edges": group_edges,
    }
    total = len(tasks)
    completed = sum(task["status"] == "complete" for task in tasks)
    active = sum(task["status"] == "active" for task in tasks)
    incomplete = total - completed - active
    group_status = (
        "active"
        if active
        else "complete"
        if tasks and completed == total
        else "incomplete"
    )
    source_mtime = datetime.fromtimestamp(
        graph_path.stat().st_mtime, timezone.utc
    ).isoformat()
    view = {
        "schema": "fractal.execution_graph_view.v1",
        "graph": graph,
        "title": f"Execution graph {graph_id}",
        "work_id": graph_id,
        "source": graph_path.name,
        "source_mtime": source_mtime,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "phase": "planning" if planning else "executing",
        "note": (
            "Lead agent is planning — tasks appear once the plan is ready."
            if planning
            else ""
        ),
        # Evolution lineage: present when this graph is a self-evolved child, so
        # the board can show an "evolved from …" marker for the grown / repaired /
        # differentiated task set.
        "lineage": (
            {
                "evolved_from": graph.get("parent_graph"),
                "arm": graph.get("evolution_arm"),
                "generation": graph.get("evolution"),
            }
            if graph.get("parent_graph")
            else None
        ),
        "development": development_summary(state),
        "totals": {
            "complete": completed,
            "active": active,
            "incomplete": incomplete,
            "all": total,
            "percent": round((completed / total) * 100) if total else 0,
        },
        "overview": {
            "nodes": [
                {
                    "id": "G0",
                    "title": "Execution graph",
                    "status": group_status,
                    "completed": completed,
                    "total": total,
                    "progress": round((completed / total) * 100) if total else 0,
                    "gate": "",
                }
            ],
            "edges": [],
        },
        "groups": [group],
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
    graph_path: Path | None = None

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, directory=str(APP_DIR), **kwargs)

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        route = self.path.split("?", 1)[0]
        if route == "/api/graph":
            if self.graph_path is not None:
                self._send_json(parse_graph(self.graph_path, self.state_path))
            else:
                self._send_json(parse_prd(self.prd_path, self.state_path))
            return
        if route == "/api/health":
            self._send_json({"ok": True, "service": "fractal-execution-graph"})
            return
        super().do_GET()

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        route = self.path.split("?", 1)[0]
        if DEVELOPMENT_RE.fullmatch(route):
            self._handle_development_post()
            return
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
            if self.graph_path is not None:
                assignment = mutate_graph_node_state(
                    match.group(2),
                    match.group(1),
                    agent_id,
                    agent_label,
                    graph_path=self.graph_path,
                    state_path=self.state_path,
                )
            else:
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

    def _handle_development_post(self) -> None:
        if self.graph_path is None:
            self._send_json(
                {"error": "development recording requires --graph board mode"},
                HTTPStatus.BAD_REQUEST,
            )
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > 8192:
                raise TaskStateError(
                    HTTPStatus.BAD_REQUEST,
                    "request body must be 1-8192 bytes",
                )
            payload = json.loads(self.rfile.read(length))
            if not isinstance(payload, dict):
                raise TaskStateError(
                    HTTPStatus.BAD_REQUEST,
                    "request body must be a JSON object",
                )
            recorded = record_development_step(
                payload,
                graph_path=self.graph_path,
                state_path=self.state_path,
            )
            self._send_json({"ok": True, "step": recorded})
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
    parser.add_argument("--state", type=Path, default=DEFAULT_STATE)
    parser.add_argument("--graph", type=Path)
    args = parser.parse_args()
    GraphHandler.prd_path = args.prd.resolve()
    GraphHandler.state_path = args.state.resolve()
    GraphHandler.graph_path = args.graph.resolve() if args.graph is not None else None
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
