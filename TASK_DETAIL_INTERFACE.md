# Numbered task inspection interface

This document is the bounded contract for the numbered-task inspection feature
in the offline execution-graph board. It describes the existing Rust
projection and browser seams that the implementation must use; it is not a new
graph schema and it does not authorize changes to graph execution or assignment
state.

## Scope and invariants

- The individual board remains served by the Rust board and `/api/graph`
  remains the only task-data request. There is no task-detail endpoint,
  network call, LLM prompt, generated explanation, or remote asset.
- `.fractal/project.fractal` graph nodes are the immutable authority for task
  identity and execution metadata. Runtime status, `assignment`, and
  `evidence` continue to come from the existing `project_view` projection.
- A canonical task number is the non-empty string at
  `graph.nodes[].execution.task_number` (for example `1.1`). It is never
  made up from array position, wave size, task ID, or display order. The
  additive `groups[].tasks[].task_number` convenience field, when present,
  must be an exact copy of that value; nested `execution.task_number` remains
  for backwards compatibility.
- Existing `/api/graph` fields, `/api/failure-graph`, pause/run control, URL
  state, graph hashes, and master-graph semantics stay unchanged. Additive
  nullable task projection fields are safe for old clients to ignore.
- Master mode continues to use `master-graph.js` and its SVG browser. The
  numbered-task controller is initialized only for the individual graph and
  must not alter master DOM, queries, or selection behavior.

## Rust projection (`src/board.rs`)

`project_view` keeps the `fractal.execution_graph_view.v1` envelope and
adds two bounded, read-only conveniences to each `groups[].tasks[]` item:

```json
{
  "task_number": "1.1",
  "expected_output": "An analysis of the work goal."
}
```

`task_number` is `null` when `execution.task_number` is absent, empty, or
not a string. `expected_output` is the first non-empty bounded string from
the canonical node's `output`, `expected_output`, or
`efficiency.expected_artifact` (in that order), and is otherwise `null`. Do
not copy learning logs, raw evidence paths, credentials, or unknown flattened
fields into this projection. A Rust board test must cover a populated node and
a node with both fields missing, and must assert that the graph hash and
existing task fields are unchanged.

## Pure browser API (`execution-graph/three-graph.js`)

The existing CommonJS/browser export `window.FractalThreeGraph` retains
`normalizeGraphPayload`, `computeLayout`, and `createThreeGraph`, and adds
these pure helpers.

### `canonicalTaskNumber(node) -> string | null`

Read `node.execution.task_number` first when it is a non-empty string, then
read the additive `node.task_number` convenience value when it is a non-empty
string. Return `null` for all other values. The helper must not trim or
renumber a valid canonical value, and must be deterministic for malformed or
conflicting input (the nested execution value wins).

### `oneLineOverview(node) -> string`

Return one plain-language line using only graph facts, in this precedence:

1. `objective`;
2. `instruction`;
3. `title`;
4. `Task <id> has no recorded purpose.`

Collapse whitespace/newlines, preserve the source wording, and cap the result
at 180 characters with a deterministic ellipsis. This function must not add a
claim about completion, readiness, dependencies, agents, or output that the
source did not record.

### `buildTaskDetail(node, model) -> TaskDetail`

`model` is optional and is used only to recover dependency IDs from valid
non-`failure` edges when `node.depends_on` is missing. The return value is
JSON safe and has this stable shape:

```text
{
  taskNumber: string | null,
  overview: string,
  purpose: string,
  why: {
    ready: true | false | null,
    reason: string,
    blockedBy: string[]
  },
  dependencies: string[],
  execution: {
    wave: number | null,
    mode: string | null,
    parallelGroup: string | null
  },
  capability: string | null,
  instruction: string | null,
  expectedOutput: string | null,
  agent: { id: string, label: string, state: string } | null,
  evidence: object,
  gate: string | null
}
```

Missing values have explicit UI copy rather than invented facts: “Purpose not
recorded.”, “Dependency explanation not recorded.”, “No dependencies
recorded.”, “Execution wave not recorded.”, “Execution mode not recorded.”,
“Capability not recorded.”, “Instruction not recorded.”, “Expected output not
recorded.”, “No agent assigned.”, “No evidence recorded yet.”, and “Gate
criteria not recorded.”. `why.ready` is `null` when readiness was not
supplied; it must not default to `true`. Dependency IDs are informational and
do not change the Rust scheduler's readiness decision.

`normalizeGraphPayload` preserves its current fields and adds
`taskNumber` and `overview` to every task node. It may attach the
`TaskDetail` value as `detail`, but must not mutate the Rust payload,
renumber nodes, or silently drop a valid task number. Overview/milestone nodes
have `taskNumber: null` unless their source explicitly has a canonical
number.

### `createThreeGraph(options)` controller additions

The existing controller methods remain available. Their task-detail behavior is:

- `focus(ref) -> boolean` accepts either a task ID or an exact canonical task
  number. It returns `false` for an unknown reference and never throws for
  malformed input. A successful focus selects the node, frames it, and starts
  a bounded ease-out camera transition (target/distance; no unbounded
  auto-zoom).
- `setReducedMotion(true)` and the initial
  `prefers-reduced-motion: reduce` match disable the transition and apply the
  final camera target immediately. They also keep existing pulse/flow effects
  disabled. Turning the preference off may resume normal transitions on later
  focus calls.
- A second focus cancels/replaces the first transition. `resetCamera()`
  returns to the fitted graph bounds, is keyboard/click safe, and is immediate
  when reduced motion is enabled. `destroy()` cancels any transition and
  remains idempotent.
- Three.js remains decorative (`aria-hidden="true"`). Pointer selection and
  the accessible list call the same `onSelect(id, kind)` callback as the SVG
  path; milestone double-click/open behavior is preserved.

## Browser surfaces and interaction

### Number visibility and selection

Every task with a canonical number visibly renders `Task <task_number>`:

- the Three.js label starts with that text;
- the SVG task node has a `.task-number-label` with that text; and
- the accessible list button starts with that text and includes ID, title,
  status, and the one-line overview.

An unnumbered task remains selectable by ID and visibly says “Task number
unavailable”; no placeholder number may look canonical. SVG, Three.js, and
list selection all enter the same `app.js::selectNode` path, update the
inspector, and request `focus` for the selected task. Selection state is
mirrored by `aria-selected`, `aria-current`, SVG focus styling, and the
list's keyboard focus ring. `Enter`/ `Space` select; arrow keys move
between list buttons.

### Inspector detail

`execution-graph/index.html` adds a short overview region (for example
`#node-overview`) near the task number/title and keeps the existing inspector
as the canonical detail surface. For a task, the inspector exposes, with
stable labels/IDs, purpose, why/readiness, dependencies, execution wave/mode/
parallel group, capability, instruction, expected output, assigned agent and
state, evidence/verification, source, and gate. Each field uses the explicit
fallback copy above when absent. The one-line overview is also available as
the task button's accessible description; do not place the full detail in the
graph label.

The existing milestone progress inspector and “Open milestone graph” button
remain unchanged. The visible “RESET CAMERA” control is present while the
Three.js scene is active; the existing “← Full architecture” back control is
visible in task view and returns to overview. Both controls are keyboard
reachable and have stable accessible names.

### SVG/WebGL fallback and resilience

If WebGL construction/context creation/context loss fails, the controller must
report `{active:false, reason}` through `onCapabilityChange`, keep the SVG
and accessible list usable, and make `focus`/`resetCamera` safe no-ops or
SVG-safe operations. The task number, overview, inspector detail, back control,
pause, refresh, failure history, and URL navigation must still work. No
exception in the detail helper or optional 3D path may block `/api/graph`
refresh.

All new text is escaped through `textContent`; no HTML interpolation is
needed. The module and tests must remain offline: no `fetch`,
`XMLHttpRequest`, CDN, dynamic import, remote font, or LLM/network
dependency.

## Required verification

Add focused coverage without touching frozen legacy graph-state/server code:

1. `src/board.rs` tests assert canonical `task_number` and
   `expected_output` projection, null fallbacks, bounded strings, and
   unchanged `fractal.execution_graph_view.v1`/graph hash behavior.
2. `execution-graph/tests/test_three_graph.py` adds CommonJS probes for
   number extraction (including malformed/missing fields), immutable
   normalization, deterministic one-line overview/detail fallbacks,
   dependency/output projection, and a duplicate-update controller fixture.
   The DOM/WebGL shim must verify that SVG/list/Three selection reaches the
   same callback, unknown focus returns `false`, reduced-motion focus is
   immediate, and labels/buttons expose the canonical number and overview.
3. Static checks cover the new inspector IDs, `.task-number-label`, reset/back
   controls, local-only assets, and preservation of `master-graph.js`/SVG
   fallback paths.

Run the focused checks first, then the board tests:

```sh
python3 -m unittest discover -s execution-graph/tests -p 'test_*.py'
cargo test board
```
