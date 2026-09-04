//! Compact, portable learning records stored beside the immutable graph.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NodeOutcome {
    VerifiedSuccess,
    UnverifiedSuccess,
    FailedExecution,
    FailedVerification,
    Cancelled,
    Superseded,
    HumanCompleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureCode {
    MissingDependency,
    NodeTooBroad,
    NodeTooNarrow,
    IncorrectAgent,
    InsufficientContext,
    ToolFailure,
    ConflictingParallelEdits,
    InvalidOutputSchema,
    WeakVerifier,
    Timeout,
    BudgetExceeded,
    PrematureCompletion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IntegrationFailureKind {
    InvalidOwnedPath,
    ScopeEscape,
    MissingArtifact,
    NoTrackedChanges,
    CanonicalWorkspaceChanged,
    IntegrationConflict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IntegrationFailureDetail {
    pub(crate) schema: String,
    pub(crate) kind: IntegrationFailureKind,
    pub(crate) summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) worker_commit: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Executor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<String>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Verification {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) passed: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) evidence_refs: Vec<String>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct NodeRecord {
    pub(crate) node_id: String,
    pub(crate) node_type: String,
    pub(crate) objective: String,
    #[serde(default)]
    pub(crate) depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ready_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) executor: Option<Executor>,
    #[serde(default)]
    pub(crate) attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<NodeOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) failure_code: Option<FailureCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) integration_failure: Option<IntegrationFailureDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verification: Option<Verification>,
    #[serde(default)]
    pub(crate) artifacts_produced: Vec<String>,
    #[serde(default)]
    pub(crate) consumed_by: Vec<String>,
    #[serde(default)]
    pub(crate) human_intervention: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) estimated_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) actual_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) notes: Option<String>,
    #[serde(default)]
    pub(crate) reopen_count: u32,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct GraphEditAction {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    #[serde(default)]
    pub(crate) created_nodes: Vec<String>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct EventualEffect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) success: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rework_reduced: Option<bool>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct GraphEditEvent {
    pub(crate) graph_before_hash: String,
    pub(crate) action: GraphEditAction,
    pub(crate) trigger: String,
    pub(crate) actor: String,
    pub(crate) timestamp: String,
    #[serde(default)]
    pub(crate) eventual_effect: EventualEffect,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct AcceptanceResult {
    pub(crate) id: String,
    pub(crate) passed: bool,
    #[serde(default)]
    pub(crate) evidence_refs: Vec<String>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct GraphOutcome {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) final_verified_success: Option<bool>,
    #[serde(default)]
    pub(crate) acceptance_criteria: Vec<AcceptanceResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total_duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) critical_path_duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total_agent_time_seconds: Option<f64>,
    #[serde(default)]
    pub(crate) maximum_parallelism: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total_cost: Option<f64>,
    #[serde(default)]
    pub(crate) retry_count: u32,
    #[serde(default)]
    pub(crate) reopened_node_count: u32,
    #[serde(default)]
    pub(crate) dead_or_unused_node_count: u32,
    #[serde(default)]
    pub(crate) human_intervention_count: u32,
    #[serde(default)]
    pub(crate) verification_coverage: f64,
    #[serde(default)]
    pub(crate) verification_coverage_denominator: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stopped_too_early: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expanded_unnecessarily: Option<bool>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct LearningData {
    pub(crate) schema: String,
    #[serde(default)]
    pub(crate) nodes: BTreeMap<String, NodeRecord>,
    #[serde(default)]
    pub(crate) graph_edits: Vec<GraphEditEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<GraphOutcome>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

impl Default for LearningData {
    fn default() -> Self {
        Self {
            schema: "fractal.learning.v1".to_owned(),
            nodes: BTreeMap::new(),
            graph_edits: Vec::new(),
            outcome: None,
            extra: BTreeMap::new(),
        }
    }
}

pub(crate) fn normalize(mut data: LearningData, graph: &Value, now: &str) -> LearningData {
    let mut dependencies: BTreeMap<String, Vec<String>> = BTreeMap::new();
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
            dependencies
                .entry(to.to_owned())
                .or_default()
                .push(from.to_owned());
        }
    }
    for node in graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        let depends_on = dependencies.remove(id).unwrap_or_default();
        let capability = node
            .get("capability")
            .and_then(Value::as_str)
            .unwrap_or("implementation");
        let node_type = if capability.starts_with("project.tests") || capability.contains("verify")
        {
            "verification"
        } else if capability.starts_with("control.") {
            "control"
        } else {
            "implementation"
        };
        let objective = node
            .get("title")
            .or_else(|| node.get("instruction"))
            .and_then(Value::as_str)
            .unwrap_or(id)
            .chars()
            .take(1_000)
            .collect::<String>();
        data.nodes
            .entry(id.to_owned())
            .and_modify(|record| {
                record.node_id = id.to_owned();
                record.depends_on = depends_on.clone();
                if record.node_type.trim().is_empty() {
                    record.node_type = node_type.to_owned();
                }
                if record.objective.trim().is_empty() {
                    record.objective = objective.clone();
                }
                if record.created_at.is_none() {
                    record.created_at = Some(now.to_owned());
                }
                if record.ready_at.is_none() && depends_on.is_empty() {
                    record.ready_at = Some(now.to_owned());
                }
            })
            .or_insert_with(|| NodeRecord {
                node_id: id.to_owned(),
                node_type: node_type.to_owned(),
                objective,
                ready_at: depends_on.is_empty().then(|| now.to_owned()),
                depends_on,
                created_at: Some(now.to_owned()),
                ..NodeRecord::default()
            });
    }
    data
}

pub(crate) fn validate(data: &LearningData) -> Result<(), String> {
    if data.schema != "fractal.learning.v1" {
        return Err("learning.schema must equal fractal.learning.v1".to_owned());
    }
    for (id, node) in &data.nodes {
        if id != &node.node_id || id.is_empty() || node.objective.trim().is_empty() {
            return Err(format!("invalid learning node `{id}`"));
        }
        if node.notes.as_ref().is_some_and(|note| note.len() > 1_000) {
            return Err(format!("learning node `{id}` notes exceed 1000 bytes"));
        }
        if let Some(detail) = &node.integration_failure {
            if detail.schema != "fractal.integration_failure.v1"
                || detail.summary.is_empty()
                || detail.summary.len() > 240
                || detail.summary.contains('\n')
                || detail.summary.contains('\r')
            {
                return Err(format!(
                    "learning node `{id}` has invalid integration failure detail"
                ));
            }
            if detail.worker_commit.as_ref().is_some_and(|commit| {
                !matches!(commit.len(), 40 | 64)
                    || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                return Err(format!("learning node `{id}` has invalid worker commit"));
            }
        }
        if node
            .estimated_cost
            .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
            || node
                .actual_cost
                .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
        {
            return Err(format!("learning node `{id}` has invalid cost"));
        }
        for reference in node
            .artifacts_produced
            .iter()
            .chain(node.verification.iter().flat_map(|v| &v.evidence_refs))
        {
            validate_reference(reference)?;
        }
        if node.outcome == Some(NodeOutcome::VerifiedSuccess)
            && node.verification.as_ref().and_then(|v| v.passed) != Some(true)
        {
            return Err(format!(
                "verified_success node `{id}` requires passed verification"
            ));
        }
        if node.outcome == Some(NodeOutcome::FailedVerification)
            && node.verification.as_ref().and_then(|v| v.passed) != Some(false)
        {
            return Err(format!(
                "failed_verification node `{id}` requires failed verification"
            ));
        }
        if node.outcome.is_some() && node.finished_at.is_none() {
            return Err(format!(
                "terminal learning node `{id}` requires finished_at"
            ));
        }
    }
    Ok(())
}

fn validate_reference(reference: &str) -> Result<(), String> {
    if reference.is_empty() || reference.len() > 240 || reference.chars().any(char::is_whitespace) {
        return Err("evidence and artifact references must be compact external IDs".to_owned());
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn aggregate(data: &LearningData) -> GraphOutcome {
    aggregate_with_acceptance(data, Vec::new())
}

pub(crate) fn aggregate_for_graph(data: &LearningData, graph: &Value) -> GraphOutcome {
    let mut criteria = acceptance_from_graph(graph);
    if let Some(values) = data
        .extra
        .get("acceptance_criteria")
        .and_then(Value::as_array)
    {
        criteria.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
        criteria.sort();
        criteria.dedup();
    }
    aggregate_with_acceptance(data, criteria)
}

fn aggregate_with_acceptance(data: &LearningData, criteria: Vec<String>) -> GraphOutcome {
    let nodes: Vec<_> = data.nodes.values().collect();
    let verification_denominator = nodes
        .iter()
        .filter(|node| node.node_type != "control")
        .count();
    let verified = nodes
        .iter()
        .filter(|node| node.node_type != "control")
        .filter(|node| node.verification.as_ref().and_then(|v| v.passed).is_some())
        .count();
    let intervals = complete_intervals(&nodes);
    let total_duration_seconds = intervals.as_ref().and_then(|intervals| {
        intervals
            .iter()
            .map(|(_, start, _)| *start)
            .min()
            .zip(intervals.iter().map(|(_, _, finish)| *finish).max())
            .map(|(start, finish)| finish.saturating_sub(start) as f64)
    });
    let total_agent_time_seconds = intervals.as_ref().map(|intervals| {
        intervals
            .iter()
            .map(|(_, start, finish)| finish.saturating_sub(*start))
            .sum::<u64>() as f64
    });
    let maximum_parallelism = intervals
        .as_ref()
        .map(|intervals| observed_parallelism(intervals))
        .unwrap_or(0);
    let total_cost = if nodes.is_empty() {
        None
    } else {
        nodes
            .iter()
            .map(|node| node.actual_cost)
            .try_fold(0.0, |sum, cost| cost.map(|cost| sum + cost))
    };
    let acceptance_criteria = acceptance_results(&criteria, &nodes);
    let failed_acceptance = acceptance_criteria
        .iter()
        .any(|criterion| !criterion.passed);
    let final_verified_success = final_verified_success(&nodes, failed_acceptance);
    GraphOutcome {
        final_verified_success,
        acceptance_criteria,
        total_duration_seconds,
        critical_path_duration_seconds: critical_path_seconds(data),
        total_agent_time_seconds,
        maximum_parallelism,
        total_cost,
        retry_count: nodes
            .iter()
            .map(|node| node.attempt_count.saturating_sub(1))
            .sum(),
        reopened_node_count: nodes.iter().filter(|node| node.reopen_count > 0).count() as u32,
        dead_or_unused_node_count: unused_nodes(data),
        human_intervention_count: human_intervention_count(data),
        verification_coverage: if verification_denominator == 0 {
            0.0
        } else {
            verified as f64 / verification_denominator as f64
        },
        verification_coverage_denominator: verification_denominator as u32,
        stopped_too_early: stopped_too_early(data, final_verified_success),
        expanded_unnecessarily: expanded_unnecessarily(data),
        ..GraphOutcome::default()
    }
}

/// Return all recorded, parseable node intervals only when every node has a
/// trustworthy start and finish timestamp.  A partial set would make duration
/// and concurrency metrics look more precise than the historical record is.
fn complete_intervals<'a>(nodes: &[&'a NodeRecord]) -> Option<Vec<(&'a str, u64, u64)>> {
    if nodes.is_empty() {
        return None;
    }
    nodes
        .iter()
        .map(|node| {
            let start = parse_rfc3339_seconds(node.started_at.as_deref()?)?;
            let finish = parse_rfc3339_seconds(node.finished_at.as_deref()?)?;
            (finish >= start).then_some((node.node_id.as_str(), start, finish))
        })
        .collect()
}

/// Sweep half-open intervals and report the largest number active at once.
/// Endings are applied before starts at a shared timestamp so adjacent nodes
/// do not appear concurrent; zero-length intervals still count as one start.
fn observed_parallelism(intervals: &[(&str, u64, u64)]) -> u32 {
    let mut events: BTreeMap<u64, (u32, u32)> = BTreeMap::new();
    for (_, start, finish) in intervals {
        let entry = events.entry(*start).or_default();
        entry.0 = entry.0.saturating_add(1);
        let entry = events.entry(*finish).or_default();
        entry.1 = entry.1.saturating_add(1);
    }
    let mut active = 0_u32;
    let mut maximum = 0_u32;
    for (_, (starts, finishes)) in events {
        active = active.saturating_sub(finishes);
        active = active.saturating_add(starts);
        maximum = maximum.max(active);
    }
    maximum
}

/// Extract criterion IDs from graph metadata.  Compiled graphs historically
/// placed this metadata in different envelopes, so accept the small set of
/// additive locations while keeping ordering deterministic and de-duplicated.
fn acceptance_from_graph(graph: &Value) -> Vec<String> {
    let locations = [
        graph.get("acceptance_criteria"),
        graph
            .get("prd")
            .and_then(|value| value.get("acceptance_criteria")),
        graph
            .get("metadata")
            .and_then(|value| value.get("acceptance_criteria")),
        graph.get("acceptance"),
    ];
    let mut ids = BTreeSet::new();
    for location in locations.into_iter().flatten() {
        let Some(entries) = location.as_array() else {
            continue;
        };
        for entry in entries {
            let id = entry
                .as_str()
                .or_else(|| entry.get("id").and_then(Value::as_str))
                .or_else(|| entry.get("criterion_id").and_then(Value::as_str));
            if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
                ids.insert(id.trim().chars().take(120).collect::<String>());
            }
        }
    }
    ids.into_iter().collect()
}

fn acceptance_results(criteria: &[String], nodes: &[&NodeRecord]) -> Vec<AcceptanceResult> {
    criteria
        .iter()
        .map(|criterion| {
            let matching = nodes.iter().copied().filter(|node| {
                node_matches_criterion(node, criterion)
                    || (criteria.len() == 1 && is_verification_node(node))
            });
            let matching = matching.collect::<Vec<_>>();
            let passed = !matching.is_empty()
                && matching.iter().any(|node| {
                    node.verification
                        .as_ref()
                        .and_then(|verification| verification.passed)
                        == Some(true)
                });
            let mut evidence_refs = BTreeSet::new();
            for node in matching {
                if node
                    .verification
                    .as_ref()
                    .and_then(|verification| verification.passed)
                    == Some(true)
                {
                    evidence_refs.extend(
                        node.verification
                            .as_ref()
                            .into_iter()
                            .flat_map(|verification| verification.evidence_refs.iter().cloned()),
                    );
                }
            }
            AcceptanceResult {
                id: criterion.clone(),
                passed,
                evidence_refs: evidence_refs.into_iter().collect(),
                ..AcceptanceResult::default()
            }
        })
        .collect()
}

fn is_verification_node(node: &NodeRecord) -> bool {
    node.node_type == "verification"
        || node.node_id.contains("accept")
        || node.node_id.contains("verify")
}

fn node_matches_criterion(node: &NodeRecord, criterion: &str) -> bool {
    const ID_KEYS: [&str; 5] = [
        "acceptance_id",
        "criterion_id",
        "acceptance_criterion",
        "acceptance_criteria",
        "criteria",
    ];
    ID_KEYS.iter().any(|key| {
        let Some(value) = node.extra.get(*key) else {
            return false;
        };
        value.as_str() == Some(criterion)
            || value
                .as_array()
                .is_some_and(|values| values.iter().any(|item| item.as_str() == Some(criterion)))
    })
}

fn final_verified_success(nodes: &[&NodeRecord], failed_acceptance: bool) -> Option<bool> {
    let relevant = nodes
        .iter()
        .copied()
        .filter(|node| node.node_type != "control")
        .collect::<Vec<_>>();
    if relevant.is_empty() || relevant.iter().any(|node| node.outcome.is_none()) {
        return None;
    }
    Some(
        !failed_acceptance
            && relevant.iter().all(|node| {
                matches!(
                    node.outcome,
                    Some(
                        NodeOutcome::VerifiedSuccess
                            | NodeOutcome::HumanCompleted
                            | NodeOutcome::Superseded
                    )
                )
            }),
    )
}

fn human_intervention_count(data: &LearningData) -> u32 {
    data.nodes
        .values()
        .filter(|node| node.human_intervention)
        .count() as u32
}

fn stopped_too_early(data: &LearningData, final_verified_success: Option<bool>) -> Option<bool> {
    if data.nodes.is_empty() {
        return None;
    }
    if data.nodes.values().any(|node| {
        node.failure_code == Some(FailureCode::PrematureCompletion)
            || node.outcome == Some(NodeOutcome::Cancelled)
                && node.failure_code == Some(FailureCode::PrematureCompletion)
    }) {
        return Some(true);
    }
    if data.graph_edits.iter().any(|event| {
        event.action.kind == "cancel_node" && event.eventual_effect.success == Some(false)
    }) {
        return Some(true);
    }
    let all_terminal = data.nodes.values().all(|node| node.outcome.is_some());
    if !all_terminal {
        return None;
    }
    final_verified_success.map(|_| false)
}

fn expanded_unnecessarily(data: &LearningData) -> Option<bool> {
    let expansion = ["add_branch", "add_wave_task", "evolve_graph", "split_node"];
    let relevant = data
        .graph_edits
        .iter()
        .filter(|event| expansion.contains(&event.action.kind.as_str()))
        .collect::<Vec<_>>();
    if relevant
        .iter()
        .any(|event| event.eventual_effect.success == Some(false))
    {
        return Some(true);
    }
    if relevant.is_empty() {
        return None;
    }
    if relevant
        .iter()
        .all(|event| event.eventual_effect.success.is_some())
    {
        Some(false)
    } else {
        None
    }
}

fn critical_path_seconds(data: &LearningData) -> Option<f64> {
    let mut remaining: BTreeSet<_> = data.nodes.keys().cloned().collect();
    let mut longest = BTreeMap::<String, u64>::new();
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .filter(|id| {
                data.nodes[*id]
                    .depends_on
                    .iter()
                    .all(|dependency| longest.contains_key(dependency))
            })
            .cloned()
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return None;
        }
        for id in ready {
            let node = &data.nodes[&id];
            let duration = parse_rfc3339_seconds(node.started_at.as_deref()?)
                .zip(parse_rfc3339_seconds(node.finished_at.as_deref()?))
                .map(|(start, finish)| finish.saturating_sub(start))?;
            let prior = node
                .depends_on
                .iter()
                .filter_map(|dependency| longest.get(dependency))
                .copied()
                .max()
                .unwrap_or(0);
            longest.insert(id.clone(), prior + duration);
            remaining.remove(&id);
        }
    }
    longest
        .values()
        .max()
        .copied()
        .map(|seconds| seconds as f64)
}

fn parse_rfc3339_seconds(value: &str) -> Option<u64> {
    if value.len() < 20 || !value.ends_with('Z') {
        return None;
    }
    let number = |range: std::ops::Range<usize>| value.get(range)?.parse::<i64>().ok();
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?;
    u64::try_from(seconds).ok()
}

fn unused_nodes(data: &LearningData) -> u32 {
    let used: BTreeSet<_> = data
        .nodes
        .values()
        .flat_map(|node| node.depends_on.iter())
        .collect();
    data.nodes
        .values()
        .filter(|node| node.artifacts_produced.is_empty() && !used.contains(&node.node_id))
        .filter(|node| {
            !matches!(
                node.outcome,
                Some(
                    NodeOutcome::VerifiedSuccess
                        | NodeOutcome::UnverifiedSuccess
                        | NodeOutcome::HumanCompleted
                )
            )
        })
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_node(
        id: &str,
        node_type: &str,
        start: Option<&str>,
        finish: Option<&str>,
        outcome: Option<NodeOutcome>,
    ) -> NodeRecord {
        NodeRecord {
            node_id: id.to_owned(),
            node_type: node_type.to_owned(),
            objective: id.to_owned(),
            started_at: start.map(str::to_owned),
            finished_at: finish.map(str::to_owned),
            outcome,
            ..NodeRecord::default()
        }
    }

    #[test]
    fn controlled_labels_are_exact() {
        let outcomes = [
            NodeOutcome::VerifiedSuccess,
            NodeOutcome::UnverifiedSuccess,
            NodeOutcome::FailedExecution,
            NodeOutcome::FailedVerification,
            NodeOutcome::Cancelled,
            NodeOutcome::Superseded,
            NodeOutcome::HumanCompleted,
        ];
        assert_eq!(
            outcomes.map(|value| serde_json::to_value(value).unwrap()),
            [
                "verified_success",
                "unverified_success",
                "failed_execution",
                "failed_verification",
                "cancelled",
                "superseded",
                "human_completed"
            ]
            .map(serde_json::Value::from)
        );
        assert!(serde_json::from_str::<NodeOutcome>("\"running\"").is_err());
        assert!(serde_json::from_str::<FailureCode>("\"internal_error\"").is_err());
    }

    #[test]
    fn aggregate_sequential_graph_is_deterministic_and_complete() {
        let mut first = fixture_node(
            "first",
            "implementation",
            Some("2024-01-01T00:00:00Z"),
            Some("2024-01-01T00:00:10Z"),
            Some(NodeOutcome::VerifiedSuccess),
        );
        first.verification = Some(Verification {
            kind: Some("automated".to_owned()),
            passed: Some(true),
            evidence_refs: vec!["evidence:first".to_owned()],
            ..Verification::default()
        });
        first.actual_cost = Some(2.0);
        first.attempt_count = 1;
        let mut second = fixture_node(
            "second",
            "verification",
            Some("2024-01-01T00:00:10Z"),
            Some("2024-01-01T00:00:20Z"),
            Some(NodeOutcome::VerifiedSuccess),
        );
        second.depends_on = vec!["first".to_owned()];
        second.verification = Some(Verification {
            kind: Some("automated".to_owned()),
            passed: Some(true),
            evidence_refs: vec!["evidence:second".to_owned()],
            ..Verification::default()
        });
        second.actual_cost = Some(3.0);
        second.attempt_count = 1;

        let mut data = LearningData::default();
        data.nodes.insert("first".to_owned(), first);
        data.nodes.insert("second".to_owned(), second);
        let outcome = aggregate(&data);

        assert_eq!(outcome.final_verified_success, Some(true));
        assert_eq!(outcome.total_duration_seconds, Some(20.0));
        assert_eq!(outcome.critical_path_duration_seconds, Some(20.0));
        assert_eq!(outcome.total_agent_time_seconds, Some(20.0));
        assert_eq!(outcome.maximum_parallelism, 1);
        assert_eq!(outcome.total_cost, Some(5.0));
        assert_eq!(outcome.verification_coverage, 1.0);
        assert_eq!(outcome.verification_coverage_denominator, 2);
        assert_eq!(outcome.retry_count, 0);
        assert_eq!(outcome.dead_or_unused_node_count, 0);
    }

    #[test]
    fn aggregate_parallel_retry_failure_and_unknown_facts_are_explicit() {
        let mut good = fixture_node(
            "good",
            "implementation",
            Some("2024-01-01T00:00:00Z"),
            Some("2024-01-01T00:00:10Z"),
            Some(NodeOutcome::VerifiedSuccess),
        );
        good.verification = Some(Verification {
            passed: Some(true),
            ..Verification::default()
        });
        let mut failed = fixture_node(
            "failed",
            "implementation",
            Some("2024-01-01T00:00:00Z"),
            Some("2024-01-01T00:00:10Z"),
            Some(NodeOutcome::FailedVerification),
        );
        failed.verification = Some(Verification {
            passed: Some(false),
            ..Verification::default()
        });
        failed.failure_code = Some(FailureCode::WeakVerifier);
        failed.attempt_count = 2;
        failed.reopen_count = 1;
        let cancelled = fixture_node(
            "cancelled",
            "implementation",
            None,
            Some("2024-01-01T00:00:10Z"),
            Some(NodeOutcome::Cancelled),
        );
        let mut data = LearningData::default();
        data.nodes.insert("good".to_owned(), good);
        data.nodes.insert("failed".to_owned(), failed);
        data.nodes.insert("cancelled".to_owned(), cancelled);

        let outcome = aggregate(&data);

        assert_eq!(outcome.final_verified_success, Some(false));
        assert_eq!(outcome.total_duration_seconds, None);
        assert_eq!(outcome.total_agent_time_seconds, None);
        assert_eq!(outcome.maximum_parallelism, 0);
        assert_eq!(outcome.total_cost, None);
        assert_eq!(outcome.retry_count, 1);
        assert_eq!(outcome.reopened_node_count, 1);
        assert_eq!(outcome.verification_coverage_denominator, 3);
        assert_eq!(outcome.verification_coverage, 2.0 / 3.0);
    }

    #[test]
    fn aggregate_acceptance_and_edit_flags_use_only_recorded_evidence() {
        let mut acceptance = fixture_node(
            "acceptance",
            "verification",
            Some("2024-01-01T00:00:00Z"),
            Some("2024-01-01T00:00:01Z"),
            Some(NodeOutcome::VerifiedSuccess),
        );
        acceptance
            .extra
            .insert("acceptance_id".to_owned(), Value::String("AC-1".to_owned()));
        acceptance.verification = Some(Verification {
            passed: Some(true),
            evidence_refs: vec!["evidence:ac-1".to_owned()],
            ..Verification::default()
        });
        let mut data = LearningData::default();
        data.nodes.insert("acceptance".to_owned(), acceptance);
        data.graph_edits.push(GraphEditEvent {
            graph_before_hash: "sha256:before".to_owned(),
            action: GraphEditAction {
                kind: "add_branch".to_owned(),
                created_nodes: vec!["acceptance".to_owned()],
                ..GraphEditAction::default()
            },
            trigger: "repair".to_owned(),
            actor: "agent".to_owned(),
            timestamp: "2024-01-01T00:00:00Z".to_owned(),
            eventual_effect: EventualEffect {
                success: Some(false),
                rework_reduced: Some(false),
                ..EventualEffect::default()
            },
            ..GraphEditEvent::default()
        });
        let graph = serde_json::json!({
            "acceptance_criteria": [{"id": "AC-1"}, {"id": "AC-2"}]
        });
        let outcome = aggregate_for_graph(&data, &graph);

        assert_eq!(
            outcome.acceptance_criteria,
            vec![
                AcceptanceResult {
                    id: "AC-1".to_owned(),
                    passed: true,
                    evidence_refs: vec!["evidence:ac-1".to_owned()],
                    ..AcceptanceResult::default()
                },
                AcceptanceResult {
                    id: "AC-2".to_owned(),
                    passed: false,
                    ..AcceptanceResult::default()
                }
            ]
        );
        assert_eq!(outcome.final_verified_success, Some(false));
        assert_eq!(outcome.expanded_unnecessarily, Some(true));
        assert_eq!(outcome.stopped_too_early, Some(false));
    }
}
