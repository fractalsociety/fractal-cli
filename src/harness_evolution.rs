//! Self-evolving harness — the genuine `fractal-evolution` engine wired into the
//! run loop, replacing the hand-rolled "append a REPAIR note" stand-in.
//!
//! On a verified failure this:
//!   1. **attributes** the failure to a cause (`fractal_evolution::attribute`),
//!   2. **bandit-selects** a mutation arm (`repair` vs. `grow-verification`) with a
//!      contextual epsilon-greedy bandit gated by verified-evidence trust — this is
//!      the RL learner; the **verifiable reward** is the fractal-verify floor verdict
//!      (RLVR: reward only on independently-verified success),
//!   3. builds the chosen **morphogen** (grow / repair) under immutable-boundary
//!      guards and produces a bounded, validated `MutationProposal`,
//!   4. **applies** that proposal to the harness graph (grow adds a verification
//!      node + edge; repair re-instructs the code nodes), commits the child graph,
//!   5. after the re-run, **records the reward** to durable evolution memory so the
//!      bandit learns which harness mutation actually fixes which failure — across
//!      runs, not just within one.
//!
//! The bandit is reconstructed from durable memory on each call (its estimates are
//! additive), so learning persists without serializing the learner itself.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _, Result};
use serde_json::{json, Value};

use fractal_evolution::{
    assert_mutation_allowed, attribute, build_morphogen, propose_verification_growth, select,
    update, AttributionReport, BanditArm, BanditMode, CompiledHarnessView, Context,
    ContextualBandit, FailedNodeKind, FailureResources, FailureSignal, FailureState,
    FiredMorphogen, GraphEvent, GraphEventKind, HarnessEdge, MorphogenDiffBounds,
    MorphogenOperation, MorphogenScale, MorphogenTrigger, NodeEdgeGrammar, OutcomeProvenance,
    Selection, TrustPolicy, VerifierVerdict,
};

use crate::graph_store;

/// The fixed set of harness-mutation arms the bandit chooses among.
const ARM_REPAIR: &str = "repair.reinstruct";
const ARM_GROW: &str = "grow.verification";
const ARMS: [&str; 2] = [ARM_REPAIR, ARM_GROW];

/// Deterministic bandit seed + 10% exploration.
const BANDIT_SEED: u64 = 0x9e37_79b9_7f4a_7c15;
const EPSILON_BP: u16 = 1_000;
/// Verified successes needed before online (production) arm selection is trusted.
const MIN_TRUSTED: u32 = 1;

/// A committed harness evolution, plus the reward key to score it later.
pub(crate) struct Evolution {
    pub child_hash: String,
    pub child_graph: Value,
    /// Bandit arm chosen (the reward key).
    pub arm: String,
    /// Bandit context features (the reward key).
    pub context_bp: Vec<i32>,
    /// Primary attributed failure cause tag.
    pub cause: String,
    /// Human-readable description of what changed.
    pub note: String,
}

fn memory_path() -> PathBuf {
    let root = match std::env::var_os("FRACTAL_HOME") {
        Some(home) => PathBuf::from(home),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".fractal"),
            None => PathBuf::from(".fractal"),
        },
    };
    root.join("harness-evolution-memory.jsonl")
}

/// Reconstruct the bandit from durable evolution memory by replaying every past
/// (arm, reward) observation. The bandit's estimates are additive, so replay
/// yields the same learned arm means without serializing the learner.
fn load_bandit() -> ContextualBandit {
    let arms = ARMS.iter().map(|id| BanditArm::new(*id)).collect();
    let mut bandit =
        ContextualBandit::new(arms, EPSILON_BP, BANDIT_SEED).expect("fixed arm set is valid");
    if let Ok(text) = std::fs::read_to_string(memory_path()) {
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let arm = record.get("arm").and_then(Value::as_str).unwrap_or("");
            if !ARMS.contains(&arm) {
                continue;
            }
            let reward = record.get("reward_bp").and_then(Value::as_i64).unwrap_or(0) as i32;
            let provenance = match record.get("provenance").and_then(Value::as_str) {
                Some("independently_verified") => OutcomeProvenance::IndependentlyVerified,
                Some("teacher_imitation") => OutcomeProvenance::TeacherImitation,
                _ => OutcomeProvenance::Synthetic,
            };
            let _ = update(&mut bandit, &BanditArm::new(arm), reward, provenance);
        }
    }
    bandit
}

/// Context features for the bandit: the attributed cause and the repair attempt,
/// bounded well under the eight-feature ceiling.
fn context_features(cause_index: i32, attempt: u32) -> Vec<i32> {
    vec![cause_index, attempt as i32]
}

/// Map a failure cause tag to a stable small index for the bandit context.
fn cause_index(tag: &str) -> i32 {
    match tag {
        "route" => 0,
        "model" => 1,
        "context" => 2,
        "tool" => 3,
        "harness" => 4,
        "resource" => 5,
        _ => 4,
    }
}

/// Build the sanitized failure signal + attribution for a failed verify node.
fn attribution_for(failed_node: &str, attempt: u32) -> AttributionReport {
    let within = FailureResources {
        duration_ms: 1,
        peak_memory_mib: 1,
        generated_tokens: 1,
    };
    let signal = FailureSignal {
        failing_node_id: failed_node.to_owned(),
        failed_node_kind: FailedNodeKind::Harness,
        failure_state: FailureState::Violation,
        verifier_verdict: VerifierVerdict::Failed,
        tests_passed: Some(false),
        observed_resources: within,
        budgeted_resources: within,
        retry_count: attempt,
        route_eligible: true,
        context_scope_present: true,
    };
    attribute(&signal)
}

/// Select the harness-mutation arm via the RL bandit, gated by verified trust.
fn select_arm(bandit: &mut ContextualBandit, context_bp: &[i32]) -> Result<String> {
    let context = Context::new(context_bp.to_vec()).map_err(|error| anyhow!("{error}"))?;
    let policy = TrustPolicy {
        min_independent_verified: MIN_TRUSTED,
    };
    let trust = fractal_evolution::trust_gate(bandit.evidence_counts(), &policy);
    // Online once we trust the accumulated verified evidence; otherwise advisory
    // shadow selection (still returns the current best arm).
    let mode = if matches!(trust, fractal_evolution::TrustDecision::Trusted) {
        BanditMode::Online
    } else {
        BanditMode::Shadow
    };
    let arm = match select(bandit, &context, mode, &trust) {
        Selection::Online { arm } | Selection::Shadow { arm, .. } => arm.as_str().to_owned(),
        Selection::Refused { .. } => ARM_REPAIR.to_owned(),
    };
    Ok(arm)
}

/// Choose and apply a governed harness mutation for a failed run, committing the
/// child graph. Anchors nothing itself — the caller records chain receipts.
pub(crate) fn evolve(
    graph: &Value,
    current_hash: &str,
    failed_node: &str,
    attempt: u32,
) -> Result<Evolution> {
    let attribution = attribution_for(failed_node, attempt);
    let cause = attribution
        .primary_cause()
        .map_or("harness", fractal_evolution::FailureCause::as_tag)
        .to_owned();
    let context_bp = context_features(cause_index(&cause), attempt);

    let mut bandit = load_bandit();
    let arm = select_arm(&mut bandit, &context_bp)?;

    let (child_graph, note) = if arm == ARM_GROW {
        apply_grow(graph, current_hash, failed_node, &attribution)?
    } else {
        apply_repair(graph, failed_node)?
    };

    let child_hash = commit_child(child_graph.clone())?;
    Ok(Evolution {
        child_hash,
        child_graph,
        arm,
        context_bp,
        cause,
        note,
    })
}

/// `grow.verification`: use the real `propose_verification_growth` to add a
/// verification node + success edge off a code node, then apply that bounded,
/// grammar-checked proposal to the harness graph.
fn apply_grow(
    graph: &Value,
    current_hash: &str,
    failed_node: &str,
    attribution: &AttributionReport,
) -> Result<(Value, String)> {
    let source = source_code_node(graph).unwrap_or_else(|| failed_node.to_owned());
    let morphogen = build_morphogen(
        "grow.verify_after_failure",
        MorphogenTrigger {
            kind: "node_failed".to_owned(),
            predicate: "verify.state == failed".to_owned(),
        },
        MorphogenOperation::Grow,
        MorphogenDiffBounds {
            max_changed_nodes: 2,
            max_changed_edges: 2,
        },
        "add a verification floor for the failed acceptance",
        MorphogenScale::Subgraph,
        vec!["harness-topology".to_owned()],
    )
    .map_err(|error| anyhow!("morphogen: {error}"))?;

    let harness = compiled_view(graph, current_hash);
    let grammar = NodeEdgeGrammar {
        node_kinds: BTreeSet::from(["verification".to_owned()]),
        edge_conditions: BTreeSet::from(["success".to_owned()]),
    };
    let fired = FiredMorphogen {
        morphogen_id: morphogen.morphogen_id.clone(),
        operation: MorphogenOperation::Grow,
        scale: MorphogenScale::Subgraph,
        causal_hypothesis: morphogen.causal_hypothesis.clone(),
        event: GraphEvent {
            kind: GraphEventKind::NodeFailed,
            node_id: Some(failed_node.to_owned()),
            observed_at_ms: now_ms(),
        },
        parent_graph_hash: current_hash.to_owned(),
    };

    let growth =
        propose_verification_growth(&morphogen, &fired, &harness, &grammar, &source, attribution)
            .map_err(|error| anyhow!("growth proposal rejected: {error}"))?;

    // Immutable-boundary guard on the mutation target (harness topology is mutable).
    assert_mutation_allowed(&["harness-topology".to_owned()])
        .map_err(|error| anyhow!("boundary: {error}"))?;

    // Apply the validated add-only diff to the JSON harness.
    let mut child = graph.clone();
    let nodes = child
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .context("graph has no nodes array")?;
    for node_id in &growth.proposal.diff.added_nodes {
        nodes.push(json!({
            "id": node_id,
            "kind": "verification",
            "capability": "python.tests.execute",
            "instruction": "Re-run the acceptance suite against the produced artifact; \
                            fail if any test fails."
        }));
    }
    if let Some(edges) = child.get_mut("edges").and_then(Value::as_array_mut) {
        for edge in &growth.proposal.diff.changed_edges {
            edges.push(json!({ "from": edge.from, "to": edge.to, "on": edge.condition }));
        }
    }
    stamp_lineage(&mut child, current_hash, ARM_GROW);
    let added = growth.proposal.diff.added_nodes.join(", ");
    Ok((
        child,
        format!("grew a verification node `{added}` off `{source}` (grow morphogen)"),
    ))
}

/// `repair.reinstruct`: a repair-scale mutation that re-instructs the code nodes
/// to fix the implementation. Guarded by the same immutable-boundary check.
fn apply_repair(graph: &Value, failed_node: &str) -> Result<(Value, String)> {
    let _morphogen = build_morphogen(
        "repair.reinstruct_code",
        MorphogenTrigger {
            kind: "node_failed".to_owned(),
            predicate: "acceptance.state == failed".to_owned(),
        },
        MorphogenOperation::Repair,
        MorphogenDiffBounds {
            max_changed_nodes: 4,
            max_changed_edges: 1,
        },
        "regenerate the failed implementation so the suite passes",
        MorphogenScale::Subgraph,
        vec!["harness-topology".to_owned()],
    )
    .map_err(|error| anyhow!("morphogen: {error}"))?;
    assert_mutation_allowed(&["harness-topology".to_owned()])
        .map_err(|error| anyhow!("boundary: {error}"))?;

    let mut child = graph.clone();
    let mut reinstructed = 0;
    if let Some(nodes) = child.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes {
            if is_code_node(node) {
                let instruction = node
                    .get("instruction")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                node["instruction"] = json!(format!(
                    "{instruction}\n\nREPAIR: a previous attempt FAILED verification at \
                     `{failed_node}`. Read any error output, fix the implementation and tests so \
                     the whole suite passes."
                ));
                reinstructed += 1;
            }
        }
    }
    stamp_lineage(&mut child, "", ARM_REPAIR);
    Ok((
        child,
        format!("re-instructed {reinstructed} code node(s) (repair morphogen)"),
    ))
}

/// Record the verifiable reward for the previous evolution so the bandit learns
/// across runs. Reward is 10,000bp for an independently-verified success, else 0.
pub(crate) fn record_reward(arm: &str, context_bp: &[i32], cause: &str, success: bool) {
    let record = json!({
        "arm": arm,
        "context_bp": context_bp,
        "reward_bp": if success { 10_000 } else { 0 },
        "provenance": "independently_verified",
        "cause": cause,
        "recorded_at": now_ms(),
    });
    let path = memory_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{record}");
    }
}

// --- helpers -------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_code_node(node: &Value) -> bool {
    let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
    capability.contains("code.generate")
        || capability.ends_with(".edit")
        || capability.contains("code.write")
}

/// The last code-generating node id (a good source for a grown verification edge).
fn source_code_node(graph: &Value) -> Option<String> {
    graph
        .get("nodes")
        .and_then(Value::as_array)?
        .iter()
        .filter(|node| is_code_node(node))
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .next_back()
}

/// Build the compiled-harness topology view the growth proposer needs.
fn compiled_view(graph: &Value, current_hash: &str) -> CompiledHarnessView {
    let node_ids = graph
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let edges = graph
        .get("edges")
        .and_then(Value::as_array)
        .map(|edges| {
            edges
                .iter()
                .filter_map(|edge| {
                    Some(HarnessEdge {
                        from: edge.get("from").and_then(Value::as_str)?.to_owned(),
                        to: edge.get("to").and_then(Value::as_str)?.to_owned(),
                        condition: edge
                            .get("on")
                            .or_else(|| edge.get("condition"))
                            .and_then(Value::as_str)
                            .unwrap_or("success")
                            .to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    CompiledHarnessView {
        graph_hash: current_hash.to_owned(),
        node_ids,
        edges,
    }
}

/// Stamp evolution + parent lineage and recompute the canonical graph hash.
fn stamp_lineage(child: &mut Value, parent_hash: &str, arm: &str) {
    let prior = child.get("evolution").and_then(Value::as_u64).unwrap_or(0);
    child["evolution"] = json!(prior + 1);
    if !parent_hash.is_empty() {
        child["parent_graph"] = json!(parent_hash);
    } else {
        child["parent_graph"] = child.get("graph_hash").cloned().unwrap_or(Value::Null);
    }
    child["evolution_arm"] = json!(arm);
}

/// Recompute the content hash (canonical over the graph minus `graph_hash`) and
/// commit the child graph to the store.
fn commit_child(mut child: Value) -> Result<String> {
    let mut hash_input = child
        .as_object()
        .cloned()
        .context("child graph must be an object")?;
    hash_input.remove("graph_hash");
    let graph_hash = fractal_contracts::canonical_sha256(&Value::Object(hash_input))
        .map_err(|error| anyhow!("child graph hashing failed: {error}"))?;
    child["graph_hash"] = json!(graph_hash);
    let record = graph_store::commit_graph(&child)?;
    Ok(record.graph_hash)
}
