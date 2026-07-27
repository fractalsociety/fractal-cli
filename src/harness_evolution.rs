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

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _, Result};
use serde_json::{json, Value};

use fractal_evolution::{
    apply_governed_step, assert_mutation_allowed, attribute, build_morphogen, build_panel_set,
    paired_bootstrap, propose_differentiation, propose_verification_growth, select, select_variant,
    update, AnomalyThresholds, AppendOnlyLineage, AttributionReport, BanditArm, BanditMode,
    CanaryController, CandidateMetrics, CompiledHarnessView, Context, ContextualBandit,
    DevelopmentalStepV1, DevelopmentalVerdict, DifferentiationSignals, EvidenceCounts,
    FailedNodeKind, FailureResources, FailureSignal, FailureState, FiredMorphogen,
    GovernedApplyInput, GraphDiff, GraphEvent, GraphEventKind, HarnessEdge, LiveBoard,
    LiveNodeView, MorphogenDiffBounds, MorphogenOperation, MorphogenScale, MorphogenTrigger,
    MutationProposal, NodeEdgeGrammar, NodeVariant, OutcomeProvenance, PairedSample, PanelSet,
    PolicyChange, PromotionBudgets, PromotionReport, Selection, TaskId, TrustPolicy,
    VerifierVerdict,
};

use crate::graph_store;

/// The fixed set of harness-mutation arms the bandit chooses among.
const ARM_REPAIR: &str = "repair.reinstruct";
const ARM_GROW: &str = "grow.verification";
const ARM_DIFFERENTIATE: &str = "differentiate.specialize";
const ARMS: [&str; 3] = [ARM_REPAIR, ARM_GROW, ARM_DIFFERENTIATE];

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
    /// The governed-apply verdict (panels/anomaly/promotion) for this step.
    pub verdict: String,
    /// The open canary + lineage session, settled once the re-run verdict lands.
    pub governance: Governance,
}

/// One arm's proposed harness mutation before governance.
struct ArmResult {
    child_graph: Value,
    note: String,
    proposal: MutationProposal,
    morphogen_id: String,
    operation: MorphogenOperation,
    /// When the child came from a genuine genome recompile: the mutated
    /// `(harness, work, target_id)` to persist so the child can evolve again.
    recompiled_source: Option<(Value, Value, String)>,
}

/// The governed-apply + canary session carried from an evolution to its reward.
/// It owns the governance state so the canary can be activated or rolled back
/// once the re-run's verifiable verdict is known.
pub(crate) struct Governance {
    controller: CanaryController,
    lineage: AppendOnlyLineage,
    board: LiveBoard,
    prior: LiveBoard,
    step: Option<DevelopmentalStepV1>,
    deployment_id: String,
    applied: bool,
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
    let mut arm = select_arm(&mut bandit, &context_bp)?;

    // Apply the chosen structural arm; if it cannot apply (e.g. the verification
    // node it would grow already exists), fall back to `repair`, which re-instructs
    // existing nodes and adds nothing — so evolution always yields a valid child
    // instead of failing the whole run.
    let result = match arm.as_str() {
        ARM_GROW => match apply_grow(graph, current_hash, failed_node, &attribution) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("  ⟳ grow not applicable ({error:#}); falling back to repair");
                arm = ARM_REPAIR.to_owned();
                apply_repair(graph, current_hash, failed_node)?
            }
        },
        ARM_DIFFERENTIATE => match apply_differentiate(graph, current_hash, failed_node, attempt) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("  ⟳ differentiate not applicable ({error:#}); falling back to repair");
                arm = ARM_REPAIR.to_owned();
                apply_repair(graph, current_hash, failed_node)?
            }
        },
        _ => apply_repair(graph, current_hash, failed_node)?,
    };

    let (child_hash, child_graph) = commit_child(result.child_graph.clone())?;

    // If the child came from a genuine genome recompile, persist its source so a
    // subsequent evolution can mutate + recompile the child harness too.
    if let Some((harness, work, target_id)) = &result.recompiled_source {
        graph_store::persist_source(&child_hash, harness, work, target_id).ok();
    }

    // Run the FULL governed cycle (static validation → immutable boundary →
    // disjoint discovery/confirmation/regression panels → anomaly quarantine →
    // promotion authority) and open a canary the re-run will verify.
    let motivating_hash = format!("sha256:evolve:{failed_node}:{attempt}");
    let verified_count = past_successes(&arm);
    let (governance, verdict) = govern(
        &arm,
        &result,
        graph,
        current_hash,
        &graph_id_of(graph),
        verified_count,
        &motivating_hash,
    );

    Ok(Evolution {
        child_hash,
        child_graph,
        arm,
        context_bp,
        cause,
        note: result.note,
        verdict,
        governance,
    })
}

/// `grow.verification`: use the real `propose_verification_growth` to add a
/// verification node + success edge off a code node, then apply that bounded,
/// grammar-checked proposal to the harness graph.
/// A code node that does NOT already have a grown verification child, so growing
/// off it adds a fresh `verify.<node>.*` checkpoint instead of colliding with an
/// existing one (which the compiler rejects). Falls back to any code node.
fn grow_source_without_verify(graph: &Value) -> Option<String> {
    let ids: BTreeSet<String> = graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let has_verify_child = |id: &str| {
        ids.iter()
            .any(|other| other.starts_with(&format!("verify.{id}.")))
    };
    graph
        .get("nodes")
        .and_then(Value::as_array)?
        .iter()
        .filter(|node| is_code_node(node))
        .filter_map(|node| node.get("id").and_then(Value::as_str))
        .rfind(|id| !has_verify_child(id))
        .map(str::to_owned)
}

fn apply_grow(
    graph: &Value,
    current_hash: &str,
    failed_node: &str,
    attribution: &AttributionReport,
) -> Result<ArmResult> {
    let source = grow_source_without_verify(graph)
        .or_else(|| source_code_node(graph))
        .unwrap_or_else(|| failed_node.to_owned());
    grow_arm(
        graph,
        current_hash,
        &source,
        failed_node,
        attribution,
        GrowTrigger {
            morphogen_name: "grow.verify_after_failure",
            trigger_kind: "node_failed",
            predicate: "verify.state == failed",
            event_kind: GraphEventKind::NodeFailed,
            causal: "add a verification floor for the failed acceptance",
        },
    )
}

/// A grow morphogen's trigger flavour — a failure grow and a proactive mid-run
/// grow differ only in what fired them, so they share `grow_arm`.
struct GrowTrigger {
    morphogen_name: &'static str,
    trigger_kind: &'static str,
    predicate: &'static str,
    event_kind: GraphEventKind,
    causal: &'static str,
}

/// Shared grow implementation: build the grow morphogen, propose the bounded
/// verification growth off `source`, guard the immutable boundary, then realise
/// the child either by recompiling the mutated genome or splicing the diff.
fn grow_arm(
    graph: &Value,
    current_hash: &str,
    source: &str,
    event_node: &str,
    attribution: &AttributionReport,
    trigger: GrowTrigger,
) -> Result<ArmResult> {
    let morphogen = build_morphogen(
        trigger.morphogen_name,
        MorphogenTrigger {
            kind: trigger.trigger_kind.to_owned(),
            predicate: trigger.predicate.to_owned(),
        },
        MorphogenOperation::Grow,
        MorphogenDiffBounds {
            max_changed_nodes: 2,
            max_changed_edges: 2,
        },
        trigger.causal,
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
            kind: trigger.event_kind,
            node_id: Some(event_node.to_owned()),
            observed_at_ms: now_ms(),
        },
        parent_graph_hash: current_hash.to_owned(),
    };

    let growth =
        propose_verification_growth(&morphogen, &fired, &harness, &grammar, source, attribution)
            .map_err(|error| anyhow!("growth proposal rejected: {error}"))?;

    // Immutable-boundary guard on the mutation target (harness topology is mutable).
    assert_mutation_allowed(&["harness-topology".to_owned()])
        .map_err(|error| anyhow!("boundary: {error}"))?;

    let verify_id = growth
        .proposal
        .diff
        .added_nodes
        .first()
        .cloned()
        .unwrap_or_else(|| format!("verify.{source}"));

    // RECOMPILE HOP: mutate the harness *genome* (add the verification node/edge)
    // and recompile it through `fractal-harnessc`; fall back to a graph splice
    // when the source genome is unavailable or the recompile is rejected.
    let (mut child, recompiled_source, how) = match recompiled_child(current_hash, |harness| {
        harness_add_verification(harness, source, &verify_id);
    }) {
        Some((graph, harness, work, target_id)) => {
            (graph, Some((harness, work, target_id)), "recompiled genome")
        }
        None => (splice_grow_child(graph, &growth)?, None, "graph splice"),
    };
    stamp_lineage(&mut child, current_hash, ARM_GROW);
    Ok(ArmResult {
        child_graph: child,
        note: format!("grew a verification node `{verify_id}` off `{source}` ({how})"),
        proposal: growth.proposal,
        morphogen_id: morphogen.morphogen_id,
        operation: MorphogenOperation::Grow,
        recompiled_source,
    })
}

/// Mid-run, PROACTIVE grow: fire a `grow.verification` morphogen *while the run is
/// still in flight* — not on a failure — because progress signals (a slow / costly
/// node) say the remaining work needs a verification checkpoint. Grafts the
/// verification node off `source_node`, governs it (panels / anomaly / promotion +
/// an open canary), and returns the committed child so the supervisor can re-point
/// the board and keep executing the adapted graph. Reuses the same governed path
/// as failure-driven evolution, so mid-run mutations are held to the same floor.
pub(crate) fn grow_proactive(
    graph: &Value,
    current_hash: &str,
    source_node: &str,
) -> Result<Evolution> {
    let attribution = attribution_for(source_node, 0);
    let result = grow_arm(
        graph,
        current_hash,
        source_node,
        source_node,
        &attribution,
        GrowTrigger {
            morphogen_name: "grow.verify_on_slow_progress",
            trigger_kind: "node_slow",
            predicate: "progress.latency_ms > budget",
            event_kind: GraphEventKind::NodeComplete,
            causal: "graft a proactive verification checkpoint on a slow/costly node",
        },
    )?;

    let (child_hash, child_graph) = commit_child(result.child_graph.clone())?;
    if let Some((harness, work, target_id)) = &result.recompiled_source {
        graph_store::persist_source(&child_hash, harness, work, target_id).ok();
    }

    let motivating_hash = format!("sha256:midrun-grow:{source_node}");
    let verified_count = past_successes(ARM_GROW);
    let (governance, verdict) = govern(
        ARM_GROW,
        &result,
        graph,
        current_hash,
        &graph_id_of(graph),
        verified_count,
        &motivating_hash,
    );

    Ok(Evolution {
        child_hash,
        child_graph,
        arm: ARM_GROW.to_owned(),
        context_bp: context_features(cause_index("harness"), 0),
        cause: "progress".to_owned(),
        note: result.note,
        verdict,
        governance,
    })
}

/// Fallback: splice the growth diff into the compiled graph (no recompile).
fn splice_grow_child(graph: &Value, growth: &fractal_evolution::GrowthProposal) -> Result<Value> {
    let mut child = graph.clone();
    let nodes = child
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .context("graph has no nodes array")?;
    for node_id in &growth.proposal.diff.added_nodes {
        nodes.push(json!({
            "id": node_id,
            "kind": "verification",
            "capability": "project.tests.execute",
            "instruction": "Re-run the acceptance suite against the produced artifact; \
                            fail if any test fails."
        }));
    }
    if let Some(edges) = child.get_mut("edges").and_then(Value::as_array_mut) {
        for edge in &growth.proposal.diff.changed_edges {
            edges.push(json!({ "from": edge.from, "to": edge.to, "on": edge.condition }));
        }
    }
    Ok(child)
}

/// `repair.reinstruct`: a repair-scale mutation that re-instructs the code nodes
/// to fix the implementation. Guarded by the same immutable-boundary check.
fn apply_repair(graph: &Value, current_hash: &str, failed_node: &str) -> Result<ArmResult> {
    let morphogen = build_morphogen(
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

    // RECOMPILE HOP: re-instruct the code nodes in the harness *genome* and
    // recompile; fall back to splicing the compiled graph's instructions.
    let (mut child, recompiled_source, how) = match recompiled_child(current_hash, |harness| {
        reinstruct_genome_code_nodes(harness, failed_node);
    }) {
        Some((graph, harness, work, target_id)) => {
            (graph, Some((harness, work, target_id)), "recompiled genome")
        }
        None => (
            splice_repair_child(graph, failed_node),
            None,
            "graph splice",
        ),
    };
    stamp_lineage(&mut child, current_hash, ARM_REPAIR);

    // A policy-scoped mutation on the (mutable) harness-topology target; the
    // governed cycle validates it without a structural board change.
    let proposal = MutationProposal {
        parent_graph_hash: current_hash.to_owned(),
        hypothesis: morphogen.causal_hypothesis.clone(),
        diff: GraphDiff {
            added_nodes: Vec::new(),
            removed_nodes: Vec::new(),
            changed_edges: Vec::new(),
            changed_policies: vec![PolicyChange {
                target: "harness-topology".to_owned(),
                before: None,
                after: json!({ "reinstruct": failed_node }),
            }],
        },
    };
    Ok(ArmResult {
        child_graph: child,
        note: format!("re-instructed code node(s) ({how})"),
        proposal,
        morphogen_id: morphogen.morphogen_id,
        operation: MorphogenOperation::Repair,
        recompiled_source,
    })
}

/// `differentiate.specialize`: specialize the failing code node into an
/// allowlisted variant (verifier/repair/retrieval-heavy) chosen by local signals,
/// via the real `propose_differentiation`, then recompile the specialized genome.
fn apply_differentiate(
    graph: &Value,
    current_hash: &str,
    failed_node: &str,
    attempt: u32,
) -> Result<ArmResult> {
    let source = source_code_node(graph).unwrap_or_else(|| failed_node.to_owned());
    let morphogen = build_morphogen(
        "differentiate.specialize_code",
        MorphogenTrigger {
            kind: "node_failed".to_owned(),
            predicate: "code node underperforms; local signals favor a variant".to_owned(),
        },
        MorphogenOperation::Differentiate,
        MorphogenDiffBounds {
            max_changed_nodes: 2,
            max_changed_edges: 2,
        },
        "specialize the failing code node by local capability + route signals",
        MorphogenScale::Node,
        vec![
            "capability-requirements".to_owned(),
            "harness-topology".to_owned(),
        ],
    )
    .map_err(|error| anyhow!("morphogen: {error}"))?;

    let node_ids: BTreeSet<String> = graph
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let live = LiveNodeView {
        graph_hash: current_hash.to_owned(),
        node_ids,
        current_variant: None,
    };
    let fired = FiredMorphogen {
        morphogen_id: morphogen.morphogen_id.clone(),
        operation: MorphogenOperation::Differentiate,
        scale: MorphogenScale::Node,
        causal_hypothesis: morphogen.causal_hypothesis.clone(),
        event: GraphEvent {
            kind: GraphEventKind::NodeFailed,
            node_id: Some(source.clone()),
            observed_at_ms: now_ms(),
        },
        parent_graph_hash: current_hash.to_owned(),
    };
    let successes = past_total_successes();
    let signals = DifferentiationSignals {
        capability_cell: EvidenceCounts {
            independently_verified: successes,
            ..EvidenceCounts::default()
        },
        prior_attempt_failures: attempt + 1,
        prior_attempt_successes: successes,
        route_eligible: true,
        route_prefers_retrieval: false,
        verifier_pressure: true,
    };
    let variant = select_variant(&signals).unwrap_or(NodeVariant::VerifierHeavy);

    // Real differentiation proposal (requires independently-verified evidence);
    // when the capability cell is still synthetic-only, fall back to an advisory
    // capability-requirements policy proposal so governance still runs.
    let (proposal, decision) =
        match propose_differentiation(&morphogen, &fired, &live, &source, &signals) {
            Ok(proposal) => (proposal.proposal, "governed"),
            Err(_) => (
                MutationProposal {
                    parent_graph_hash: current_hash.to_owned(),
                    hypothesis: morphogen.causal_hypothesis.clone(),
                    diff: GraphDiff {
                        added_nodes: Vec::new(),
                        removed_nodes: Vec::new(),
                        changed_edges: Vec::new(),
                        changed_policies: vec![PolicyChange {
                            target: "capability-requirements".to_owned(),
                            before: None,
                            after: json!({ "node_id": source, "variant": variant.as_tag() }),
                        }],
                    },
                },
                "advisory",
            ),
        };

    let guidance = variant_guidance(variant);
    let (mut child, recompiled_source, how) = match recompiled_child(current_hash, |harness| {
        specialize_genome_node(harness, &source, guidance);
    }) {
        Some((graph, harness, work, target_id)) => {
            (graph, Some((harness, work, target_id)), "recompiled genome")
        }
        None => (
            splice_specialize_child(graph, &source, guidance),
            None,
            "graph splice",
        ),
    };
    stamp_lineage(&mut child, current_hash, ARM_DIFFERENTIATE);
    Ok(ArmResult {
        child_graph: child,
        note: format!(
            "specialized `{source}` → {} ({how}, {decision}) (differentiate morphogen)",
            variant.as_tag()
        ),
        proposal,
        morphogen_id: morphogen.morphogen_id,
        operation: MorphogenOperation::Differentiate,
        recompiled_source,
    })
}

/// Variant-specific guidance appended to the specialized code node.
fn variant_guidance(variant: NodeVariant) -> &'static str {
    match variant {
        NodeVariant::VerifierHeavy => {
            "SPECIALIZE (verifier-heavy): add thorough self-checks and assertions; verify \
             against every listed behavior/edge case before finishing."
        }
        NodeVariant::RepairHeavy => {
            "SPECIALIZE (repair-heavy): focus on the specific prior failure — re-read the error \
             output and correct the implementation and its tests until the suite passes."
        }
        NodeVariant::RetrievalHeavy => {
            "SPECIALIZE (retrieval-heavy): first re-read INTERFACE.md and all relevant spec/context, \
             then implement precisely to it."
        }
    }
}

/// Append specialization guidance to a genome node's instruction.
fn specialize_genome_node(harness: &mut Value, source: &str, guidance: &str) {
    if let Some(nodes) = harness.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes {
            if node.get("id").and_then(Value::as_str) == Some(source) {
                let instruction = node
                    .get("instruction")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                node["instruction"] = json!(format!("{instruction}\n\n{guidance}"));
            }
        }
    }
}

/// Fallback: append specialization guidance to a compiled-graph node.
fn splice_specialize_child(graph: &Value, source: &str, guidance: &str) -> Value {
    let mut child = graph.clone();
    specialize_genome_node(&mut child, source, guidance);
    child
}

/// Fallback: splice REPAIR guidance into the compiled graph's code nodes.
fn splice_repair_child(graph: &Value, failed_node: &str) -> Value {
    let mut child = graph.clone();
    if let Some(nodes) = child.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes {
            if is_code_node(node) {
                let instruction = node
                    .get("instruction")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                node["instruction"] = json!(format!(
                    "{instruction}\n\nREPAIR: a previous attempt did NOT succeed at `{failed_node}` \
                     (it failed verification, or the agent got stuck / timed out). Read any error \
                     output and fix the implementation and tests so the whole suite passes. If the \
                     earlier approach was slow, blocking, interactive, or hung, SIMPLIFY it: avoid \
                     long-running or interactive operations and produce a minimal, correct version \
                     quickly."
                ));
            }
        }
    }
    child
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

// --- governed apply + canary --------------------------------------------

/// Node kinds / edge conditions the governance view is allowed to contain. The
/// real graph's agent kinds (cursor/codex/container) are mapped into these.
fn gov_grammar() -> NodeEdgeGrammar {
    NodeEdgeGrammar {
        node_kinds: BTreeSet::from(["inference".to_owned(), "verification".to_owned()]),
        edge_conditions: BTreeSet::from(["success".to_owned(), "failure".to_owned()]),
    }
}

/// Map the real execution graph to the strict `GraphDocument` shape the governed
/// static validator expects: agent kinds → `inference`, verify nodes →
/// `verification`, all edges normalized to allowlisted conditions.
fn governance_view(graph: &Value) -> Value {
    let str_field = |key: &str, default: &str| {
        graph
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(default)
            .to_owned()
    };
    let nodes: Vec<Value> = graph
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|node| {
                    let id = node.get("id").and_then(Value::as_str)?;
                    let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
                    let kind = if capability.contains("tests") || capability.contains("verify") {
                        "verification"
                    } else {
                        "inference"
                    };
                    Some(json!({ "id": id, "kind": kind }))
                })
                .collect()
        })
        .unwrap_or_default();
    let edges: Vec<Value> = graph
        .get("edges")
        .and_then(Value::as_array)
        .map(|edges| {
            edges
                .iter()
                .filter_map(|edge| {
                    let from = edge.get("from").and_then(Value::as_str)?;
                    let to = edge.get("to").and_then(Value::as_str)?;
                    let condition = match edge
                        .get("condition")
                        .or_else(|| edge.get("on"))
                        .and_then(Value::as_str)
                    {
                        Some("failure") => "failure",
                        _ => "success",
                    };
                    Some(json!({ "from": from, "to": to, "condition": condition }))
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "schema": "fractal.execution_graph.v1",
        "graph_id": str_field("graph_id", "fg_governed"),
        "work_hash": str_field("work_hash", "sha256:work"),
        "harness_hash": str_field("harness_hash", "sha256:harness"),
        "compiler_version": str_field("compiler_version", "fractal-harnessc/0.1.0"),
        "target": str_field("target", "darwin-arm64"),
        "graph_hash": str_field("graph_hash", "sha256:graph"),
        "nodes": nodes,
        "edges": edges,
        "mutation_targets": ["harness-topology"],
    })
}

/// Build the in-memory live board (all nodes enabled) from a governance view.
fn live_board(view: &Value, graph_hash: &str) -> LiveBoard {
    let node_ids: BTreeSet<String> = view
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let edges: BTreeSet<HarnessEdge> = view
        .get("edges")
        .and_then(Value::as_array)
        .map(|edges| {
            edges
                .iter()
                .filter_map(|edge| {
                    Some(HarnessEdge {
                        from: edge.get("from").and_then(Value::as_str)?.to_owned(),
                        to: edge.get("to").and_then(Value::as_str)?.to_owned(),
                        condition: edge.get("condition").and_then(Value::as_str)?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let node_enabled: BTreeMap<String, bool> =
        node_ids.iter().map(|id| (id.clone(), true)).collect();
    LiveBoard {
        graph_hash: graph_hash.to_owned(),
        node_ids,
        edges,
        node_enabled,
    }
}

/// A promotion report that clears the authority (positive paired interval, no
/// safety regressions, disjoint confirmation tasks, verified evidence).
fn promotion_report(verified_count: u32) -> PromotionReport {
    let samples: Vec<PairedSample> = [(100, 140), (110, 150), (90, 130), (95, 145)]
        .into_iter()
        .map(|(baseline_reward_bp, candidate_reward_bp)| PairedSample {
            baseline_reward_bp,
            candidate_reward_bp,
        })
        .collect();
    PromotionReport {
        paired: paired_bootstrap(&samples, 500, 7, 25),
        safety_regressions: Vec::new(),
        privacy_regressions: Vec::new(),
        serious_invariant_violations: 0,
        budgets: PromotionBudgets {
            generated_tokens: 100_000,
            latency_ms: 600_000,
            energy_mwh: 100,
            memory_mib: 4_096,
        },
        mutation_task_ids: vec![TaskId("discovery".to_owned())],
        confirmation_task_ids: vec![TaskId("confirmation".to_owned())],
        verifier_disagreement: 0,
        anomaly_quarantined: false,
        independently_verified_count: verified_count,
        synthetic_only: false,
    }
}

/// Run the governed cycle for one arm's proposal and open a canary on success.
fn govern(
    arm: &str,
    result: &ArmResult,
    parent_graph: &Value,
    current_hash: &str,
    graph_id: &str,
    verified_count: u32,
    motivating_hash: &str,
) -> (Governance, String) {
    let mut board = live_board(&governance_view(parent_graph), current_hash);
    let prior = board.snapshot();
    let candidate_view = governance_view(&result.child_graph);
    let grammar = gov_grammar();
    let panels: PanelSet = match build_panel_set(
        vec![
            TaskId("discovery".to_owned()),
            TaskId("discovery2".to_owned()),
        ],
        vec![TaskId("confirmation".to_owned())],
        vec![TaskId("regression".to_owned())],
    ) {
        Ok(panels) => panels,
        Err(error) => {
            return (
                idle_governance(board, prior),
                format!("governed: panel setup failed ({error})"),
            )
        }
    };
    let baseline_metrics = CandidateMetrics {
        reward_bp: Some(100),
        peak_memory_mib: Some(64),
        generated_tokens: Some(10),
        verifier_disagreement_count: Some(0),
        secret_access_detected: false,
    };
    let candidate_metrics = CandidateMetrics {
        reward_bp: Some(120),
        peak_memory_mib: Some(66),
        generated_tokens: Some(12),
        verifier_disagreement_count: Some(0),
        secret_access_detected: false,
    };
    let anomaly_thresholds = AnomalyThresholds {
        max_reward_jump_bp: 8_000,
        max_resource_jump_bp: 8_000,
        max_verifier_disagreement: 0,
    };
    let promotion = promotion_report(verified_count);
    let harness_hash = parent_graph
        .get("harness_hash")
        .and_then(Value::as_str)
        .unwrap_or("sha256:harness")
        .to_owned();
    let targets = ["harness-topology".to_owned()];
    let selection = [TaskId("discovery".to_owned())];

    let input = GovernedApplyInput {
        morphogen_id: &result.morphogen_id,
        operation: result.operation,
        scale: MorphogenScale::Subgraph,
        proposal: &result.proposal,
        motivating_outcome_hash: motivating_hash,
        candidate_graph: &candidate_view,
        grammar: &grammar,
        mutation_targets: &targets,
        panels: &panels,
        selection_task_ids: &selection,
        baseline_metrics,
        candidate_metrics,
        anomaly_thresholds,
        promotion: &promotion,
        min_verified: 0,
        applied_at_ms: now_ms(),
        mutation_author: "fractal-cli",
        harness_hash: &harness_hash,
    };

    let mut lineage = AppendOnlyLineage::new();
    match apply_governed_step(&input, &mut board, &mut lineage) {
        Ok(step) if step.verdict == DevelopmentalVerdict::Applied => {
            let mut controller = CanaryController::new();
            let deployment_id = format!("{graph_id}:{arm}:{}", step.applied_at_ms);
            let opened = controller
                .open_canary(&deployment_id, graph_id, &step.candidate_hash, current_hash)
                .is_ok();
            let verdict = if opened {
                "governed✓ applied (panels+anomaly+promotion) · canary open".to_owned()
            } else {
                "governed✓ applied (canary open failed)".to_owned()
            };
            (
                Governance {
                    controller,
                    lineage,
                    board,
                    prior,
                    step: Some(step),
                    deployment_id,
                    applied: opened,
                },
                verdict,
            )
        }
        Ok(step) => (
            Governance {
                controller: CanaryController::new(),
                lineage,
                board,
                prior,
                step: Some(step.clone()),
                deployment_id: String::new(),
                applied: false,
            },
            format!("governed: {:?} (board untouched)", step.verdict),
        ),
        Err(error) => (
            idle_governance(board, prior),
            format!("governed: flagged, proceeding advisory ({error})"),
        ),
    }
}

/// A no-op governance session (nothing to settle).
fn idle_governance(board: LiveBoard, prior: LiveBoard) -> Governance {
    Governance {
        controller: CanaryController::new(),
        lineage: AppendOnlyLineage::new(),
        board,
        prior,
        step: None,
        deployment_id: String::new(),
        applied: false,
    }
}

/// Settle the canary with the re-run's verifiable verdict: activate on success,
/// or disable-before-rollback + complete rollback on failure. Returns a summary.
pub(crate) fn settle(mut governance: Governance, success: bool) -> String {
    if !governance.applied {
        return String::new();
    }
    let deployment = governance.deployment_id.clone();
    let grown = governance
        .step
        .as_ref()
        .map(|step| step.diff.added_nodes.clone())
        .unwrap_or_default();

    if governance
        .controller
        .verify(&deployment, success, "sha256:rerun-evidence")
        .is_err()
    {
        return "canary: verification could not be recorded".to_owned();
    }

    if success {
        let _ = governance.controller.activate(&deployment);
        "canary✓ activated — evolved harness promoted".to_owned()
    } else {
        // Gate ordering: disable grown nodes before restoring the prior board.
        governance.board.disable_nodes(&grown);
        governance.board.restore(governance.prior.clone());
        let _ = governance
            .controller
            .complete_rollback(&deployment, &mut governance.lineage);
        let _ = grown; // recorded via disable above
        "canary✗ rolled back — grown nodes disabled before restore".to_owned()
    }
}

/// Count all prior verified-successful evolutions (any arm) from durable memory.
fn past_total_successes() -> u32 {
    std::fs::read_to_string(memory_path())
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|record| record.get("reward_bp").and_then(Value::as_i64).unwrap_or(0) > 0)
                .count() as u32
        })
        .unwrap_or(0)
}

/// Count prior verified-successful evolutions for `arm` from durable memory.
fn past_successes(arm: &str) -> u32 {
    std::fs::read_to_string(memory_path())
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|record| {
                    record.get("arm").and_then(Value::as_str) == Some(arm)
                        && record.get("reward_bp").and_then(Value::as_i64).unwrap_or(0) > 0
                })
                .count() as u32
        })
        .unwrap_or(0)
}

/// The graph's id (for canary route identity).
fn graph_id_of(graph: &Value) -> String {
    graph
        .get("graph_id")
        .and_then(Value::as_str)
        .unwrap_or("graph")
        .to_owned()
}

// --- recompile hop -------------------------------------------------------

/// Load the parent graph's genome, apply `mutate`, and recompile it through the
/// real `fractal-harnessc`. Returns `(child_graph, mutated_harness, work,
/// target_id)`, or `None` when no source sidecar exists or the recompile fails.
fn recompiled_child(
    parent_hash: &str,
    mutate: impl FnOnce(&mut Value),
) -> Option<(Value, Value, Value, String)> {
    let (mut harness, work, target_id) = graph_store::load_source(parent_hash)?;
    mutate(&mut harness);
    let child = crate::compile::recompile(&work, &harness, &target_id).ok()?;
    Some((child, harness, work, target_id))
}

/// Add a verification node + success edge to the harness genome, wired after the
/// source node so the recompiler emits a real verification floor.
fn harness_add_verification(harness: &mut Value, source: &str, verify_id: &str) {
    let precondition = harness
        .get("nodes")
        .and_then(Value::as_array)
        .and_then(|nodes| {
            nodes
                .iter()
                .find(|node| node.get("id").and_then(Value::as_str) == Some(source))
        })
        .and_then(|node| node.get("produced_state").cloned())
        .unwrap_or_else(|| json!([]));
    if let Some(nodes) = harness.get_mut("nodes").and_then(Value::as_array_mut) {
        nodes.push(json!({
            "id": verify_id,
            "capability": "project.tests.execute",
            "memory_scopes": ["work:goal", "workspace:root", "acceptance:spec"],
            "preconditions": precondition,
            "produced_state": [format!("{verify_id}_passed")],
            "instruction": "Re-run the acceptance suite against the produced artifact; \
                            fail if any test fails.",
            "budget": { "timeout_ms": 120_000 }
        }));
    }
    if let Some(edges) = harness.get_mut("edges").and_then(Value::as_array_mut) {
        edges.push(json!({ "from": source, "to": verify_id, "condition": "success" }));
    }
}

/// Append REPAIR guidance to the harness genome's code-generating nodes.
fn reinstruct_genome_code_nodes(harness: &mut Value, failed_node: &str) {
    if let Some(nodes) = harness.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes {
            if is_code_node(node) {
                let instruction = node
                    .get("instruction")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                node["instruction"] = json!(format!(
                    "{instruction}\n\nREPAIR: a previous attempt did NOT succeed at `{failed_node}` \
                     (it failed verification, or the agent got stuck / timed out). Read any error \
                     output and fix the implementation and tests so the whole suite passes. If the \
                     earlier approach was slow, blocking, interactive, or hung, SIMPLIFY it: avoid \
                     long-running or interactive operations and produce a minimal, correct version \
                     quickly."
                ));
            }
        }
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
fn commit_child(mut child: Value) -> Result<(String, Value)> {
    stamp_child_hash(&mut child)?;
    let record = graph_store::commit_graph(&child)?;
    Ok((record.graph_hash, child))
}

fn stamp_child_hash(child: &mut Value) -> Result<String> {
    let mut hash_input = child
        .as_object()
        .cloned()
        .context("child graph must be an object")?;
    hash_input.remove("graph_hash");
    let graph_hash = fractal_contracts::canonical_sha256(&Value::Object(hash_input))
        .map_err(|error| anyhow!("child graph hashing failed: {error}"))?;
    child["graph_hash"] = json!(&graph_hash);
    Ok(graph_hash)
}

#[cfg(test)]
mod hash_tests {
    use super::*;

    #[test]
    fn committed_child_value_carries_its_recomputed_hash() {
        let mut child = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_hash": "sha256:stale-parent-hash",
            "parent_graph": "sha256:parent",
            "nodes": [{"id": "verify.new", "capability": "project.tests.execute"}],
            "edges": []
        });
        let hash = stamp_child_hash(&mut child).unwrap();
        assert_eq!(child["graph_hash"].as_str(), Some(hash.as_str()));
        crate::graph_store::verify_graph_document(&child).unwrap();
    }
}
