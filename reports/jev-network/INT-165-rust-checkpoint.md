# INT-165 Rust workflow checkpoint

This isolated checkpoint is based on `3f785c71cfa1e325a10046f2b1e24b0cb9297a57` (`INT-162: wire node intelligence through managed runtime`) on branch `codex/int162-rust-checkpoint`. It carries the bounded Rust INT-165 materialization, durable-effect recovery, and network-pin changes without copying the unrelated dirty project-harness, model-selection, and run-control edits from the main `fractal-cli` worktree.

The checkpoint changes four Rust source files:

- `src/node_intelligence.rs` adds the task materialization and host-bound validation path, persisted effect intents/snapshots and exact recovery, reviewed-attempt effect checks, and owner-selected network resolution with a durable per-attempt pin and fresh policy rechecks.
- `src/execute.rs` routes supported deterministic `analysis`, `intake`, and `check` operations through the adapter; commits the canonical graph outcome before resolving an effect intent; fails closed when canonical outcome persistence fails; and contains a test-only crash point used to exercise effect recovery.
- `src/main.rs` registers the workflow integration tests.
- `src/node_intelligence_workflow_tests.rs` exercises real child-process/coordinator recovery, review admission/resume, three-node materialization and execution, and changed-unit rejection followed by repaired-source resume. Two explicitly ignored tests are a helper entry point and a saved-head diagnostic; neither was run here.

Verification used `FRACTAL_OFFLINE=1` and a separate Cargo target directory under this isolated checkpoint:

- `cargo test --bin fractal node_intelligence::tests -- --nocapture`: 11 passed.
- `cargo test --bin fractal node_intelligence_workflow_tests:: -- --nocapture`: 4 passed, 2 ignored.
- `cargo check --bin fractal`: passed; one pre-existing dead-code warning for `run_agent_prompt` remains.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

No saved model/head diagnostic, provider call, or graph transition was run. The Rust tests use temporary synthetic projects and bounded fake child processes. The ignored saved-head diagnostic still needs an explicitly configured bounded run if separately authorized.
