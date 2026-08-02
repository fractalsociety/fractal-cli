//! Read-only selection and rendering of verified failure-graph lessons.
//!
//! Lessons are deliberately treated as evidence-backed hints, not as a second
//! prompt or an instruction source.  This module only reads the additive
//! failure graph, ranks deterministic matches, and renders a very small
//! prompt-safe projection for a worker.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::failure_graph::{
    EvidenceRef, FailureEdgeType, FailureGraph, LessonRecord, LessonStatus,
};

/// Maximum number of lessons sent to a worker for one node.
pub(crate) const MAX_RELEVANT_LESSONS: usize = 3;
/// Maximum UTF-8 bytes in the rendered lesson section.  The worker wrapper
/// adds its own task text around this section.
pub(crate) const MAX_RENDERED_LESSONS_BYTES: usize = 2 * 1024;

/// Inputs used by the deterministic selector.  All fields are optional so a
/// caller can select by capability alone, while retries can additionally pass
/// the observed failure code and objective fingerprint.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LessonQuery {
    pub node_id: Option<String>,
    pub capability: Option<String>,
    pub failure_code: Option<String>,
    pub objective_fingerprint: Option<String>,
}

impl LessonQuery {
    pub(crate) fn for_node(
        node_id: impl Into<String>,
        capability: impl Into<String>,
        objective: Option<&str>,
    ) -> Self {
        Self {
            node_id: Some(node_id.into()),
            capability: Some(capability.into()),
            failure_code: None,
            objective_fingerprint: objective.map(objective_fingerprint),
        }
    }
}

/// Stable objective fingerprint used in lesson applicability metadata.  The
/// objective itself is never rendered into a worker prompt or persisted as a
/// failure summary.
pub(crate) fn objective_fingerprint(objective: &str) -> String {
    fractal_contracts::canonical_sha256(&Value::String(objective.trim().to_owned()))
        .unwrap_or_else(|_| "sha256:unknown".to_owned())
}

/// Select at most [`MAX_RELEVANT_LESSONS`] adopted, evidence-backed lessons.
/// Selection is pure and does not update reuse counters or any project state.
pub(crate) fn select_relevant_lessons(
    graph: &FailureGraph,
    query: &LessonQuery,
) -> Vec<LessonRecord> {
    let mut ranked = graph
        .lessons
        .values()
        .filter(|lesson| is_eligible(graph, lesson))
        .filter_map(|lesson| relevance_score(graph, lesson, query).map(|score| (score, lesson)))
        .collect::<Vec<_>>();
    ranked.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            // BTreeMap order is stable, but retain the explicit tie-breaker so
            // callers get the same result for maps assembled in another order.
            .then_with(|| left.id.cmp(&right.id))
    });
    ranked
        .into_iter()
        .take(MAX_RELEVANT_LESSONS)
        .map(|(_, lesson)| lesson.clone())
        .collect()
}

/// Compatibility alias for callers that use the shorter selector name.
#[allow(dead_code)]
pub(crate) fn relevant_lessons(graph: &FailureGraph, query: &LessonQuery) -> Vec<LessonRecord> {
    select_relevant_lessons(graph, query)
}

/// Select and render matching lessons for one graph node.  The returned IDs
/// are used by runtime bookkeeping to append a `reused_in` edge after the final
/// outcome is known.
pub(crate) fn render_for_node(graph: &FailureGraph, node: &Value) -> (String, Vec<String>) {
    let node_id = node.get("id").and_then(Value::as_str).unwrap_or_default();
    let capability = node
        .get("capability")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let objective = node
        .get("title")
        .or_else(|| node.get("instruction"))
        .and_then(Value::as_str);
    let mut query = LessonQuery::for_node(node_id, capability, objective);

    // A retry's learning record is cleared at checkout.  Preserve matching
    // failure-code relevance by consulting unresolved records for this node;
    // this remains a read-only projection and never dumps the graph.
    if let Some(failure) = graph
        .failures
        .values()
        .filter(|failure| {
            failure.node_id == node_id
                && failure.state == crate::failure_graph::FailureState::Unresolved
                && (failure.capability.is_none()
                    || failure.capability.as_deref() == Some(capability))
        })
        .min_by(|left, right| left.failure_code.cmp(&right.failure_code))
    {
        query.failure_code = Some(failure.failure_code.clone());
    }

    let selected = select_relevant_lessons(graph, &query);
    let ids = selected.iter().map(|lesson| lesson.id.clone()).collect();
    (render_lessons(&selected), ids)
}

/// Render a compact, explicitly labelled lesson section.  Summaries and
/// evidence are bounded by the canonical failure-graph validator; this extra
/// byte bound protects the worker prompt even when future fields are added.
pub(crate) fn render_lessons(lessons: &[LessonRecord]) -> String {
    if lessons.is_empty() {
        return String::new();
    }
    let mut rendered = String::from(
        "Prior verified lessons (evidence-backed hints only; validate against current source):\n",
    );
    for lesson in lessons.iter().take(MAX_RELEVANT_LESSONS) {
        let evidence = lesson
            .evidence
            .iter()
            .map(evidence_label)
            .collect::<Vec<_>>()
            .join(",");
        let node =
            extra_string(lesson, "node_id").unwrap_or_else(|| "matched-capability".to_owned());
        let capability = lesson
            .capability
            .as_deref()
            .unwrap_or("unspecified-capability");
        let line = format!(
            "- {} [node={}, capability={}, evidence={}] {}\n",
            lesson.id,
            compact_token(&node),
            compact_token(capability),
            if evidence.is_empty() {
                "none"
            } else {
                evidence.as_str()
            },
            lesson.summary
        );
        if rendered.len().saturating_add(line.len()) > MAX_RENDERED_LESSONS_BYTES {
            break;
        }
        rendered.push_str(&line);
    }
    // A future lesson with an unusually large extension must not make this a
    // hidden prompt dump.  Keep complete UTF-8 boundaries and ensure the
    // section remains clearly labelled after truncation.
    if rendered.len() > MAX_RENDERED_LESSONS_BYTES {
        rendered.truncate(MAX_RENDERED_LESSONS_BYTES);
    }
    rendered
}

/// Compatibility alias used by prompt callers and tests.
#[allow(dead_code)]
pub(crate) fn render_relevant_lessons(lessons: &[LessonRecord]) -> String {
    render_lessons(lessons)
}

fn is_eligible(graph: &FailureGraph, lesson: &LessonRecord) -> bool {
    if lesson.status != LessonStatus::Adopted || lesson.evidence.is_empty() {
        return false;
    }
    let stale = lesson.extra.get("stale").is_some_and(|value| {
        value.as_bool() == Some(true)
            || value.as_str().is_some_and(|text| {
                matches!(text.trim().to_ascii_lowercase().as_str(), "true" | "stale")
            })
    });
    let contradicted = lesson
        .extra
        .get("contradicted")
        .is_some_and(|value| value.as_bool() == Some(true));
    if lesson.superseded_by.is_some()
        || stale
        || contradicted
        || lesson
            .extra
            .get("stale_by")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    {
        return false;
    }
    if !prompt_safe(lesson.summary.as_str())
        || extra_string(lesson, "node_id").is_some_and(|value| !prompt_safe(&value))
        || lesson
            .capability
            .as_deref()
            .is_some_and(|value| !prompt_safe(value))
        || lesson.evidence.iter().any(|evidence| {
            evidence
                .sha256
                .as_deref()
                .or(evidence.legacy_ref.as_deref())
                .is_some_and(|value| !prompt_safe(value))
        })
    {
        return false;
    }
    !graph.edges.values().any(|edge| {
        edge.edge_type == FailureEdgeType::Contradicts
            && (edge.from == lesson.id || edge.to == lesson.id)
    })
}

fn prompt_safe(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    !value.chars().any(char::is_control)
        && !value.starts_with('/')
        && !value.starts_with('~')
        && !value.contains("../")
        && !value.contains('\\')
        && !value.contains("://")
        && !value.contains("/tmp/")
        && !lower.contains("system prompt")
        && !lower.contains("assistant prompt")
        && !lower.contains("chain of thought")
        && !lower.contains("stack trace")
        && !lower.contains("transcript")
        && !lower.contains("raw output")
        && !lower.contains("api_key")
        && !lower.contains("authorization")
        && !lower.contains("password")
        && !lower.contains("secret")
        && !lower.contains("token")
        && !value.contains("```")
}

/// The tuple is ordered from strongest to weakest relevance.  The final
/// timestamp string is compared lexically because timestamps are canonical
/// RFC3339 values in runtime-produced records; no wall clock is consulted.
#[allow(clippy::type_complexity)]
fn relevance_score(
    graph: &FailureGraph,
    lesson: &LessonRecord,
    query: &LessonQuery,
) -> Option<(u8, u8, u8, u8, u8, u32, u32, String)> {
    let lesson_node = extra_string(lesson, "node_id");
    let lesson_failure = extra_string(lesson, "failure_code");
    let lesson_objective = extra_string(lesson, "objective_fingerprint")
        .or_else(|| extra_string(lesson, "objective_hash"));
    let exact_node = query
        .node_id
        .as_deref()
        .zip(lesson_node.as_deref())
        .is_some_and(|(query, lesson)| query == lesson);
    let exact_capability = query
        .capability
        .as_deref()
        .zip(lesson.capability.as_deref())
        .is_some_and(|(query, lesson)| query == lesson);
    let exact_failure = query
        .failure_code
        .as_deref()
        .zip(lesson_failure.as_deref())
        .is_some_and(|(query, lesson)| query == lesson);
    let exact_objective = query
        .objective_fingerprint
        .as_deref()
        .zip(lesson_objective.as_deref())
        .is_some_and(|(query, lesson)| query == lesson);

    // A lesson is relevant only when at least one declared applicability key
    // matches.  Empty applicability metadata is never broadcast globally.
    if !(exact_node || exact_capability || exact_failure || exact_objective) {
        return None;
    }
    let reuse_count = graph
        .edges
        .values()
        .filter(|edge| {
            edge.edge_type == FailureEdgeType::ReusedIn
                && (edge.from == lesson.id || edge.to == lesson.id)
        })
        .count()
        .min(u32::MAX as usize) as u32;
    let confidence = lesson
        .extra
        .get("confidence")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_f64().map(|number| number.max(0.0) as u64))
                .or_else(|| value.as_str()?.parse().ok())
        })
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;
    let recency = extra_string(lesson, "resolved_at")
        .or_else(|| extra_string(lesson, "updated_at"))
        .or_else(|| extra_string(lesson, "created_at"))
        .or_else(|| {
            lesson
                .observed
                .extra
                .get("resolved_at")
                .or_else(|| lesson.observed.extra.get("updated_at"))
                .or_else(|| lesson.observed.extra.get("created_at"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    Some((
        exact_node as u8,
        exact_capability as u8,
        exact_failure as u8,
        exact_objective as u8,
        (!lesson.evidence.is_empty()) as u8,
        reuse_count,
        confidence,
        recency,
    ))
}

fn extra_string(lesson: &LessonRecord, key: &str) -> Option<String> {
    lesson
        .extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
}

fn evidence_label(evidence: &EvidenceRef) -> String {
    evidence
        .sha256
        .as_deref()
        .or(evidence.legacy_ref.as_deref())
        .unwrap_or("unknown")
        .chars()
        .take(100)
        .collect()
}

fn compact_token(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() && !character.is_whitespace())
        .take(160)
        .collect()
}

/// Build a lesson's deterministic applicability extension fields.  Keeping
/// this helper here avoids several runtime call sites inventing different key
/// spellings while preserving the additive, forward-compatible record shape.
pub(crate) fn applicability_fields(
    node_id: &str,
    capability: &str,
    failure_code: &str,
    objective: &str,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("node_id".to_owned(), Value::String(node_id.to_owned())),
        (
            "failure_code".to_owned(),
            Value::String(failure_code.to_owned()),
        ),
        (
            "objective_fingerprint".to_owned(),
            Value::String(objective_fingerprint(objective)),
        ),
        (
            "applicability".to_owned(),
            Value::String(format!("node={node_id}; capability={capability}")),
        ),
        ("confidence".to_owned(), Value::Number(100_u64.into())),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure_graph::{
        edge_id, lesson_id, EdgeRecord, FailureEdgeType, GraphGitProvenance,
    };

    fn lesson(id: &str, node: &str, capability: &str, summary: &str) -> LessonRecord {
        let mut record = LessonRecord {
            id: id.to_owned(),
            summary: summary.to_owned(),
            status: LessonStatus::Adopted,
            capability: Some(capability.to_owned()),
            evidence: vec![EvidenceRef::legacy(format!("evidence:{id}"))],
            ..LessonRecord::default()
        };
        record.extra = applicability_fields(node, capability, "tool_failure", "build source");
        record.observed = GraphGitProvenance {
            graph_hash: Some("sha256:graph".to_owned()),
            ..GraphGitProvenance::default()
        };
        record
    }

    #[test]
    fn selector_is_relevant_bounded_and_deterministic() {
        let mut graph = FailureGraph::empty();
        for index in 0..5 {
            let id = format!("lesson-{index}");
            let mut record = lesson(&id, "build", "code.generate", "verified fact");
            record.extra.insert(
                "created_at".to_owned(),
                Value::String(format!("2026-01-0{}T00:00:00Z", index + 1)),
            );
            graph.lessons.insert(id, record);
        }
        graph.lessons.insert(
            "rejected".to_owned(),
            LessonRecord {
                id: "rejected".to_owned(),
                summary: "do not use".to_owned(),
                status: LessonStatus::Rejected,
                capability: Some("code.generate".to_owned()),
                evidence: vec![EvidenceRef::legacy("evidence:rejected")],
                ..LessonRecord::default()
            },
        );
        let query = LessonQuery::for_node("build", "code.generate", Some("build source"));
        let selected = select_relevant_lessons(&graph, &query);
        assert_eq!(selected.len(), MAX_RELEVANT_LESSONS);
        assert_eq!(selected[0].id, "lesson-4");
        assert_eq!(selected[1].id, "lesson-3");
        assert_eq!(selected[2].id, "lesson-2");
        let rendered = render_lessons(&selected);
        assert!(rendered.len() <= MAX_RENDERED_LESSONS_BYTES);
        assert!(rendered.contains("Prior verified lessons"));
        assert!(!rendered.contains("rejected"));
    }

    #[test]
    fn contradiction_and_stale_lessons_are_excluded() {
        let mut graph = FailureGraph::empty();
        let mut stale = lesson("stale", "build", "code.generate", "stale");
        stale.extra.insert("stale".to_owned(), Value::Bool(true));
        graph.lessons.insert(stale.id.clone(), stale);
        let contradicted = lesson("contradicted", "build", "code.generate", "contradicted");
        graph.lessons.insert(contradicted.id.clone(), contradicted);
        let other = lesson("other", "other", "code.generate", "valid");
        graph.lessons.insert(other.id.clone(), other);
        graph.edges.insert(
            edge_id(FailureEdgeType::Contradicts, "other", "contradicted"),
            EdgeRecord {
                edge_type: FailureEdgeType::Contradicts,
                from: "other".to_owned(),
                to: "contradicted".to_owned(),
                ..EdgeRecord::default()
            },
        );
        let query = LessonQuery::for_node("build", "code.generate", None);
        let selected = select_relevant_lessons(&graph, &query);
        assert!(selected.is_empty());
    }

    #[test]
    fn generated_ids_and_evidence_are_prompt_safe() {
        let id = lesson_id("verified fact", Some("code.generate"), None);
        let record = lesson(&id, "build", "code.generate", "verified fact");
        let rendered = render_lessons(&[record]);
        assert!(rendered.contains(&id));
        assert!(rendered.contains("evidence:"));
        assert!(!rendered.contains("/Users/"));
    }
}
