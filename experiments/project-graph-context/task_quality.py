#!/usr/bin/env python3
"""Automated quality gate for sanitized project-graph-context corpus v2.

The gate is deliberately deterministic and offline.  It runs private
behavioral checkers against a clean baseline, a reference implementation, and
named mutants; no LLM, network, package installation, or external service is
used.  Reports contain clause names and hashes only, never checker source or
reference answers.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping

try:
    from . import corpus_v2
except ImportError:  # pragma: no cover - direct script/import compatibility
    import corpus_v2


QUALITY_SCHEMA = "project-graph-context.task-quality.v1"
MIN_MUTATION_DETECTION = 0.80
MIN_CLAUSES = 8
TASK_TIMEOUT_SECONDS = 20


def _clause(name: str, check: Callable[[], Any]) -> tuple[str, bool]:
    try:
        value = check()
        return name, bool(value)
    except Exception:
        return name, False


def _result(clauses: Iterable[tuple[str, bool]], *, failure_code: str | None = None) -> dict[str, Any]:
    values = list(clauses)
    passed = sum(1 for _, ok in values if ok)
    failures = [name for name, ok in values if not ok]
    return {
        "passed": not failures,
        "failure_code": failure_code or (None if not failures else "oracle_assertion_failed"),
        "clauses_passed": passed,
        "clauses_total": len(values),
    }


def _import_storage(worktree: Path):
    sys.path.insert(0, str(worktree))
    for name in list(sys.modules):
        if name == "app" or name.startswith("app."):
            del sys.modules[name]
    try:
        return importlib.import_module("app.storage"), importlib.import_module("app.clock")
    finally:
        try:
            sys.path.remove(str(worktree))
        except ValueError:
            pass


def _storage_normal(worktree: Path) -> dict[str, Any]:
    storage, clock_module = _import_storage(worktree)
    with tempfile.TemporaryDirectory(prefix="pgc-v2-storage-") as tmp:
        path = Path(tmp) / "state.json"
        clock = clock_module.ManualClock(100.0)

        def fresh():
            return storage.StateStore(path, clock.now)

        store = fresh()
        clauses: list[tuple[str, bool]] = []
        clauses.append(_clause("construct_store", lambda: store.path == path))
        clauses.append(_clause("put_get_value", lambda: (store.put("alpha", {"n": 3}) is None and store.get("alpha") == {"n": 3})))
        clauses.append(_clause("missing_default", lambda: store.get("missing", "fallback") == "fallback"))
        store.put("short", "live", ttl=5)
        clauses.append(_clause("ttl_before_deadline", lambda: (clock.advance(4), store.get("short"))[1] == "live"))
        clock.advance(1)
        clauses.append(_clause("ttl_at_deadline", lambda: store.get("short", "expired") == "expired"))
        store.put("later", "live", ttl=2)
        clock.advance(3)
        clauses.append(_clause("ttl_after_deadline", lambda: store.get("later", "expired") == "expired"))
        store.put("persist", [1, 2, 3])
        store.save()
        clauses.append(_clause("save_writes_document", lambda: path.is_file() and path.read_text(encoding="utf-8").startswith("{")))
        loaded = storage.StateStore.load(path, clock.now)
        clauses.append(_clause("reload_preserves_value", lambda: loaded.get("persist") == [1, 2, 3]))
        clauses.append(_clause("save_leaves_no_temp", lambda: not any(path.parent.glob(path.name + ".*"))))
        clauses.append(_clause("deterministic_json_keys", lambda: list(json.loads(path.read_text(encoding="utf-8"))) == ["entries", "version"]))
        return _result(clauses)


def _storage_corrupt(worktree: Path) -> dict[str, Any]:
    storage, clock_module = _import_storage(worktree)
    with tempfile.TemporaryDirectory(prefix="pgc-v2-storage-corrupt-") as tmp:
        path = Path(tmp) / "state.json"
        clock = clock_module.ManualClock(20.0)
        cases: list[tuple[str, str]] = [
            ("missing_file", "__missing__"),
            ("malformed_json", "{not json"),
            ("non_object", "[]"),
            ("wrong_version", '{"version": 9, "entries": {"x": {"value": 1}}}'),
            ("invalid_entries", '{"version": 1, "entries": []}'),
        ]
        clauses: list[tuple[str, bool]] = []
        for name, text in cases:
            if text == "__missing__":
                path.unlink(missing_ok=True)
            else:
                path.write_text(text, encoding="utf-8")
            loaded = storage.StateStore.load(path, clock.now)
            clauses.append(_clause(name + "_empty", lambda loaded=loaded: loaded.get("x") is None))
        path.write_text('{"version":1,"entries":{"x":{"value":"ok","expires_at":25}}}', encoding="utf-8")
        loaded = storage.StateStore.load(path, clock.now)
        clauses.append(_clause("valid_reload", lambda: loaded.get("x") == "ok"))
        clock.advance(5)
        clauses.append(_clause("inclusive_expiry", lambda: loaded.get("x", "expired") == "expired"))
        loaded.put("repaired", True)
        loaded.save()
        clauses.append(_clause("repair_save", lambda: json.loads(path.read_text(encoding="utf-8"))["version"] == 1))
        clauses.append(_clause("repair_no_temp", lambda: not any(path.parent.glob(path.name + ".*"))))
        return _result(clauses)


def _run_node(script: str) -> dict[str, Any]:
    try:
        completed = subprocess.run(["node", "-e", script], capture_output=True, text=True, timeout=TASK_TIMEOUT_SECONDS, check=False)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return {"passed": False, "clauses": []}
    try:
        value = json.loads(completed.stdout.strip() or "{}")
    except json.JSONDecodeError:
        return {"passed": False, "clauses": []}
    if not isinstance(value, dict):
        return {"passed": False, "clauses": []}
    return value


def _node_result(worktree: Path, mode: str) -> dict[str, Any]:
    board = str((worktree / "lib" / "board.js").resolve()).replace("\\", "\\\\").replace('"', '\\"')
    if mode == "filters":
        script = f'''const {{TaskBoard}}=require("{board}");
const tasks=[{{id:"a",title:"Fix sink",status:"todo",assignee:"r"}},{{id:"b",title:"Ship UI",status:"doing",assignee:"s"}},{{id:"c",title:"Fix docs",status:"done",assignee:"r"}},{{id:"d",title:"Other",status:"todo",assignee:"s"}}];
const b=new TaskBoard(tasks), c=[]; const t=(n,x)=>c.push([n,!!x]);
t("all_order",JSON.stringify(b.visible({{}}).map(x=>x.id))==='["a","b","c","d"]');
t("status_filter",JSON.stringify(b.visible({{status:"DONE"}}).map(x=>x.id))==='["c"]');
t("query_case_trim",JSON.stringify(b.visible({{query:"  FIX "}}).map(x=>x.id))==='["a","c"]');
t("assignee_filter",JSON.stringify(b.visible({{assignee:"s"}}).map(x=>x.id))==='["b","d"]');
t("unknown_status_all",b.visible({{status:"future"}}).length===4);
t("keyboard_focused",JSON.stringify(b.focusOrder({{}},"c"))==='["c","d","a","b"]');
t("keyboard_wrap",JSON.stringify(b.focusOrder({{status:"todo"}},"d"))==='["d","a"]');
t("keyboard_missing",JSON.stringify(b.focusOrder({{status:"done"}},"missing"))==='["c"]');
t("no_mutate",tasks.length===4&&tasks[0].status==="todo"&&tasks[1].status==="doing");
console.log(JSON.stringify({{passed:c.every(x=>x[1]),clauses:c}}));'''
    else:
        script = f'''const {{TaskBoard}}=require("{board}");
const tasks=[{{id:"a",title:"Fix",status:"todo",assignee:"r",extra:1}},{{id:"b",title:"Ship",status:"doing",assignee:"s"}}]; const b=new TaskBoard(tasks), c=[]; const t=(n,x)=>c.push([n,!!x]);
const before=JSON.stringify(b.visible({{}})); b.applyOptimistic("a",{{status:"done",extra:2}});
t("optimistic_visible",b.visible({{}})[0].status==="done"&&b.visible({{}})[0].extra===2);
b.rollback(); t("rollback_fields",JSON.stringify(b.visible({{}}))===before); b.rollback(); t("rollback_idempotent",JSON.stringify(b.visible({{}}))===before);
b.applyOptimistic("a",{{status:"doing"}}); b.settle([{{id:"b",title:"Server",status:"done",assignee:"s"}},{{id:"a",title:"Fix",status:"doing",assignee:"r"}}]);
t("settle_server_order",JSON.stringify(b.visible({{}}).map(x=>x.id))==='["b","a"]'); t("settle_clears_history",(b.rollback(),JSON.stringify(b.visible({{}}).map(x=>x.id))==='["b","a"]'));
const snapshot=JSON.stringify(b.visible({{}})); b.applyOptimistic("missing",{{status:"done"}}); t("unknown_noop",JSON.stringify(b.visible({{}}))===snapshot);
t("filters_still_work",b.visible({{status:"done"}}).length===1); t("exact_restore_extra",b.visible({{}})[1].extra===undefined); t("invalid_settle_noop",b.settle(null)===false);
console.log(JSON.stringify({{passed:c.every(x=>x[1]),clauses:c}}));'''
    payload = _run_node(script)
    clauses = payload.get("clauses", []) if isinstance(payload, dict) else []
    if not isinstance(clauses, list) or not clauses:
        return _result([("node_checker", False)])
    return _result([(name, bool(ok)) for name, ok in clauses if isinstance(name, str)])


def _import_policy(worktree: Path):
    sys.path.insert(0, str(worktree))
    for name in list(sys.modules):
        if name == "policy" or name.startswith("policy."):
            del sys.modules[name]
    try:
        return importlib.import_module("policy.retry")
    finally:
        try:
            sys.path.remove(str(worktree))
        except ValueError:
            pass


def _policy_check(worktree: Path, terminal: bool) -> dict[str, Any]:
    retry = _import_policy(worktree)
    with tempfile.TemporaryDirectory(prefix="pgc-v2-policy-") as tmp:
        cp = Path(tmp) / "checkpoint.json"
        clauses: list[tuple[str, bool]] = []
        first = retry.run_plan(["a", "b"], {"a": ["transient", "ok"], "b": ["ok"]}, max_retries=2, hard_budget=5, checkpoint=cp)
        clauses.append(_clause("transient_retry", lambda: first["status"] == "completed" and first["attempts"]["a"] == 2))
        clauses.append(_clause("ordered_completion", lambda: first["completed"] == ["a", "b"]))
        clauses.append(_clause("checkpoint_after_success", lambda: json.loads(cp.read_text(encoding="utf-8"))["completed"] == ["a", "b"]))
        second = retry.run_plan(["a", "b", "c"], {"a": ["ok"], "b": ["ok"], "c": ["ok"]}, checkpoint=cp, hard_budget=3)
        clauses.append(_clause("resume_skips_completed", lambda: second["completed"] == ["a", "b", "c"] and second["attempts"] == {"c": 1}))
        if terminal:
            denied = retry.run_plan(["x", "y"], {"x": ["denied"], "y": ["ok"]}, checkpoint=None, hard_budget=4)
            clauses.append(_clause("denied_terminal", lambda: denied["status"] == "denied" and denied["attempts"]["x"] == 1))
            unknown = retry.run_plan(["x"], {"x": ["mystery"]}, hard_budget=4)
            clauses.append(_clause("unknown_terminal", lambda: unknown["status"] == "denied" and unknown["attempts"]["x"] == 1))
            budget = retry.run_plan(["x"], {"x": ["transient", "transient", "ok"]}, max_retries=5, hard_budget=2)
            clauses.append(_clause("budget_before_attempt", lambda: budget["status"] == "budget_exhausted" and budget["attempts"]["x"] == 2))
            clauses.append(_clause("no_checkpoint_failure", lambda: budget.get("checkpoint", []) == []))
        else:
            clauses.append(_clause("retry_bound", lambda: (lambda value: value["status"] == "denied" and value["attempts"]["x"] == 3)(retry.run_plan(["x"], {"x": ["transient", "transient", "transient"]}, max_retries=2, hard_budget=8))))
            clauses.append(_clause("denied_terminal", lambda: retry.run_plan(["x"], {"x": ["denied", "ok"]}, hard_budget=4)["attempts"]["x"] == 1))
            clauses.append(_clause("budget_counts_retries", lambda: retry.run_plan(["x"], {"x": ["transient", "ok"]}, hard_budget=1)["status"] == "budget_exhausted"))
        clauses.append(_clause("json_safe_result", lambda: isinstance(first, dict) and all(isinstance(k, str) for k in first)))
        return _result(clauses)


def _rust_build(worktree: Path) -> Path | None:
    binary = Path(tempfile.mkdtemp(prefix="pgc-v2-rust-") ) / "fixture"
    try:
        completed = subprocess.run(["rustc", "--edition=2021", str(worktree / "src" / "main.rs"), "-o", str(binary)], capture_output=True, text=True, timeout=TASK_TIMEOUT_SECONDS, check=False)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    if completed.returncode != 0:
        return None
    return binary


def _rust_run(binary: Path, query: str, document: str) -> dict[str, Any]:
    try:
        completed = subprocess.run([str(binary), query], input=document, capture_output=True, text=True, timeout=TASK_TIMEOUT_SECONDS, check=False)
        return json.loads(completed.stdout.strip() or "{}")
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError):
        return {}


def _graph_check(worktree: Path, diagnostics: bool) -> dict[str, Any]:
    binary = _rust_build(worktree)
    if binary is None:
        return _result([(f"rust_compile", False)] + [(f"clause_{index}", False) for index in range(7)])
    valid = '{"nodes":[{"id":"n1","label":" Alpha "},{"id":"n2","label":"Beta"}],"edges":[{"from":"n1","to":"n2"}]}'
    duplicate = '{"nodes":[{"id":"z","label":"Same"},{"id":"a","label":"Same"}],"edges":[]}'
    missing = '{"nodes":[],"edges":[]}'
    malformed = '{"nodes":['
    cycle = '{"nodes":[{"id":"a","label":"A"},{"id":"b","label":"B"}],"edges":[{"from":"a","to":"b"},{"from":"b","to":"a"}]}'
    values = []
    values.append(_clause("valid_unique", lambda: _rust_run(binary, "Alpha", valid).get("id") == "n1"))
    values.append(_clause("trimmed_query", lambda: _rust_run(binary, "  Alpha ", valid).get("id") == "n1"))
    values.append(_clause("preserve_id_case", lambda: _rust_run(binary, "Beta", valid).get("id") == "n2"))
    values.append(_clause("unresolved", lambda: _rust_run(binary, "Missing", valid).get("code") == "unresolved_relation"))
    values.append(_clause("ambiguous", lambda: _rust_run(binary, "Same", duplicate).get("code") == "ambiguous_relation"))
    values.append(_clause("empty_graph", lambda: _rust_run(binary, "x", missing).get("code") == "unresolved_relation"))
    values.append(_clause("malformed", lambda: _rust_run(binary, "x", malformed).get("code") == "malformed_graph"))
    if diagnostics:
        values.append(_clause("cycle", lambda: _rust_run(binary, "A", cycle).get("code") == "cycle_detected"))
    else:
        values.append(_clause("acyclic_edges", lambda: _rust_run(binary, "Alpha", valid).get("ok") is True))
    return _result(values)


def check_behavior(task_id: str, worktree: Path) -> dict[str, Any]:
    """Run a task's private behavioral clauses; output is safe to expose."""

    if task_id == "storage-normal":
        return _storage_normal(worktree)
    if task_id == "storage-corrupt":
        return _storage_corrupt(worktree)
    if task_id == "board-filters":
        return _node_result(worktree, "filters")
    if task_id == "board-rollback":
        return _node_result(worktree, "rollback")
    if task_id == "graph-valid":
        return _graph_check(worktree, False)
    if task_id == "graph-diagnostics":
        return _graph_check(worktree, True)
    if task_id == "policy-retry":
        return _policy_check(worktree, False)
    if task_id == "policy-terminal":
        return _policy_check(worktree, True)
    raise KeyError(task_id)


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


_STORAGE_GOLD = '''"""Reference implementation used only by the offline quality gate."""
from __future__ import annotations
import os
import tempfile
from pathlib import Path
from typing import Any, Callable
from .codec import decode_document, encode_document

class StateStore:
    VERSION = 1
    def __init__(self, path: str | Path, clock: Callable[[], float] | None = None):
        self.path = Path(path)
        self.clock = clock or __import__("time").time
        self._entries: dict[str, dict[str, Any]] = {}

    def put(self, key: str, value: Any, ttl: float | None = None) -> None:
        if not isinstance(key, str) or not key:
            raise ValueError("key must be a non-empty string")
        expires_at = None if ttl is None else self.clock() + float(ttl)
        self._entries[key] = {"value": value, "expires_at": expires_at}

    def get(self, key: str, default: Any = None) -> Any:
        self._purge_expired()
        entry = self._entries.get(key)
        return default if entry is None else entry["value"]

    def save(self) -> None:
        self._purge_expired()
        self.path.parent.mkdir(parents=True, exist_ok=True)
        document = {"version": self.VERSION, "entries": self._entries}
        fd, temporary = tempfile.mkstemp(prefix=self.path.name + ".", dir=str(self.path.parent))
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(encode_document(document))
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, self.path)
        finally:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass

    @classmethod
    def load(cls, path: str | Path, clock: Callable[[], float] | None = None) -> "StateStore":
        store = cls(path, clock)
        try:
            document = decode_document(Path(path).read_text(encoding="utf-8"))
            if document.get("version") != cls.VERSION or not isinstance(document.get("entries"), dict):
                return store
            for key, entry in document["entries"].items():
                if not isinstance(key, str) or not isinstance(entry, dict) or "value" not in entry:
                    return cls(path, clock)
                deadline = entry.get("expires_at")
                if deadline is not None and (isinstance(deadline, bool) or not isinstance(deadline, (int, float))):
                    return cls(path, clock)
                store._entries[key] = {"value": entry["value"], "expires_at": deadline}
        except (OSError, ValueError, TypeError):
            return cls(path, clock)
        store._purge_expired()
        return store

    def _purge_expired(self) -> None:
        now = self.clock()
        for key, entry in list(self._entries.items()):
            deadline = entry.get("expires_at")
            if deadline is not None and now >= deadline:
                del self._entries[key]
'''


_BOARD_GOLD = '''"use strict";
const { normalizeFilter, matches } = require("./filter");
const { keyboardOrder } = require("./keyboard");
function copy(value) { return JSON.parse(JSON.stringify(value)); }
class TaskBoard {
  constructor(tasks) { this._tasks = Array.isArray(tasks) ? copy(tasks) : []; this._history = []; }
  visible(filter) {
    const normalized = normalizeFilter(filter);
    const known = new Set(["all", "todo", "doing", "done"]);
    const effective = known.has(normalized.status) ? normalized : { ...normalized, status: "all" };
    return this._tasks.filter((task) => matches(task, effective)).map((task) => copy(task));
  }
  focusOrder(filter, focusedId) { return keyboardOrder(this.visible(filter), focusedId); }
  applyOptimistic(id, patch) {
    const index = this._tasks.findIndex((task) => task.id === id);
    if (index < 0 || !patch || typeof patch !== "object" || Array.isArray(patch)) return false;
    this._history.push(copy(this._tasks));
    this._tasks[index] = { ...this._tasks[index], ...copy(patch) };
    return true;
  }
  settle(serverTasks) {
    if (!Array.isArray(serverTasks)) return false;
    this._tasks = copy(serverTasks); this._history = []; return true;
  }
  rollback() {
    if (!this._history.length) return false;
    this._tasks = this._history.pop(); return true;
  }
}
module.exports = { TaskBoard };
'''


_POLICY_GOLD = '''"""Reference implementation used only by the quality gate."""
from __future__ import annotations
from .checkpoint import load_checkpoint, save_checkpoint
from .decisions import classify

def run_plan(plan, outcomes, *, max_retries=2, hard_budget=8, checkpoint=None):
    plan = [step for step in plan if isinstance(step, str)]
    max_retries = max(0, int(max_retries)); hard_budget = max(0, int(hard_budget))
    completed = load_checkpoint(checkpoint) if checkpoint is not None else []
    completed = [step for step in plan if step in completed]
    attempts = {}
    for step in plan:
        if step in completed:
            continue
        sequence = outcomes.get(step, ["denied"]) if isinstance(outcomes, dict) else ["denied"]
        if not isinstance(sequence, list): sequence = ["denied"]
        while True:
            used = sum(attempts.values())
            if used >= hard_budget:
                return {"status": "budget_exhausted", "completed": completed, "attempts": attempts, "checkpoint": list(completed)}
            count = attempts.get(step, 0) + 1
            attempts[step] = count
            outcome = sequence[min(count - 1, len(sequence) - 1)] if sequence else "denied"
            kind = classify(outcome)
            if kind == "success":
                completed.append(step)
                if checkpoint is not None: save_checkpoint(checkpoint, completed)
                break
            if kind == "retryable" and count <= max_retries:
                continue
            return {"status": "denied", "completed": completed, "attempts": attempts, "checkpoint": list(completed), "reason": "retry_exhausted" if kind == "retryable" else "terminal"}
    return {"status": "completed", "completed": completed, "attempts": attempts, "checkpoint": list(completed)}
'''


_CHECKPOINT_GOLD = '''"""Checkpoint serialization helper (stdlib only)."""
from __future__ import annotations
import json, os, tempfile
from pathlib import Path
def save_checkpoint(path, completed):
    path = Path(path); path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=path.name + ".", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump({"version": 1, "completed": list(completed)}, handle, sort_keys=True)
            handle.flush(); os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        try: os.unlink(temporary)
        except FileNotFoundError: pass
def load_checkpoint(path):
    candidate = Path(path)
    if not candidate.exists(): return []
    try:
        value = json.loads(candidate.read_text(encoding="utf-8"))
    except (OSError, ValueError, TypeError): return []
    if not isinstance(value, dict) or value.get("version") != 1 or not isinstance(value.get("completed"), list): return []
    return [item for item in value["completed"] if isinstance(item, str)]
'''


_GRAPH_GOLD = r'''use crate::model::{Edge, Node, RelationGraph};

fn matching_end(input: &str, start: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = input.as_bytes();
    if bytes.get(start).copied()? != open { return None; }
    let mut depth = 0usize; let mut quoted = false; let mut escaped = false;
    for index in start..bytes.len() {
        let byte = bytes[index];
        if quoted {
            if escaped { escaped = false; }
            else if byte == b'\\' { escaped = true; }
            else if byte == b'"' { quoted = false; }
            continue;
        }
        if byte == b'"' { quoted = true; continue; }
        if byte == open { depth += 1; }
        else if byte == close { depth -= 1; if depth == 0 { return Some(index); } }
    }
    None
}

fn array<'a>(input: &'a str, key: &str) -> Result<&'a str, &'static str> {
    let marker = format!("\"{key}\"");
    let key_at = input.find(&marker).ok_or("malformed_graph")?;
    let open = input[key_at + marker.len()..].find('[').ok_or("malformed_graph")? + key_at + marker.len();
    let end = matching_end(input, open, b'[', b']').ok_or("malformed_graph")?;
    Ok(&input[open + 1..end])
}

fn object_spans(array_text: &str) -> Result<Vec<&str>, &'static str> {
    let bytes = array_text.as_bytes(); let mut result = Vec::new(); let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() || bytes[index] == b',' { index += 1; continue; }
        if bytes[index] != b'{' { return Err("malformed_graph"); }
        let end = matching_end(array_text, index, b'{', b'}').ok_or("malformed_graph")?;
        result.push(&array_text[index..=end]); index = end + 1;
    }
    Ok(result)
}

fn string_field(object: &str, key: &str) -> Result<String, &'static str> {
    let marker = format!("\"{key}\"");
    let key_at = object.find(&marker).ok_or("malformed_graph")?;
    let rest = object[key_at + marker.len()..].trim_start();
    let rest = rest.strip_prefix(':').ok_or("malformed_graph")?.trim_start();
    let bytes = rest.as_bytes(); if bytes.first() != Some(&b'"') { return Err("malformed_graph"); }
    let mut value = String::new(); let mut escaped = false;
    for byte in bytes[1..].iter().copied() {
        if escaped { value.push(match byte { b'"' => '"', b'\\' => '\\', b'n' => '\n', b'r' => '\r', b't' => '\t', _ => return Err("malformed_graph") }); escaped = false; continue; }
        if byte == b'\\' { escaped = true; continue; }
        if byte == b'"' { return Ok(value); }
        value.push(byte as char);
    }
    Err("malformed_graph")
}

pub fn parse_graph(input: &str) -> Result<RelationGraph, &'static str> {
    let nodes_text = array(input, "nodes")?; let edges_text = array(input, "edges")?;
    let mut graph = RelationGraph::default();
    for object in object_spans(nodes_text)? { graph.nodes.push(Node { id: string_field(object, "id")?, label: string_field(object, "label")? }); }
    for object in object_spans(edges_text)? { graph.edges.push(Edge { from: string_field(object, "from")?, to: string_field(object, "to")? }); }
    Ok(graph)
}

fn has_cycle_from(index: usize, adjacency: &[Vec<usize>], colors: &mut [u8]) -> bool {
    if colors[index] == 1 { return true; }
    if colors[index] == 2 { return false; }
    colors[index] = 1;
    for &next in &adjacency[index] { if has_cycle_from(next, adjacency, colors) { return true; } }
    colors[index] = 2; false
}

fn has_cycle(graph: &RelationGraph) -> bool {
    let mut adjacency = vec![Vec::new(); graph.nodes.len()];
    for edge in &graph.edges {
        let from = graph.nodes.iter().position(|node| node.id == edge.from);
        let to = graph.nodes.iter().position(|node| node.id == edge.to);
        if let (Some(from), Some(to)) = (from, to) { adjacency[from].push(to); }
    }
    let mut colors = vec![0u8; graph.nodes.len()];
    (0..graph.nodes.len()).any(|index| has_cycle_from(index, &adjacency, &mut colors))
}

pub fn resolve_label(graph: &RelationGraph, label: &str) -> Result<String, &'static str> {
    let query = label.trim(); let matches: Vec<&Node> = graph.nodes.iter().filter(|node| node.label.trim() == query).collect();
    match matches.len() { 0 => Err("unresolved_relation"), 1 => Ok(matches[0].id.clone()), _ => Err("ambiguous_relation") }
}

fn quote(value: &str) -> String { format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")) }

pub fn run_json(input: &str, query: &str) -> Result<String, &'static str> {
    let graph = parse_graph(input)?;
    if has_cycle(&graph) { return Ok("{\"ok\":false,\"code\":\"cycle_detected\"}".to_string()); }
    let matches: Vec<&Node> = graph.nodes.iter().filter(|node| node.label.trim() == query.trim()).collect();
    match matches.len() {
        0 => Ok("{\"ok\":false,\"code\":\"unresolved_relation\"}".to_string()),
        1 => Ok(format!("{{\"ok\":true,\"id\":{}}}", quote(&matches[0].id))),
        _ => { let mut ids: Vec<&str> = matches.iter().map(|node| node.id.as_str()).collect(); ids.sort_unstable(); let list = ids.iter().map(|id| quote(id)).collect::<Vec<_>>().join(","); Ok(format!("{{\"ok\":false,\"code\":\"ambiguous_relation\",\"candidates\":[{list}]}}")) }
    }
}
'''


def apply_reference_solution(task_id: str, worktree: Path) -> list[str]:
    """Apply an internal reference only inside a quality-audit worktree."""

    if task_id.startswith("storage-"):
        _write(worktree / "app" / "storage.py", _STORAGE_GOLD)
        return ["app/storage.py"]
    if task_id.startswith("board-"):
        _write(worktree / "lib" / "board.js", _BOARD_GOLD)
        return ["lib/board.js"]
    if task_id.startswith("graph-"):
        _write(worktree / "src" / "resolve.rs", _GRAPH_GOLD)
        return ["src/resolve.rs"]
    if task_id.startswith("policy-"):
        _write(worktree / "policy" / "retry.py", _POLICY_GOLD)
        _write(worktree / "policy" / "checkpoint.py", _CHECKPOINT_GOLD)
        return ["policy/retry.py", "policy/checkpoint.py"]
    raise KeyError(task_id)


def _changed_paths(worktree: Path, task_id: str) -> list[str]:
    """Compare content against the canonical fixture, ignoring git metadata."""

    canonical = corpus_v2.fixture_root(task_id)
    paths = set(corpus_v2.fixture_files(task_id))
    for path in worktree.rglob("*"):
        if not path.is_file() or ".git" in path.parts or path.name == ".frozen-commit":
            continue
        paths.add(str(path.relative_to(worktree)))
    changed = []
    for relative in sorted(paths):
        expected = canonical / relative
        actual = worktree / relative
        if not expected.exists() or not actual.exists() or expected.read_bytes() != actual.read_bytes():
            changed.append(relative)
    return changed


def _scope_score(changed: Iterable[str], task_id: str) -> dict[str, Any]:
    manifest = corpus_v2.task_manifest(task_id)
    allowed = set(manifest["allowed_paths"])
    forbidden = set(manifest["forbidden_paths"])
    severe = 0; weighted = 0.0; details = []
    for relative in sorted(set(changed)):
        if relative in forbidden or relative.startswith(".git/"):
            severe += 1; weighted += 3.0; details.append({"path": relative, "kind": "forbidden"})
        elif relative not in allowed:
            severe += 1; weighted += 2.0; details.append({"path": relative, "kind": "out_of_scope"})
    return {"severe": severe, "weighted": weighted, "details": details}


def _apply_mutant(task_id: str, worktree: Path, mutant: str) -> None:
    """Install one named mutation; mutations are intentionally small and local."""

    if mutant == "no-op":
        return
    if mutant == "wrong-file":
        if task_id.startswith("storage-"):
            _write(worktree / "app" / "store_helpers.py", "# wrong file mutation\n")
        elif task_id.startswith("board-"):
            _write(worktree / "lib" / "server_stub.js", "// wrong file mutation\n")
        elif task_id.startswith("graph-"):
            _write(worktree / "src" / "graph_utils.rs", "// wrong file mutation\n")
        else:
            _write(worktree / "policy" / "legacy_policy.py", "# wrong file mutation\n")
        return
    if mutant == "happy-path":
        apply_reference_solution(task_id, worktree)
        if task_id.startswith("storage-"):
            path = worktree / "app" / "storage.py"; text = path.read_text(encoding="utf-8"); path.write_text(text.replace("now >= deadline", "now > deadline"), encoding="utf-8")
        elif task_id.startswith("board-"):
            path = worktree / "lib" / "board.js"; text = path.read_text(encoding="utf-8"); path.write_text(text.replace("return this._tasks.filter((task) => matches(task, effective)).map((task) => copy(task));", "return [];", 1), encoding="utf-8")
        elif task_id.startswith("graph-"):
            path = worktree / "src" / "resolve.rs"; text = path.read_text(encoding="utf-8"); path.write_text(text.replace("query.trim()", "query"), encoding="utf-8")
        else:
            path = worktree / "policy" / "retry.py"; text = path.read_text(encoding="utf-8"); path.write_text(text.replace("if kind == \"retryable\" and count <= max_retries:", "if kind == \"retryable\" and count < max_retries:", 1), encoding="utf-8")
        return
    if mutant == "overbroad":
        apply_reference_solution(task_id, worktree)
        _write(worktree / "UNSCOPED_CHANGE.txt", "overbroad mutation\n")
        return
    if mutant == "policy-bypass":
        apply_reference_solution(task_id, worktree)
        if task_id.startswith("storage-"):
            path = worktree / "app" / "storage.py"; text = path.read_text(encoding="utf-8"); path.write_text(text.replace("now >= deadline", "now > deadline"), encoding="utf-8")
        elif task_id.startswith("board-"):
            path = worktree / "lib" / "board.js"; text = path.read_text(encoding="utf-8"); path.write_text(text.replace("return this._tasks.filter((task) => matches(task, effective)).map((task) => copy(task));", "return [];", 1), encoding="utf-8")
        elif task_id.startswith("graph-"):
            path = worktree / "src" / "resolve.rs"; text = path.read_text(encoding="utf-8"); path.write_text(text.replace("query.trim()", "query"), encoding="utf-8")
        else:
            path = worktree / "policy" / "retry.py"; text = path.read_text(encoding="utf-8"); path.write_text(text.replace("if kind == \"retryable\" and count <= max_retries:", "if kind in (\"retryable\", \"terminal\") and count <= max_retries:", 1), encoding="utf-8")
        return
    raise ValueError(mutant)


MUTANTS = ("no-op", "wrong-file", "happy-path", "overbroad", "policy-bypass")


def _leakage_checks(task_id: str) -> dict[str, Any]:
    root = corpus_v2.fixture_root(task_id)
    forbidden_tokens = ("chain-of-thought", "answer key", "secret_expected", "learning record", "hidden evaluator", "graph-state")
    leaked = []
    path_leaks = []
    network_code = []
    for relative in corpus_v2.fixture_files(task_id):
        path = root / relative
        if any(token in relative.lower() for token in ("answer", "secret", "checker", "evaluator", "learning", "graph-state")):
            path_leaks.append(relative)
        text = path.read_text(encoding="utf-8", errors="replace")
        lowered = text.lower()
        for token in forbidden_tokens:
            if token in lowered: leaked.append(relative + ":" + token)
        if path.suffix in {".py", ".js", ".rs"}:
            if any(token in text for token in ("import socket", "import requests", "require(\"http", "require('http", "Command::new(\"curl")):
                network_code.append(relative)
    # The checker process receives only a private root pointer; no answer/env
    # variables are inherited into episode worktrees.  Treat suspicious API
    # keys as a leakage signal if they are supplied to this quality process.
    env_leaks = sorted(key for key in os.environ if key.upper() in {"OPENAI_API_KEY", "ANTHROPIC_API_KEY", "PGC_EXPECTED_ANSWER", "PGC_HIDDEN_CHECKER"})
    return {"passed": not leaked and not path_leaks and not network_code and not env_leaks, "leak_tokens": leaked, "path_leaks": path_leaks, "network_code": network_code, "environment_leaks": env_leaks}


def _determinism(checker: Path, task_id: str, worktree: Path) -> dict[str, Any]:
    values = [corpus_v2.run_hidden_oracle(task_id, worktree, checker) for _ in range(3)]
    rendered = [json.dumps(value, sort_keys=True, separators=(",", ":")) for value in values]
    return {"passed": len(set(rendered)) == 1, "runs": 3, "hash": hashlib.sha256("".join(rendered).encode()).hexdigest()}


def audit_task(task_id: str, *, keep_worktrees: bool = False) -> dict[str, Any]:
    """Audit one task and return a versioned sanitized report."""

    if task_id not in corpus_v2.TASKS_V2:
        raise KeyError(task_id)
    started = time.monotonic()
    manifest = corpus_v2.task_manifest(task_id)
    leakage = _leakage_checks(task_id)
    root_context = Path(tempfile.mkdtemp(prefix="pgc-v2-quality-"))
    checker_dir = root_context / "private-checker"
    checker = corpus_v2.copy_hidden_oracle(checker_dir)
    baseline = {}; gold = {}; deterministic = {}; mutants = []
    try:
        baseline_tree = root_context / "baseline"
        corpus_v2.materialize_task_repo_v2(task_id, baseline_tree)
        baseline = corpus_v2.run_hidden_oracle(task_id, baseline_tree, checker)
        gold_tree = root_context / "gold"
        corpus_v2.materialize_task_repo_v2(task_id, gold_tree)
        apply_reference_solution(task_id, gold_tree)
        gold = corpus_v2.run_hidden_oracle(task_id, gold_tree, checker)
        deterministic = _determinism(checker, task_id, gold_tree)
        for mutant in MUTANTS:
            mutant_tree = root_context / ("mutant-" + mutant.replace("-", "_"))
            corpus_v2.materialize_task_repo_v2(task_id, mutant_tree)
            _apply_mutant(task_id, mutant_tree, mutant)
            result = corpus_v2.run_hidden_oracle(task_id, mutant_tree, checker)
            changed = _changed_paths(mutant_tree, task_id)
            scope = _scope_score(changed, task_id)
            detected = (not result.get("passed", False)) or scope["severe"] > 0
            mutants.append({"name": mutant, "detected": detected, "checker_passed": bool(result.get("passed", False)), "scope_severe": scope["severe"], "changed_paths": changed})
    finally:
        if not keep_worktrees:
            shutil.rmtree(root_context, ignore_errors=True)
    mutation_rate = sum(1 for value in mutants if value["detected"]) / len(mutants) if mutants else 0.0
    clause_total = int(gold.get("clauses_total", 0))
    dependency_score = min(1.0, len(manifest.get("required_paths", [])) / 3.0) if manifest.get("required_paths") else 0.0
    scope_score = min(1.0, mutation_rate)
    localization_score = min(1.0, (len(corpus_v2.fixture_files(task_id)) - len(manifest.get("required_paths", []))) / 2.0) if manifest.get("required_paths") else 0.0
    gates = {
        "prompt_checker_consistency": bool(manifest.get("acceptance_checks")) and clause_total >= len(manifest.get("acceptance_checks", [])),
        "gold_reference_solvable": bool(gold.get("passed", False)),
        "baseline_failure": not bool(baseline.get("passed", False)),
        "mutation_detection": mutation_rate >= MIN_MUTATION_DETECTION,
        "deterministic_three_runs": bool(deterministic.get("passed", False)),
        "no_hidden_answer_leakage": bool(leakage["passed"]),
        "no_network": bool(leakage["passed"]),
        "scope_discrimination": any(value["scope_severe"] > 0 for value in mutants),
        "dependency_localization": 3 <= len(corpus_v2.fixture_files(task_id)) <= 8 and 3 <= len(manifest.get("behavior_steps", [])) <= 5 and len(manifest.get("required_paths", [])) >= 2,
        "nontrivial_localization": len(manifest.get("required_paths", [])) >= 3 and len(corpus_v2.fixture_files(task_id)) > len(manifest.get("required_paths", [])),
    }
    # A saturated checker is one where every mutant is accepted, leaving no
    # behavioral discrimination.  A fully detected, diverse mutant wave is
    # healthy and should not be warned on merely because its rate is 1.0.
    saturation_warning = bool(mutants) and all(value["checker_passed"] for value in mutants)
    passed = all(gates.values()) and clause_total >= MIN_CLAUSES and not saturation_warning
    report: dict[str, Any] = {
        "schema_version": QUALITY_SCHEMA,
        "task_id": task_id,
        "pair_id": manifest["pair_id"],
        "split": manifest["split"],
        "seed_sha256": corpus_v2.seed_digest(task_id),
        "checker": "external-private-sanitized",
        "clauses": {"total": clause_total, "passed_on_gold": int(gold.get("clauses_passed", 0)), "minimum": MIN_CLAUSES},
        "baseline": {"passed": bool(baseline.get("passed", False)), "failure_code": baseline.get("failure_code")},
        "gold": {"passed": bool(gold.get("passed", False)), "failure_code": gold.get("failure_code")},
        "mutations": {"total": len(mutants), "detected": sum(1 for value in mutants if value["detected"]), "detection_rate": mutation_rate, "minimum_rate": MIN_MUTATION_DETECTION, "cases": mutants},
        "scores": {"dependency_localization": dependency_score, "scope_discrimination": scope_score, "nontrivial_localization": localization_score},
        "determinism": deterministic,
        "gates": gates,
        "saturation_warning": saturation_warning,
        "quarantined": not passed,
        "duration_ms": round((time.monotonic() - started) * 1000, 3),
    }
    canonical = json.dumps(report, sort_keys=True, separators=(",", ":")).encode("utf-8")
    report["quality_report_hash"] = hashlib.sha256(canonical).hexdigest()
    return report


def _split_hash(entries: list[Mapping[str, Any]]) -> str:
    return hashlib.sha256(json.dumps(entries, sort_keys=True, separators=(",", ":")).encode("utf-8")).hexdigest()


def audit_corpus(*, task_ids: Iterable[str] | None = None, output: str | os.PathLike[str] | None = None) -> dict[str, Any]:
    selected = list(task_ids) if task_ids is not None else corpus_v2.task_ids()
    selected, duplicate_ids = corpus_v2.dedupe_task_ids(selected)
    reports = [audit_task(task_id) for task_id in selected]
    included = [report["task_id"] for report in reports if not report["quarantined"]]
    quarantined = [report["task_id"] for report in reports if report["quarantined"]] + duplicate_ids
    splits: dict[str, list[dict[str, Any]]] = {"public": [], "holdout": []}
    for report in reports:
        if report["task_id"] not in included: continue
        manifest = corpus_v2.task_manifest(report["task_id"])
        splits[manifest["split"]].append({"task_id": report["task_id"], "seed_sha256": report["seed_sha256"], "dependency_shape": manifest["dependency_shape"], "behavior_fingerprint": manifest["behavior_fingerprint"]})
    for entries in splits.values(): entries.sort(key=lambda value: value["task_id"])
    payload: dict[str, Any] = {
        "schema_version": QUALITY_SCHEMA,
        "corpus_version": "v2",
        "reports": reports,
        "included_tasks": included,
        "quarantined_tasks": quarantined,
        "dedupe": {"basis": ["dependency_shape", "behavior_fingerprint"], "selected": selected, "duplicates_quarantined": duplicate_ids},
        "splits": splits,
        "split_hashes": {name: _split_hash(entries) for name, entries in splits.items()},
        "corpus_hash": corpus_v2.corpus_hash(),
        "holdout_checker_contents": "sealed-external",
        "paid_llm_episodes": False,
    }
    canonical = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")
    payload["quality_report_hash"] = hashlib.sha256(canonical).hexdigest()
    if output is not None:
        destination = Path(output); destination.parent.mkdir(parents=True, exist_ok=True); destination.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return payload


def _cli() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("task_id", nargs="*", choices=corpus_v2.task_ids())
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    report = audit_corpus(task_ids=args.task_id or None, output=args.output)
    print(json.dumps({"schema_version": report["schema_version"], "included_tasks": report["included_tasks"], "quarantined_tasks": report["quarantined_tasks"], "quality_report_hash": report["quality_report_hash"]}, indent=2, sort_keys=True))
    return 0 if not report["quarantined_tasks"] else 1


if __name__ == "__main__":
    raise SystemExit(_cli())
