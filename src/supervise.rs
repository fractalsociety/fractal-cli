//! Mid-run morphogenesis supervisor.
//!
//! The default executor (`run_multi_agent`) runs the whole graph and only evolves
//! the harness *after* a run, on a verified failure. This supervisor instead
//! drives the graph **wave by wave** (one dependency-ready frontier at a time) and,
//! between waves, reads the just-finished nodes' **progress signals**. When a
//! signal fires a morphogen trigger — e.g. a code node that ran slow/costly — it
//! grafts a governed, proactive verification checkpoint onto the *remaining* graph
//! and keeps executing the adapted graph. So the graph adapts continuously as it
//! runs, and a morphogen can fire multiple times mid-run, not just on a failure.
//!
//! Every mid-run mutation goes through the same governed path as failure-driven
//! evolution (`grow_proactive` → panels / anomaly / promotion + an open canary),
//! so proactive adaptations are held to the same floor. The open canaries are
//! settled once the run's final verifiable verdict is known, feeding the RL bandit.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use fractal_chain::{payload_hash_str, DevelopmentalOp, DevelopmentalStep, Hash256, ScaleLevel};

use crate::chain::RunLedger;
use crate::execute::{self, NodeRun, RunOutcome};
use crate::harness_evolution::{self, Governance};
use crate::orchestrate::hex_to_hash;

/// Bounded so a single run cannot spawn unbounded mid-run mutations.
const MAX_MIDRUN: u32 = 2;

/// The result of a supervised run: the outcome plus the (possibly evolved) graph
/// so the caller continues its failure-evolution loop from the adapted harness.
pub(crate) struct Supervised {
    pub outcome: RunOutcome,
    pub graph: Value,
    pub hash: String,
}

/// Whether the mid-run supervisor drives the in-process executor. On by default;
/// set `FRACTAL_MIDRUN=0` to fall back to the whole-graph `run_multi_agent`.
pub(crate) fn enabled() -> bool {
    !matches!(
        std::env::var("FRACTAL_MIDRUN").ok().as_deref(),
        Some("0") | Some("off") | Some("false")
    )
}

/// Latency (ms) above which a completed code node is considered "slow" and grafts
/// a proactive verification checkpoint. Tunable via `FRACTAL_SLOW_MS`.
fn slow_threshold_ms() -> u64 {
    std::env::var("FRACTAL_SLOW_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(60_000)
}

/// Every node id declared in the graph.
fn all_node_ids(graph: &Value) -> BTreeSet<String> {
    graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// A node's capability string, for `is_build` classification of progress signals.
fn capability_of(graph: &Value, id: &str) -> String {
    graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|node| node.get("capability").and_then(Value::as_str))
        .unwrap_or("")
        .to_owned()
}

/// Human-readable run detail, matching `run_multi_agent`'s summary.
fn detail_for(built: bool, verified: Option<bool>, failed: &Option<String>) -> String {
    if let Some(node) = failed {
        format!("stopped at node `{node}`")
    } else {
        match verified {
            Some(true) => "built and verified by the agent team".to_owned(),
            Some(false) => "built but verification failed".to_owned(),
            None if built => "built (no tests to verify)".to_owned(),
            None => "graph completed".to_owned(),
        }
    }
}

/// Drive the graph wave by wave, firing proactive morphogens between waves.
pub(crate) fn run_supervised(
    graph: Value,
    graph_hash: &str,
    graph_id: &str,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
    ledger: &RunLedger,
) -> Result<Supervised> {
    let mut graph = graph;
    let mut hash = graph_hash.to_owned();

    let mut completed: BTreeSet<String> = BTreeSet::new();
    let mut log: Vec<NodeRun> = Vec::new();
    let mut built = false;
    let mut verified: Option<bool> = None;
    let mut failed: Option<String> = None;

    // Mid-run governed grows awaiting the run's final verifiable verdict (RLVR):
    // (arm, bandit context, cause, open canary session).
    let mut pending: Vec<(String, Vec<i32>, String, Governance)> = Vec::new();
    // Sources we have already grafted a checkpoint off, so we don't re-graft.
    let mut grown: BTreeSet<String> = BTreeSet::new();
    let mut midrun = 0u32;
    let threshold = slow_threshold_ms();

    loop {
        let frontier = execute::ready_frontier(&graph, &completed);
        if frontier.is_empty() {
            break;
        }

        let runs = execute::run_wave(&frontier, &graph, agents, workspace, board);

        for run in &runs {
            if execute::is_build(&capability_of(&graph, &run.node)) && run.ok {
                built = true;
            }
            if let Some(value) = run.verified {
                verified = Some(value);
            }
            if run.ok {
                completed.insert(run.node.clone());
            } else if failed.is_none() {
                failed = Some(run.node.clone());
            }
            log.push(run.clone());
        }

        if failed.is_some() {
            break; // the caller's failure-evolution loop takes over from here.
        }
        if completed.len() >= all_node_ids(&graph).len() {
            break; // whole graph done.
        }

        // ── Mid-run morphogenesis: read this wave's progress signals ──
        if midrun >= MAX_MIDRUN {
            continue;
        }
        let slow = runs.iter().find(|run| {
            run.ok
                && !grown.contains(&run.node)
                && execute::is_build(&capability_of(&graph, &run.node))
                && run.latency_ms >= threshold
        });
        let Some(run) = slow else { continue };

        match harness_evolution::grow_proactive(&graph, &hash, &run.node) {
            Ok(evolution) => {
                println!(
                    "  ⟳ mid-run morphogen fired on slow `{}` ({} ms) — {} [{}]",
                    run.node, run.latency_ms, evolution.note, evolution.arm
                );
                println!("  ⟳ {}", evolution.verdict);
                anchor_midrun(
                    ledger,
                    graph_id,
                    &run.node,
                    &run.evidence_hex,
                    &evolution,
                    midrun,
                );

                // Board follows the adapted graph immediately.
                if let Some(url) = board {
                    if let Some(port) = crate::orchestrate::board_port(url) {
                        println!("  ⟳ board now following the mid-run graph…");
                        if let Err(error) =
                            crate::board::serve_graph(&evolution.child_hash, port, None, true)
                        {
                            eprintln!("  (board follow unavailable: {error:#})");
                        }
                    }
                }

                grown.insert(run.node.clone());
                pending.push((
                    evolution.arm,
                    evolution.context_bp,
                    evolution.cause,
                    evolution.governance,
                ));
                graph = evolution.child_graph;
                hash = evolution.child_hash;
                midrun += 1;
            }
            Err(error) => {
                eprintln!("  (mid-run morphogen skipped: {error:#})");
                grown.insert(run.node.clone()); // don't retry the same node.
            }
        }
    }

    // Settle every mid-run canary with the run's final verifiable verdict, and
    // reward the bandit so proactive grows learn from independently-verified wins.
    let succeeded = verified != Some(false) && failed.is_none();
    for (arm, context_bp, cause, governance) in pending {
        harness_evolution::record_reward(&arm, &context_bp, &cause, succeeded);
        let settle = harness_evolution::settle(governance, succeeded);
        if !settle.is_empty() {
            println!("  ⟳ mid-run {settle}");
        }
        ledger.promotion(
            &format!("{graph_id}:midrun:{arm}"),
            &format!("reward:{}:{settle}", if succeeded { "10000" } else { "0" }),
        );
    }

    let detail = detail_for(built, verified, &failed);
    Ok(Supervised {
        outcome: RunOutcome {
            built,
            verified,
            detail,
            failed_node: failed,
            log,
        },
        graph,
        hash,
    })
}

/// Anchor a mid-run developmental step + child lineage on the signed chain.
fn anchor_midrun(
    ledger: &RunLedger,
    graph_id: &str,
    source: &str,
    source_evidence_hex: &str,
    evolution: &harness_evolution::Evolution,
    midrun: u32,
) {
    let motivating: Hash256 = hex_to_hash(source_evidence_hex);
    let produced = payload_hash_str(&format!("midrun-grow:{graph_id}:{source}:{midrun}"));
    let step = DevelopmentalStep {
        scale: ScaleLevel::Graph,
        subject: format!("{graph_id}#{source}"),
        operation: DevelopmentalOp::Grow,
        step_id: format!("midrun-grow-{source}-{midrun}"),
        motivating_outcome: motivating,
        produced_outcome: produced,
    };
    ledger.developmental(&step);
    ledger.promotion(
        &format!("{graph_id}->{}", evolution.child_hash),
        &format!("midrun-lineage:{}:{}", evolution.arm, step.step_id),
    );
    println!(
        "  ⟳ grafted mid-run harness {} (lineage on-chain)",
        &evolution.child_hash[..23.min(evolution.child_hash.len())]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn harness_graph() -> Value {
        json!({
            "graph_id": "g",
            "nodes": [
                { "id": "plan", "capability": "code.generate" },
                { "id": "impl", "capability": "code.generate" },
                { "id": "tests", "capability": "code.write" },
                { "id": "review", "capability": "code.edit" },
                { "id": "accept", "capability": "python.tests.execute" },
                { "id": "done", "capability": "control.complete" }
            ],
            "edges": [
                { "from": "plan", "to": "impl" },
                { "from": "plan", "to": "tests" },
                { "from": "impl", "to": "review" },
                { "from": "tests", "to": "review" },
                { "from": "review", "to": "accept" },
                { "from": "accept", "to": "done" }
            ]
        })
    }

    fn ids(nodes: &[Value]) -> Vec<String> {
        nodes
            .iter()
            .map(|node| node.get("id").and_then(Value::as_str).unwrap().to_owned())
            .collect()
    }

    #[test]
    fn frontier_advances_wave_by_wave_respecting_dependencies() {
        let graph = harness_graph();
        let mut completed: BTreeSet<String> = BTreeSet::new();
        let mut waves: Vec<Vec<String>> = Vec::new();
        loop {
            let frontier = execute::ready_frontier(&graph, &completed);
            if frontier.is_empty() {
                break;
            }
            let wave = ids(&frontier);
            for id in &wave {
                completed.insert(id.clone());
            }
            waves.push(wave);
        }
        // plan alone, then impl ∥ tests in one wave, then review, accept, done.
        assert_eq!(
            waves,
            vec![
                vec!["plan".to_owned()],
                vec!["impl".to_owned(), "tests".to_owned()],
                vec!["review".to_owned()],
                vec!["accept".to_owned()],
                vec!["done".to_owned()],
            ]
        );
    }

    #[test]
    fn a_grown_verification_off_a_completed_node_becomes_a_later_wave() {
        // Simulate a mid-run graft: add verify.impl off the already-run `impl`.
        let mut graph = harness_graph();
        graph["nodes"].as_array_mut().unwrap().push(json!({
            "id": "verify.impl", "capability": "python.tests.execute"
        }));
        graph["edges"].as_array_mut().unwrap().push(json!({
            "from": "impl", "to": "verify.impl", "on": "success"
        }));
        let completed: BTreeSet<String> = ["plan", "impl", "tests"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let frontier = ids(&execute::ready_frontier(&graph, &completed));
        // review needs impl+tests (ready); the grafted verify.impl needs impl (ready).
        assert!(frontier.contains(&"verify.impl".to_owned()));
        assert!(frontier.contains(&"review".to_owned()));
    }

    #[test]
    fn detail_reflects_verification_state() {
        assert_eq!(
            detail_for(true, Some(true), &None),
            "built and verified by the agent team"
        );
        assert_eq!(
            detail_for(true, Some(false), &None),
            "built but verification failed"
        );
        assert_eq!(detail_for(true, None, &None), "built (no tests to verify)");
        assert_eq!(
            detail_for(true, None, &Some("impl".to_owned())),
            "stopped at node `impl`"
        );
    }

    #[test]
    fn build_capabilities_are_classified_for_progress_signals() {
        let graph = harness_graph();
        assert!(execute::is_build(&capability_of(&graph, "impl")));
        assert!(execute::is_build(&capability_of(&graph, "tests")));
        // A verification node is not a build node, so it never triggers a graft.
        assert!(!execute::is_build(&capability_of(&graph, "accept")));
    }
}
