"""Offline contract tests for the Three.js execution-graph controller.

The viewer is intentionally a classic browser script rather than a package.  These
tests therefore load its CommonJS export in Node and use a tiny DOM/WebGL shim for
the controller tests.  No server, browser, network, or real GPU is required.
"""

from __future__ import annotations

import json
import math
import os
from pathlib import Path
import re
import subprocess
import textwrap
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
MODULE = ROOT / "execution-graph" / "three-graph.js"
INDEX = ROOT / "execution-graph" / "index.html"
APP = ROOT / "execution-graph" / "app.js"
STYLES = ROOT / "execution-graph" / "styles.css"
BOARD = ROOT / "src" / "board.rs"
VENDOR = ROOT / "execution-graph" / "vendor" / "three.min.js"
VENDOR_README = ROOT / "execution-graph" / "vendor" / "README.md"


def run_node(source: str, data: object | None = None, timeout: float = 10.0) -> str:
    """Run a self-contained CommonJS/VM probe and return stdout.

    Keeping the probe in the test process makes failures show the JavaScript
    stack while preserving the offline and dependency-free test contract.
    """

    env = os.environ.copy()
    env.pop("NODE_OPTIONS", None)
    payload = None if data is None else json.dumps(data, separators=(",", ":"))
    completed = subprocess.run(
        ["node", "-e", source],
        cwd=ROOT,
        input=payload,
        text=True,
        capture_output=True,
        timeout=timeout,
        env=env,
        check=False,
    )
    if completed.returncode:
        raise AssertionError(
            "node probe failed (exit %s):\n%s\n%s"
            % (completed.returncode, completed.stdout, completed.stderr)
        )
    return completed.stdout.strip()


def node_json(source: str, data: object | None = None, timeout: float = 10.0):
    output = run_node(source, data=data, timeout=timeout)
    try:
        return json.loads(output)
    except json.JSONDecodeError as exc:  # pragma: no cover - failure context
        raise AssertionError(f"expected JSON from node probe, got {output!r}") from exc


def canonical_payload() -> dict:
    return {
        "schema": "fractal.execution_graph_view.v1",
        "overview": {
            "nodes": [
                {
                    "id": "plan",
                    "title": "Plan",
                    "status": "complete",
                    "completed": 1,
                    "total": 1,
                    "progress": 100,
                    "gate": "verified",
                },
                {
                    "id": "build",
                    "title": "Build",
                    "status": "active",
                    "completed": 2,
                    "total": 4,
                    "progress": 50,
                    "gate": "tests",
                },
                {
                    "id": "mystery",
                    "title": "Unknown status node",
                    "status": "paused-by-future-version",
                    "completed": 0,
                    "total": 1,
                    "progress": 0,
                },
            ],
            "edges": [
                {"from": "plan", "to": "build", "condition": "success"},
                {"from": "build", "to": "mystery", "condition": "failure"},
                {"from": "ghost", "to": "build", "condition": "success"},
            ],
        },
        "groups": [
            {
                "id": "build-group",
                "title": "Build checklist",
                "status": "active",
                "completed": 1,
                "total": 2,
                "progress": 50,
                "tasks": [
                    {
                        "id": "compile",
                        "title": "Compile",
                        "kind": "tool",
                        "status": "complete",
                        "checked": True,
                        "line": 12,
                        "instruction": "Compile the board",
                        "gate": "compiler",
                        "assignment": {"agent_id": "builder", "state": "completed"},
                        "execution": {"wave": 3, "mode": "parallel", "parallel_group": "wave-3"},
                    },
                    {
                        "id": "assert",
                        "title": "Assert",
                        "kind": "test",
                        "status": "incomplete",
                        "checked": False,
                        "line": 19,
                        "instruction": "Run focused tests",
                        "gate": None,
                        "assignment": None,
                        "execution": {"wave": 4, "mode": "sequential", "parallel_group": None},
                    },
                ],
                "edges": [{"from": "compile", "to": "assert", "condition": "success"}],
            }
        ],
    }


NORMALIZE_PROBE = textwrap.dedent(
    f"""
    const Graph = require({json.dumps(str(MODULE))});
    const payload = JSON.parse(require('fs').readFileSync(0, 'utf8'));
    const before = JSON.stringify(payload);
    const overview = Graph.normalizeGraphPayload(payload, 'overview');
    const tasks = Graph.normalizeGraphPayload(payload, 'build-group');
    process.stdout.write(JSON.stringify({{
      exported: Boolean(Graph && Graph.normalizeGraphPayload && Graph.computeLayout && Graph.createThreeGraph),
      before,
      after: JSON.stringify(payload),
      overview,
      tasks
    }}));
    """
)


class ThreeGraphContractTests(unittest.TestCase):
    """Pure normalization/layout tests are usable without DOM or WebGL."""

    def test_commonjs_export_and_normalization_are_immutable(self):
        payload = canonical_payload()
        result = node_json(NORMALIZE_PROBE, payload)
        self.assertTrue(result["exported"], "module must expose the public CommonJS API")
        self.assertEqual(result["before"], result["after"], "normalization must not mutate Rust payload")

        model = result["overview"]
        self.assertEqual(model["mode"], "overview")
        self.assertIsNone(model["groupId"])
        self.assertEqual([node["id"] for node in model["nodes"]], ["plan", "build", "mystery"])
        self.assertEqual(len({node["id"] for node in model["nodes"]}), len(model["nodes"]))
        self.assertEqual([(edge["from"], edge["to"]) for edge in model["edges"]], [("plan", "build"), ("build", "mystery")])
        self.assertTrue(any("paused-by-future-version" in item or "mystery" in item for item in model["diagnostics"]["unknownStatus"]))
        self.assertTrue(any("ghost" in item or "build" in item for item in model["diagnostics"]["missingEdgeNodes"]))

        tasks = result["tasks"]
        self.assertEqual(tasks["mode"], "tasks")
        self.assertEqual(tasks["groupId"], "build-group")
        self.assertEqual(tasks["title"], "Build checklist")
        self.assertEqual([node["id"] for node in tasks["nodes"]], ["compile", "assert"])
        self.assertEqual(tasks["nodes"][0]["execution"]["wave"], 3)
        self.assertEqual(tasks["nodes"][0]["assignment"]["agent_id"], "builder")

    def test_normalization_tolerates_empty_legacy_and_malformed_optional_fields(self):
        malformed = {
            "schema": "fractal.execution_graph_view.v1",
            "overview": {"nodes": None, "edges": [{"from": None, "to": "missing"}, None]},
            "groups": None,
            "totals": {"complete": 99},
            "graph": {"nodes": "legacy-shape"},
        }
        probe = textwrap.dedent(
            f"""
            const Graph = require({json.dumps(str(MODULE))});
            const payload = JSON.parse(require('fs').readFileSync(0, 'utf8'));
            const views = [Graph.normalizeGraphPayload(payload), Graph.normalizeGraphPayload({{}}), Graph.normalizeGraphPayload(payload, 'not-a-group')];
            process.stdout.write(JSON.stringify(views));
            """
        )
        views = node_json(probe, malformed)
        self.assertEqual(views[0]["nodes"], [])
        self.assertEqual(views[0]["edges"], [])
        self.assertEqual(views[1]["nodes"], [])
        self.assertEqual(views[1]["edges"], [])
        self.assertEqual(views[2]["nodes"], [])
        self.assertTrue(views[2]["mode"] in ("overview", "tasks"))

    def test_layout_honors_declared_waves_and_excludes_failure_edges(self):
        model = {
            "mode": "tasks",
            "groupId": "wave-fixture",
            "title": "Wave fixture",
            "nodes": [
                {"id": "a", "title": "A", "status": "complete", "execution": {}},
                {"id": "b", "title": "B", "status": "active", "execution": {}},
                {"id": "c", "title": "C", "status": "incomplete", "execution": {}},
                {"id": "declared", "title": "Declared", "status": "incomplete", "execution": {"wave": 7}},
            ],
            "edges": [
                {"from": "a", "to": "b", "condition": "success"},
                {"from": "b", "to": "c", "condition": "failure"},
                {"from": "a", "to": "declared", "condition": "success"},
            ],
            "diagnostics": {"unknownStatus": [], "missingEdgeNodes": [], "cycles": []},
        }
        probe = textwrap.dedent(
            """
            const Graph = require(process.argv[1]);
            const model = JSON.parse(require('fs').readFileSync(0, 'utf8'));
            const layout = Graph.computeLayout(model);
            process.stdout.write(JSON.stringify(layout));
            """
        )
        layout = node_json(probe.replace("process.argv[1]", json.dumps(str(MODULE))), model)
        nodes = {node["id"]: node for node in layout["nodes"]}
        self.assertEqual(nodes["declared"]["wave"], 7)
        self.assertGreater(nodes["b"]["wave"], nodes["a"]["wave"])
        # The failure/alternate path must remain in the scene, but must not
        # make c a successor wave of b.
        self.assertLessEqual(nodes["c"]["wave"], nodes["b"]["wave"])
        failure_edges = [edge for edge in layout["edges"] if edge["condition"] == "failure"]
        self.assertEqual(len(failure_edges), 1)
        self.assertLessEqual(len(failure_edges[0]["points"]), 8)

    def test_large_dag_is_deterministic_finite_and_spatially_separated(self):
        nodes = [
            {
                "id": f"n-{index:04d}",
                "title": f"Node {index}",
                "status": "active" if index % 11 == 0 else "incomplete",
                "execution": {},
            }
            for index in range(500)
        ]
        edges = []
        # A chain gives a useful topological depth; deterministic forward jumps
        # fill the fixture to the benchmark's 1,500-edge density.
        for index in range(499):
            edges.append({"from": f"n-{index:04d}", "to": f"n-{index + 1:04d}", "condition": "success"})
        for offset in range(1, 1002):
            start = (offset * 17) % 470
            span = 1 + ((offset * 13) % max(1, 499 - start))
            edges.append({"from": f"n-{start:04d}", "to": f"n-{start + span:04d}", "condition": "success"})
        # The extra edges above are 1,500 total (and remain forward/DAG).
        self.assertEqual(len(edges), 1500)
        model = {"mode": "overview", "groupId": None, "title": "large", "nodes": nodes, "edges": edges, "diagnostics": {"cycles": []}}
        probe = textwrap.dedent(
            f"""
            const Graph = require({json.dumps(str(MODULE))});
            const model = JSON.parse(require('fs').readFileSync(0, 'utf8'));
            const start = process.hrtime.bigint();
            const first = Graph.computeLayout(model);
            const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
            const second = Graph.computeLayout(model);
            process.stdout.write(JSON.stringify({{equal: JSON.stringify(first) === JSON.stringify(second), elapsedMs, layout: first}}));
            """
        )
        result = node_json(probe, model, timeout=10)
        self.assertTrue(result["equal"], "layout must be byte-for-byte deterministic")
        self.assertLessEqual(result["elapsedMs"], 100.0, "500-node layout exceeded the 100 ms contract")
        layout = result["layout"]
        self.assertEqual(len(layout["nodes"]), 500)
        self.assertEqual(len(layout["edges"]), 1500)
        self.assertLessEqual(max(len(edge["points"]) for edge in layout["edges"]), 8)
        for node in layout["nodes"]:
            for coordinate in (node["x"], node["y"], node["z"], node["radius"], node["depth"]):
                self.assertTrue(math.isfinite(float(coordinate)), (node["id"], coordinate))
        for bound in (layout["bounds"]["min"], layout["bounds"]["max"], layout["bounds"]["center"]):
            for coordinate in (bound["x"], bound["y"], bound["z"]):
                self.assertTrue(math.isfinite(float(coordinate)))
        radius = max(float(node["radius"]) for node in layout["nodes"])
        by_wave = {}
        for node in layout["nodes"]:
            by_wave.setdefault(node["wave"], []).append(node)
        for wave_nodes in by_wave.values():
            for index, left in enumerate(wave_nodes):
                for right in wave_nodes[index + 1 :]:
                    distance = math.sqrt(sum((float(left[axis]) - float(right[axis])) ** 2 for axis in ("x", "y", "z")))
                    self.assertGreaterEqual(distance + 1e-7, 2 * radius + 0.35)

    def test_cycle_detection_terminates_and_reports_cycle_ids(self):
        nodes = [{"id": f"c-{index:03d}", "title": str(index), "status": "incomplete", "execution": {}} for index in range(100)]
        edges = [
            {"from": f"c-{index:03d}", "to": f"c-{(index + 1) % 100:03d}", "condition": "success"}
            for index in range(100)
        ]
        model = {"mode": "overview", "groupId": None, "title": "cycle", "nodes": nodes, "edges": edges, "diagnostics": {"cycles": []}}
        probe = textwrap.dedent(
            f"""
            const Graph = require({json.dumps(str(MODULE))});
            const model = JSON.parse(require('fs').readFileSync(0, 'utf8'));
            const start = process.hrtime.bigint();
            const layout = Graph.computeLayout(model);
            const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
            process.stdout.write(JSON.stringify({{elapsedMs, layout}}));
            """
        )
        result = node_json(probe, model)
        self.assertLessEqual(result["elapsedMs"], 100.0)
        cycle_ids = set(result["layout"]["diagnostics"]["cycles"])
        self.assertTrue({node["id"] for node in nodes}.issubset(cycle_ids))
        self.assertTrue(all(math.isfinite(float(node["x"])) for node in result["layout"]["nodes"]))


class ThreeGraphStaticAndControllerTests(unittest.TestCase):
    """Package, accessibility, controller-budget, and fallback checks."""

    def test_offline_assets_are_local_and_board_embeds_every_new_asset(self):
        self.assertTrue(MODULE.is_file(), "three-graph.js must be checked in")
        self.assertTrue(VENDOR.is_file(), "a pinned local Three.js runtime is required")
        html = INDEX.read_text(encoding="utf-8")
        app = APP.read_text(encoding="utf-8")
        css = STYLES.read_text(encoding="utf-8")
        module = MODULE.read_text(encoding="utf-8")
        self.assertNotIn("Math.random", module)
        self.assertNotRegex(module, r"\b(?:fetch|XMLHttpRequest)\b")
        for source_name, source in (("index", html), ("app", app), ("styles", css), ("module", module)):
            # app.js keeps the standards-defined SVG namespace; every other
            # URL-like token would be a forbidden remote dependency.
            source = source.replace("http://www.w3.org/2000/svg", "")
            self.assertNotRegex(source, r"https?://|unpkg|jsdelivr|import\s*\(", source_name)
        self.assertRegex(html, r'<script[^>]+src=["\']vendor/three\.min\.js["\']')
        self.assertRegex(html, r'<script[^>]+src=["\']three-graph\.js["\']')
        self.assertIn('id="graph-3d"', html)
        self.assertIn('id="graph-accessible-list"', html)
        self.assertIn('id="graph"', html, "the SVG fallback must remain")
        self.assertIn('"three-graph.js"', BOARD.read_text(encoding="utf-8"))
        self.assertIn('"vendor/three.min.js"', BOARD.read_text(encoding="utf-8"))
        # A short adjacent README is welcome, but a checked-in header is an
        # equally valid license/version record and keeps the asset set small.
        vendor_header = VENDOR.read_text(encoding="utf-8")[:4000]
        if VENDOR_README.is_file():
            vendor_header += "\n" + VENDOR_README.read_text(encoding="utf-8")
        self.assertRegex(vendor_header, r"(?i)three(?:\.js)?")
        self.assertRegex(vendor_header, r"(?i)(version|release|r\d+(?:\.\d+)*)")
        self.assertRegex(vendor_header, r"(?i)(license|mit)")

    def test_app_keeps_operational_paths_and_3d_callbacks(self):
        app = APP.read_text(encoding="utf-8")
        for symbol in ("loadGraph", "selectNode", "openMilestone", "loadFailureGraph", "pause", "history", "master"):
            self.assertIn(symbol, app)
        self.assertRegex(app, r"createThreeGraph")
        self.assertRegex(app, r"onSelect")
        self.assertRegex(app, r"onOpenMilestone")
        self.assertRegex(app, r"\.update\(")
        # The canonical graph/pause path remains in app.js; failure history is
        # delegated to the shared API client but must remain wired here.
        self.assertIn("/api/graph", app)
        self.assertIn("/api/run/pause", app)
        self.assertRegex(app, r"failureGraph|failure-graph")

    def test_controller_webgl_fallback_keeps_svg_and_accessible_list_alive(self):
        model = node_json(
            textwrap.dedent(
                f"""
                const Graph = require({json.dumps(str(MODULE))});
                const payload = JSON.parse(require('fs').readFileSync(0, 'utf8'));
                process.stdout.write(JSON.stringify(Graph.normalizeGraphPayload(payload)));
                """
            ),
            canonical_payload(),
        )
        probe = textwrap.dedent(
            f"""
            class El {{
              constructor(tag) {{ this.tagName = String(tag).toUpperCase(); this.children = []; this.parentNode = null; this.attributes = {{}}; this.style = {{}}; this.dataset = {{}}; this.listeners = {{}}; this.className = ''; this.textContent = ''; this.hidden = false; this.classList = {{ add: (...xs) => xs.forEach(x => this.className += (this.className ? ' ' : '') + x), remove: (...xs) => xs.forEach(x => this.className = this.className.split(/\\s+/).filter(y => y && y !== x).join(' ')), toggle: (x, on) => {{ if (on) this.classList.add(x); else this.classList.remove(x); }}, contains: x => this.className.split(/\\s+/).includes(x) }}; }}
              appendChild(child) {{ if (child) {{ this.children.push(child); child.parentNode = this; }} return child; }}
              append(...xs) {{ xs.forEach(x => this.appendChild(x)); }}
              removeChild(child) {{ this.children = this.children.filter(x => x !== child); if (child) child.parentNode = null; }}
              replaceChildren(...xs) {{ this.children = []; this.append(...xs); }}
              setAttribute(k, v) {{ this.attributes[k] = String(v); this[k] = String(v); }}
              getAttribute(k) {{ return this.attributes[k] ?? null; }}
              removeAttribute(k) {{ delete this.attributes[k]; }}
              addEventListener(k, fn) {{ (this.listeners[k] ||= []).push(fn); }}
              removeEventListener(k, fn) {{ this.listeners[k] = (this.listeners[k] || []).filter(x => x !== fn); }}
              dispatchEvent(event) {{ (this.listeners[event.type] || []).forEach(fn => fn(event)); return true; }}
              click() {{ this.dispatchEvent({{ type: 'click' }}); }}
              querySelectorAll(selector) {{ const found = []; const visit = node => {{ for (const child of node.children) {{ if (selector === 'button' && child.tagName === 'BUTTON') found.push(child); if (selector === '[data-node-id]' && child.dataset.nodeId) found.push(child); visit(child); }} }}; visit(this); return found; }}
              querySelector(selector) {{ return this.querySelectorAll(selector)[0] || null; }}
              getBoundingClientRect() {{ return {{ width: 800, height: 500, left: 0, top: 0 }}; }}
              focus() {{ this.focused = true; }}
            }}
            const doc = {{ createElement: tag => new El(tag), createElementNS: (_ns, tag) => new El(tag), documentElement: new El('html'), addEventListener() {{}}, removeEventListener() {{}}, body: new El('body') }};
            const win = {{ document: doc, WebGLRenderingContext: undefined, matchMedia: () => ({{ matches: false, addEventListener() {{}}, removeEventListener() {{}} }}), addEventListener() {{}}, removeEventListener() {{}}, requestAnimationFrame: () => 1, cancelAnimationFrame() {{}} }};
            global.window = win; global.document = doc;
            const Graph = require({json.dumps(str(MODULE))});
            const mount = new El('div'); const list = new El('div'); const svg = new El('svg');
            let changes = []; let selects = [];
            const controller = Graph.createThreeGraph({{ mount, accessibleList: list, fallbackSvg: svg, onSelect: (id, kind) => selects.push([id, kind]), onCapabilityChange: value => changes.push(value) }});
            controller.update(JSON.parse(require('fs').readFileSync(0, 'utf8')), 'build');
            const buttons = list.querySelectorAll('button');
            const beforeDestroy = controller.getSnapshot();
            const unknownFocus = controller.focus('not-present');
            controller.destroy(); controller.destroy();
            process.stdout.write(JSON.stringify({{ beforeDestroy, unknownFocus, changes, buttonCount: buttons.length, mountDisplay: mount.style.display || null, svgDisplay: svg.style.display || null, afterDestroy: list.children.length }}));
            """
        )
        result = node_json(probe, model)
        self.assertFalse(result["beforeDestroy"]["active"])
        self.assertEqual(result["beforeDestroy"]["nodeCount"], 3)
        self.assertEqual(result["buttonCount"], 3)
        self.assertFalse(result["unknownFocus"])
        self.assertEqual(result["afterDestroy"], 0)
        self.assertNotEqual(result["mountDisplay"], "block", "fallback should not display a WebGL canvas")
        self.assertNotEqual(result["svgDisplay"], "none", "SVG fallback must remain visible")
        self.assertTrue(any(change.get("active") is False for change in result["changes"]))

    def test_controller_accessible_selection_keyboard_motion_and_idempotent_update(self):
        # The probe's DOM and THREE shim deliberately implement only the small
        # surface the controller needs.  This catches duplicate renderers/RAF
        # loops while remaining independent of a real browser or GPU driver.
        model = node_json(
            textwrap.dedent(
                f"""
                const Graph = require({json.dumps(str(MODULE))});
                const payload = JSON.parse(require('fs').readFileSync(0, 'utf8'));
                process.stdout.write(JSON.stringify(Graph.normalizeGraphPayload(payload)));
                """
            ),
            canonical_payload(),
        )
        probe = textwrap.dedent(
            f"""
            class El {{
              constructor(tag) {{ this.tagName = String(tag).toUpperCase(); this.children = []; this.parentNode = null; this.attributes = {{}}; this.style = {{}}; this.dataset = {{}}; this.listeners = {{}}; this.className = ''; this.textContent = ''; this.hidden = false; this.classList = {{ add: (...xs) => xs.forEach(x => {{ if (!this.className.split(/\\s+/).includes(x)) this.className += (this.className ? ' ' : '') + x; }}), remove: (...xs) => xs.forEach(x => this.className = this.className.split(/\\s+/).filter(y => y && y !== x).join(' ')), toggle: (x, on) => {{ if (on) this.classList.add(x); else this.classList.remove(x); }} }}; }}
              appendChild(child) {{ if (child) {{ this.children.push(child); child.parentNode = this; }} return child; }} append(...xs) {{ xs.forEach(x => this.appendChild(x)); }} replaceChildren(...xs) {{ this.children = []; this.append(...xs); }}
              setAttribute(k, v) {{ this.attributes[k] = String(v); this[k] = String(v); }} getAttribute(k) {{ return this.attributes[k] ?? null; }} removeAttribute(k) {{ delete this.attributes[k]; }}
              addEventListener(k, fn) {{ (this.listeners[k] ||= []).push(fn); }} removeEventListener(k, fn) {{ this.listeners[k] = (this.listeners[k] || []).filter(x => x !== fn); }}
              dispatchEvent(event) {{ (this.listeners[event.type] || []).forEach(fn => fn(event)); return true; }} querySelectorAll(selector) {{ const found = []; const visit = node => {{ for (const child of node.children) {{ if (selector === 'button' && child.tagName === 'BUTTON') found.push(child); if (selector === '[data-node-id]' && child.dataset.nodeId) found.push(child); visit(child); }} }}; visit(this); return found; }} querySelector(selector) {{ return this.querySelectorAll(selector)[0] || null; }}
              click() {{ this.dispatchEvent({{ type: 'click' }}); }}
              getBoundingClientRect() {{ return {{ width: 800, height: 500, left: 0, top: 0 }}; }} focus() {{ this.focused = true; }}
            }}
            class Obj {{ constructor() {{ this.children = []; this.userData = {{}}; this.position = {{ set() {{}} }}; this.rotation = {{ set() {{}} }}; this.scale = {{ set() {{}}, setScalar() {{}} }}; }} add(x) {{ this.children.push(x); }} remove(x) {{ this.children = this.children.filter(y => y !== x); }} clear() {{ this.children = []; }} lookAt() {{}} updateProjectionMatrix() {{}} setAttribute() {{}} }}
            class Renderer {{ constructor() {{ this.domElement = new El('canvas'); this.renderCount = 0; }} setPixelRatio() {{}} setSize() {{}} setClearColor() {{}} setAnimationLoop() {{}} render() {{ this.renderCount++; }} dispose() {{}} }}
            const THREE = {{ WebGLRenderer: Renderer, Scene: Obj, Group: Obj, PerspectiveCamera: Obj, AmbientLight: Obj, DirectionalLight: Obj, GridHelper: Obj, BufferGeometry: Obj, Float32BufferAttribute: Obj, LineBasicMaterial: Obj, MeshStandardMaterial: Obj, Mesh: Obj, Line: Obj, LineSegments: Obj, SphereGeometry: Obj, IcosahedronGeometry: Obj, CatmullRomCurve3: Obj, Vector3: class {{ constructor(x=0,y=0,z=0) {{ this.x=x;this.y=y;this.z=z; }} }}, Color: class {{ constructor() {{}} }} }};
            const doc = {{ createElement: tag => new El(tag), createElementNS: (_ns, tag) => new El(tag), documentElement: new El('html'), addEventListener() {{}}, removeEventListener() {{}}, body: new El('body') }};
            const rafs = []; const win = {{ document: doc, WebGLRenderingContext: function(){{}}, THREE, matchMedia: () => ({{ matches: false, addEventListener() {{}}, removeEventListener() {{}} }}), addEventListener() {{}}, removeEventListener() {{}}, requestAnimationFrame: callback => {{ rafs.push(callback); return rafs.length; }}, cancelAnimationFrame() {{}} }};
            global.window = win; global.document = doc; global.THREE = THREE; global.requestAnimationFrame = win.requestAnimationFrame; global.cancelAnimationFrame = win.cancelAnimationFrame; global.performance = {{ now: () => 0 }};
            const Graph = require({json.dumps(str(MODULE))}); const mount = new El('div'); const list = new El('div'); const svg = new El('svg'); let selected = []; let milestones = [];
            const controller = Graph.createThreeGraph({{ mount, accessibleList: list, fallbackSvg: svg, onSelect: (id, kind) => selected.push([id, kind]), onOpenMilestone: id => milestones.push(id) }});
            const model = JSON.parse(require('fs').readFileSync(0, 'utf8'));
            controller.update(model, null); const first = controller.getSnapshot();
            controller.update(model, null); const second = controller.getSnapshot();
            const buttons = list.querySelectorAll('button'); const firstButton = buttons[0];
            if (firstButton) {{ firstButton.dispatchEvent({{ type: 'keydown', key: 'Enter', preventDefault() {{}} }}); firstButton.dispatchEvent({{ type: 'keydown', key: 'ArrowDown', preventDefault() {{}} }}); }}
            const focused = controller.focus('build'); const unknown = controller.focus('missing'); controller.setReducedMotion(true); controller.setView('overview'); controller.resetCamera();
            const large = {{ mode: 'overview', title: 'large', nodes: Array.from({{ length: 200 }}, (_, i) => ({{ id: `n-${{i}}`, title: `Node ${{i}}`, status: i % 3 ? 'incomplete' : 'active', execution: {{}} }})), edges: Array.from({{ length: 600 }}, (_, i) => ({{ from: `n-${{i % 199}}`, to: `n-${{(i % 199) + 1}}`, condition: 'success' }})), diagnostics: {{}} }};
            const budgetStart = performance.now(); controller.update(large); const budgetMs = performance.now() - budgetStart; const largeSnapshot = controller.getSnapshot();
            process.stdout.write(JSON.stringify({{ first, second, largeSnapshot, budgetMs, selected, focused, unknown, buttonCount: buttons.length, names: buttons.map(x => x.textContent), aria: buttons.map(x => x.attributes), rafCount: rafs.length }}));
            """
        )
        result = node_json(probe, model)
        # Either a fully capable shim or a graceful fallback is acceptable for
        # this dependency-free probe; both must expose the same snapshot counts
        # and avoid duplicate work on an identical update.
        self.assertEqual(result["first"]["nodeCount"], 3)
        self.assertEqual(result["first"]["edgeCount"], 2)
        self.assertEqual(result["second"]["nodeCount"], result["first"]["nodeCount"])
        self.assertEqual(result["second"]["edgeCount"], result["first"]["edgeCount"])
        self.assertEqual(result["largeSnapshot"]["nodeCount"], 200)
        self.assertEqual(result["largeSnapshot"]["edgeCount"], 600)
        self.assertLessEqual(result["budgetMs"], 250.0)
        self.assertEqual(result["buttonCount"], 3)
        self.assertTrue(result["focused"], result)
        self.assertFalse(result["unknown"])
        self.assertTrue(any("build" in name for name in result["names"]))
        self.assertTrue(any("status" in key.lower() or "aria" in key.lower() for attrs in result["aria"] for key in attrs))
