# Failure Graph Contract — `fractal.failure_graph.v1`

The failure graph is an additive, reference-only memory of execution failures,
retries, resolutions, and reusable lessons. It is stored as the optional
top-level `failure_graph` member of a `fractal.project.v1` document. It is
never nested in `graph`, and therefore adding or updating a failure record does
not change the immutable execution `graph_hash`.

## Envelope and deterministic storage

The envelope has this shape:

```json
{
  "schema": "fractal.failure_graph.v1",
  "failures": {"failure:build:tool_failure": {"...": "..."}},
  "lessons": {"lesson:use-narrow-checks": {"...": "..."}},
  "edges": {"edge:resolved_by:f:lesson": {"...": "..."}},
  "cross_project_links": {},
  "failure_graph_hash": "sha256:..."
}
```

All collections are `BTreeMap`s keyed by stable IDs. A map key must equal the
record's `id` field. `failure_id(node_id, failure_code)` groups retries for a
node/failure-code family; each retry is appended to `observations` with its
attempt number. `lesson_id(summary, capability, component)` and
`edge_id(type, from, to)` are deterministic helpers for producers. Unknown
fields are flattened into each typed record and are retained on every
read/write.

`failure_graph_hash` is SHA-256 over canonical JSON after removing the
`failure_graph_hash` member and unstable timestamp members (`timestamp`,
`generated_at`, `observed_at`, `created_at`, `resolved_at`, `superseded_at`,
and `*_timestamp`). This makes retries and hand-edited future metadata
deterministic without smuggling wall-clock values into the identity hash.

## Records and invariants

`FailureRecord` includes `node_id`, positive `attempt`, `failure_code`,
`outcome`, a short sanitized `summary`, optional `capability`, `component`, a
repo-relative `source_ref`, compact evidence, `agent`/`model`/`version`,
observed graph/Git provenance, append-only retry observations, and optional
resolution or supersession. `state` is exactly `unresolved`, `resolved`, or
`superseded`:

- unresolved records have no resolution or supersession;
- resolved records require `resolution.success: true` and at least one
  evidence reference;
- superseded records require `superseded_by`, which names an existing failure.

`FailureObservation` repeats the outcome, summary, attempt, evidence,
executor, and observed provenance for one occurrence. Existing observations are
never discarded or reordered when `append_failure` receives a retry.

`LessonRecord` has a summary, `proposed`/`adopted`/`superseded`/`rejected`
status, optional capability/component/source reference, compact evidence,
executor, provenance, and (for superseded lessons) `superseded_by`.

`EdgeRecord` is typed and uses exactly one of `caused_by`, `resolved_by`,
`lesson_from`, `applies_to`, `related_component`, `supersedes`, `reused_in`,
`contradicts`, or `retry_of`. Ordinary edges must reference existing failure
or lesson IDs. `applies_to`, `related_component`, and `reused_in` may target a
bounded external capability/component/project identifier. A `retry_of` edge
must connect two failure IDs. `CrossProjectLink` names one local failure or
lesson and a compact project key; it never embeds a prompt, path, log, or
remote document.

Evidence is either `{ "sha256": "..." }` or `{ "legacy_ref": "..." }`, not
both. Evidence arrays contain at most 20 items per record. Source references
are repository-relative and reject absolute paths, URLs, backslashes, and
`..` traversal. Summaries collapse whitespace and reject control characters,
prompt/transcript markers, and log-shaped payloads. Secret-shaped keys are
rejected recursively, including in preserved unknown fields.

The bounds are 512 failures, 512 lessons, 2,048 edges, 512 cross-project
links, at most 80 retry observations per failure, and a serialized envelope no
larger than 256 KiB. Producers must reject a bound violation; truncation is
never silent.

## Guarded project-file APIs

Runtime and UI workers use typed `project_file` seams only:

```text
load_failure_graph(workspace) -> FailureGraph
failure_graph(document) -> FailureGraph
append_failure(workspace, FailureRecord) -> failure_id
resolve_failure(workspace, failure_id, FailureResolution)
supersede_failure(workspace, failure_id, replacement_failure_id)
upsert_lesson(workspace, LessonRecord) -> lesson_id
add_failure_edge(workspace, EdgeRecord) -> edge_id
replace_failure_graph(workspace, FailureGraph)
```

Every write acquires the existing process and lock-file guards, loads the
current project, validates the typed graph, and uses the existing atomic
rename. It changes only `failure_graph` and the project-level `updated_at`.
Identity, graph bytes/hash, execution, learning, efficiency, catalog, and all
unknown sibling fields are retained. No arbitrary JSON mutation API is
provided.

For a legacy project with learning failures but no `failure_graph` key,
`load_failure_graph` and `failure_graph` return a pure in-memory projection.
Reading does not rewrite the project. The first guarded append/upsert starts
from that projection so historical failures are not silently lost.
