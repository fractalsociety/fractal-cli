//! Optional Coordinate backend. The default executor is in-process and turnkey.
//! When the Coordinate backend is selected, the committed graph is reconciled
//! into the **real** Coordinate durable pull-queue (via the tested
//! `squad graph-supervisor` path) before it runs — so the run is recorded in
//! Coordinate's SQLite store, survives restarts, and external
//! `squad host-bridge` workers can lease and consume the very same nodes across
//! processes or machines. Execution is then driven to completion by the local
//! host so the experience stays turnkey; the receipts, governed evolution, and
//! DataEvol export wrap around it identically to the in-process backend.
//!
//! This fails closed: if `squad` is not runnable, the backend refuses rather than
//! silently degrading to in-process, so `--coordinate` always means Coordinate.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::{execute, run};

/// Reconcile the committed graph into the durable Coordinate queue, then drive it
/// to completion. `graph_hash` must be the hash of the *current* committed graph
/// (the child hash after an evolution step).
pub(crate) fn run_via_coordinate(
    graph_hash: &str,
    graph: &Value,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
) -> Result<execute::RunOutcome> {
    // Resolve the real Coordinate reconcile invocation for the committed graph.
    let plan = run::build_run_plan(graph_hash, None, None, false)
        .context("preparing the Coordinate reconcile")?;
    let db = plan
        .args
        .windows(2)
        .find(|pair| pair[0] == "--db")
        .and_then(|pair| pair[1].to_str())
        .unwrap_or("")
        .to_owned();

    println!("  ⛓  Coordinate backend: reconciling into the durable queue…");
    let output = Command::new(&plan.program)
        .args(&plan.args)
        .output()
        .with_context(|| {
            format!(
                "Coordinate backend needs `squad` on PATH (or $SQUAD_BIN / --squad-bin), and \
                 `squad serve` running. Install/start Coordinate, or use the default in-process \
                 backend. (tried `{}`)",
                plan.program.to_string_lossy()
            )
        })?;
    if !output.status.success() {
        bail!(
            "Coordinate reconcile failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let last = String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_owned();
    println!(
        "  ⛓  reconciled into Coordinate durable queue (db {}){}",
        if db.is_empty() { "default" } else { &db },
        if last.is_empty() {
            String::new()
        } else {
            format!(" — {last}")
        }
    );
    println!(
        "  ⛓  nodes are leasable by `squad host-bridge` workers; driving to completion locally…"
    );

    // Drive execution to completion. External Coordinate worker bridges could
    // consume the same durable queue; the local host executes so the run is
    // turnkey either way.
    execute::run_multi_agent(graph, workspace, agents, board)
}
