# Canonical Fractal Execution Graph

This is the current and only supported Fractal graph frontend. It is the
`fractal-graph-ui.v1` shared renderer backed by the Rust
`fractal.graph_snapshot.v1` API. Do not add a parallel renderer or restore the
retired standalone Three.js implementation.

This directory contains the static frontend served by the Rust CLI:

```sh
fractal graph board GRAPH_HASH
```

That command is manual mode: it serves the graph for inspection and task
selection without starting a coordinator, launching agents, or checking out a
node. Execution begins only through an explicit Rust CLI transition.

The visible individual-project frontend is the generated browser bundle from
Fractal Society's `@fractalsociety/graph-ui` package. Its pinned source commit,
build command, and asset hashes live in `fractal-graph-ui.manifest.json`; do not
hand-edit the vendored JavaScript or CSS. Rebuild the package in the Society
repository, copy its generated browser assets here, and update the manifest.

The shared renderer reads the typed Rust `/api/snapshot` projection of
`.fractal/project.fractal`:

```text
fractal.graph_snapshot.v1
  graph
  execution
  learning
  efficiency
  intelligence (optional)
```

It exposes the same seven read-only lenses locally and on Fractal Society.
`POST /api/intelligence/query` accepts the bounded
`fractal.intelligence.query.v1` contract. Presentation is not authorization:
the endpoint is read-only, rejects unknown fields and unsupported lenses, and
enforces request, traversal, node, and edge limits in Rust.

The compatibility `/api/graph` projection remains available to CLI status and
master-estate consumers. Neither projection parses PRD markdown or owns
assignments or status. Checkout, completion, release, dependency checks, and
status are Rust operations:

```sh
fractal node NODE --show --repo PROJECT
fractal node NODE --checkout --repo PROJECT
fractal node NODE --complete --repo PROJECT
fractal node NODE --release --repo PROJECT
```

`server.py`, `task-state.py`, PRD regex parsing, `MILESTONE_DEPS`,
`graph-state*.json`, and Python task-action endpoints are frozen compatibility
code for the retired Mac Runtime board. They refuse to run unless the operator
passes `--legacy-mac-runtime`. Do not use that mode for new projects.

Import an active legacy state once with:

```sh
fractal graph import-legacy --state execution-graph/graph-state.json --repo PROJECT
```

When the project already has a compiled portable graph, the importer maps
matching assignments onto it. A state-only import cannot recover trustworthy
dependencies, so it creates a halted historical graph and releases active
claims for safe replanning.
