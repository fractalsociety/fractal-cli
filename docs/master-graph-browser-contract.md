# Master Graph Browser Contract

Status: normative for the Individual and Master Graph Browser and the read-only
board API surfaces that feed it. Data schemas and composition rules live in
[`project-catalog-contract.md`](./project-catalog-contract.md),
[`schemas/fractal.catalog.v1.schema.json`](../schemas/fractal.catalog.v1.schema.json),
and
[`schemas/fractal.master_graph_view.v1.schema.json`](../schemas/fractal.master_graph_view.v1.schema.json).
Where this document and a schema disagree, the stricter rule wins.

The key words MUST, MUST NOT, SHOULD, and MAY are used as in RFC 2119.

Evidence base:

- `artifacts/audit/repository-inventory.json` (`fractal.repository_inventory.v1`,
  39 deduplicated records, 1 unavailable, inventory hash
  `sha256:2778d1096b9db5cfbdf63930d368a6eb9d0654a1fa13652b231a7b0aa80f6bad`)
- `artifacts/audit/graph-browser-analysis.md` and
  `artifacts/audit/graph-browser-evidence/` (live onelink45 board: 23 tasks,
  `run_control.phase=executing`, totals `complete=3 / active=2 / incomplete=18`)
- PRD acceptance criteria **AC-8**, **AC-9**, **AC-10** (and performance budgets
  that the browser MUST respect from **AC-11**)

This contract is additive. It MUST NOT replace, weaken, or mute the existing
single-repo execution graph board, its 2-second poll, hero metrics, efficiency
panel, inspector, or token-gated pause control.

---

## 1. Purpose and non-goals

### 1.1 Purpose

Give developers a dependency-free HTML/CSS/JS browser that can:

1. Browse one project's live `fractal.execution_graph_view.v1` (individual mode).
2. Switch among inventoried projects without restarting the board server.
3. Browse the composed `fractal.master_graph_view.v1` (master mode).
4. Search, filter, follow cross-graph links, and inspect evidence / tests /
   decisions / diagnostics.
5. Remain usable at 320 CSS pixels and under keyboard-only operation.

### 1.2 Non-goals

- Persisting the master view, writing catalogs, or mutating
  `.fractal/project.fractal` from the browser.
- Calling Python `task-state` / `graph-state*.json` / legacy Mac Runtime mutation
  endpoints.
- Inventing graph nodes for unavailable or invalid repositories.
- Replacing pause / run-control semantics or inventing a second pause path.
- Introducing a framework, bundler, or second runtime.

---

## 2. Information architecture

### 2.1 Modes

| Mode id | Primary data | Primary surface | When available |
| --- | --- | --- | --- |
| `individual` | `fractal.execution_graph_view.v1` | Existing SVG stage + inspector (extended) | Always when a selected project resolves inside the frozen inventory **or** the board's bound cwd project |
| `master` | `fractal.master_graph_view.v1` | Master canvas **or** list/detail fallback | When inventory + composition succeed enough to return a view body (may include unavailable/invalid entries) |

Exactly one mode is active. Mode is part of URL state (§6).

### 2.2 Regions (shared chrome)

Extend the existing shell; do not invent a second page.

| Region | Role | Existing anchors to preserve |
| --- | --- | --- |
| **Brand / home** | Returns to the board root for the current mode | `.brand` |
| **Project switcher** | Lists frozen inventory; selects individual project | new control in header cluster |
| **Mode toggle** | `Individual` / `Master` | new control; mutually exclusive with switcher's "open as individual" |
| **Live phase pill** | Shows `run_control.phase` in individual mode | `#live-state` |
| **Efficiency** | Optional efficiency `<details>` | `#efficiency-counter` |
| **Pause** | Token-gated confirm → `POST /api/run/pause` | `#pause-build` — **individual mode only** |
| **Refresh** | Manual refetch of the active payload | `#refresh` |
| **Search** | Debounced text query over visible index | new |
| **Filters** | Status + relationship-type facets | new |
| **Hero metrics** | Individual: execution totals. Master: summary counts | `#percent`, `#completed`, `#active`, `#remaining` (relabeled in master) |
| **Primary stage** | SVG graph **or** list/detail at ≤560 CSS px width for master, and whenever the operator opts into list mode | `#graph-stage`, `#graph` |
| **Inspector / detail** | Selection details; evidence / tests / decisions / diagnostics panels | `#inspector` |
| **Diagnostics rail** | Compact count + expandable list of view diagnostics | new; MAY live inside inspector when empty selection |
| **Footer source** | Provenance line | `#source-label` |
| **Live summary region** | Assertive/polite announcements for mode, selection, filter, and error changes | new `aria-live` element |

### 2.3 Individual hierarchy

```text
Project (selected project_key or cwd binding)
  └─ Overview milestones (groups / overview.nodes)     [optional multi-group]
       └─ Task / gate nodes (groups[].tasks)
            ├─ assignment, execution, instruction, gate
            └─ (catalog overlays when present) evidence, tests, decisions
```

Single-group Rust projections MAY auto-open the task view on first load, but
chrome (`#graph-kicker`, `#graph-title`, `#back`, document title, SVG
`aria-label`, hero eyebrow) MUST reflect the live `title` / `work_id` / group —
not hardcoded Mac Runtime copy (defect tracked in the browser analysis; this
contract makes the fix mandatory for new work).

### 2.4 Master hierarchy

```text
Frozen inventory
  ├─ projects[]          (available / unavailable / catalog_state)
  ├─ nodes[]             (project | component | capability)
  ├─ edges[]             (dep:* internal, link:* cross-graph; retained unresolved/self/cyclic)
  ├─ diagnostics[]
  ├─ sources[]           (byte provenance)
  └─ unavailable[]       (inventory-level absences)
```

Visual grouping SHOULD cluster by `project_key`. Namespaces on node ids
(`project:…`, `component:…`, `capability:…`) and edge ids (`dep:…`, `link:…`)
are the only identity the UI may use for selection and deep links.

### 2.5 Selection model

A selection is a discriminated union stored in UI state and URL:

| `selection.kind` | Payload | Inspector shows |
| --- | --- | --- |
| `none` | — | Empty orbit copy |
| `task` | `{ project_key, task_id }` | Existing task/gate inspector + optional catalog overlays |
| `milestone` | `{ project_key, group_id }` | Existing milestone inspector |
| `master_node` | `{ node_id }` | Title, kind, status, project label, evidence/tests/decisions links |
| `master_edge` | `{ edge_id }` | Type, from/to, resolution, cycle_group, confidence, rationale |
| `project` | `{ project_key }` | Inventory row + catalog_state + status_counts + open-individual CTA |
| `diagnostic` | `{ diagnostic_index or code+context }` | Message, severity, context |

Selecting MUST NOT mutate server state.

---

## 3. API-to-view mapping

### 3.1 Endpoints (normative)

All new routes are loopback GET JSON on the existing Rust board (`board.rs`
allowlist). Non-GET on graph/master/inventory routes MUST return **405** with the
existing read-only message pattern. Paths that escape the frozen inventory MUST
return **404** or **403** without filesystem disclosure.

| Method | Path | Response schema / body | UI consumer |
| --- | --- | --- | --- |
| `GET` | `/api/health` | existing health JSON | readiness |
| `GET` | `/api/identity` | `fractal.board_identity.v1` | board identity checks |
| `GET` | `/api/graph` | `fractal.execution_graph_view.v1` for the **board-bound cwd** project | default individual load (backward compatible) |
| `GET` | `/api/graph?project=<project_key>` | same schema for an inventory member | project switcher / deep link |
| `GET` | `/api/projects` | inventory projection (§3.2) | project switcher |
| `GET` | `/api/master-graph` | `fractal.master_graph_view.v1` | master mode |
| `GET` | `/api/master` | **alias** of `/api/master-graph` (optional; if present MUST be identical) | — |
| `POST` | `/api/run/pause` | existing token-gated pause | `#pause-build` only |
| `POST`/`PUT`/`PATCH`/`DELETE` | `/api/graph`, `/api/projects`, `/api/master-graph`, `/api/master` | **405** | negative acceptance |
| `GET` | any path outside allowlist or `..` traversal | **404** JSON | negative acceptance |

Static assets remain: `/`, `/app.js`, `/styles.css`, `/assets/*`, plus new modular
`/master-graph.js` and `/master-graph.css` when integrated. No other mutation
surface is introduced.

`/api/inventory` and `/api/graphs` MUST NOT be required by the browser. If added
later they are non-normative aliases.

### 3.2 `GET /api/projects` projection

Minimal contract (field names stable):

```json
{
  "schema": "fractal.board_projects.v1",
  "inventory_hash": "sha256:…",
  "bound_project_key": "… or null",
  "projects": [
    {
      "project_key": "fractal-cli-3c8b9dde9efc",
      "labels": ["fractal-cli"],
      "registry_numbers": [18],
      "canonical_workspace": "/workspace/fractal-cli",
      "available": true,
      "catalog_state": "valid",
      "graph_hash": "sha256:… or null",
      "unavailable_reason": null
    }
  ],
  "unavailable": [
    {
      "canonical_workspace": "…",
      "reason": "workspace_path_does_not_exist",
      "registry_numbers": [33]
    }
  ]
}
```

Ordering: `projects` ascending by `project_key`. `unavailable` ascending by
`canonical_workspace`. Keys and fingerprints follow the catalog contract §3.
The browser MUST treat this list as the only switcher source — never
`projects.json` directly.

### 3.3 Individual view binding

| UI element | API field |
| --- | --- |
| Document title / eyebrow / SVG aria-label | `title`, `work_id` |
| Hero percent / completed / active / remaining | `totals.percent`, `.complete`, `.active`, `.incomplete` |
| Live pill | `run_control.phase` (+ `available`) |
| Pause enabled | `run_control.available === true` and token present |
| Efficiency | `efficiency` (optional) |
| Overview SVG | `overview.nodes` / `overview.edges` |
| Task SVG | matching `groups[]` |
| Inspector task body | task `status`, `kind`, `assignment`, `execution`, `instruction`, `gate` |
| Footer | `source`, `source_mtime` |

Catalog overlays (when the selected project's catalog is `valid`) MAY be fetched
from the master view cache by `project_key` rather than a separate endpoint:
capabilities/components/tests/decisions whose `project_key` matches. Overlay
absence MUST NOT block the execution inspector.

### 3.4 Master view binding

| UI element | API field |
| --- | --- |
| Hero “projects” | `summary.projects_total` |
| Hero “audited” | `summary.audited_available` |
| Hero “nodes” | `summary.node_count` |
| Hero “issues” | `summary.diagnostic_counts.error + .warning` (or dedicated remaining slot) |
| Canvas / list rows | `nodes[]` |
| Edges / relationship filter | `edges[]` (`type`, `resolution`, `cycle_group`) |
| Project switcher badges | `projects[].catalog_state`, `.available` |
| Diagnostics panel | `diagnostics[]` + `unavailable[]` |
| Footer | `inventory_hash`, `view_hash`, truncated flag if present |
| Evidence panel | selected node's backing catalog claim `evidence[]` (joined client-side from composition payload if embedded, else from node detail fields the API includes) |
| Tests panel | catalog `tests[]` for the node's `project_key` / linked `test_keys` |
| Decisions panel | catalog `decisions[]` for the project's key |

The board MAY embed per-node `evidence`, `tests`, and `decisions` summaries on
`GET /api/master-graph` **or** expose them only inside already-returned
`projects`/`nodes` detail objects. The browser MUST NOT require a writable API
to show them. Secrets, absolute evidence paths, and raw logs MUST NOT appear
(catalog contract §6 / §13).

### 3.5 Error bodies

JSON errors SHOULD use `{ "error": "<human message>", "code": "<stable_code>" }`
where `code` is one of: `not_found`, `not_in_inventory`, `unavailable_project`,
`invalid_project`, `read_only`, `compose_failed`, `bad_request`. The UI maps
these to degraded panels (§9), never to a blank document.

---

## 4. Project switcher

### 4.1 Behavior

1. On board load, fetch `GET /api/projects` once (then on Refresh / inventory hash
   change).
2. Render every `projects[]` row and surface `unavailable[]` as disabled entries
   with reason text.
3. Indicate the **current** individual project with `aria-current="true"`.
4. Choosing an **available** project:
   - sets mode to `individual`;
   - fetches `GET /api/graph?project=<project_key>`;
   - updates URL (§6);
   - replaces the stage; does **not** write any `project.fractal`.
5. Choosing an **unavailable** or **invalid** row opens the degraded panel (§9)
   and MUST NOT fabricate tasks.
6. The bound cwd project (`bound_project_key` or implicit `/api/graph` without
   query) remains valid even if temporarily missing from a stale client cache;
   Refresh reconciles.

### 4.2 Accessible name

Control accessible name: `Project switcher`. Each option name:
`{primary label}, {catalog_state}, {available|unavailable}`. Primary label is
the first inventory label, else the final path segment of
`canonical_workspace`, else `project_key`.

### 4.3 Keyboard

Opener is in the header tab order (§11). Listbox / dialog pattern:

- `Enter` / `Space` opens.
- Arrow keys move among options.
- `Enter` selects.
- `Escape` closes and restores focus to the opener.

At 320px the switcher MUST remain fully operable without SVG hit targets
(analysis gap P-04).

---

## 5. Individual / master modes

### 5.1 Toggle

A two-state control labeled `View mode` with options `Individual` and `Master`
(`role="tablist"` or equivalent radiogroup). Changing mode:

| Transition | Action |
| --- | --- |
| → `master` | `GET /api/master-graph` (use cache if warm and fingerprints match); clear execution-only chrome that does not apply; hide Pause; show master metrics |
| → `individual` | Restore last individual `project_key` or bound project; show Pause if `run_control.available`; resume execution poll |

### 5.2 Preservation rules (normative)

While browsing either mode, the browser and board MUST:

1. Leave every source `.fractal/project.fractal` byte, mtime, `graph_hash`, and
   `execution.assignments` unchanged (AC-6).
2. Keep the existing individual poll interval (**2000 ms**) and selection
   identity preservation across polls.
3. Keep `#pause-build` behavior identical: `window.confirm`, header
   `X-Fractal-Control-Token`, no auto-pause, no pause invocation from master
   mode or acceptance harnesses unless explicitly testing pause.
4. Keep `POST /api/graph` → 405.
5. Not call legacy Python mutation endpoints.

Pause visibility today is removed under `≤560px` via CSS. New work MUST relocate
pause into an accessible overflow/menu on narrow viewports rather than leaving
the feature unreachable when `run_control.available` is true (individual mode).

### 5.3 Mode-specific chrome

| Chrome | Individual | Master |
| --- | --- | --- |
| Live phase pill | yes | hidden or shows `MASTER VIEW` informational (not a run phase) |
| Pause | yes when available | hidden / disabled |
| Efficiency | yes when present | optional estate rollup only if API provides it; otherwise hide |
| Back / milestone | existing overview drill | breadcrumb back from cross-link jump (§8) |
| Legend | status vocabulary for tasks | status vocabulary for catalog statuses + relationship types |

---

## 6. Query and URL state

All shareable state lives in the URL query string on the board origin. The
browser MUST use `history.replaceState` for poll-driven noise and
`pushState` for user-driven mode/project/selection/filter changes.

### 6.1 Parameters

| Param | Values | Default |
| --- | --- | --- |
| `mode` | `individual` \| `master` | `individual` |
| `project` | `project_key` | bound / cwd project |
| `view` | `overview` \| group id \| `list` \| `graph` | individual: existing auto-drill rules; master desktop: `graph`; master ≤560px: `list` |
| `sel` | selection id (task id, node id, edge id, or `project:<key>`) | empty |
| `q` | search string (max 200 chars) | empty |
| `status` | comma-separated status tokens (§7) | empty (= all) |
| `rel` | comma-separated relationship types (§7) | empty (= all) |
| `panel` | `info` \| `evidence` \| `tests` \| `decisions` \| `diagnostics` | `info` |

Unknown params MUST be ignored (forward compatible). Values MUST be URI-encoded.
`project` values failing inventory membership → degraded state, not a guessed
filesystem path.

### 6.2 Restoration

On load: parse → fetch required payloads → apply filters → restore selection if
still present → announce via live region. If `sel` is missing after filter, clear
selection and keep filters. Hard refresh MUST reproduce the same view for the
same query (AC-9).

### 6.3 Cache busting

Individual poll may keep `?t=<epoch>` on `/api/graph` requests. URL application
state params above are orthogonal and MUST NOT be overwritten by the cache-bust
token.

---

## 7. Search and filters

### 7.1 Search

- Input accessible name: `Search projects, components, and features`.
- Debounce: **200 ms** after last keystroke (implementations MAY use 150–300 ms;
  acceptance uses 200 ms).
- Scope:
  - **Individual:** task/milestone `id`, `title`, `instruction`, agent label.
  - **Master:** `project_key`, labels, node `title`/`key`, capability titles,
    component names, decision titles.
- Match: case-insensitive substring over normalized whitespace.
- Empty query resets highlighting and list filtering.
- Results: matching nodes remain full opacity; non-matches are hidden in list
  mode and dimmed (not removed from accessibility tree unless also excluded by
  a status/rel filter) in graph mode.
- Live region announces `{n} matches` when the debounced query settles.

### 7.2 Status filter

Closed token sets:

| Mode | Allowed `status` tokens |
| --- | --- |
| Individual | `complete`, `active`, `incomplete` |
| Master | `verified`, `implemented_unverified`, `partial`, `unknown`, plus project-level `available`, `unavailable`, `invalid`, `missing` |

Multi-select is OR within the status facet. Unknown tokens ignored.

### 7.3 Relationship filter

Applies to master edges (and individual dependency edges when shown):

`depends_on`, `uses_component`, `derived_from`, `forked_from`, `supersedes`,
`shares_component`, `related_to`, plus internal `dep` (component dependency) and
execution `predecessor` when individual edges are filterable.

Multi-select is OR. Clearing `rel` restores all relationship types. Nodes with
no remaining visible incident edge MAY remain visible if they match search/status;
acceptance scenario M-03 requires non-matching **edges** hidden/dimmed and
restored on clear.

### 7.4 Combined predicate

A node is shown when it matches (search) AND (status facet) AND (has at least
one visible edge OR is selected OR search matched it directly). Filters MUST be
reversible without reload.

---

## 8. Cross-link traversal

### 8.1 Activation

Activating a master `link:*` edge or a “Open target” control:

1. If `resolution === "resolved"` and `to.node_id` present:
   - Prefer navigating to **individual** mode for `to.project_key` with
     `sel` focused on the target component/capability when mappable, else project
     root.
   - Set breadcrumb stack entry `{ from: "master", edge_id, return_url }`.
2. If `unresolved` / `ambiguous` / `self`: do **not** navigate; open inspector on
   the edge with diagnostic explanation.
3. Cyclic edges remain activable; inspector shows `cycle_group` and the
   `cross_project_cycle` diagnostic — traversal still follows the resolved
   endpoint once, without recursive expansion.

### 8.2 Return

Breadcrumb control `Back to master` restores `mode=master`, prior `q`/`status`/
`rel`, and selects the originating `edge_id`. Keyboard: control is in tab order
immediately after mode toggle when the stack is non-empty.

### 8.3 Safety

Traversal NEVER requests paths outside inventory keys. Client MUST NOT turn
aliases into filesystem fetches; only `project_key` query params are sent.

---

## 9. Evidence, test, and decision detail panels

### 9.1 Panels

Inspector tabs (or disclosure set) with `panel` URL binding:

| Panel | Content rules |
| --- | --- |
| `info` | Identity, status, kind, instruction/gate or catalog summary |
| `evidence` | Repo-relative `path`, `sha256`, `kind`, optional `spans`, `observed_commit` / dirty fingerprint. No absolute paths. No file bodies. |
| `tests` | `command`, `classification`, `exit_code`, `duration_ms`, `log_sha256`, bounded `log_excerpt` (≤1024). Classifications other than `pass` never imply verified. |
| `decisions` | `title`, `status` (`adopted`/`proposed`/`superseded`/`rejected`), `summary`, evidence refs |
| `diagnostics` | severity, code, message, context for the selection or the whole view |

Empty panel copy MUST explain whether data is missing, not-yet-audited
(`catalog_state: missing`), or filtered away.

### 9.2 Conservative display

UI chrome MUST NOT upgrade status. Color is supplementary; text status labels
are required (analysis: status-by-color-only is insufficient for AC-10).

---

## 10. Invalid, unavailable, loading, and cache states

### 10.1 Invalid / unavailable

| Condition | UI |
| --- | --- |
| Inventory fetch failed | Full-page recoverable error in stage + footer; Retry button; no fabricated projects |
| Project not in inventory | Degraded panel `code=not_in_inventory`; switcher still usable |
| `available: false` / inventory `unavailable[]` | Row disabled; reason from inventory; selecting shows degraded panel — **no graph nodes** |
| `catalog_state: invalid` / `unsupported_schema` | Project appears; master shows project node + diagnostics; opening catalog panels shows error, not partial parse |
| `catalog_state: missing` | Project appears; info explains audit not run; execution individual graph still loadable |
| Compose failure | Master mode shows `compose_failed` with message; individual mode unaffected |
| Individual `/api/graph` network error | Preserve existing “Graph unavailable” title + footer error (analysis I-17) |

Never crash to a blank white view (AC-9).

### 10.2 Loading

- Initial loads show a non-blocking skeleton/busy state on the stage with
  `aria-busy="true"` until the first successful payload.
- Refresh disables itself during flight (existing behavior).
- Mode switches MAY show a short busy state; previous content SHOULD remain
  visible until replaced to avoid flicker.

### 10.3 Cache

| Cache | Key | Invalidate when |
| --- | --- | --- |
| Projects list | `inventory_hash` | hash changes or manual Refresh |
| Master view | `inventory_hash` + aggregate source fingerprints / `view_hash` | any source fingerprint change (AC-11); Refresh; mode re-entry after invalidation |
| Individual graph | `project_key` + `source_mtime` / `graph_hash` | poll sees change; project switch |

Caches are **never authoritative**: they accelerate UI only. Warm master repeat
targets ≤500 ms client render when the HTTP payload is already warm; cold
composition budgets belong to the CLI (AC-11) and the browser MUST NOT
re-implement composition in JS.

Individual poll continues to bypass HTTP caching via `t=` as today.

---

## 11. Keyboard order, accessible names, and focus

### 11.1 Tab order (desktop individual)

1. Brand home
2. Project switcher
3. Mode toggle
4. Search
5. Status filter controls
6. Relationship filter controls (master; skipped when hidden)
7. Efficiency summary (if present)
8. Pause (if present)
9. Refresh
10. Back / breadcrumb (if visible)
11. Graph nodes **or** list rows in visual/reading order
12. Open milestone / Open target (if visible)
13. Inspector panel tabs
14. Inspector interactive controls

Master inserts relationship filters and may insert diagnostics summary before
the stage. No keyboard traps.

### 11.2 Activation

- Nodes/rows: `role="button"` or listbox option, `tabindex="0"` (or roving
  tabindex within the stage).
- `Enter` and `Space` activate; **Space MUST `preventDefault()`** to avoid page
  scroll (analysis defect I-05).
- Cross-link activation follows §8.

### 11.3 Focus restoration

Full SVG rebuilds on poll MUST restore focus to the previously focused node id
when it still exists; if not, focus moves to the stage container. Selection
identity is independent of focus but SHOULD stay aligned after keyboard
activation.

### 11.4 Visible focus

Every interactive control and node MUST have a `:focus-visible` style with at
least 2px contrasting outline or ring (analysis: `hasCssFocusRule: false` is a
defect; AC-10 requires visible focus).

### 11.5 Accessible names (required examples)

| Control | Accessible name |
| --- | --- |
| Project switcher | `Project switcher` |
| Mode | `View mode` |
| Search | `Search projects, components, and features` |
| Status filter group | `Filter by status` |
| Relationship filter group | `Filter by relationship type` |
| Refresh | `Refresh graph` (existing) |
| Pause | `Pause build` (existing semantics) |
| List/graph toggle | `Show list view` / `Show graph view` |
| Node (individual) | `{id}, {title}, {status phrase}, agent {label?}` |
| Node (master) | `{kind}, {title}, {status}, project {label}` |
| Edge | `{type} from {from} to {to}, {resolution}` |
| Live region | (unnamed) `aria-live="polite"` for matches; `assertive` for hard failures |

SVG root `aria-label` MUST include the live project title or `Master graph`
plus inventory hash short prefix — never stale Mac Runtime product copy.

### 11.6 Live summaries

Announce on settle: mode changes, project changes, debounced search match
counts, filter applications, selection titles, and degraded-state titles.

---

## 12. Contrast expectations

Target theme remains the existing dark board.

| Element | Minimum |
| --- | --- |
| Body / inspector text vs background | WCAG 2.2 AA **4.5:1** |
| Large metrics (≥18pt / 14pt bold) | **3:1** |
| Focus ring vs adjacent background | **3:1** |
| Status text labels | AA; color alone insufficient |
| Dimmed non-match nodes | Still distinguishable as “inactive” but not used as the only status channel; dimmed text that remains readable for ids SHOULD stay ≥3:1 or be removed from the reading path in favor of list filtering |
| Disabled unavailable rows | Visible disabled styling + textual reason |

Automated a11y scans on individual and master states MUST report **zero
critical** violations (analysis A-01/A-02; AC-10).

`prefers-reduced-motion: reduce` continues to disable edge-flow, aura, breathe,
and efficiency pulse animations.

---

## 13. 320-pixel list/detail fallback

### 13.1 Trigger

When the viewport width is **≤560 CSS pixels**, master mode MUST default to
`view=list`. Individual mode SHOULD offer the same list fallback for task
graphs whose SVG stage scrollWidth exceeds 2× client width (onelink45 measured
2920×732 on a 302-wide stage). Users MAY toggle `view=graph` explicitly.

### 13.2 List/detail pattern

```text
┌─────────────────────────┐
│ chrome / search/filters │
├─────────────────────────┤
│ scrollable result list  │  ← primary; no horizontal pan required
├─────────────────────────┤
│ detail / inspector      │  ← selected row; reachable without precision pointer
└─────────────────────────┘
```

Rules:

1. Every searchable node appears as a row with visible title + status text.
2. Rows are keyboard activable; selecting updates the detail region and `sel`.
3. Graph SVG MAY be omitted entirely in list mode to meet the rendering budget.
4. Pause (individual) remains reachable via relocated control (§5.2).
5. Document horizontal overflow outside `#graph-stage` MUST be ≈0 (analysis
   I-14 noted 332 vs 320 — new work MUST NOT regress beyond intentional
   stage scrolling when `view=graph`).
6. `html { min-width: 320px }` may remain; layout MUST be usable at 320×568.

---

## 14. Large-graph rendering budget

Observed constraint: individual boards destroy/recreate all SVG nodes and dual
edge paths every 2000 ms (analysis §5). That model MUST NOT be extended
unbounded to estate-scale master views.

| Budget | Limit |
| --- | --- |
| Master SVG nodes mounted | ≤ **300** at once without clustering/virtualization |
| Master SVG edges mounted | ≤ **400** path elements (count `edge` + `edge-flow` separately toward this cap) |
| List rows mounted | Virtualize when > **200** rows; keep selected + ± overscan |
| Detail panel evidence refs rendered | ≤ **20** (schema cap) before “show more” |
| Master poll / auto-refresh | Default **off**; manual Refresh or ≥10s when enabled |
| Individual 23-node board | First interactive paint < **2 s** local; poll without input jank (Perf-01) |
| Master interaction | Search debounce and filter application ≤ **100 ms** script time on the 39-repo frozen inventory class; large fixtures use aggregation |

When caps would be exceeded, the UI MUST:

1. Prefer list mode, or
2. Cluster by `project_key` (one cluster node per project until expanded), or
3. Show an explicit “graph truncated” diagnostic with counts —

never silently drop diagnostics or unresolved edges from the **data model**
(filters may hide them from the canvas).

Edge-flow animations SHOULD be omitted in master mode even when motion is
allowed, to halve edge DOM cost.

---

## 15. Exact browser acceptance scenarios

Run against the Rust board with a frozen inventory (synthetic fixture under
`artifacts/verification/browser-fixture` for CI; real inventory optional). Do
**not** call Python task-action endpoints. Do **not** mutate graph nodes for
read-only cases. Do **not** invoke pause unless the scenario says so.

Mark each scenario pass/fail with screenshot or CDP evidence. Map to PRD:
**AC-8** (API), **AC-9** (interactions), **AC-10** (a11y / 320 list).

### 15.1 API (AC-8)

| ID | Request | Expected |
| --- | --- | --- |
| API-01 | `GET /api/projects` | 200, `fractal.board_projects.v1`, inventory members only |
| API-02 | `GET /api/graph` | 200, `fractal.execution_graph_view.v1`, totals match live assignments |
| API-03 | `GET /api/graph?project=<valid key>` | 200 for inventory member |
| API-04 | `GET /api/graph?project=../etc/passwd` and non-inventory key | 404/403; no file contents |
| API-05 | `GET /api/master-graph` | 200, `fractal.master_graph_view.v1`, schema-valid |
| API-06 | `POST /api/master-graph` (and `/api/projects`, `/api/graph`) | 405; source hashes unchanged |
| API-07 | `POST /api/run/pause` route shape | Unchanged token header requirement; **not invoked** in default acceptance |
| API-08 | Static allowlist + unknown path | 200 for assets; 404 JSON otherwise |

### 15.2 Individual board regressions (preserve progress / pause affordance)

| ID | Viewport | Interaction | Expected |
| --- | --- | --- | --- |
| I-01 | 1440×900 | Cold load | Metrics match `/api/graph` totals; footer shows `.fractal/project.fractal` |
| I-02 | 1440×900 | Single-group project | Task nodes visible; kicker/title/back/document title reflect live project (no Mac Runtime stale chrome) |
| I-03 | 1440×900 | Click complete/active/incomplete | Inspector status, kind, instruction, assignment as present |
| I-04 | 1440×900 | Tab to node, Enter | Inspector populates; `.selected` present |
| I-05 | 1440×900 | Space on focused node | Selects **and** does not scroll page |
| I-06 | 1440×900 | Tab order | Matches §11.1 |
| I-07 | 1440×900 | Focus ring | Perceptible on controls and nodes |
| I-08 | 1440×900 | Refresh | Disables during fetch; metrics update |
| I-09 | 1440×900 | ≥2 poll cycles | Totals may change; selection identity preserved; focus restored per §11.3 |
| I-10 | 1440×900 | Efficiency open | Adjusted/realized/episodes when present |
| I-11 | 1440×900 | Pause control visible when `run_control.available` | Confirm dialog path remains; **cancel** leaves graph unchanged; do not confirm in default suite |
| I-12 | 900×800 | Resize | Stacked workspace; legend may hide; inspector under graph |
| I-13 | 375×812 | Load | Metrics usable; pause relocated not deleted; inspector reachable |
| I-14 | 320×568 | Load | Usable min width; no harmful page overflow |
| I-15 | any | `prefers-reduced-motion` | Animations off |
| I-16 | any | `POST /api/graph` | 405 |
| I-17 | any | Stopped board | “Graph unavailable” + error footer; no blank crash |

### 15.3 Project switcher (AC-9)

| ID | Viewport | Interaction | Expected |
| --- | --- | --- | --- |
| P-01 | 1440 | Open switcher | Lists inventory labels; current indicated |
| P-02 | 1440 | Choose another available project | Individual graph replaces; URL `project` updates; prior project file untouched |
| P-03 | 1440 | Choose unavailable | Degraded panel + reason; no fabricated nodes |
| P-04 | 320 | Switcher | Keyboard operable; names visible |

### 15.4 Master / cross-graph (AC-9)

| ID | Viewport | Interaction | Expected |
| --- | --- | --- | --- |
| M-01 | 1440 | Toggle Master | Renders master view; namespaces visible; Pause hidden |
| M-02 | 1440 | Search feature/component | Matches highlighted/listed; empty query resets |
| M-03 | 1440 | Filter status + relationship | Non-matches dimmed/hidden; clear restores |
| M-04 | 1440 | Activate resolved cross-graph edge | Individual (or focused target) + breadcrumb back to master |
| M-05 | 1440 | Open evidence/tests/decisions | Repo-relative refs + hashes + conservative status; no secrets |
| M-06 | 1440 | Invalid/unavailable catalogs | Diagnostics visible; composition UI remains interactive |
| M-07 | 320 | Master fallback | List/detail works without SVG pan / precise pointer |
| M-08 | 1440 | Keyboard-only | M-01–M-05 reachable with visible focus |
| M-09 | any | Mutation on master routes | 405; source hashes unchanged |
| M-10 | any | Path outside inventory | 404/403; no filesystem escape |
| M-11 | 1440 | URL restore | Copy URL → new session restores mode/project/sel/q/filters/panel |
| M-12 | 1440 | Unresolved edge activate | Inspector explains; no navigation |

### 15.5 Accessibility and performance gates (AC-10 / budgets)

| ID | Check | Pass bar |
| --- | --- | --- |
| A-01 | Automated a11y scan (individual) | Zero critical violations |
| A-02 | Automated a11y scan (master) | Zero critical violations |
| A-03 | Contrast sampling | Text/status/focus meet §12 |
| A-04 | 320 list/detail | Full keyboard path for search → row → evidence panel |
| Perf-01 | Individual 23-node | Interactive < 2 s local; poll without input jank |
| Perf-02 | Master large fixture | Obeys §14 caps; no unbounded 2 s full-SVG recreate for 500+ nodes |

### 15.6 Fixture matrix (for `run_browser_acceptance`)

The verification fixture MUST include at least: one valid catalog project, one
invalid catalog, one unavailable workspace, one cross-linked pair, one cycle
group, and one unresolved link — without copying credentials or absolute
machine evidence paths into the fixture tree.

---

## 16. Implementation boundaries

| Wave | Owns | MUST NOT touch |
| --- | --- | --- |
| This contract | `docs/master-graph-browser-contract.md` only | implementation files |
| Modular behaviors | `execution-graph/master-graph.js`, `master-graph.css` | existing `app.js` until integrate wave; no mutation requests |
| Integrate wave | wires switcher/mode into `index.html` / `app.js` / `styles.css` | pause semantics, Python legacy servers, controller JSON |
| Board API wave | `src/board.rs` GET routes | catalog writes, `graph_hash` changes |
| Acceptance wave | fixtures + automation evidence | confirming pause; weakening assertions |

Downstream implementers MUST treat AC-8–AC-10 scenarios in §15 as the
definition of done for browser work. Execution progress polling and pause
affordance preservation in §5.2 are release blockers equal to new master
features.

---

## 17. Traceability

| Requirement | Section |
| --- | --- |
| Information architecture | §2 |
| API-to-view mapping | §3 |
| Project switcher | §4 |
| Individual/master modes | §5 |
| Query and URL state | §6 |
| Search and filters | §7 |
| Cross-link traversal | §8 |
| Evidence/test/decision panels | §9 |
| Invalid/unavailable/loading/cache | §10 |
| Keyboard / names / focus | §11 |
| Contrast | §12 |
| 320 list/detail | §13 |
| Large-graph budget | §14 |
| Acceptance scenarios | §15 |
| Preserve execution progress + pause | §5.2, I-09, I-11, API-07 |
| AC-8 / AC-9 / AC-10 | §15.1–§15.5 |
