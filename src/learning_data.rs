//! Compact, portable learning records stored beside the immutable graph.

use serde::{Deserialize, Serialize};
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Executor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Verification {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) passed: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) evidence_refs: Vec<String>,
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
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct GraphEditAction {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    #[serde(default)]
    pub(crate) created_nodes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct EventualEffect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) success: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rework_reduced: Option<bool>,
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
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct AcceptanceResult {
    pub(crate) id: String,
    pub(crate) passed: bool,
    #[serde(default)]
    pub(crate) evidence_refs: Vec<String>,
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
    #[serde(default)]
    pub(crate) total_cost: f64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stopped_too_early: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expanded_unnecessarily: Option<bool>,
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
}

impl Default for LearningData {
    fn default() -> Self {
        Self {
            schema: "fractal.learning.v1".to_owned(),
            nodes: BTreeMap::new(),
            graph_edits: Vec::new(),
            outcome: None,
        }
    }
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

pub(crate) fn aggregate(data: &LearningData) -> GraphOutcome {
    let nodes: Vec<_> = data.nodes.values().collect();
    let verified = nodes
        .iter()
        .filter(|node| node.verification.as_ref().and_then(|v| v.passed).is_some())
        .count();
    let terminal_success = nodes.iter().all(|node| {
        matches!(
            node.outcome,
            Some(
                NodeOutcome::VerifiedSuccess
                    | NodeOutcome::UnverifiedSuccess
                    | NodeOutcome::HumanCompleted
                    | NodeOutcome::Superseded
            )
        )
    });
    let intervals = nodes
        .iter()
        .filter_map(|node| {
            Some((
                node.node_id.as_str(),
                parse_rfc3339_seconds(node.started_at.as_deref()?)?,
                parse_rfc3339_seconds(node.finished_at.as_deref()?)?,
            ))
        })
        .collect::<Vec<_>>();
    let total_duration_seconds = intervals
        .iter()
        .map(|(_, start, _)| *start)
        .min()
        .zip(intervals.iter().map(|(_, _, finish)| *finish).max())
        .map(|(start, finish)| finish.saturating_sub(start) as f64);
    let total_agent_time_seconds = (!intervals.is_empty()).then(|| {
        intervals
            .iter()
            .map(|(_, start, finish)| finish.saturating_sub(*start))
            .sum::<u64>() as f64
    });
    let mut events = intervals
        .iter()
        .flat_map(|(_, start, finish)| [(*start, 1_i32), (*finish, -1_i32)])
        .collect::<Vec<_>>();
    events.sort_by_key(|(at, delta)| (*at, *delta));
    let mut active = 0_i32;
    let mut maximum_parallelism = 0_i32;
    for (_, delta) in events {
        active += delta;
        maximum_parallelism = maximum_parallelism.max(active);
    }
    GraphOutcome {
        final_verified_success: Some(
            terminal_success
                && nodes
                    .iter()
                    .filter(|n| n.node_type != "control")
                    .all(|node| {
                        matches!(
                            node.outcome,
                            Some(
                                NodeOutcome::VerifiedSuccess
                                    | NodeOutcome::HumanCompleted
                                    | NodeOutcome::Superseded
                            )
                        )
                    }),
        ),
        total_duration_seconds,
        critical_path_duration_seconds: critical_path_seconds(data),
        total_agent_time_seconds,
        maximum_parallelism: maximum_parallelism as u32,
        total_cost: nodes.iter().filter_map(|node| node.actual_cost).sum(),
        retry_count: nodes
            .iter()
            .map(|node| node.attempt_count.saturating_sub(1))
            .sum(),
        reopened_node_count: nodes.iter().filter(|node| node.reopen_count > 0).count() as u32,
        dead_or_unused_node_count: unused_nodes(data),
        human_intervention_count: nodes.iter().filter(|node| node.human_intervention).count()
            as u32,
        verification_coverage: if nodes.is_empty() {
            0.0
        } else {
            verified as f64 / nodes.len() as f64
        },
        ..GraphOutcome::default()
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
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
