# INT-165 Rust workflow checkpoint

This isolated checkpoint is based on `10c0f68002f5473352c7fbc7481887baca061d06` (`INT-165: add durable node workflow recovery`) on branch `codex/int162-rust-checkpoint`. It carries the bounded Rust INT-165 materialization, durable-effect recovery, and network-pin changes without copying the unrelated dirty project-harness, model-selection, and run-control edits from the main `fractal-cli` worktree.

The checkpoint changes five Rust source files:

- `src/node_intelligence.rs` adds the task materialization and host-bound validation path, persisted effect intents/snapshots and exact recovery, reviewed-attempt effect checks, and owner-selected network resolution with a durable per-attempt pin and fresh policy rechecks. The final resolver checks distinguish resolver-config schema from resolution-receipt schema, bind recognized managed measurement operations to the canonical graph node, and validate inherited network/capability/model pins against completed producer receipts and content-addressed contracts.
- `src/execute.rs` routes supported deterministic `analysis`, `intake`, and `check` operations through the adapter; commits the canonical graph outcome before resolving an effect intent; fails closed when canonical outcome persistence fails; and contains a test-only crash point used to exercise effect recovery.
- `src/node_intelligence_workflow_tests.rs` exercises real child-process/coordinator recovery, review admission/resume, three-node materialization and execution, changed-unit rejection followed by repaired-source resume, and includes the nested network integration tests. The parent test module was already registered by `src/main.rs` at the base commit, so no `main.rs` edit was required.
- `src/node_intelligence_network_workflow_tests.rs` exercises owner-selected network resolution through all three actual deterministic workflow actions, verifies producer-output handoffs inherit the selected network, and checks that owner-policy revocation blocks before any action.
- `src/node_intelligence_learning_workflow_tests.rs` adds an ignored one-run cohort-capture harness. It constructs three canonical project graphs via `project_file::persist`, imports frozen source-only rows through the paired fixture, and runs the existing Rust coordinator child. Its training graph adds a dependency barrier so the two frozen four-event pages stay ordered. A normal in-memory test checks that barrier; the ignored cohort checks canonical completion, durable action rows, feedback bindings, and provider/model-call absence before writing a ref-only cohort report.

The network integration fixture is `runs/jev-network/network_workflow_fixture.py` in the paired FractalMaster checkout. It derives its records from the existing three-node fixture and writes only temporary project and owner-policy stores. The fixture and tests make no model or provider calls.

Verification used `FRACTAL_OFFLINE=1` and the isolated checkpoint worktree:

- `cargo test --bin fractal node_intelligence_workflow_tests::node_intelligence_network_workflow_tests -- --nocapture`: 2 passed.
- `cargo test --bin fractal training_graph_has_frozen_batch_barrier -- --nocapture`: 1 passed.
- `cargo check --bin fractal`: passed; one pre-existing dead-code warning for `run_agent_prompt` remains.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

The same final Rust source in the main worktree also passed
`FRACTAL_OFFLINE=1 cargo test --bin fractal node_intelligence_workflow_tests:: -- --nocapture` (6 passed, 2 ignored), including coordinator crash recovery, review resume, unit-repair resume, and the resolver cases.

No saved model/head diagnostic, provider call, or graph transition was run. The Rust tests use temporary synthetic projects and bounded fake child processes. The ignored saved-head diagnostic still needs an explicitly configured bounded run if separately authorized.

The ignored managed-learning cohort capture has **not passed acceptance**. Its
first opt-in preflight stopped before any task action because the paired Python
fixture still expected 32 graph edges while the frozen two-batch barrier
correctly supplies 40. The failed v1 artifact is preserved in FractalMaster;
the fixture owner is correcting the validator before a separately reserved
v2 capture. This checkpoint contains the planned capture harness, not evidence
that the complete cohort or subsequent model/controller lifecycle ran.
