# Fractal Execution Graph Frontend

This directory contains the static frontend served by the Rust CLI:

```sh
fractal graph board GRAPH_HASH
```

The frontend reads the Rust `/api/graph` projection of
`.fractal/project.fractal`. It does not parse PRD markdown and it does not own
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
