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

## Frozen cohort harness follow-up

The capture helper now invokes its own nested ignored coordinator test by exact
test name, passes all three per-split policy paths to the fixture, and uses
unique canonical workspace slugs (`jev-managed-learning-{split}-v1`). Its
preflight persists all three split projects through the Rust project API and
calls the real Python fixture without running a task action. It checks the
namespaced project IDs, graph/config pin round-trip, distinct owner-policy
paths, private policy modes, and absence of an action database.

The capture audit now reads each action's parsed nonempty output-ref array,
checks the three expected adapter roles, counts unique output artifacts, and
reports checker outcomes from `verified_outcome`. The frozen all-positive
cohort expects 48 completed tasks/actions, 80 distinct output artifacts, 16
passed checks, zero failed or unknown checks, 16 matching feedback events, and
zero provider/model calls.

Focused checkpoint verification:

- `cargo test --bin fractal node_intelligence_workflow_tests -- --nocapture`:
  9 passed, 4 ignored (79.48s). This includes the three ordinary cohort
  barrier/project-ID/fixture-preflight checks and deterministic scheduler,
  resume, unit-rejection, and resolver tests. The ignored capture and saved-head
  test were not run.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

The preflight failures from earlier opt-in attempts remain preserved in
FractalMaster. The root operator started a separately frozen v4 capture after
these harness changes; this isolated checkpoint does not claim its result or
the complete data/training/controller acceptance.
