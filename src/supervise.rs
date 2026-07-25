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

use std::collections::{BTreeMap, BTreeSet};
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

/// A proactive graft is only worthwhile when the node's output would otherwise go
/// unverified for at least this many steps — so an early check prevents that
/// intervening work from being wasted on a defect (faster to the objective) or
/// fills a real coverage gap (higher quality), without adding redundant work when
/// a gate is already near. Tunable via `FRACTAL_GRAFT_MIN_HOPS`.
fn min_verify_hops() -> usize {
    std::env::var("FRACTAL_GRAFT_MIN_HOPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3)
}

/// Sentinel for "no downstream verifier at all" — a coverage gap. Large so such
/// nodes rank highest (a graft there adds a missing check).
const NO_DOWNSTREAM_VERIFIER: usize = usize::MAX;

/// Downstream successor adjacency (`from` → `[to]`).
fn successors(graph: &Value) -> BTreeMap<String, Vec<String>> {
    let mut succ: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(from), Some(to)) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        ) {
            succ.entry(from.to_owned()).or_default().push(to.to_owned());
        }
    }
    succ
}

/// Count of not-yet-completed nodes that transitively depend on `node` — the
/// rework a defect in `node` would put at risk.
fn downstream_unrun(graph: &Value, node: &str, completed: &BTreeSet<String>) -> usize {
    let succ = successors(graph);
    let mut seen = BTreeSet::new();
    let mut stack = vec![node.to_owned()];
    let mut count = 0;
    while let Some(current) = stack.pop() {
        for next in succ.get(&current).into_iter().flatten() {
            if seen.insert(next.clone()) {
                if !completed.contains(next) {
                    count += 1;
                }
                stack.push(next.clone());
            }
        }
    }
    count
}

/// Hops to the nearest still-pending verify/test node downstream of `node` — how
/// much work runs before `node`'s output is checked by an EXISTING gate. Returns
/// [`NO_DOWNSTREAM_VERIFIER`] when nothing downstream verifies it.
fn hops_to_nearest_pending_verifier(
    graph: &Value,
    node: &str,
    completed: &BTreeSet<String>,
) -> usize {
    let succ = successors(graph);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut frontier: Vec<String> = succ.get(node).cloned().unwrap_or_default();
    for id in &frontier {
        seen.insert(id.clone());
    }
    let mut depth = 1usize;
    while !frontier.is_empty() {
        for id in &frontier {
            if !completed.contains(id) && execute::is_verify(&capability_of(graph, id)) {
                return depth;
            }
        }
        let mut next = Vec::new();
        for id in &frontier {
            for child in succ.get(id).into_iter().flatten() {
                if seen.insert(child.clone()) {
                    next.push(child.clone());
                }
            }
        }
        frontier = next;
        depth += 1;
    }
    NO_DOWNSTREAM_VERIFIER
}

fn gap_display(hops: usize) -> String {
    if hops == NO_DOWNSTREAM_VERIFIER {
        "no downstream gate".to_owned()
    } else {
        format!("{hops} steps to next gate")
    }
}

/// A native/compiled project (iOS/Swift, SwiftPM) whose tests require the WHOLE
/// project to compile — so it cannot be verified mid-build. Detected by an Xcode
/// project, an xcodegen `project.yml`, a SwiftPM manifest, or any `.swift` source.
fn native_project(workspace: &Path) -> bool {
    if workspace.join("project.yml").exists() || workspace.join("Package.swift").exists() {
        return true;
    }
    std::fs::read_dir(workspace)
        .map(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".xcodeproj") || name.ends_with(".swift")
            })
        })
        .unwrap_or(false)
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_supervised(
    graph: Value,
    graph_hash: &str,
    graph_id: &str,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
    ledger: &RunLedger,
    resume_completed: &BTreeSet<String>,
    recorder: Option<&crate::checkpoint::Recorder>,
) -> Result<Supervised> {
    let mut graph = graph;
    let mut hash = graph_hash.to_owned();

    // Resume: seed the completed set (only nodes still present in the graph) so the
    // ready frontier skips them and execution continues from where it stopped.
    let present = all_node_ids(&graph);
    let mut completed: BTreeSet<String> = resume_completed
        .iter()
        .filter(|id| present.contains(id.as_str()))
        .cloned()
        .collect();
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
    let min_hops = min_verify_hops();
    // Native/compiled projects (iOS/Swift, SwiftPM) cannot be verified mid-build —
    // the whole project must compile before any test runs, so a proactive graft
    // that runs `xcodebuild test` on a half-written project always fails and turns
    // the board red. Skip proactive grafts for them; the final acceptance gate
    // verifies once everything is in place.
    let native = native_project(workspace);

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

        // Checkpoint after every wave so an interruption can resume from here.
        if let Some(recorder) = recorder {
            recorder.record(&hash, &completed, all_node_ids(&graph).len());
        }

        if failed.is_some() {
            break; // the caller's failure-evolution loop takes over from here.
        }
        if completed.len() >= all_node_ids(&graph).len() {
            break; // whole graph done.
        }

        // ── Mid-run morphogenesis: read this wave's progress signals ──
        if midrun >= MAX_MIDRUN || native {
            continue;
        }
        let nodes_now = all_node_ids(&graph);
        let has_verify_child =
            |id: &str| nodes_now.iter().any(|other| other.starts_with(&format!("verify.{id}.")));
        // Value-of-grafting: only graft a verification if it is genuinely worth it
        // — the node has dependent work at risk AND its output is not already
        // checked for at least `min_hops` steps (so an early check catches a defect
        // before that work is wasted, or fills a missing gate). Among candidates,
        // pick the highest-value one (largest verification gap, then most at-risk
        // work). Slowness is NOT a trigger; a well-gated plan grafts nothing.
        let scored = runs
            .iter()
            .filter(|run| {
                run.ok
                    && !grown.contains(&run.node)
                    && !has_verify_child(&run.node)
                    && execute::is_build(&capability_of(&graph, &run.node))
            })
            .filter_map(|run| {
                let at_risk = downstream_unrun(&graph, &run.node, &completed);
                let gap = hops_to_nearest_pending_verifier(&graph, &run.node, &completed);
                (at_risk >= 1 && gap >= min_hops).then_some((run, at_risk, gap))
            })
            .max_by_key(|&(_, at_risk, gap)| (gap, at_risk));
        let Some((run, at_risk, gap)) = scored else {
            continue;
        };

        match harness_evolution::grow_proactive(&graph, &hash, &run.node) {
            Ok(evolution) => {
                println!(
                    "  ⟳ mid-run morphogen: verifying `{}` early ({} ms) — {at_risk} dependent task(s) at risk, {} — {} [{}]",
                    run.node,
                    run.latency_ms,
                    gap_display(gap),
                    evolution.note,
                    evolution.arm
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
                            crate::board::serve_graph(&evolution.child_hash, port, None, true, None)
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
                // Point the checkpoint at the newly-grown child graph.
                if let Some(recorder) = recorder {
                    recorder.record(&hash, &completed, all_node_ids(&graph).len());
                }
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

    fn linear_with_gate() -> Value {
        // foundation → a → b → c → gate(verify) → done
        json!({
            "graph_id": "g",
            "nodes": [
                {"id":"foundation","capability":"code.generate"},
                {"id":"a","capability":"code.generate"},
                {"id":"b","capability":"code.generate"},
                {"id":"c","capability":"code.generate"},
                {"id":"gate","capability":"python.tests.execute"},
                {"id":"done","capability":"control.complete"}
            ],
            "edges": [
                {"from":"foundation","to":"a"},{"from":"a","to":"b"},
                {"from":"b","to":"c"},{"from":"c","to":"gate"},{"from":"gate","to":"done"}
            ]
        })
    }

    #[test]
    fn graft_value_favors_far_from_gate_high_leverage_nodes() {
        let g = linear_with_gate();
        let none: BTreeSet<String> = BTreeSet::new();
        // A foundational node: many dependents, gate is far → worth an early check.
        assert!(downstream_unrun(&g, "foundation", &none) >= 4);
        assert_eq!(hops_to_nearest_pending_verifier(&g, "foundation", &none), 4);
        // A node right before the gate: the gate is 1 hop away → NOT worth grafting.
        assert_eq!(hops_to_nearest_pending_verifier(&g, "c", &none), 1);
        // With min_hops = 3: foundation qualifies, c does not.
        assert!(hops_to_nearest_pending_verifier(&g, "foundation", &none) >= 3);
        assert!(hops_to_nearest_pending_verifier(&g, "c", &none) < 3);
    }

    #[test]
    fn coverage_gap_ranks_highest() {
        // No verifier anywhere downstream → a graft fills a real gap.
        let g = json!({
            "graph_id":"g",
            "nodes":[{"id":"x","capability":"code.generate"},{"id":"y","capability":"code.generate"}],
            "edges":[{"from":"x","to":"y"}]
        });
        let none: BTreeSet<String> = BTreeSet::new();
        assert_eq!(hops_to_nearest_pending_verifier(&g, "x", &none), NO_DOWNSTREAM_VERIFIER);
    }

    #[test]
    fn native_projects_are_detected_so_grafts_are_skipped() {
        let dir = std::env::temp_dir().join(format!("frac-native-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Empty / python workspace → not native.
        assert!(!native_project(&dir));
        std::fs::write(dir.join("test_x.py"), "x").unwrap();
        assert!(!native_project(&dir));
        // An xcodegen project.yml → native.
        std::fs::write(dir.join("project.yml"), "name: X").unwrap();
        assert!(native_project(&dir));
        std::fs::remove_dir_all(&dir).ok();
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
