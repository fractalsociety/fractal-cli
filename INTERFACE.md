# 3D execution-graph viewer interface

This plan is for the real Fractal viewer, not for a standalone Python module.
The board is a static HTML/CSS/JavaScript bundle embedded by
`src/board.rs::embedded_asset` and served by the Rust board at `/`.  The
implementation must preserve the existing Rust API and inspector/run-control
behavior while adding a Three.js scene for the individual graph.

## Alternatives considered

1. **Chosen — an additive Three.js scene with a deterministic pure layout
   layer.**  Add `execution-graph/three-graph.js`, load a pinned local Three.js
   UMD asset, and let `app.js` continue owning fetch, URL state, metrics,
   failures, pause, and the inspector.  The new controller consumes a normalized
   projection and reports selection back through callbacks.  Existing SVG
   rendering remains as a no-WebGL/reduced-capability fallback.  This gives
   depth, lighting, animation, ray-picking, and camera framing without changing
   the graph contract or making the board depend on a build tool or network.

2. **Rejected — a force-directed SVG/canvas rewrite.**  It could reuse the
   current no-dependency stack, but it is still fundamentally 2D, makes layout
   nondeterministic, and performs poorly on dense dependency graphs.  It also
   weakens the requested spatial hierarchy and makes camera/occlusion behavior
   difficult to communicate.

3. **Rejected — a CDN/import-map or WebGPU application rewrite.**  It could
   produce a richer demo, but the board is embedded/offline and must work when
   served from localhost with no internet.  It would add a new runtime/build
   dependency, risk older WebGL-only browsers, and force changes into the Rust
   asset boundary and the master-graph controller.

## Files and ownership

The primary module is **`execution-graph/three-graph.js`**.  It is a classic
browser script (no bundler and no network imports) and exposes
`window.FractalThreeGraph`.  It must also assign the same object to
`module.exports` when a CommonJS test harness is present, so deterministic
layout functions can be tested without a browser.

The implementation may make the following narrowly scoped companion changes:

- `execution-graph/index.html`: load a local, pinned
  `execution-graph/vendor/three.min.js` before `three-graph.js`; add a
  `#graph-3d` mount and an accessible node-list mount beside the existing
  `#graph` SVG.  Do not remove the SVG fallback or existing controls.
- `execution-graph/app.js`: create one controller after DOM startup, send the
  current graph/view/selection to it after each `loadGraph`/view transition,
  and route its `onSelect`/`onOpenMilestone` callbacks through the existing
  `selectNode`/`openMilestone` paths.  Failure history, pause, metrics, URL
  state, and inspector remain owned by this file.
- `execution-graph/styles.css` (and, only if needed for master-mode scoping,
  `master-graph.css`): style the canvas shell, list/fallback toggle, focus
  rings, status legend, reduced-motion state, and mobile layout.  Keep all
  selectors namespaced so the master browser is unchanged.
- `execution-graph/vendor/three.min.js`: a checked-in, pinned Three.js UMD
  runtime.  No CDN URL, dynamic import, remote texture, or runtime fetch may
  be introduced.  A short adjacent `vendor/README.md` or header comment must
  record the upstream version and license.
- `execution-graph/tests/test_three_graph.py` (or an equivalently named test
  file): Python `unittest` that drives the pure functions through Node's VM or
  CommonJS export and performs static offline/accessibility checks.  It must
  not start a server or require internet access.

The Rust board already embeds `index.html`, `app.js`, both style sheets, and
the graph assets in `src/board.rs`; if a new vendor asset is added, update the
asset route and `embedded_asset` match in that Rust file so packaged and
checkout-based boards serve identical bytes.  Do not change `/api/graph`,
`/api/failure-graph`, `/api/run/pause`, identity, or master-graph semantics.

## Data contract consumed by the module

`three-graph.js` must treat the Rust response as immutable and tolerate legacy
or partially populated values.  It consumes only these existing fields:

```text
payload.schema                 "fractal.execution_graph_view.v1"
payload.overview.nodes[]       {id,title,status,completed,total,progress,gate}
payload.overview.edges[]       {from,to,condition}
payload.groups[]               {id,title,status,completed,total,progress,
                                 tasks[],edges[]}
payload.groups[].tasks[]       {id,title,kind,status,checked,line,instruction,
                                 gate,assignment,execution}
payload.groups[].tasks[].execution
                                {wave,mode,parallel_group}
```

`payload.graph`, `totals`, `efficiency`, `failure_summary`, `run_control`, and
`development` remain available to `app.js`; the 3D module must not reinterpret
or mutate them.  `status` values are `complete`, `active`, and `incomplete`;
unknown values render as incomplete with a diagnostic in the returned model.
Edges that point at missing IDs are ignored for rendering and reported in the
diagnostic list.  A `condition: "failure"` edge is kept and styled as a
failure/alternate path, but is excluded from predecessor-wave calculation.

## Public JavaScript API

### `normalizeGraphPayload(payload, view = "overview")`

Returns a new, JSON-safe `GraphModel` without mutating `payload`:

```js
{
  mode: "overview" | "tasks",
  groupId: string | null,
  title: string,
  nodes: [{
    id, title, kind, status, gate, instruction, line,
    completed, total, progress, checked, assignment, execution
  }],
  edges: [{from, to, condition}],
  diagnostics: {unknownStatus: string[], missingEdgeNodes: string[], cycles: string[]}
}
```

For a group ID, `nodes`/`edges` come from that group; for `overview`, they
come from `payload.overview`.  Array order is preserved only as a display hint;
all layout and diagnostics sorting is by stable node ID.

### `computeLayout(model, options = {})`

This is pure and deterministic.  It returns:

```js
{
  nodes: [{id, x, y, z, wave, radius, depth, status}],
  edges: [{from, to, condition, points: [{x, y, z}, ...]}],
  bounds: {min: {x,y,z}, max: {x,y,z}, center: {x,y,z}, radius: number},
  diagnostics: {cycles: string[], overlaps: string[]}
}
```

Supported options and defaults are:

```js
{
  waveGap: 8, rowGap: 4.2, depthSpread: 3.2,
  nodeRadius: 1.25, seed: "fractal-execution-graph"
}
```

The algorithm is a stable topological wave layout: declared
`execution.wave` wins; otherwise Kahn levels are computed from non-failure
edges with lexicographic ID tie-breaking.  Remaining cyclic nodes are assigned
successive deterministic fallback waves and listed in `diagnostics.cycles`.
Within a wave, IDs are sorted and centered on Y; Z is a bounded, hash-derived
jitter (never `Math.random`) so refreshes do not move nodes.  The implementation
must guarantee finite coordinates, a stable node order, and center separation of
at least `2 * nodeRadius + 0.35` for nodes in the same wave.  Edge `points` are
short cubic-bezier polylines between the returned node positions, with no
unbounded subdivision.

### `createThreeGraph(options)`

Creates or returns a controller; it must not start a second renderer for the
same mount.  Required options:

```js
{
  mount: HTMLElement,                 // #graph-3d
  accessibleList: HTMLElement,        // node buttons/listbox mount
  fallbackSvg: SVGElement,            // existing #graph
  onSelect: (nodeId, kind) => void,   // existing inspector path
  onOpenMilestone?: (nodeId) => void,
  onCapabilityChange?: ({active, reason}) => void
}
```

The returned `GraphController` has these methods:

```js
update(model, selectedId = null) -> void
setView(viewId) -> void
focus(nodeId) -> boolean
resetCamera() -> void
setReducedMotion(enabled) -> void
getSnapshot() -> {active, nodeCount, edgeCount, renderer, frameCount}
destroy() -> void
```

`update` is idempotent for the same model hash, updates existing meshes in
place, disposes removed geometries/materials, and synchronizes the accessible
list. `focus` selects and frames an existing node, returning `false` for an
unknown ID. `destroy` removes listeners, animation frames, WebGL resources,
and the list contents; calling it twice is safe.

## Visual and interaction contract

- Use a dark Fractal field with a restrained grid/star layer, physically
  coherent depth, and a clear camera target.  Nodes are small beveled/icosphere
  meshes with status colors: complete green, active amber with a subtle pulse,
  incomplete cool gray, and failure-path red.  Edges are low-opacity curves
  with directional flow; active/selected paths receive an emissive highlight.
- Fit the camera to `bounds` on first update and expose a visible reset-camera
  control.  Pointer drag orbits, wheel/pinch dolly is bounded, and selection is
  ray-picked.  Do not auto-rotate while a user is interacting; honor
  `prefers-reduced-motion` and `setReducedMotion` by disabling pulses, flow,
  and automatic motion.
- Keep the existing inspector as the canonical detail surface.  Selecting a
  3D node invokes the existing callback; selecting a milestone still opens its
  task graph.  The pause button, failure history, refresh, URL query state,
  master/individual toggle, and all existing metrics must continue to work.
- Canvas/WebGL is decorative and `aria-hidden`.  The accessible list contains
  one real button per visible node with ID, title, status, and agent text;
  `Enter`/`Space` selects it, arrow keys move between nodes, and focus rings
  meet the existing contrast rule.  A “List / 3D” control exposes the list on
  demand, and the list is automatically shown when WebGL is unavailable.
- If `WebGLRenderer` construction, context creation, or a runtime context-loss
  event fails, call `onCapabilityChange({active:false, reason})`, show the
  existing SVG renderer, and leave every current control usable.  No exception
  may prevent `/api/graph` refresh or inspector updates.  Master mode may keep
  its existing SVG browser; it must not regress while the individual scene is
  active.

## Acceptance benchmarks

These are the measurable bars for the implementation and the parallel test
author.  Tests may use Node for pure functions and a lightweight DOM/WebGL stub
for controller behavior; visual checks can be a browser screenshot/manual QA.

1. **Contract and determinism:** `normalizeGraphPayload` accepts the canonical
   fixture plus empty/legacy/malformed optional fields without throwing,
   preserves every valid ID exactly once, and reports unknown statuses and
   dangling edges.  Calling `computeLayout` twice on the same 500-node,
   1,500-edge fixture returns byte-for-byte equal JSON; no `Math.random` or
   network API is used.
2. **Layout safety/performance:** a 500-node/1,500-edge DAG completes
   normalization plus layout in ≤100 ms in Node on the test machine, returns
   finite coordinates for every node, keeps all same-wave centers at least
   `2r + 0.35` apart, and bounds every edge to ≤8 points.  A cyclic graph of
   100 nodes terminates in ≤100 ms and reports the cycle IDs.
3. **Rendering budget:** with a 200-node/600-edge model, the first scene update
   completes in ≤250 ms and creates no more than 200 node meshes plus 2 edge
   line objects (instancing/batched buffers are preferred); a refresh with an
   unchanged model creates no duplicate renderer, canvas, listeners, or RAF
   loop.  `getSnapshot()` reports the expected counts.
4. **Selection and operational continuity:** pointer/raycast and accessible
   list selection both invoke `onSelect` with the same node ID; unknown focus
   returns `false`; milestone callbacks still reach `openMilestone`.  Existing
   `loadGraph`, inspector, failure history, pause, refresh, URL navigation, and
   master-mode controls remain callable after 20 refresh/update cycles.
5. **Accessibility and motion:** every visible node has a keyboard-focusable
   button with an accessible name containing ID/title/status; selection mirrors
   `aria-current`/selected styling; arrow/Enter/Space interactions work; a
   reduced-motion preference disables animation and no focus indicator is lost.
6. **Fallback and resilience:** when `window.WebGLRenderingContext` or
   `THREE.WebGLRenderer` is absent/throws, the controller reports inactive,
   hides the canvas, leaves `#graph` SVG visible, renders the accessible list,
   and does not throw during update/destroy.  Simulated context loss follows
   the same path.
7. **Offline/package integrity:** all script/style/image references are local
   relative paths; no source contains `https://`, `http://`, `unpkg`,
   `jsdelivr`, dynamic `import()`, or a remote font/texture.  Rust embedded
   asset routing serves the new vendor and module files, and a clean board
   load produces no 404s.  The pinned Three.js version/license is documented.
8. **Visual QA:** at 1280×800 and 390×844, the board shows a legible status
   legend, node labels/selection, camera reset, inspector, and metrics without
   horizontal page overflow; active nodes visibly pulse only when motion is
   allowed, selected dependency paths are brighter, and the screenshot remains
   intelligible at both zoomed-out overview and a dense task wave.

## Verification commands

Run the focused browser-contract tests first (`python3 -m unittest
discover -s execution-graph/tests -p 'test_*.py'`), then the Rust board tests
that cover static asset embedding (`cargo test board`).  For visual QA, serve
the canonical graph with `fractal graph serve --repo "$PWD" --port 8092
--no-open` and inspect both the individual URL and `?mode=master`; do not use
the frozen Python legacy server or mutate graph-state files.
