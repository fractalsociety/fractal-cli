# Fractal CLI

Fractal CLI turns a natural-language or voice request into a structured PRD,
dependency-aware execution graph, and coordinated multi-agent build.

## Repository layout

- `src/` — the `fractal` command-line application.
- `crates/fractal-chain/` — signed execution receipts and graph lineage.
- `execution-graph/` — the local live graph viewer.
- `scripts/` — local routing and DataEvol adapters.

## Development setup

The CLI currently consumes runtime contracts and graph evolution from the
private `fractalsociety/FractalRuntime` repository. Keep both repositories as
siblings:

```text
~/fractal-cli
~/FractalRuntime
```

Then build and test:

```sh
cd ~/fractal-cli
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
cargo build --release
install -m 755 target/release/fractal ~/.cargo/bin/fractal
```

Run `fractal version` to verify the installed binary.

## Fractal Society

Authenticated projects publish their portable `.fractal/project.fractal`
execution state to [fractalsociety.com](https://fractalsociety.com). Planner
heartbeats, dependency waves, agent checkout, and completion state are reflected
in the live online graph.
