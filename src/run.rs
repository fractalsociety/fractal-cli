//! `fractal run --graph <hash>` — hand a committed execution graph to the
//! Coordinate supervisor.
//!
//! This wires the front door to Coordinate's `squad graph-supervisor`, which
//! reconciles the graph's *ready* nodes into the pull-queue (nodes whose
//! dependencies are satisfied become available for leased workers). Reconciling
//! only enqueues work — it does not itself spawn any worker — so this path is
//! side-effect-light; the actual (paid) node execution is performed by separate
//! Coordinate worker bridges pulling leased nodes. `--watch` keeps reconciling
//! until every node completes; `--dry-run` prints the invocation without running
//! it.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::graph_store;

/// A fully-resolved Coordinate `graph-supervisor` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunPlan {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl RunPlan {
    /// A shell-ish rendering for display (not for re-parsing).
    pub fn rendered(&self) -> String {
        let mut parts = vec![self.program.to_string_lossy().into_owned()];
        parts.extend(self.args.iter().map(|arg| arg.to_string_lossy().into_owned()));
        parts.join(" ")
    }
}

fn fractal_home_root() -> PathBuf {
    match env::var_os("FRACTAL_HOME") {
        Some(home) => PathBuf::from(home),
        None => match env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".fractal"),
            None => PathBuf::from(".fractal"),
        },
    }
}

fn resolve_squad_bin(explicit: Option<&Path>) -> OsString {
    if let Some(path) = explicit {
        return path.as_os_str().to_owned();
    }
    match env::var_os("SQUAD_BIN") {
        Some(value) if !value.is_empty() => value,
        _ => OsString::from("squad"),
    }
}

/// Build the Coordinate invocation for a committed graph, failing closed if the
/// graph is not present / not hash-valid in the store.
pub(crate) fn build_run_plan(
    graph_hash: &str,
    db: Option<&Path>,
    squad_bin: Option<&Path>,
    watch: bool,
) -> Result<RunPlan> {
    // Re-verify the committed graph (hash-checked read) before handing it off.
    graph_store::load_graph(graph_hash)
        .with_context(|| format!("no verifiable committed graph for {graph_hash}"))?;
    let graph_file = graph_store::graph_path(graph_hash);
    if !graph_file.is_file() {
        bail!(
            "committed execution graph file is missing: {}",
            graph_file.display()
        );
    }

    let db_path = db
        .map(Path::to_path_buf)
        .unwrap_or_else(|| fractal_home_root().join("coordinate.sqlite3"));

    let mut args: Vec<OsString> = vec![
        OsString::from("graph-supervisor"),
        OsString::from("--graph"),
        graph_file.into_os_string(),
        OsString::from("--db"),
        db_path.into_os_string(),
    ];
    if watch {
        args.push(OsString::from("--watch"));
    }

    Ok(RunPlan {
        program: resolve_squad_bin(squad_bin),
        args,
    })
}

/// Resolve and (unless `dry_run`) execute the Coordinate graph-supervisor for a
/// committed graph.
pub(crate) fn run_graph(
    graph_hash: &str,
    db: Option<&Path>,
    squad_bin: Option<&Path>,
    watch: bool,
    dry_run: bool,
) -> Result<()> {
    let plan = build_run_plan(graph_hash, db, squad_bin, watch)?;
    if dry_run {
        println!("Would run: {}", plan.rendered());
        println!(
            "(reconciles ready nodes into the Coordinate pull-queue; leased worker bridges execute them)"
        );
        return Ok(());
    }
    println!("Running: {}", plan.rendered());
    let status = Command::new(&plan.program)
        .args(&plan.args)
        .status()
        .with_context(|| {
            format!(
                "failed to launch Coordinate supervisor `{}` (set --squad-bin or $SQUAD_BIN)",
                plan.program.to_string_lossy()
            )
        })?;
    if !status.success() {
        bail!("Coordinate graph-supervisor exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn commit_sample_graph() -> String {
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_run_test",
            "nodes": [{"id": "only", "kind": "control", "capability": "control.complete"}],
            "edges": []
        });
        // The store verifies a self-consistent `graph_hash`, so stamp the
        // canonical hash of the graph (computed without the field) first.
        let hash = fractal_contracts::canonical_sha256(&graph).expect("canonical hash");
        graph["graph_hash"] = json!(hash);
        crate::graph_store::commit_graph(&graph)
            .expect("commit sample graph")
            .graph_hash
    }

    #[test]
    fn plan_targets_graph_supervisor_with_committed_file() {
        let _lock = crate::graph_store::ENV_LOCK.lock().expect("env lock");
        let _home = crate::graph_store::TestHome::new("run-plan").expect("home");
        let hash = commit_sample_graph();
        let plan = build_run_plan(&hash, None, None, false).expect("plan");

        assert_eq!(plan.program, OsString::from("squad"));
        let rendered = plan.rendered();
        assert!(rendered.contains("graph-supervisor"), "{rendered}");
        assert!(rendered.contains("--graph"), "{rendered}");
        assert!(rendered.contains(&hash.trim_start_matches("sha256:").to_string()));
        assert!(rendered.contains("--db"));
        assert!(rendered.contains("coordinate.sqlite3"));
        assert!(!rendered.contains("--watch"));
    }

    #[test]
    fn watch_and_overrides_are_honored() {
        let _lock = crate::graph_store::ENV_LOCK.lock().expect("env lock");
        let _home = crate::graph_store::TestHome::new("run-watch").expect("home");
        let hash = commit_sample_graph();
        let plan = build_run_plan(
            &hash,
            Some(Path::new("/tmp/coord.db")),
            Some(Path::new("/opt/squad")),
            true,
        )
        .expect("plan");

        assert_eq!(plan.program, OsString::from("/opt/squad"));
        let rendered = plan.rendered();
        assert!(rendered.contains("/tmp/coord.db"));
        assert!(rendered.ends_with("--watch"), "{rendered}");
    }

    #[test]
    fn missing_graph_fails_closed() {
        let _lock = crate::graph_store::ENV_LOCK.lock().expect("env lock");
        let _home = crate::graph_store::TestHome::new("run-missing").expect("home");
        let error = build_run_plan("sha256:deadbeef", None, None, false)
            .expect_err("missing graph must fail");
        assert!(format!("{error:#}").contains("no verifiable committed graph"));
    }

    #[test]
    fn dry_run_does_not_execute() {
        let _lock = crate::graph_store::ENV_LOCK.lock().expect("env lock");
        let _home = crate::graph_store::TestHome::new("run-dry").expect("home");
        let hash = commit_sample_graph();
        // squad_bin points at a nonexistent program; dry-run must still succeed
        // because it never spawns it.
        run_graph(&hash, None, Some(Path::new("/nonexistent/squad")), false, true)
            .expect("dry run succeeds without executing");
    }
}
