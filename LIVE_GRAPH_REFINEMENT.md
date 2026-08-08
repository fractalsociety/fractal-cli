# Live execution graph refinement contract

This note is the bounded hand-off from the graph audit to the implementation
worker. It describes the smallest additive contract that can make an active
run explain **what is happening, who is doing it, and why it is eligible**
without changing the canonical graph or the master graph.

## Findings from the current system

`.fractal/project.fractal` is the authority. The immutable `graph` already
contains the planned facts needed for explanation:

- `nodes[].id`, `title`, `objective`, `instruction`, `capability`, `kind`,
  `node_type`, `memory_scopes`, `depends_on`, `execution` (wave, task number,
  sequential/parallel mode), `budget`, `policy_contract`, and `efficiency`.
- `edges[]` contains `from`, `to`, and `condition` (`success` or `failure`).
- `execution` contains the run `phase`, optional planning `progress`, and an
  assignment map keyed by node id. Each assignment has `agent_id`,
  `agent_label`, `state` (`checked_out`, `completed`, or `released`), and
  lifecycle timestamps.
- `learning.nodes[id]` contains runtime evidence that is deliberately outside
  the immutable graph: `created_at`, `ready_at`, `started_at`, `finished_at`,
  `executor` (agent/model/version), `attempt_count`, `outcome`,
  `failure_code`, `verification` (`type`, `passed`, `evidence_refs`),
  `artifacts_produced`, `consumed_by`, `human_intervention`, estimated/actual
  cost, notes, and reopen count. `learning.graph_edits` and `learning.outcome`
  provide graph-level history and terminal metrics.
- `/api/failure-graph` separately exposes bounded failure observations,
  evidence hashes, resolutions, lessons, and causal edges.

The current `/api/graph` projection exposes assignment and execution metadata,
but omits learning node records and execution progress. Its `gate` field reads
`node.verification_plan`; compiled nodes normally store the verification plan
at `node.efficiency.verification_plan`, so the current inspector often shows a
blank gate even though the plan is present. This is a projection bug, not a
missing canonical fact.

The browser already polls `/api/graph` every two seconds. The renderer is
offline-capable and deterministic; it must continue to work when no WebGL,
network, learning record, or failure graph is available.

## Additive `/api/graph` projection

Keep `schema: fractal.execution_graph_view.v1`, existing keys, ETag behavior,
master mode, and SVG fallback unchanged. Add only these bounded fields:

```json
{
  "execution": {
    "phase": "planning|executing|halted|completed",
    "updated_at": "RFC3339",
    "progress": {
      "message": "bounded planning message",
      "step": 2,
      "elapsed_seconds": 30,
      "agent_label": "Luna · Planner",
      "source": "planner",
      "updated_at": "RFC3339"
    }
  },
  "groups": [{
    "tasks": [{
      "id": "implement",
      "objective": "bounded objective",
      "capability": "code.generate",
      "depends_on": ["analyze"],
      "why": {
        "ready": true,
        "blocked_by": [],
        "reason": "Dependency analyze completed; wave 2 is eligible."
      },
      "evidence": {
        "started_at": "RFC3339",
        "finished_at": null,
        "attempt_count": 1,
        "outcome": null,
        "failure_code": null,
        "verification": {"type": "automated", "passed": null, "evidence_refs": []},
        "artifacts_produced": [],
        "consumed_by": [],
        "executor": {"agent": "Luna · Worker", "model": "", "version": ""},
        "human_intervention": false,
        "reopen_count": 0
      }
    }]
  }]
}
```

Projection rules:

1. `execution` is a compact, read-only copy of phase/progress; do not expose
   arbitrary `execution.extra` values.
2. `objective`, `capability`, and `depends_on` come from the immutable graph.
   `instruction` and `gate` remain as-is, except `gate` falls back to
   `node.efficiency.verification_plan` when `node.verification_plan` is absent.
3. `evidence` is a safe allowlist copied from `learning.nodes[id]`. Include
   identifiers/timestamps and evidence references, but never logs, workspace
   paths, credentials, or flattened unknown fields. The separate failure API
   remains the source for full failure history.
4. `why.ready` is true only when all non-`failure` incoming dependencies have a
   `completed` assignment. Otherwise `why.blocked_by` lists stable predecessor
   ids and `why.reason` is a short deterministic sentence. An assigned node is
   `active` only for `checked_out`; released nodes remain incomplete but carry
   their evidence/outcome.
5. Missing learning records normalize to `null`/empty values, not an API error.
   Existing clients can ignore all additive keys.

The projection should be implemented as a pure helper near `project_view` so
it can be unit-tested without a running server. Keep the response bounded to
the existing graph node count and the existing safe field limits.

## Browser normalization and animation contract

`execution-graph/three-graph.js` should normalize the additive fields without
changing `normalizeGraphPayload`'s existing return shape. Each normalized node
may carry `objective`, `capability`, `depends_on`, `why`, and `evidence`; the
model may carry `execution` and a monotonically comparable `updated_at`.

`app.js` owns snapshot comparison because it already polls. On every changed
snapshot it should:

- identify transitions (`incomplete → active`, `active → complete/released`,
  assignment/agent changes, and new evidence); and
- pass a bounded `transitions` list plus the current `execution` summary to the
  Three.js controller.

The 3D scene should communicate those transitions, not simulate progress that
the server did not report:

- active node: a restrained pulse/halo, agent badge, and a moving luminous
  edge only along its dependency path;
- newly completed node: one short completion sweep, then settle to the normal
  complete material;
- released/failed or failed-verification evidence: red/amber edge pulse and a
  visible evidence marker; do not hide the node;
- blocked node: dim dependency edge and a compact `blocked by A, B` label;
- active run: a small live-work HUD listing phase, active agent(s), selected
  objective, and the one-line `why.reason`.

Animation requirements: deterministic, bounded (no unbounded timers or DOM
nodes), no network or CDN, and disabled/settled when either
`prefers-reduced-motion: reduce` or the controller's reduced-motion switch is
active. Master mode must not instantiate this controller.

## Inspector and accessibility contract

The existing inspector remains the canonical detail surface. Add compact
sections for `OBJECTIVE / WHY`, `DEPENDENCIES`, `EVIDENCE`, and `LAST EVENT`.
Use text from the normalized safe fields; link evidence by stable reference only.
Do not make color, pulse, depth, or motion the sole status signal.

The accessible node list must include the same objective, readiness reason,
agent, outcome, and verification summary as the visual node. Live transition
announcements go through one `aria-live="polite"` region and are debounced so a
two-second poll cannot flood assistive technology. Hidden labels remain
`aria-hidden`; the list is complete and keyboard navigable even when labels are
occluded or WebGL fails.

## Bounded file ownership and verification

Implementation should touch at most these six files:

1. `src/board.rs` — safe projection helper and API tests.
2. `execution-graph/app.js` — snapshot diff/transition hand-off and inspector
   rendering.
3. `execution-graph/three-graph.js` — normalized live state and bounded scene
   animations.
4. `execution-graph/index.html` — live HUD, detail sections, and one polite
   announcement region.
5. `execution-graph/styles.css` — HUD/detail styling and reduced-motion rules.
6. `execution-graph/tests/test_three_graph.py` — normalization, transitions,
   fallback, accessibility, and reduced-motion tests.

Do not change `master-graph.js`, graph hashing, `project_file.rs`, the failure
graph schema, or the offline vendor asset. Add Rust unit coverage for:

- gate fallback from `efficiency.verification_plan`;
- projection of phase/progress, readiness blockers, assignments, and safe
  learning evidence; and
- omission of unknown/secret flattened fields.

Add browser/controller coverage for deterministic transition classification,
active/completed/released animation flags, missing-evidence normalization,
reduced motion, list parity, and WebGL/SVG fallback. Run the existing Python
suite, `cargo test board`, `git diff --check`, and a desktop plus narrow browser
smoke test against a live 8094 board.

## Non-goals

- no websocket or new server process;
- no guessed token/cost/percent progress for an active worker;
- no full learning/failure payload in the visual graph;
- no changes to master graph composition or canonical graph hashes;
- no motion when reduced motion is requested;
- no dependency on an online Three.js/CDN asset.
