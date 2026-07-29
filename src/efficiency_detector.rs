#![allow(dead_code, clippy::too_many_arguments, clippy::redundant_closure)]

use crate::efficiency::{RepairAction, WasteType};
use std::collections::{BTreeMap, BTreeSet};

const DUPLICATE_TASK_SIMILARITY: f64 = 0.92;
const DUPLICATE_TEST_SIMILARITY: f64 = 0.88;
const DUPLICATE_RESEARCH_SIMILARITY: f64 = 0.90;
const OVERLAP_MIN_SHARED_FILES: usize = 1;
const EXCESSIVE_RETRY_ATTEMPTS: u32 = 3;
const LOW_VALUE_CONFIDENCE: f64 = 0.25;
const UNUSED_OUTPUT_CONFIDENCE: f64 = 0.35;
const TINY_NODE_TOKENS: u64 = 300;
const OVER_DECOMPOSED_GROUP_SIZE: usize = 4;
const EXCESSIVE_VERIFIER_COUNT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotState {
    Active,
    Queued,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NodeSnapshot {
    pub(crate) id: String,
    pub(crate) state: SnapshotState,
    pub(crate) title: String,
    pub(crate) instruction: String,
    pub(crate) dependencies: Vec<String>,
    pub(crate) estimated_remaining_tokens: u64,
    pub(crate) expected_artifact: String,
    pub(crate) files_or_systems_affected: Vec<String>,
    pub(crate) verification_plan: String,
    pub(crate) current_assumptions: Vec<String>,
    pub(crate) similarity_to_other_active_nodes: BTreeMap<String, f64>,
    pub(crate) confidence_still_useful: f64,
    pub(crate) attempt_count: u32,
    pub(crate) produced_artifacts: Vec<String>,
    pub(crate) referenced_artifacts: Vec<String>,
    pub(crate) verifies_node_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SimilarityEvidence {
    pub(crate) peer_node_id: String,
    pub(crate) score: f64,
    pub(crate) basis: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EfficiencyDetection {
    pub(crate) waste_type: WasteType,
    pub(crate) detected_node: String,
    pub(crate) affected_node_ids: Vec<String>,
    pub(crate) affected_count: u32,
    pub(crate) estimation_basis: String,
    pub(crate) gross_avoidable_tokens: u64,
    pub(crate) confidence: f64,
    pub(crate) similarity_evidence: Vec<SimilarityEvidence>,
    pub(crate) proposed_action: RepairAction,
    pub(crate) proposed_stable_repair: String,
}

pub(crate) fn detect_waste(nodes: &[NodeSnapshot]) -> Vec<EfficiencyDetection> {
    let mut ordered: Vec<&NodeSnapshot> = nodes.iter().collect();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));

    let mut detections = Vec::new();
    detections.extend(detect_duplicate_like(
        &ordered,
        WasteType::DuplicateTask,
        RepairAction::Cancel,
        DUPLICATE_TASK_SIMILARITY,
        |n| !is_test(n) && !is_research(n) && !is_verifier(n),
        "cancel later duplicate and keep earliest node",
    ));
    detections.extend(detect_duplicate_like(
        &ordered,
        WasteType::DuplicateTest,
        RepairAction::ConsolidateVerifiers,
        DUPLICATE_TEST_SIMILARITY,
        |n| is_test(n),
        "merge duplicate test scope into earliest verifier",
    ));
    detections.extend(detect_duplicate_like(
        &ordered,
        WasteType::DuplicateResearch,
        RepairAction::Merge,
        DUPLICATE_RESEARCH_SIMILARITY,
        |n| is_research(n),
        "merge research notes and cancel redundant research node",
    ));
    detections.extend(detect_consolidatable_tests(&ordered));
    detections.extend(detect_unused_output(&ordered));
    detections.extend(detect_superseded_assumption(&ordered));
    detections.extend(detect_spec_drift(&ordered));
    detections.extend(detect_excessive_retries(&ordered));
    detections.extend(detect_overlapping_files(&ordered));
    detections.extend(detect_over_decomposition(&ordered));
    detections.extend(detect_low_value_branch(&ordered));
    detections.extend(detect_premature_verification(&ordered));
    detections.extend(detect_excessive_verification(&ordered));

    detections.sort_by(|a, b| {
        waste_rank(a.waste_type)
            .cmp(&waste_rank(b.waste_type))
            .then(a.detected_node.cmp(&b.detected_node))
            .then(a.affected_node_ids.cmp(&b.affected_node_ids))
    });
    detections
}

fn detect_duplicate_like<F>(
    nodes: &[&NodeSnapshot],
    waste_type: WasteType,
    action: RepairAction,
    threshold: f64,
    class_filter: F,
    repair: &'static str,
) -> Vec<EfficiencyDetection>
where
    F: Fn(&NodeSnapshot) -> bool,
{
    let mut detections = Vec::new();
    for (left_idx, left) in nodes.iter().enumerate() {
        if !class_filter(left) {
            continue;
        }
        for right in nodes.iter().skip(left_idx + 1) {
            if !class_filter(right)
                || has_dependency_path(left, right)
                || has_dependency_path(right, left)
            {
                continue;
            }
            let structural = normalized(&left.title) == normalized(&right.title)
                && !left.expected_artifact.is_empty()
                && left.expected_artifact == right.expected_artifact;
            let score = similarity(left, right);
            if (structural || score >= threshold) && same_primary_scope(left, right) {
                let (detected, kept) = later_node(left, right);
                detections.push(make_detection(
                    waste_type,
                    detected.id.clone(),
                    vec![kept.id.clone(), detected.id.clone()],
                    format!(
                        "{} threshold {:.2}; observed {:.2}; same primary scope",
                        waste_type.as_str(),
                        threshold,
                        score
                    ),
                    detected.estimated_remaining_tokens,
                    if structural {
                        threshold.max(0.93)
                    } else {
                        score.min(0.98)
                    },
                    vec![SimilarityEvidence {
                        peer_node_id: kept.id.clone(),
                        score,
                        basis: if structural {
                            "normalized title and expected artifact match".to_owned()
                        } else {
                            "explicit peer similarity above threshold".to_owned()
                        },
                    }],
                    action,
                    repair,
                ));
            }
        }
    }
    detections
}

fn detect_consolidatable_tests(nodes: &[&NodeSnapshot]) -> Vec<EfficiencyDetection> {
    let mut groups: BTreeMap<String, Vec<&NodeSnapshot>> = BTreeMap::new();
    for node in nodes.iter().copied().filter(|n| is_test(n)) {
        let key = verifier_key(node);
        if !key.is_empty() {
            groups.entry(key).or_default().push(node);
        }
    }
    groups
        .into_values()
        .filter_map(|mut group| {
            group.sort_by(|a, b| a.id.cmp(&b.id));
            if group.len() >= 2 && group.iter().all(|n| !n.verifies_node_ids.is_empty()) {
                let ids = ids(&group);
                let gross = group.iter().skip(1).map(|n| n.estimated_remaining_tokens).sum();
                Some(make_detection(
                    WasteType::ConsolidatableTests,
                    group[0].id.clone(),
                    ids,
                    "multiple test/verifier nodes share one verified target and can run as one verifier".to_owned(),
                    gross,
                    0.82,
                    vec![SimilarityEvidence {
                        peer_node_id: group[1].id.clone(),
                        score: 0.82,
                        basis: "same verifier target".to_owned(),
                    }],
                    RepairAction::ConsolidateVerifiers,
                    "replace sibling verifier nodes with one stable verifier node",
                ))
            } else {
                None
            }
        })
        .collect()
}

fn detect_unused_output(nodes: &[&NodeSnapshot]) -> Vec<EfficiencyDetection> {
    let mut referenced = BTreeSet::new();
    for node in nodes {
        for artifact in &node.referenced_artifacts {
            referenced.insert(artifact.clone());
        }
        for dependency in &node.dependencies {
            referenced.insert(format!("node:{dependency}"));
        }
    }
    nodes
        .iter()
        .filter_map(|node| {
            let has_unreferenced_output = node.produced_artifacts.iter().any(|a| !referenced.contains(a));
            if has_unreferenced_output
                && !node.produced_artifacts.is_empty()
                && node.confidence_still_useful <= UNUSED_OUTPUT_CONFIDENCE
                && node.referenced_artifacts.is_empty()
            {
                Some(make_detection(
                    WasteType::UnusedOutput,
                    node.id.clone(),
                    vec![node.id.clone()],
                    "produced artifacts have no active/queued references and usefulness confidence is low".to_owned(),
                    node.estimated_remaining_tokens,
                    0.78,
                    vec![],
                    RepairAction::StopDownstream,
                    "stop downstream work until an active consumer references the artifact",
                ))
            } else {
                None
            }
        })
        .collect()
}

fn detect_superseded_assumption(nodes: &[&NodeSnapshot]) -> Vec<EfficiencyDetection> {
    let id_set: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    nodes
        .iter()
        .flat_map(|node| {
            node.current_assumptions.iter().filter_map(|assumption| {
                let target = marker_value(assumption, "supersedes:")
                    .or_else(|| marker_value(assumption, "replaces:"));
                target.and_then(|target| {
                    if id_set.contains(target.as_str()) {
                        Some(make_detection(
                            WasteType::SupersededAssumption,
                            target.clone(),
                            vec![target.clone(), node.id.clone()],
                            format!(
                                "node {} declares supersedes/replaces marker for {target}",
                                node.id
                            ),
                            node.estimated_remaining_tokens,
                            0.84,
                            vec![SimilarityEvidence {
                                peer_node_id: node.id.clone(),
                                score: similarity_by_id(nodes, &target, &node.id),
                                basis: "explicit supersedes/replaces assumption marker".to_owned(),
                            }],
                            RepairAction::Cancel,
                            "cancel node whose assumptions are explicitly superseded by newer node",
                        ))
                    } else {
                        None
                    }
                })
            })
        })
        .collect()
}

fn detect_spec_drift(nodes: &[&NodeSnapshot]) -> Vec<EfficiencyDetection> {
    let mut detections = Vec::new();
    for (i, left) in nodes.iter().enumerate() {
        for right in nodes.iter().skip(i + 1) {
            let left_spec = spec_marker(left);
            let right_spec = spec_marker(right);
            if let (Some(a), Some(b)) = (left_spec, right_spec) {
                let score = similarity(left, right);
                if a != b && score >= 0.70 && same_primary_scope(left, right) {
                    let (detected, peer) = later_node(left, right);
                    detections.push(make_detection(
                        WasteType::SpecDrift,
                        detected.id.clone(),
                        vec![peer.id.clone(), detected.id.clone()],
                        format!("same scope but conflicting spec markers {a} vs {b}"),
                        detected.estimated_remaining_tokens,
                        0.76,
                        vec![SimilarityEvidence {
                            peer_node_id: peer.id.clone(),
                            score,
                            basis: "conflicting spec markers on similar scope".to_owned(),
                        }],
                        RepairAction::SplitDrift,
                        "split drift for lead review before either branch continues",
                    ));
                }
            }
        }
    }
    detections
}

fn detect_excessive_retries(nodes: &[&NodeSnapshot]) -> Vec<EfficiencyDetection> {
    nodes
        .iter()
        .filter(|n| {
            n.attempt_count >= EXCESSIVE_RETRY_ATTEMPTS && n.confidence_still_useful <= 0.50
        })
        .map(|node| {
            make_detection(
                WasteType::ExcessiveRetries,
                node.id.clone(),
                vec![node.id.clone()],
                format!(
                    "attempt_count {} >= {} and usefulness confidence {:.2} <= 0.50",
                    node.attempt_count, EXCESSIVE_RETRY_ATTEMPTS, node.confidence_still_useful
                ),
                node.estimated_remaining_tokens,
                0.80,
                vec![],
                RepairAction::Reassign,
                "release or reassign after preserving failure evidence",
            )
        })
        .collect()
}

fn detect_overlapping_files(nodes: &[&NodeSnapshot]) -> Vec<EfficiencyDetection> {
    let mut detections = Vec::new();
    for (i, left) in nodes.iter().enumerate() {
        for right in nodes.iter().skip(i + 1) {
            let shared = shared_files(left, right);
            let score = similarity(left, right);
            if shared.len() >= OVERLAP_MIN_SHARED_FILES
                && !has_dependency_path(left, right)
                && !has_dependency_path(right, left)
                && score < DUPLICATE_TASK_SIMILARITY
                && !(is_test(left) && is_test(right))
            {
                let (detected, peer) = later_node(left, right);
                detections.push(make_detection(
                    WasteType::OverlappingFiles,
                    detected.id.clone(),
                    vec![peer.id.clone(), detected.id.clone()],
                    format!("parallel nodes touch shared path(s): {}", shared.join(",")),
                    detected.estimated_remaining_tokens / 2,
                    0.74,
                    vec![SimilarityEvidence {
                        peer_node_id: peer.id.clone(),
                        score,
                        basis: "shared files without dependency edge".to_owned(),
                    }],
                    RepairAction::Reassign,
                    "serialize or reassign one node to avoid conflicting file ownership",
                ));
            }
        }
    }
    detections
}

fn detect_over_decomposition(nodes: &[&NodeSnapshot]) -> Vec<EfficiencyDetection> {
    let mut groups: BTreeMap<String, Vec<&NodeSnapshot>> = BTreeMap::new();
    for node in nodes.iter().copied().filter(|n| {
        n.state == SnapshotState::Queued
            && n.estimated_remaining_tokens <= TINY_NODE_TOKENS
            && !n.files_or_systems_affected.is_empty()
            && !is_verifier(n)
    }) {
        groups
            .entry(primary_file(node).unwrap_or_default())
            .or_default()
            .push(node);
    }
    groups
        .into_values()
        .filter_map(|mut group| {
            group.sort_by(|a, b| a.id.cmp(&b.id));
            if group.len() >= OVER_DECOMPOSED_GROUP_SIZE {
                let gross: u64 = group
                    .iter()
                    .skip(1)
                    .map(|n| n.estimated_remaining_tokens)
                    .sum();
                Some(make_detection(
                    WasteType::OverDecomposition,
                    group[0].id.clone(),
                    ids(&group),
                    format!("{} tiny queued nodes share one file scope", group.len()),
                    gross,
                    0.79,
                    vec![SimilarityEvidence {
                        peer_node_id: group[1].id.clone(),
                        score: similarity(group[0], group[1]),
                        basis: "tiny queued nodes share primary file".to_owned(),
                    }],
                    RepairAction::Merge,
                    "merge tiny sibling nodes into one bounded implementation node",
                ))
            } else {
                None
            }
        })
        .collect()
}

fn detect_low_value_branch(nodes: &[&NodeSnapshot]) -> Vec<EfficiencyDetection> {
    nodes
        .iter()
        .filter(|n| {
            n.confidence_still_useful <= LOW_VALUE_CONFIDENCE
                && n.estimated_remaining_tokens <= 500
                && !is_verifier(n)
        })
        .map(|node| {
            make_detection(
                WasteType::LowValueBranch,
                node.id.clone(),
                vec![node.id.clone()],
                format!(
                    "usefulness confidence {:.2} <= {:.2} and remaining token estimate <= 500",
                    node.confidence_still_useful, LOW_VALUE_CONFIDENCE
                ),
                node.estimated_remaining_tokens,
                0.77,
                vec![],
                RepairAction::Cancel,
                "cancel low-value branch unless lead supplies new acceptance evidence",
            )
        })
        .collect()
}

fn detect_premature_verification(nodes: &[&NodeSnapshot]) -> Vec<EfficiencyDetection> {
    let active_or_queued: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    nodes
        .iter()
        .filter(|n| is_verifier(n))
        .filter_map(|node| {
            let target = node
                .verifies_node_ids
                .iter()
                .find(|target| active_or_queued.contains(target.as_str()));
            target.map(|target| {
                make_detection(
                    WasteType::PrematureVerification,
                    node.id.clone(),
                    vec![target.clone(), node.id.clone()],
                    format!("verification node targets active/queued unfinished node {target}"),
                    node.estimated_remaining_tokens,
                    0.86,
                    vec![SimilarityEvidence {
                        peer_node_id: target.clone(),
                        score: similarity_by_id(nodes, target, &node.id),
                        basis: "verifies_node_ids references unfinished snapshot node".to_owned(),
                    }],
                    RepairAction::DelayVerification,
                    "delay verifier until target node exits active/queued snapshot",
                )
            })
        })
        .collect()
}

fn detect_excessive_verification(nodes: &[&NodeSnapshot]) -> Vec<EfficiencyDetection> {
    let mut groups: BTreeMap<String, Vec<&NodeSnapshot>> = BTreeMap::new();
    for node in nodes.iter().copied().filter(|n| is_verifier(n)) {
        let key = verifier_key(node);
        if !key.is_empty() {
            groups.entry(key).or_default().push(node);
        }
    }
    groups
        .into_values()
        .filter_map(|mut group| {
            group.sort_by(|a, b| a.id.cmp(&b.id));
            if group.len() >= EXCESSIVE_VERIFIER_COUNT {
                let gross: u64 = group
                    .iter()
                    .skip(1)
                    .map(|n| n.estimated_remaining_tokens)
                    .sum();
                Some(make_detection(
                    WasteType::ExcessiveVerification,
                    group[0].id.clone(),
                    ids(&group),
                    format!("{} verifier nodes share the same target/scope", group.len()),
                    gross,
                    0.83,
                    vec![SimilarityEvidence {
                        peer_node_id: group[1].id.clone(),
                        score: 0.83,
                        basis: "same verification target repeated at least three times".to_owned(),
                    }],
                    RepairAction::ConsolidateVerifiers,
                    "keep one verifier and fold redundant checks into its verification plan",
                ))
            } else {
                None
            }
        })
        .collect()
}

fn make_detection(
    waste_type: WasteType,
    detected_node: String,
    mut affected_node_ids: Vec<String>,
    estimation_basis: String,
    gross_avoidable_tokens: u64,
    confidence: f64,
    mut similarity_evidence: Vec<SimilarityEvidence>,
    proposed_action: RepairAction,
    proposed_stable_repair: &'static str,
) -> EfficiencyDetection {
    affected_node_ids.sort();
    affected_node_ids.dedup();
    similarity_evidence.sort_by(|a, b| a.peer_node_id.cmp(&b.peer_node_id));
    EfficiencyDetection {
        waste_type,
        detected_node,
        affected_count: affected_node_ids.len() as u32,
        affected_node_ids,
        estimation_basis,
        gross_avoidable_tokens,
        confidence: round2(confidence.clamp(0.0, 1.0)),
        similarity_evidence,
        proposed_action,
        proposed_stable_repair: proposed_stable_repair.to_owned(),
    }
}

fn is_test(n: &NodeSnapshot) -> bool {
    let text = haystack(n);
    text.contains("test") || text.contains("verify") || text.contains("verification")
}

fn is_verifier(n: &NodeSnapshot) -> bool {
    is_test(n) || !n.verifies_node_ids.is_empty()
}

fn is_research(n: &NodeSnapshot) -> bool {
    let text = haystack(n);
    text.contains("research") || text.contains("investigate") || text.contains("survey")
}

fn same_primary_scope(a: &NodeSnapshot, b: &NodeSnapshot) -> bool {
    (!a.expected_artifact.is_empty() && a.expected_artifact == b.expected_artifact)
        || primary_file(a).is_some() && primary_file(a) == primary_file(b)
        || !shared_files(a, b).is_empty()
}

fn shared_files(a: &NodeSnapshot, b: &NodeSnapshot) -> Vec<String> {
    let left: BTreeSet<String> = a.files_or_systems_affected.iter().cloned().collect();
    let right: BTreeSet<String> = b.files_or_systems_affected.iter().cloned().collect();
    left.intersection(&right).cloned().collect()
}

fn primary_file(n: &NodeSnapshot) -> Option<String> {
    n.files_or_systems_affected.first().cloned()
}

fn has_dependency_path(a: &NodeSnapshot, b: &NodeSnapshot) -> bool {
    a.dependencies.iter().any(|dep| dep == &b.id)
}

fn later_node<'a>(
    a: &'a NodeSnapshot,
    b: &'a NodeSnapshot,
) -> (&'a NodeSnapshot, &'a NodeSnapshot) {
    if a.id <= b.id {
        (b, a)
    } else {
        (a, b)
    }
}

fn similarity(a: &NodeSnapshot, b: &NodeSnapshot) -> f64 {
    let explicit = a
        .similarity_to_other_active_nodes
        .get(&b.id)
        .or_else(|| b.similarity_to_other_active_nodes.get(&a.id))
        .copied();
    explicit.unwrap_or_else(|| jaccard(&haystack(a), &haystack(b)))
}

fn similarity_by_id(nodes: &[&NodeSnapshot], left: &str, right: &str) -> f64 {
    let a = nodes.iter().find(|n| n.id == left);
    let b = nodes.iter().find(|n| n.id == right);
    match (a, b) {
        (Some(a), Some(b)) => similarity(a, b),
        _ => 0.0,
    }
}

fn jaccard(a: &str, b: &str) -> f64 {
    let left: BTreeSet<String> = tokens(a).into_iter().collect();
    let right: BTreeSet<String> = tokens(b).into_iter().collect();
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(&right).count() as f64;
    let union = left.union(&right).count() as f64;
    round2(intersection / union)
}

fn tokens(value: &str) -> Vec<String> {
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|part| {
            let token = part.to_ascii_lowercase();
            if token.len() >= 3 {
                Some(token)
            } else {
                None
            }
        })
        .collect()
}

fn haystack(n: &NodeSnapshot) -> String {
    format!(
        "{} {} {} {} {}",
        n.title,
        n.instruction,
        n.expected_artifact,
        n.verification_plan,
        n.current_assumptions.join(" ")
    )
    .to_ascii_lowercase()
}

fn normalized(value: &str) -> String {
    tokens(value).join(" ")
}

fn marker_value(value: &str, marker: &str) -> Option<String> {
    let lowered = value.to_ascii_lowercase();
    let start = lowered.find(marker)? + marker.len();
    let tail = &value[start..];
    tail.split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .next()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn spec_marker(node: &NodeSnapshot) -> Option<String> {
    node.current_assumptions
        .iter()
        .find_map(|a| marker_value(a, "spec:"))
}

fn verifier_key(node: &NodeSnapshot) -> String {
    if !node.verifies_node_ids.is_empty() {
        let mut ids = node.verifies_node_ids.clone();
        ids.sort();
        ids.join("+")
    } else {
        node.expected_artifact.clone()
    }
}

fn ids(group: &[&NodeSnapshot]) -> Vec<String> {
    group.iter().map(|n| n.id.clone()).collect()
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn waste_rank(waste: WasteType) -> usize {
    WasteType::ALL
        .iter()
        .position(|known| *known == waste)
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> NodeSnapshot {
        NodeSnapshot {
            id: id.to_owned(),
            state: SnapshotState::Queued,
            title: "implement payments api".to_owned(),
            instruction: "build the payments api".to_owned(),
            dependencies: Vec::new(),
            estimated_remaining_tokens: 1_000,
            expected_artifact: "src/payments.rs".to_owned(),
            files_or_systems_affected: vec!["src/payments.rs".to_owned()],
            verification_plan: "cargo test payments".to_owned(),
            current_assumptions: Vec::new(),
            similarity_to_other_active_nodes: BTreeMap::new(),
            confidence_still_useful: 0.9,
            attempt_count: 1,
            produced_artifacts: Vec::new(),
            referenced_artifacts: Vec::new(),
            verifies_node_ids: Vec::new(),
        }
    }

    fn pair_for(waste: WasteType) -> Vec<NodeSnapshot> {
        match waste {
            WasteType::DuplicateTask => {
                let mut a = node("a");
                a.verification_plan.clear();
                let mut b = a.clone();
                b.id = "b".to_owned();
                b.similarity_to_other_active_nodes
                    .insert("a".to_owned(), 0.94);
                vec![a, b]
            }
            WasteType::DuplicateTest => {
                let mut a = node("a");
                a.title = "test payments api".to_owned();
                a.verifies_node_ids = vec!["target".to_owned()];
                let mut b = a.clone();
                b.id = "b".to_owned();
                b.similarity_to_other_active_nodes
                    .insert("a".to_owned(), 0.90);
                vec![a, b]
            }
            WasteType::DuplicateResearch => {
                let mut a = node("a");
                a.title = "research payment provider options".to_owned();
                a.instruction = "research Stripe and Adyen for payments".to_owned();
                a.verification_plan.clear();
                let mut b = a.clone();
                b.id = "b".to_owned();
                b.similarity_to_other_active_nodes
                    .insert("a".to_owned(), 0.91);
                vec![a, b]
            }
            WasteType::ConsolidatableTests => {
                let mut a = node("a");
                a.title = "test payments unit".to_owned();
                a.verifies_node_ids = vec!["target".to_owned()];
                let mut b = a.clone();
                b.id = "b".to_owned();
                b.title = "test payments integration".to_owned();
                vec![a, b]
            }
            WasteType::UnusedOutput => {
                let mut a = node("a");
                a.produced_artifacts = vec!["artifact:unused-report".to_owned()];
                a.confidence_still_useful = 0.30;
                vec![a]
            }
            WasteType::SupersededAssumption => {
                let a = node("a");
                let mut b = node("b");
                b.current_assumptions = vec!["supersedes:a because updated API landed".to_owned()];
                vec![a, b]
            }
            WasteType::SpecDrift => {
                let mut a = node("a");
                a.current_assumptions = vec!["spec:v1".to_owned()];
                let mut b = node("b");
                b.current_assumptions = vec!["spec:v2".to_owned()];
                b.similarity_to_other_active_nodes
                    .insert("a".to_owned(), 0.75);
                vec![a, b]
            }
            WasteType::ExcessiveRetries => {
                let mut a = node("a");
                a.attempt_count = 3;
                a.confidence_still_useful = 0.45;
                vec![a]
            }
            WasteType::OverlappingFiles => {
                let mut a = node("a");
                a.title = "implement payments model".to_owned();
                a.verification_plan.clear();
                let mut b = node("b");
                b.title = "add refunds model".to_owned();
                b.instruction = "add refunds data model".to_owned();
                b.verification_plan.clear();
                b.similarity_to_other_active_nodes
                    .insert("a".to_owned(), 0.40);
                vec![a, b]
            }
            WasteType::OverDecomposition => (0..4)
                .map(|i| {
                    let mut n = node(&format!("n{i}"));
                    n.estimated_remaining_tokens = 200;
                    n.title = format!("tiny payments step {i}");
                    n.verification_plan.clear();
                    n
                })
                .collect(),
            WasteType::LowValueBranch => {
                let mut a = node("a");
                a.estimated_remaining_tokens = 400;
                a.confidence_still_useful = 0.20;
                a.verification_plan.clear();
                vec![a]
            }
            WasteType::PrematureVerification => {
                let target = node("target");
                let mut verifier = node("verify");
                verifier.title = "verify target".to_owned();
                verifier.verifies_node_ids = vec!["target".to_owned()];
                vec![target, verifier]
            }
            WasteType::ExcessiveVerification => {
                let mut target = node("target");
                target.verification_plan.clear();
                let verifiers = (0..3).map(|i| {
                    let mut n = node(&format!("v{i}"));
                    n.title = format!("verify target pass {i}");
                    n.verifies_node_ids = vec!["done_target".to_owned()];
                    n
                });
                std::iter::once(target).chain(verifiers).collect()
            }
        }
    }

    #[test]
    fn detects_each_waste_type_with_required_fields() {
        for waste in WasteType::ALL {
            let nodes = pair_for(waste);
            let detections = detect_waste(&nodes);
            let detection = detections
                .iter()
                .find(|d| d.waste_type == waste)
                .unwrap_or_else(|| {
                    panic!("missing detection for {:?}; got {:?}", waste, detections)
                });
            assert!(!detection.detected_node.is_empty());
            assert_eq!(
                detection.affected_count as usize,
                detection.affected_node_ids.len()
            );
            assert!(detection.affected_count >= 1);
            assert!(!detection.estimation_basis.is_empty());
            assert!(detection.gross_avoidable_tokens > 0);
            assert!((0.0..=1.0).contains(&detection.confidence));
            assert!(!detection.proposed_stable_repair.is_empty());
        }
    }

    #[test]
    fn false_positive_protection_for_adversarial_near_matches() {
        for waste in WasteType::ALL {
            let mut nodes = pair_for(waste);
            match waste {
                WasteType::DuplicateTask
                | WasteType::DuplicateTest
                | WasteType::DuplicateResearch => {
                    nodes[1].expected_artifact = "src/other.rs".to_owned();
                    nodes[1].files_or_systems_affected = vec!["src/other.rs".to_owned()];
                    nodes[1].similarity_to_other_active_nodes.clear();
                }
                WasteType::ConsolidatableTests => {
                    nodes[1].verifies_node_ids = vec!["different".to_owned()]
                }
                WasteType::UnusedOutput => {
                    nodes[0].referenced_artifacts = vec!["artifact:input".to_owned()]
                }
                WasteType::SupersededAssumption => {
                    nodes[1].current_assumptions =
                        vec!["related to a but not replacing it".to_owned()]
                }
                WasteType::SpecDrift => nodes[1].current_assumptions = vec!["spec:v1".to_owned()],
                WasteType::ExcessiveRetries => nodes[0].attempt_count = 2,
                WasteType::OverlappingFiles => nodes[1].dependencies = vec![nodes[0].id.clone()],
                WasteType::OverDecomposition => nodes.truncate(3),
                WasteType::LowValueBranch => nodes[0].confidence_still_useful = 0.70,
                WasteType::PrematureVerification => {
                    nodes[1].verifies_node_ids = vec!["completed_not_in_snapshot".to_owned()]
                }
                WasteType::ExcessiveVerification => nodes.truncate(3),
            }
            let detections = detect_waste(&nodes);
            assert!(
                detections.iter().all(|d| d.waste_type != waste),
                "near match should not detect {:?}: {:?}",
                waste,
                detections
            );
        }
    }

    #[test]
    fn deterministic_ordering_is_stable_regardless_of_input_order() {
        let mut nodes = pair_for(WasteType::DuplicateTask);
        nodes.extend(pair_for(WasteType::ExcessiveRetries));
        let forward = detect_waste(&nodes);
        nodes.reverse();
        let reverse = detect_waste(&nodes);
        assert_eq!(forward, reverse);
    }
}
