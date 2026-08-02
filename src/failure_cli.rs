//! Read-only inspection of the additive failure and lesson graph.
//!
//! The project file is the only source consulted by these commands.  The
//! command layer deliberately builds a small, typed projection instead of
//! serializing the envelope back to callers: flattened extension fields may be
//! useful to runtime code, but they are not safe review output.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::cli::{
    GraphFailureArgs, GraphFailureCommand, GraphFailureDiffArgs, GraphFailureLessonsArgs,
    GraphFailureShowArgs, GraphFailureValidateArgs,
};
use crate::failure_graph::{
    self, EdgeRecord, EvidenceRef, FailureGraph, FailureRecord, FailureState, LessonRecord,
    LessonStatus, MAX_EVIDENCE_PER_RECORD, MAX_FAILURE_GRAPH_BYTES, MAX_STRING_CHARS,
};
use crate::learning_data::LearningData;
use crate::lessons::{self, LessonQuery};

const SHOW_SCHEMA: &str = "fractal.failure_graph_cli.show.v1";
const VALIDATE_SCHEMA: &str = "fractal.failure_graph_cli.validate.v1";
const LESSONS_SCHEMA: &str = "fractal.failure_graph_cli.lessons.v1";
const DIFF_SCHEMA: &str = "fractal.failure_graph_cli.diff.v1";
const MAX_PROJECT_BYTES: usize = MAX_FAILURE_GRAPH_BYTES.saturating_mul(8);
const MAX_OUTPUT_ITEMS: usize = 128;
const MAX_RELATED_EDGES: usize = 128;
const MAX_REF_CHARS: usize = 256;

/// Run one `fractal graph failure` operation.  Every operation is read-only:
/// no projection is materialized and no project lock is acquired.
pub(crate) fn run(args: &GraphFailureArgs) -> Result<()> {
    match &args.command {
        GraphFailureCommand::Show(args) => run_show(args),
        GraphFailureCommand::Validate(args) => run_validate(args),
        GraphFailureCommand::Lessons(args) => run_lessons(args),
        GraphFailureCommand::Diff(args) => run_diff(args),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotSource {
    Canonical,
    LegacyProjection,
    Absent,
}

impl SnapshotSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::LegacyProjection => "legacy_projection",
            Self::Absent => "absent",
        }
    }

    fn status(self) -> &'static str {
        match self {
            Self::Canonical => "valid",
            Self::LegacyProjection => "legacy_projection",
            Self::Absent => "absent",
        }
    }
}

#[derive(Clone, Debug)]
struct Snapshot {
    graph: FailureGraph,
    source: SnapshotSource,
    /// The envelope's declared hash, if it has a canonical failure graph.
    declared_hash: Option<String>,
}

fn run_show(args: &GraphFailureShowArgs) -> Result<()> {
    let snapshot = load_workspace_snapshot(&args.repo)?;
    let response = if let Some(id) = args.id.as_deref() {
        show_detail(&snapshot, id)?
    } else {
        show_summary(&snapshot)
    };
    emit_json_or_text(&response, args.json, || print_show_text(&response))
}

fn run_validate(args: &GraphFailureValidateArgs) -> Result<()> {
    match load_workspace_snapshot(&args.repo) {
        Ok(snapshot) => {
            let mut response = base_response(VALIDATE_SCHEMA, &snapshot);
            response.insert("valid".to_owned(), Value::Bool(true));
            response.insert(
                "canonical".to_owned(),
                Value::Bool(snapshot.source == SnapshotSource::Canonical),
            );
            response.insert(
                "checks".to_owned(),
                json!({
                    "schema": snapshot.source == SnapshotSource::Canonical,
                    "hash": snapshot.source == SnapshotSource::Canonical,
                    "references": snapshot.source == SnapshotSource::Canonical,
                    "secrets": snapshot.source == SnapshotSource::Canonical,
                }),
            );
            let response = Value::Object(response);
            emit_json_or_text(&response, args.json, || print_validate_text(&response))
        }
        Err(error) => {
            let response = json!({
                "schema": VALIDATE_SCHEMA,
                "status": "invalid",
                "source": "canonical",
                "valid": false,
                "canonical": true,
                "message": safe_error(&error),
            });
            emit_json_or_text(&response, args.json, || print_validate_text(&response))?;
            bail!("failure graph validation failed")
        }
    }
}

fn run_lessons(args: &GraphFailureLessonsArgs) -> Result<()> {
    let snapshot = load_workspace_snapshot(&args.repo)?;
    let query = LessonQuery {
        node_id: Some(args.node.clone()),
        capability: args.capability.clone(),
        failure_code: args.failure_code.clone(),
        objective_fingerprint: None,
    };
    let selected = lessons::select_relevant_lessons(&snapshot.graph, &query);
    let selected = selected
        .into_iter()
        .take(lessons::MAX_RELEVANT_LESSONS)
        .collect::<Vec<_>>();
    let rendered = lessons::render_lessons(&selected);
    let mut response = base_response(LESSONS_SCHEMA, &snapshot);
    response.insert(
        "node".to_owned(),
        Value::String(safe_token(&args.node, MAX_STRING_CHARS)),
    );
    if let Some(capability) = &args.capability {
        response.insert(
            "capability".to_owned(),
            Value::String(safe_token(capability, MAX_STRING_CHARS)),
        );
    }
    if let Some(failure_code) = &args.failure_code {
        response.insert(
            "failure_code".to_owned(),
            Value::String(safe_token(failure_code, MAX_STRING_CHARS)),
        );
    }
    response.insert(
        "selected_ids".to_owned(),
        Value::Array(
            selected
                .iter()
                .map(|lesson| Value::String(safe_token(&lesson.id, MAX_STRING_CHARS)))
                .collect(),
        ),
    );
    response.insert(
        "lessons".to_owned(),
        Value::Array(selected.iter().map(safe_lesson).collect()),
    );
    response.insert("rendered".to_owned(), Value::String(rendered));
    let response = Value::Object(response);
    emit_json_or_text(&response, args.json, || print_lessons_text(&response))
}

fn run_diff(args: &GraphFailureDiffArgs) -> Result<()> {
    validate_git_ref(&args.base)?;
    let base = load_git_snapshot(&args.repo, &args.base)?;
    let mut warnings = Vec::new();
    let (head, head_ref) = if let Some(head_ref) = args.head.as_deref() {
        validate_git_ref(head_ref)?;
        (
            load_git_snapshot(&args.repo, head_ref)?,
            head_ref.to_owned(),
        )
    } else {
        let snapshot = load_workspace_snapshot(&args.repo)?;
        if workspace_is_dirty(&args.repo) {
            warnings.push(
                "workspace is dirty; the default head includes uncommitted project-file changes"
                    .to_owned(),
            );
        }
        (snapshot, "workspace".to_owned())
    };
    let response = diff_response(&base, &head, &args.base, &head_ref, warnings);
    emit_json_or_text(&response, args.json, || print_diff_text(&response))
}

fn load_workspace_snapshot(repo: &Path) -> Result<Snapshot> {
    let path = repo.join(".fractal").join("project.fractal");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return empty_snapshot(SnapshotSource::Absent);
        }
        Err(_) => bail!("unable to read the project envelope"),
    };
    decode_snapshot(&bytes)
}

fn load_git_snapshot(repo: &Path, reference: &str) -> Result<Snapshot> {
    validate_git_ref(reference)?;
    let spec = format!("{reference}:.fractal/project.fractal");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("show")
        .arg("--no-ext-diff")
        .arg("--no-textconv")
        .arg(spec)
        .output()
        .context("run git show")?;
    if !output.status.success() {
        bail!("git revision does not contain a readable project envelope")
    }
    if output.stdout.len() > MAX_PROJECT_BYTES {
        bail!("git project envelope exceeds the bounded review size")
    }
    decode_snapshot(&output.stdout)
}

fn decode_snapshot(bytes: &[u8]) -> Result<Snapshot> {
    if bytes.len() > MAX_PROJECT_BYTES {
        bail!("project envelope exceeds the bounded review size")
    }
    let value: Value = serde_json::from_slice(bytes).context("project envelope is not JSON")?;
    let object = value
        .as_object()
        .context("project envelope must be a JSON object")?;
    if let Some(raw) = object.get("failure_graph") {
        if raw.is_null() {
            bail!("failure graph envelope is null")
        }
        let graph: FailureGraph =
            serde_json::from_value(raw.clone()).context("failure graph envelope is malformed")?;
        validate_canonical_graph(&graph)?;
        let declared_hash = Some(graph.failure_graph_hash.clone());
        return Ok(Snapshot {
            graph,
            source: SnapshotSource::Canonical,
            declared_hash,
        });
    }

    let graph_hash = object.get("graph_hash").and_then(Value::as_str);
    let legacy = match object.get("learning") {
        None => None,
        Some(raw) => Some(
            serde_json::from_value::<LearningData>(raw.clone())
                .context("legacy learning envelope is malformed")?,
        ),
    };
    let source = if legacy.as_ref().is_some_and(has_legacy_failures) {
        SnapshotSource::LegacyProjection
    } else {
        SnapshotSource::Absent
    };
    let graph = legacy
        .as_ref()
        .map(|learning| failure_graph::project_legacy_failures(learning, graph_hash))
        .unwrap_or_else(FailureGraph::empty);
    graph
        .normalized()
        .context("legacy failure projection is invalid")
        .map(|graph| Snapshot {
            graph,
            source,
            declared_hash: None,
        })
}

fn validate_canonical_graph(graph: &FailureGraph) -> Result<()> {
    failure_graph::validate(graph).context("failure graph references are invalid")?;
    failure_graph::validate_unknown_fields(graph)
        .context("failure graph contains forbidden credential-shaped fields")?;
    if graph.failure_graph_hash.trim().is_empty() {
        bail!("failure graph is missing its canonical hash")
    }
    let expected = failure_graph::failure_graph_hash(graph)
        .context("calculate canonical failure graph hash")?;
    if graph.failure_graph_hash != expected {
        bail!("failure graph hash does not match its canonical envelope")
    }
    Ok(())
}

fn has_legacy_failures(learning: &LearningData) -> bool {
    learning
        .nodes
        .values()
        .any(|node| node.failure_code.is_some() && node.outcome.is_some())
}

fn empty_snapshot(source: SnapshotSource) -> Result<Snapshot> {
    let mut graph = FailureGraph::empty();
    failure_graph::normalize(&mut graph).context("initialize empty failure graph")?;
    Ok(Snapshot {
        graph,
        source,
        declared_hash: None,
    })
}

fn base_response(schema: &str, snapshot: &Snapshot) -> Map<String, Value> {
    let mut response = Map::new();
    response.insert("schema".to_owned(), Value::String(schema.to_owned()));
    response.insert(
        "status".to_owned(),
        Value::String(snapshot.source.status().to_owned()),
    );
    response.insert(
        "source".to_owned(),
        Value::String(snapshot.source.as_str().to_owned()),
    );
    response.insert(
        "canonical".to_owned(),
        Value::Bool(snapshot.source == SnapshotSource::Canonical),
    );
    response.insert(
        "failure_graph_hash".to_owned(),
        Value::String(snapshot.graph.failure_graph_hash.clone()),
    );
    response.insert(
        "canonical_hash".to_owned(),
        snapshot
            .declared_hash
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    response
}

fn show_summary(snapshot: &Snapshot) -> Value {
    let (unresolved, resolved, superseded) = snapshot.graph.failures.values().fold(
        (0usize, 0usize, 0usize),
        |(unresolved, resolved, superseded), failure| match failure.state {
            FailureState::Unresolved => (unresolved + 1, resolved, superseded),
            FailureState::Resolved => (unresolved, resolved + 1, superseded),
            FailureState::Superseded => (unresolved, resolved, superseded + 1),
        },
    );
    let observations = snapshot
        .graph
        .failures
        .values()
        .map(|failure| failure.observations.len())
        .sum::<usize>();
    let mut response = base_response(SHOW_SCHEMA, snapshot);
    response.insert(
        "summary".to_owned(),
        json!({
            "failures": snapshot.graph.failures.len(),
            "lessons": snapshot.graph.lessons.len(),
            "edges": snapshot.graph.edges.len(),
            "cross_project_links": snapshot.graph.cross_project_links.len(),
            "observations": observations,
            "unresolved": unresolved,
            "resolved": resolved,
            "superseded": superseded,
        }),
    );
    Value::Object(response)
}

fn show_detail(snapshot: &Snapshot, id: &str) -> Result<Value> {
    let (record_type, record) = if let Some(record) = snapshot.graph.failures.get(id) {
        ("failure", safe_failure(record))
    } else if let Some(record) = snapshot.graph.lessons.get(id) {
        ("lesson", safe_lesson(record))
    } else if let Some(record) = snapshot.graph.edges.get(id) {
        ("edge", safe_edge(record))
    } else {
        bail!("failure graph record was not found")
    };
    let related_edges = snapshot
        .graph
        .edges
        .values()
        .filter(|edge| edge.from == id || edge.to == id)
        .take(MAX_RELATED_EDGES)
        .map(safe_edge)
        .collect::<Vec<_>>();
    let mut response = base_response(SHOW_SCHEMA, snapshot);
    response.insert(
        "id".to_owned(),
        Value::String(safe_token(id, MAX_STRING_CHARS)),
    );
    response.insert(
        "record_type".to_owned(),
        Value::String(record_type.to_owned()),
    );
    response.insert("record".to_owned(), record.clone());
    response.insert("detail".to_owned(), record.clone());
    response.insert(record_type.to_owned(), record);
    response.insert("related_edges".to_owned(), Value::Array(related_edges));
    response.insert(
        "summary".to_owned(),
        show_summary(snapshot)["summary"].clone(),
    );
    Ok(Value::Object(response))
}

fn validate_git_ref(reference: &str) -> Result<()> {
    if reference.is_empty()
        || reference.chars().count() > MAX_REF_CHARS
        || reference.starts_with('-')
        || reference.ends_with('.')
        || reference.ends_with('/')
        || reference.contains("..")
        || reference.contains("@{")
        || reference.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
    {
        bail!("unsafe git ref")
    }
    if matches!(reference, "HEAD" | "FETCH_HEAD" | "ORIG_HEAD") || is_hex_object_id(reference) {
        return Ok(());
    }
    let components = reference.split('/').collect::<Vec<_>>();
    if components.is_empty()
        || components.iter().any(|component| {
            component.is_empty()
                || *component == "."
                || *component == ".."
                || component.starts_with('.')
                || component.ends_with('.')
                || component.ends_with('\u{7f}')
        })
    {
        bail!("unsafe git ref")
    }
    if !components.iter().all(|component| {
        component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    }) {
        bail!("unsafe git ref")
    }
    Ok(())
}

fn is_hex_object_id(reference: &str) -> bool {
    matches!(reference.len(), 40 | 64) && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn workspace_is_dirty(repo: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain=v1", "--untracked-files=no"])
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}

fn diff_response(
    base: &Snapshot,
    head: &Snapshot,
    base_ref: &str,
    head_ref: &str,
    warnings: Vec<String>,
) -> Value {
    let mut changes = ChangeSet::default();
    diff_failures(base, head, &mut changes);
    diff_lessons(base, head, &mut changes);
    diff_edges(base, head, &mut changes);

    let added = json!({
        "failures": changes.added_failures,
        "lessons": changes.added_lessons,
        "edges": changes.added_edges,
    });
    let changed = json!({
        "failures": changes.changed_failures,
        "lessons": changes.changed_lessons,
        "edges": changes.changed_edges,
    });
    let removed = json!({
        "failures": changes.removed_failures,
        "lessons": changes.removed_lessons,
        "edges": changes.removed_edges,
    });
    let resolved = json!({"failures": changes.resolved_failures});
    let superseded = json!({
        "failures": changes.superseded_failures,
        "lessons": changes.superseded_lessons,
    });
    let lesson_changes = json!({
        "added": changes.added_lessons.clone(),
        "changed": changes.changed_lessons.clone(),
        "removed": changes.removed_lessons.clone(),
        "superseded": changes.superseded_lessons.clone(),
    });
    json!({
        "schema": DIFF_SCHEMA,
        "status": "ok",
        "base_ref": safe_token(base_ref, MAX_REF_CHARS),
        "head_ref": safe_token(head_ref, MAX_REF_CHARS),
        "base_source": base.source.as_str(),
        "head_source": head.source.as_str(),
        "base_hash": base.graph.failure_graph_hash,
        "head_hash": head.graph.failure_graph_hash,
        "hashes": {
            "base": base.graph.failure_graph_hash,
            "head": head.graph.failure_graph_hash,
        },
        "warnings": warnings,
        "added": added,
        "changed": changed,
        "removed": removed,
        "resolved": resolved,
        "superseded": superseded,
        "lesson_changes": lesson_changes,
        "counts": {
            "added": changes.added_count(),
            "changed": changes.changed_count(),
            "removed": changes.removed_count(),
            "resolved": changes.resolved_failures.len(),
            "superseded": changes.superseded_failures.len() + changes.superseded_lessons.len(),
        },
    })
}

#[derive(Default)]
struct ChangeSet {
    added_failures: Vec<Value>,
    added_lessons: Vec<Value>,
    added_edges: Vec<Value>,
    changed_failures: Vec<Value>,
    changed_lessons: Vec<Value>,
    changed_edges: Vec<Value>,
    removed_failures: Vec<Value>,
    removed_lessons: Vec<Value>,
    removed_edges: Vec<Value>,
    resolved_failures: Vec<Value>,
    superseded_failures: Vec<Value>,
    superseded_lessons: Vec<Value>,
}

impl ChangeSet {
    fn added_count(&self) -> usize {
        self.added_failures.len() + self.added_lessons.len() + self.added_edges.len()
    }

    fn changed_count(&self) -> usize {
        self.changed_failures.len() + self.changed_lessons.len() + self.changed_edges.len()
    }

    fn removed_count(&self) -> usize {
        self.removed_failures.len() + self.removed_lessons.len() + self.removed_edges.len()
    }
}

fn diff_failures(base: &Snapshot, head: &Snapshot, changes: &mut ChangeSet) {
    for (id, current) in &head.graph.failures {
        let Some(previous) = base.graph.failures.get(id) else {
            push_bounded(&mut changes.added_failures, compact_failure(current));
            continue;
        };
        if semantic_equal(previous, current) {
            continue;
        }
        if previous.state != FailureState::Resolved && current.state == FailureState::Resolved {
            push_bounded(&mut changes.resolved_failures, compact_failure(current));
        } else if previous.state != FailureState::Superseded
            && current.state == FailureState::Superseded
        {
            push_bounded(&mut changes.superseded_failures, compact_failure(current));
        } else {
            push_bounded(&mut changes.changed_failures, compact_failure(current));
        }
    }
    for (id, previous) in &base.graph.failures {
        if !head.graph.failures.contains_key(id) {
            push_bounded(&mut changes.removed_failures, compact_failure(previous));
        }
    }
}

fn diff_lessons(base: &Snapshot, head: &Snapshot, changes: &mut ChangeSet) {
    for (id, current) in &head.graph.lessons {
        let Some(previous) = base.graph.lessons.get(id) else {
            push_bounded(&mut changes.added_lessons, compact_lesson(current));
            continue;
        };
        if semantic_equal(previous, current) {
            continue;
        }
        if previous.status != LessonStatus::Superseded && current.status == LessonStatus::Superseded
        {
            push_bounded(&mut changes.superseded_lessons, compact_lesson(current));
        } else {
            push_bounded(&mut changes.changed_lessons, compact_lesson(current));
        }
    }
    for (id, previous) in &base.graph.lessons {
        if !head.graph.lessons.contains_key(id) {
            push_bounded(&mut changes.removed_lessons, compact_lesson(previous));
        }
    }
}

fn diff_edges(base: &Snapshot, head: &Snapshot, changes: &mut ChangeSet) {
    for (id, current) in &head.graph.edges {
        let Some(previous) = base.graph.edges.get(id) else {
            push_bounded(&mut changes.added_edges, compact_edge(current));
            continue;
        };
        if !semantic_equal(previous, current) {
            push_bounded(&mut changes.changed_edges, compact_edge(current));
        }
    }
    for (id, previous) in &base.graph.edges {
        if !head.graph.edges.contains_key(id) {
            push_bounded(&mut changes.removed_edges, compact_edge(previous));
        }
    }
}

fn semantic_equal<T: Serialize>(left: &T, right: &T) -> bool {
    let Ok(mut left) = serde_json::to_value(left) else {
        return false;
    };
    let Ok(mut right) = serde_json::to_value(right) else {
        return false;
    };
    strip_unstable_fields(&mut left);
    strip_unstable_fields(&mut right);
    left == right
}

fn strip_unstable_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let keys = object
                .keys()
                .filter(|key| {
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "timestamp"
                            | "generated_at"
                            | "observed_at"
                            | "created_at"
                            | "resolved_at"
                            | "superseded_at"
                            | "updated_at"
                    ) || key.to_ascii_lowercase().ends_with("_timestamp")
                })
                .cloned()
                .collect::<Vec<_>>();
            for key in keys {
                object.remove(&key);
            }
            for child in object.values_mut() {
                strip_unstable_fields(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                strip_unstable_fields(child);
            }
        }
        _ => {}
    }
}

fn safe_failure(record: &FailureRecord) -> Value {
    let mut value = Map::new();
    value.insert(
        "id".to_owned(),
        Value::String(safe_token(&record.id, MAX_STRING_CHARS)),
    );
    value.insert(
        "node_id".to_owned(),
        Value::String(safe_token(&record.node_id, MAX_STRING_CHARS)),
    );
    value.insert("attempt".to_owned(), json!(record.attempt));
    value.insert(
        "failure_code".to_owned(),
        Value::String(safe_token(&record.failure_code, MAX_STRING_CHARS)),
    );
    value.insert(
        "outcome".to_owned(),
        Value::String(safe_token(&record.outcome, MAX_STRING_CHARS)),
    );
    value.insert(
        "state".to_owned(),
        Value::String(failure_state(record.state).to_owned()),
    );
    value.insert(
        "summary".to_owned(),
        Value::String(safe_summary(&record.summary)),
    );
    insert_optional_token(&mut value, "capability", record.capability.as_deref());
    insert_optional_token(&mut value, "component", record.component.as_deref());
    insert_optional_ref(&mut value, "source_ref", record.source_ref.as_deref());
    value.insert(
        "evidence".to_owned(),
        Value::Array(
            record
                .evidence
                .iter()
                .take(MAX_EVIDENCE_PER_RECORD)
                .map(safe_evidence)
                .collect(),
        ),
    );
    value.insert(
        "observations".to_owned(),
        Value::Array(
            record
                .observations
                .iter()
                .take(MAX_EVIDENCE_PER_RECORD.saturating_mul(4))
                .map(|observation| {
                    let mut observation_value = Map::new();
                    observation_value.insert("attempt".to_owned(), json!(observation.attempt));
                    observation_value.insert(
                        "outcome".to_owned(),
                        Value::String(safe_token(&observation.outcome, MAX_STRING_CHARS)),
                    );
                    observation_value.insert(
                        "summary".to_owned(),
                        Value::String(safe_summary(&observation.summary)),
                    );
                    observation_value.insert(
                        "evidence".to_owned(),
                        Value::Array(
                            observation
                                .evidence
                                .iter()
                                .take(MAX_EVIDENCE_PER_RECORD)
                                .map(safe_evidence)
                                .collect(),
                        ),
                    );
                    Value::Object(observation_value)
                })
                .collect(),
        ),
    );
    insert_optional_token(&mut value, "agent", record.agent.as_deref());
    insert_optional_token(&mut value, "model", record.model.as_deref());
    insert_optional_token(&mut value, "version", record.version.as_deref());
    value.insert("observed".to_owned(), safe_provenance(&record.observed));
    if let Some(resolution) = &record.resolution {
        let mut resolution_value = Map::new();
        resolution_value.insert("success".to_owned(), Value::Bool(resolution.success));
        resolution_value.insert(
            "summary".to_owned(),
            Value::String(safe_summary(&resolution.summary)),
        );
        resolution_value.insert(
            "evidence".to_owned(),
            Value::Array(
                resolution
                    .evidence
                    .iter()
                    .take(MAX_EVIDENCE_PER_RECORD)
                    .map(safe_evidence)
                    .collect(),
            ),
        );
        insert_optional_token(
            &mut resolution_value,
            "resolved_by",
            resolution.resolved_by.as_deref(),
        );
        resolution_value.insert("observed".to_owned(), safe_provenance(&resolution.observed));
        value.insert("resolution".to_owned(), Value::Object(resolution_value));
    }
    insert_optional_token(&mut value, "superseded_by", record.superseded_by.as_deref());
    Value::Object(value)
}

fn safe_lesson(record: &LessonRecord) -> Value {
    let mut value = Map::new();
    value.insert(
        "id".to_owned(),
        Value::String(safe_token(&record.id, MAX_STRING_CHARS)),
    );
    value.insert(
        "summary".to_owned(),
        Value::String(safe_summary(&record.summary)),
    );
    value.insert(
        "status".to_owned(),
        Value::String(lesson_status(record.status).to_owned()),
    );
    insert_optional_token(&mut value, "capability", record.capability.as_deref());
    insert_optional_token(&mut value, "component", record.component.as_deref());
    insert_optional_ref(&mut value, "source_ref", record.source_ref.as_deref());
    value.insert(
        "evidence".to_owned(),
        Value::Array(
            record
                .evidence
                .iter()
                .take(MAX_EVIDENCE_PER_RECORD)
                .map(safe_evidence)
                .collect(),
        ),
    );
    // Applicability metadata is flattened for forward compatibility in the
    // typed record.  Expose only the small selector fields used by lessons.rs.
    for key in [
        "node_id",
        "failure_code",
        "objective_fingerprint",
        "objective_hash",
    ] {
        if let Some(text) = record.extra.get(key).and_then(Value::as_str) {
            if is_safe_public_text(text) {
                value.insert(
                    key.to_owned(),
                    Value::String(safe_token(text, MAX_STRING_CHARS)),
                );
            }
        }
    }
    if let Some(confidence) = record.extra.get("confidence") {
        if let Some(number) = confidence.as_f64().filter(|number| number.is_finite()) {
            value.insert(
                "confidence".to_owned(),
                json!(number.clamp(0.0, 1_000_000.0)),
            );
        }
    }
    insert_optional_token(&mut value, "agent", record.agent.as_deref());
    insert_optional_token(&mut value, "model", record.model.as_deref());
    insert_optional_token(&mut value, "version", record.version.as_deref());
    value.insert("observed".to_owned(), safe_provenance(&record.observed));
    insert_optional_token(&mut value, "superseded_by", record.superseded_by.as_deref());
    Value::Object(value)
}

fn safe_edge(record: &EdgeRecord) -> Value {
    let mut value = Map::new();
    value.insert(
        "id".to_owned(),
        Value::String(safe_token(&record.id, MAX_STRING_CHARS)),
    );
    value.insert(
        "type".to_owned(),
        Value::String(record.edge_type.as_str().to_owned()),
    );
    value.insert(
        "from".to_owned(),
        Value::String(safe_token(&record.from, MAX_STRING_CHARS)),
    );
    value.insert(
        "to".to_owned(),
        Value::String(safe_token(&record.to, MAX_STRING_CHARS)),
    );
    if let Some(evidence) = &record.evidence {
        value.insert("evidence".to_owned(), safe_evidence(evidence));
    }
    Value::Object(value)
}

fn safe_evidence(evidence: &EvidenceRef) -> Value {
    let mut value = Map::new();
    if let Some(hash) = evidence.sha256.as_deref() {
        value.insert(
            "sha256".to_owned(),
            Value::String(safe_token(hash, MAX_STRING_CHARS)),
        );
    }
    if let Some(legacy) = evidence
        .legacy_ref
        .as_deref()
        .filter(|value| is_safe_public_text(value))
    {
        value.insert(
            "legacy_ref".to_owned(),
            Value::String(safe_token(legacy, MAX_STRING_CHARS)),
        );
    }
    insert_optional_token(&mut value, "kind", evidence.kind.as_deref());
    Value::Object(value)
}

fn safe_provenance(provenance: &failure_graph::GraphGitProvenance) -> Value {
    let mut value = Map::new();
    insert_optional_token(&mut value, "graph_hash", provenance.graph_hash.as_deref());
    insert_optional_token(&mut value, "git_commit", provenance.git_commit.as_deref());
    insert_optional_token(&mut value, "git_branch", provenance.git_branch.as_deref());
    if let Some(dirty) = provenance.dirty {
        value.insert("dirty".to_owned(), Value::Bool(dirty));
    }
    Value::Object(value)
}

fn compact_failure(record: &FailureRecord) -> Value {
    json!({
        "id": safe_token(&record.id, MAX_STRING_CHARS),
        "kind": "failure",
        "node_id": safe_token(&record.node_id, MAX_STRING_CHARS),
        "failure_code": safe_token(&record.failure_code, MAX_STRING_CHARS),
        "state": failure_state(record.state),
        "summary": safe_summary(&record.summary),
        "superseded_by": record.superseded_by.as_deref().map(|id| safe_token(id, MAX_STRING_CHARS)),
    })
}

fn compact_lesson(record: &LessonRecord) -> Value {
    json!({
        "id": safe_token(&record.id, MAX_STRING_CHARS),
        "kind": "lesson",
        "status": lesson_status(record.status),
        "summary": safe_summary(&record.summary),
        "capability": record.capability.as_deref().map(|text| safe_token(text, MAX_STRING_CHARS)),
        "superseded_by": record.superseded_by.as_deref().map(|id| safe_token(id, MAX_STRING_CHARS)),
    })
}

fn compact_edge(record: &EdgeRecord) -> Value {
    json!({
        "id": safe_token(&record.id, MAX_STRING_CHARS),
        "kind": "edge",
        "type": record.edge_type.as_str(),
        "from": safe_token(&record.from, MAX_STRING_CHARS),
        "to": safe_token(&record.to, MAX_STRING_CHARS),
    })
}

fn push_bounded(values: &mut Vec<Value>, value: Value) {
    if values.len() < MAX_OUTPUT_ITEMS {
        values.push(value);
    }
}

fn insert_optional_token(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| is_safe_public_text(value)) {
        map.insert(
            key.to_owned(),
            Value::String(safe_token(value, MAX_STRING_CHARS)),
        );
    }
}

fn insert_optional_ref(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| is_safe_public_ref(value)) {
        map.insert(
            key.to_owned(),
            Value::String(safe_token(value, MAX_STRING_CHARS)),
        );
    }
}

fn safe_summary(value: &str) -> String {
    let value = failure_graph::redact_summary(value);
    if is_safe_public_text(&value) {
        value
    } else {
        "redacted summary".to_owned()
    }
}

fn safe_token(value: &str, limit: usize) -> String {
    value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(limit)
        .collect()
}

fn is_safe_public_text(value: &str) -> bool {
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
        && !lower.contains("raw log")
        && !lower.contains("log payload")
        && !lower.contains("transcript")
        && !lower.contains("raw output")
        && !lower.contains("api_key")
        && !lower.contains("authorization")
        && !lower.contains("password")
        && !lower.contains("secret")
        && !lower.contains("token")
        && !value.contains("```")
}

fn is_safe_public_ref(value: &str) -> bool {
    is_safe_public_text(value)
}

fn failure_state(state: FailureState) -> &'static str {
    match state {
        FailureState::Unresolved => "unresolved",
        FailureState::Resolved => "resolved",
        FailureState::Superseded => "superseded",
    }
}

fn lesson_status(status: LessonStatus) -> &'static str {
    match status {
        LessonStatus::Proposed => "proposed",
        LessonStatus::Adopted => "adopted",
        LessonStatus::Superseded => "superseded",
        LessonStatus::Rejected => "rejected",
    }
}

fn safe_error(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    let text = failure_graph::redact_summary(&text);
    if text.is_empty() {
        "failure graph operation failed".to_owned()
    } else {
        text
    }
}

fn emit_json_or_text(response: &Value, json_output: bool, print_text: impl FnOnce()) -> Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(response).context("encode failure graph response")?
        );
    } else {
        print_text();
    }
    Ok(())
}

fn print_show_text(response: &Value) {
    println!(
        "Failure graph: {} · source {} · hash {}",
        response["status"].as_str().unwrap_or("unknown"),
        response["source"].as_str().unwrap_or("unknown"),
        response["failure_graph_hash"].as_str().unwrap_or("unknown")
    );
    if let Some(summary) = response.get("summary") {
        println!(
            "Failures: {} ({} unresolved, {} resolved, {} superseded) · lessons: {} · edges: {} · observations: {}",
            summary["failures"].as_u64().unwrap_or(0),
            summary["unresolved"].as_u64().unwrap_or(0),
            summary["resolved"].as_u64().unwrap_or(0),
            summary["superseded"].as_u64().unwrap_or(0),
            summary["lessons"].as_u64().unwrap_or(0),
            summary["edges"].as_u64().unwrap_or(0),
            summary["observations"].as_u64().unwrap_or(0),
        );
    }
    if let Some(record) = response.get("record") {
        println!(
            "{} {}: {}",
            response["record_type"].as_str().unwrap_or("record"),
            record["id"].as_str().unwrap_or("unknown"),
            record["summary"]
                .as_str()
                .unwrap_or_else(|| record["type"].as_str().unwrap_or("")),
        );
        if let Some(edges) = response.get("related_edges").and_then(Value::as_array) {
            println!("Related edges: {}", edges.len());
        }
    }
}

fn print_validate_text(response: &Value) {
    println!(
        "Failure graph validation: {} ({})",
        response["status"].as_str().unwrap_or("unknown"),
        response["source"].as_str().unwrap_or("unknown")
    );
    if let Some(message) = response.get("message").and_then(Value::as_str) {
        println!("  {message}");
    }
}

fn print_lessons_text(response: &Value) {
    println!(
        "Relevant lessons: {} · source {}",
        response
            .get("lessons")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        response["source"].as_str().unwrap_or("unknown")
    );
    if let Some(lessons) = response.get("lessons").and_then(Value::as_array) {
        for lesson in lessons {
            println!(
                "- {} [{}] {}",
                lesson["id"].as_str().unwrap_or("unknown"),
                lesson["status"].as_str().unwrap_or("unknown"),
                lesson["summary"].as_str().unwrap_or("")
            );
        }
    }
}

fn print_diff_text(response: &Value) {
    println!(
        "Failure graph diff {} → {}",
        response["base_ref"].as_str().unwrap_or("base"),
        response["head_ref"].as_str().unwrap_or("head")
    );
    for key in ["added", "changed", "removed", "resolved", "superseded"] {
        if let Some(value) = response.get(key) {
            println!("{key}: {}", count_nested(value));
        }
    }
    if let Some(warnings) = response.get("warnings").and_then(Value::as_array) {
        for warning in warnings.iter().filter_map(Value::as_str) {
            println!("warning: {warning}");
        }
    }
}

fn count_nested(value: &Value) -> usize {
    value
        .as_object()
        .into_iter()
        .flat_map(|object| object.values())
        .filter_map(Value::as_array)
        .map(Vec::len)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure_graph::{edge_id, failure_id, lesson_id, EdgeRecord, FailureEdgeType};

    fn graph_with_records() -> FailureGraph {
        let failure_id = failure_id("build", "tool_failure");
        let lesson_id = lesson_id("Use the native checker", Some("verify"), None);
        let edge_id = edge_id(FailureEdgeType::LessonFrom, &lesson_id, &failure_id);
        let mut graph = FailureGraph::empty();
        graph.failures.insert(
            failure_id.clone(),
            FailureRecord {
                id: failure_id.clone(),
                node_id: "build".to_owned(),
                attempt: 1,
                failure_code: "tool_failure".to_owned(),
                outcome: "failed_execution".to_owned(),
                summary: "checker failed".to_owned(),
                ..FailureRecord::default()
            },
        );
        graph.lessons.insert(
            lesson_id.clone(),
            LessonRecord {
                id: lesson_id.clone(),
                summary: "Use the native checker".to_owned(),
                status: LessonStatus::Adopted,
                capability: Some("verify".to_owned()),
                evidence: vec![EvidenceRef::legacy("evidence:checker")],
                ..LessonRecord::default()
            },
        );
        graph.edges.insert(
            edge_id.clone(),
            EdgeRecord {
                id: edge_id,
                edge_type: FailureEdgeType::LessonFrom,
                from: lesson_id,
                to: failure_id,
                ..EdgeRecord::default()
            },
        );
        graph.normalized().expect("test graph normalizes")
    }

    #[test]
    fn canonical_snapshot_and_summary_are_deterministic() {
        let graph = graph_with_records();
        let value = json!({
            "schema": "fractal.project.v1",
            "graph_hash": "sha256:graph",
            "learning": {"schema": "fractal.learning.v1", "nodes": {}},
            "failure_graph": graph,
        });
        let bytes = serde_json::to_vec(&value).unwrap();
        let first = decode_snapshot(&bytes).unwrap();
        let second = decode_snapshot(&bytes).unwrap();
        assert_eq!(show_summary(&first), show_summary(&second));
        assert_eq!(first.source, SnapshotSource::Canonical);
    }

    #[test]
    fn absent_and_legacy_snapshots_are_explicit() {
        let absent = decode_snapshot(br#"{"schema":"fractal.project.v1"}"#).unwrap();
        assert_eq!(absent.source, SnapshotSource::Absent);
        let legacy = json!({
            "schema": "fractal.project.v1",
            "graph_hash": "sha256:graph",
            "learning": {
                "schema": "fractal.learning.v1",
                "nodes": {
                    "build": {
                        "node_id": "build",
                        "node_type": "implementation",
                        "objective": "Build",
                        "failure_code": "tool_failure",
                        "outcome": "failed_execution"
                    }
                }
            }
        });
        let legacy = decode_snapshot(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(legacy.source, SnapshotSource::LegacyProjection);
        assert_eq!(legacy.graph.failures.len(), 1);
    }

    #[test]
    fn unsafe_refs_are_rejected_without_shell_interpolation() {
        for reference in ["--upload-pack=x", "../../tmp", "HEAD~1", "main;touch"] {
            assert!(validate_git_ref(reference).is_err(), "{reference}");
        }
        for reference in ["HEAD", "main", "refs/heads/main", &"a".repeat(40)] {
            assert!(validate_git_ref(reference).is_ok(), "{reference}");
        }
    }

    #[test]
    fn diff_classifies_resolution_and_lesson_supersession() {
        let base = graph_with_records();
        let mut head = base.clone();
        let failure_id = failure_id("build", "tool_failure");
        let failure = head.failures.get_mut(&failure_id).unwrap();
        failure.state = FailureState::Resolved;
        failure.resolution = Some(failure_graph::FailureResolution {
            success: true,
            summary: "fixed".to_owned(),
            evidence: vec![EvidenceRef::legacy("evidence:fixed")],
            ..Default::default()
        });
        let lesson = head.lessons.values_mut().next().unwrap();
        lesson.status = LessonStatus::Superseded;
        lesson.superseded_by = Some(lesson.id.clone());
        head = head.normalized().unwrap();
        let base = Snapshot {
            declared_hash: Some(base.failure_graph_hash.clone()),
            graph: base,
            source: SnapshotSource::Canonical,
        };
        let head = Snapshot {
            declared_hash: Some(head.failure_graph_hash.clone()),
            graph: head,
            source: SnapshotSource::Canonical,
        };
        let response = diff_response(&base, &head, "HEAD", "workspace", Vec::new());
        assert_eq!(
            response["resolved"]["failures"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            response["superseded"]["lessons"].as_array().unwrap().len(),
            1
        );
        assert!(response["changed"]["failures"]
            .as_array()
            .is_some_and(Vec::is_empty));
    }

    #[test]
    fn safe_projection_omits_raw_paths_and_log_like_summaries() {
        let record = FailureRecord {
            id: "failure:n1:tool_failure".to_owned(),
            node_id: "n1".to_owned(),
            attempt: 1,
            failure_code: "tool_failure".to_owned(),
            outcome: "failed_execution".to_owned(),
            summary: "/tmp/raw.log stack trace".to_owned(),
            source_ref: Some("src/main.rs#L1".to_owned()),
            evidence: vec![EvidenceRef::legacy("raw log /tmp/raw.log")],
            ..FailureRecord::default()
        };
        let rendered = serde_json::to_string(&safe_failure(&record)).unwrap();
        assert!(!rendered.contains("/tmp/raw.log"));
        assert!(!rendered.contains("stack trace"));
        assert!(rendered.contains("redacted summary"));
    }
}
