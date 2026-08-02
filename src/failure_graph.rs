//! Canonical, additive failure and lesson records for a portable project.
//!
//! The failure graph deliberately lives beside (and never inside) the immutable
//! execution graph.  Records are reference-only, bounded, deterministic JSON;
//! prompts, transcripts, logs, diffs, secrets, and machine-local paths are not
//! part of this contract.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FAILURE_GRAPH_SCHEMA: &str = "fractal.failure_graph.v1";
pub const MAX_FAILURES: usize = 512;
pub const MAX_LESSONS: usize = 512;
pub const MAX_EDGES: usize = 2_048;
pub const MAX_CROSS_PROJECT_LINKS: usize = 512;
pub const MAX_EVIDENCE_PER_RECORD: usize = 20;
pub const MAX_FAILURE_GRAPH_BYTES: usize = 262_144;
pub const MAX_ID_BYTES: usize = 256;
pub const MAX_SUMMARY_CHARS: usize = 512;
pub const MAX_STRING_CHARS: usize = 512;
pub const MAX_SOURCE_REF_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureState {
    #[default]
    Unresolved,
    Resolved,
    Superseded,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonStatus {
    #[default]
    Proposed,
    Adopted,
    Superseded,
    Rejected,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureEdgeType {
    #[default]
    CausedBy,
    ResolvedBy,
    LessonFrom,
    AppliesTo,
    RelatedComponent,
    Supersedes,
    ReusedIn,
    Contradicts,
    RetryOf,
}

impl FailureEdgeType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CausedBy => "caused_by",
            Self::ResolvedBy => "resolved_by",
            Self::LessonFrom => "lesson_from",
            Self::AppliesTo => "applies_to",
            Self::RelatedComponent => "related_component",
            Self::Supersedes => "supersedes",
            Self::ReusedIn => "reused_in",
            Self::Contradicts => "contradicts",
            Self::RetryOf => "retry_of",
        }
    }
}

/// A reference to a small, externally stored evidence item.  `sha256` is the
/// preferred form; `legacy_ref` permits importing historical IDs without
/// pretending that the content was hashed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl EvidenceRef {
    pub fn sha256(value: impl Into<String>) -> Self {
        Self {
            sha256: Some(value.into()),
            ..Self::default()
        }
    }

    pub fn legacy(value: impl Into<String>) -> Self {
        Self {
            legacy_ref: Some(value.into()),
            ..Self::default()
        }
    }

    fn stable_key(&self) -> String {
        self.sha256
            .as_deref()
            .map(|value| format!("sha256:{value}"))
            .or_else(|| {
                self.legacy_ref
                    .as_deref()
                    .map(|value| format!("legacy:{value}"))
            })
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphGitProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureObservation {
    pub attempt: u32,
    pub outcome: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(alias = "evidence_refs")]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    #[serde(alias = "provenance")]
    pub observed: GraphGitProvenance,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureResolution {
    pub success: bool,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(alias = "evidence_refs")]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    #[serde(default)]
    #[serde(alias = "provenance")]
    pub observed: GraphGitProvenance,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    /// Stable key repeated in the value so hand-edited files can be checked.
    #[serde(default)]
    pub id: String,
    pub node_id: String,
    pub attempt: u32,
    pub failure_code: String,
    pub outcome: String,
    #[serde(default)]
    pub state: FailureState,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// A repo-relative source path, optionally with a `#fragment` selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(alias = "evidence_refs")]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<FailureObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub observed: GraphGitProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<FailureResolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonRecord {
    #[serde(default)]
    pub id: String,
    pub summary: String,
    #[serde(default)]
    pub status: LessonStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub observed: GraphGitProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRecord {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type")]
    pub edge_type: FailureEdgeType,
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<EvidenceRef>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossProjectLink {
    #[serde(default)]
    pub id: String,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<EvidenceRef>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureGraph {
    pub schema: String,
    #[serde(default)]
    pub failures: BTreeMap<String, FailureRecord>,
    #[serde(default)]
    pub lessons: BTreeMap<String, LessonRecord>,
    #[serde(default)]
    pub edges: BTreeMap<String, EdgeRecord>,
    #[serde(default, alias = "links")]
    pub cross_project_links: BTreeMap<String, CrossProjectLink>,
    #[serde(default)]
    pub failure_graph_hash: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl FailureGraph {
    pub fn empty() -> Self {
        Self {
            schema: FAILURE_GRAPH_SCHEMA.to_owned(),
            ..Self::default()
        }
    }

    /// Normalize, validate, and calculate the graph hash in one operation.
    pub fn normalized(mut self) -> Result<Self> {
        normalize(&mut self)?;
        Ok(self)
    }
}

/// Stable key for all observations belonging to a node/failure-code family.
/// Retry attempts are observations, not separate records, so a retry never
/// overwrites the prior evidence.
pub fn failure_id(node_id: &str, failure_code: &str) -> String {
    format!("failure:{}:{}", node_id.trim(), failure_code.trim())
}

pub fn failure_key(node_id: &str, failure_code: &str) -> String {
    failure_id(node_id, failure_code)
}

pub fn lesson_id(summary: &str, capability: Option<&str>, component: Option<&str>) -> String {
    let capability = capability.unwrap_or_default();
    let component = component.unwrap_or_default();
    format!(
        "lesson:{}:{}:{}",
        slug_key(summary),
        slug_key(capability),
        slug_key(component)
    )
}

pub fn lesson_key(summary: &str, capability: Option<&str>, component: Option<&str>) -> String {
    lesson_id(summary, capability, component)
}

pub fn edge_id(edge_type: FailureEdgeType, from: &str, to: &str) -> String {
    format!("edge:{}:{}:{}", edge_type.as_str(), from.trim(), to.trim())
}

pub fn edge_key(edge_type: FailureEdgeType, from: &str, to: &str) -> String {
    edge_id(edge_type, from, to)
}

/// Collapse whitespace/control characters while retaining useful Unicode text.
/// Inputs that look like prompts or logs are rejected by `validate_summary`;
/// this helper is intentionally only a bounded presentation sanitizer.
pub fn redact_summary(value: &str) -> String {
    let mut result = String::new();
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !result.is_empty();
            continue;
        }
        if pending_space {
            result.push(' ');
            pending_space = false;
        }
        result.push(character);
        if result.chars().count() >= MAX_SUMMARY_CHARS {
            break;
        }
    }
    result.trim().to_owned()
}

pub fn sanitize_summary(value: &str) -> Result<String> {
    if value.chars().count() > MAX_SUMMARY_CHARS {
        bail!("summary exceeds {MAX_SUMMARY_CHARS} characters");
    }
    let value = redact_summary(value);
    validate_summary(&value)?;
    Ok(value)
}

/// Normalize all bounded values, canonical map IDs, ordering, and the hash.
pub fn normalize(graph: &mut FailureGraph) -> Result<()> {
    // A caller may hand us a stale/self-referential hash. It is derived data,
    // so discard it before validating the canonical payload and recalculate it
    // below from the hash-free representation.
    graph.failure_graph_hash.clear();
    if graph.schema.trim().is_empty() {
        graph.schema = FAILURE_GRAPH_SCHEMA.to_owned();
    }
    if graph.schema != FAILURE_GRAPH_SCHEMA {
        bail!("failure_graph.schema must equal {FAILURE_GRAPH_SCHEMA}");
    }

    let mut failures = BTreeMap::new();
    for (map_key, mut record) in std::mem::take(&mut graph.failures) {
        let id = if record.id.trim().is_empty() {
            failure_id(&record.node_id, &record.failure_code)
        } else {
            record.id.clone()
        };
        record.id = id.clone();
        normalize_failure(&mut record)?;
        if failures.insert(id.clone(), record).is_some() {
            bail!("duplicate failure id `{id}` (map key `{map_key}`)");
        }
    }
    graph.failures = failures;

    let mut lessons = BTreeMap::new();
    for (map_key, mut lesson) in std::mem::take(&mut graph.lessons) {
        let id = if lesson.id.trim().is_empty() {
            lesson_id(
                &lesson.summary,
                lesson.capability.as_deref(),
                lesson.component.as_deref(),
            )
        } else {
            lesson.id.clone()
        };
        lesson.id = id.clone();
        normalize_lesson(&mut lesson)?;
        if lessons.insert(id.clone(), lesson).is_some() {
            bail!("duplicate lesson id `{id}` (map key `{map_key}`)");
        }
    }
    graph.lessons = lessons;

    let mut edges = BTreeMap::new();
    for (map_key, mut edge) in std::mem::take(&mut graph.edges) {
        if edge.from.trim().is_empty() || edge.to.trim().is_empty() {
            bail!("failure edge endpoints must not be empty");
        }
        let id = if edge.id.trim().is_empty() {
            edge_id(edge.edge_type, &edge.from, &edge.to)
        } else {
            edge.id.clone()
        };
        edge.id = id.clone();
        validate_id(&id, "failure edge id")?;
        if let Some(evidence) = edge.evidence.as_mut() {
            normalize_evidence(evidence)?;
        }
        if edges.insert(id.clone(), edge).is_some() {
            bail!("duplicate failure edge id `{id}` (map key `{map_key}`)");
        }
    }
    graph.edges = edges;

    let mut links = BTreeMap::new();
    for (map_key, mut link) in std::mem::take(&mut graph.cross_project_links) {
        if link.id.trim().is_empty() {
            let target = link
                .failure_id
                .as_deref()
                .or(link.lesson_id.as_deref())
                .unwrap_or("project");
            link.id = format!("link:{}:{}", link.project.trim(), target);
        }
        let id = link.id.clone();
        normalize_link(&mut link)?;
        if links.insert(id.clone(), link).is_some() {
            bail!("duplicate cross-project link id `{id}` (map key `{map_key}`)");
        }
    }
    graph.cross_project_links = links;

    validate(graph)?;
    graph.failure_graph_hash = hash_without_hash(graph)?;
    validate(graph)?;
    let encoded = serde_json::to_vec(graph).context("encode failure graph")?;
    if encoded.len() > MAX_FAILURE_GRAPH_BYTES {
        bail!(
            "failure graph is {} bytes, exceeding {} byte sync limit",
            encoded.len(),
            MAX_FAILURE_GRAPH_BYTES
        );
    }
    Ok(())
}

pub fn validate(graph: &FailureGraph) -> Result<()> {
    if graph.schema != FAILURE_GRAPH_SCHEMA {
        bail!("failure_graph.schema must equal {FAILURE_GRAPH_SCHEMA}");
    }
    let encoded = serde_json::to_value(graph).context("encode failure graph for validation")?;
    legacy_value_has_secret(&encoded)?;
    if graph.failures.len() > MAX_FAILURES {
        bail!("failure graph exceeds {MAX_FAILURES} failures");
    }
    if graph.lessons.len() > MAX_LESSONS {
        bail!("failure graph exceeds {MAX_LESSONS} lessons");
    }
    if graph.edges.len() > MAX_EDGES {
        bail!("failure graph exceeds {MAX_EDGES} edges");
    }
    if graph.cross_project_links.len() > MAX_CROSS_PROJECT_LINKS {
        bail!("failure graph exceeds {MAX_CROSS_PROJECT_LINKS} cross-project links");
    }
    for (id, failure) in &graph.failures {
        if id != &failure.id {
            bail!(
                "failure map key `{id}` does not match record id `{}`",
                failure.id
            );
        }
        validate_failure(failure)?;
        if let Some(target) = &failure.superseded_by {
            if !graph.failures.contains_key(target) {
                bail!("failure `{id}` supersedes unknown failure `{target}`");
            }
        }
    }
    for (id, lesson) in &graph.lessons {
        if id != &lesson.id {
            bail!(
                "lesson map key `{id}` does not match record id `{}`",
                lesson.id
            );
        }
        validate_lesson(lesson)?;
        if let Some(target) = &lesson.superseded_by {
            if !graph.lessons.contains_key(target) {
                bail!("lesson `{id}` supersedes unknown lesson `{target}`");
            }
        }
    }
    for (id, edge) in &graph.edges {
        if id != &edge.id {
            bail!("edge map key `{id}` does not match record id `{}`", edge.id);
        }
        validate_edge(edge, graph)?;
    }
    for (id, link) in &graph.cross_project_links {
        if id != &link.id {
            bail!(
                "cross-project link map key `{id}` does not match record id `{}`",
                link.id
            );
        }
        validate_link(link, graph)?;
    }
    if !graph.failure_graph_hash.is_empty() {
        let expected = hash_without_hash(graph)?;
        if graph.failure_graph_hash != expected {
            bail!("failure_graph_hash does not match canonical failure graph");
        }
    }
    let encoded = serde_json::to_vec(graph).context("encode failure graph")?;
    if encoded.len() > MAX_FAILURE_GRAPH_BYTES {
        bail!(
            "failure graph is {} bytes, exceeding {} byte sync limit",
            encoded.len(),
            MAX_FAILURE_GRAPH_BYTES
        );
    }
    Ok(())
}

pub fn failure_graph_hash(graph: &FailureGraph) -> Result<String> {
    hash_without_hash(graph)
}

/// Compatibility-friendly names used by runtime/UI callers and contract
/// tests. They all route through the single canonical implementation above.
pub fn canonical_failure_graph_hash(graph: &FailureGraph) -> Result<String> {
    failure_graph_hash(graph)
}

pub fn canonical_hash(graph: &FailureGraph) -> Result<String> {
    failure_graph_hash(graph)
}

pub fn normalize_failure_graph(graph: &mut FailureGraph) -> Result<()> {
    normalize(graph)
}

pub fn validate_failure_graph(graph: &FailureGraph) -> Result<()> {
    validate(graph)
}

pub fn redact(graph: &mut FailureGraph) -> Result<()> {
    normalize(graph)
}

pub fn redact_failure_graph(graph: &mut FailureGraph) -> Result<()> {
    normalize(graph)
}

fn hash_without_hash(graph: &FailureGraph) -> Result<String> {
    let mut value = serde_json::to_value(graph).context("encode failure graph for hashing")?;
    let object = value
        .as_object_mut()
        .context("failure graph must serialize as an object")?;
    object.remove("failure_graph_hash");
    strip_unstable_timestamps(&mut value);
    fractal_contracts::canonical_sha256(&value).context("hash canonical failure graph")
}

fn strip_unstable_timestamps(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let keys = object
                .keys()
                .filter(|key| is_unstable_timestamp_key(key))
                .cloned()
                .collect::<Vec<_>>();
            for key in keys {
                object.remove(&key);
            }
            for child in object.values_mut() {
                strip_unstable_timestamps(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                strip_unstable_timestamps(child);
            }
        }
        _ => {}
    }
}

fn is_unstable_timestamp_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "timestamp"
        || key == "generated_at"
        || key == "observed_at"
        || key == "created_at"
        || key == "resolved_at"
        || key == "superseded_at"
        || key.ends_with("_timestamp")
}

fn normalize_failure(record: &mut FailureRecord) -> Result<()> {
    record.node_id = bounded_string(&record.node_id, MAX_ID_BYTES, "failure node_id")?;
    record.failure_code = bounded_string(&record.failure_code, MAX_ID_BYTES, "failure code")?;
    record.outcome = bounded_string(&record.outcome, MAX_ID_BYTES, "failure outcome")?;
    record.summary = sanitize_summary(&record.summary)?;
    validate_id(&record.id, "failure id")?;
    if record.attempt == 0 {
        record.attempt = 1;
    }
    normalize_optional_string(
        &mut record.capability,
        MAX_STRING_CHARS,
        "failure capability",
    )?;
    normalize_optional_string(&mut record.component, MAX_STRING_CHARS, "failure component")?;
    normalize_source_ref(&mut record.source_ref)?;
    normalize_optional_string(&mut record.agent, MAX_STRING_CHARS, "failure agent")?;
    normalize_optional_string(&mut record.model, MAX_STRING_CHARS, "failure model")?;
    normalize_optional_string(&mut record.version, MAX_STRING_CHARS, "failure version")?;
    normalize_provenance(&mut record.observed)?;
    for evidence in &mut record.evidence {
        normalize_evidence(evidence)?;
    }
    record.evidence.sort_by_key(EvidenceRef::stable_key);
    if record.evidence.len() > MAX_EVIDENCE_PER_RECORD {
        bail!(
            "failure `{}` exceeds {MAX_EVIDENCE_PER_RECORD} evidence refs",
            record.id
        );
    }
    for observation in &mut record.observations {
        normalize_observation(observation)?;
    }
    if record.observations.len() > MAX_EVIDENCE_PER_RECORD.saturating_mul(4) {
        bail!("failure `{}` has too many retry observations", record.id);
    }
    if let Some(resolution) = record.resolution.as_mut() {
        normalize_resolution(resolution)?;
    }
    normalize_optional_string(
        &mut record.superseded_by,
        MAX_ID_BYTES,
        "superseded failure",
    )?;
    Ok(())
}

fn validate_failure(record: &FailureRecord) -> Result<()> {
    validate_id(&record.id, "failure id")?;
    validate_id(&record.node_id, "failure node_id")?;
    validate_id(&record.failure_code, "failure code")?;
    validate_id(&record.outcome, "failure outcome")?;
    validate_summary(&record.summary)?;
    if record.attempt == 0 {
        bail!("failure `{}` attempt must be positive", record.id);
    }
    validate_optional_string(
        record.capability.as_deref(),
        MAX_STRING_CHARS,
        "failure capability",
    )?;
    validate_optional_string(
        record.component.as_deref(),
        MAX_STRING_CHARS,
        "failure component",
    )?;
    validate_source_ref(record.source_ref.as_deref())?;
    validate_optional_string(record.agent.as_deref(), MAX_STRING_CHARS, "failure agent")?;
    validate_optional_string(record.model.as_deref(), MAX_STRING_CHARS, "failure model")?;
    validate_optional_string(
        record.version.as_deref(),
        MAX_STRING_CHARS,
        "failure version",
    )?;
    validate_provenance(&record.observed)?;
    validate_evidence_list(&record.evidence, &format!("failure `{}`", record.id))?;
    for observation in &record.observations {
        validate_observation(observation)?;
    }
    if record.observations.len() > MAX_EVIDENCE_PER_RECORD.saturating_mul(4) {
        bail!("failure `{}` has too many retry observations", record.id);
    }
    if let Some(resolution) = &record.resolution {
        validate_resolution(resolution)?;
    }
    match record.state {
        FailureState::Unresolved => {
            if record.resolution.is_some() {
                bail!(
                    "unresolved failure `{}` cannot have a resolution",
                    record.id
                );
            }
            if record.superseded_by.is_some() {
                bail!("unresolved failure `{}` cannot be superseded", record.id);
            }
        }
        FailureState::Resolved => {
            let resolution = record
                .resolution
                .as_ref()
                .context("resolved failure requires resolution")?;
            if !resolution.success || resolution.evidence.is_empty() {
                bail!(
                    "resolved failure `{}` requires successful evidence",
                    record.id
                );
            }
        }
        FailureState::Superseded => {
            let target = record
                .superseded_by
                .as_ref()
                .context("superseded failure requires superseded_by")?;
            validate_id(target, "superseded failure")?;
        }
    }
    Ok(())
}

fn normalize_lesson(lesson: &mut LessonRecord) -> Result<()> {
    lesson.summary = sanitize_summary(&lesson.summary)?;
    validate_id(&lesson.id, "lesson id")?;
    normalize_optional_string(
        &mut lesson.capability,
        MAX_STRING_CHARS,
        "lesson capability",
    )?;
    normalize_optional_string(&mut lesson.component, MAX_STRING_CHARS, "lesson component")?;
    normalize_source_ref(&mut lesson.source_ref)?;
    normalize_optional_string(&mut lesson.agent, MAX_STRING_CHARS, "lesson agent")?;
    normalize_optional_string(&mut lesson.model, MAX_STRING_CHARS, "lesson model")?;
    normalize_optional_string(&mut lesson.version, MAX_STRING_CHARS, "lesson version")?;
    normalize_provenance(&mut lesson.observed)?;
    for evidence in &mut lesson.evidence {
        normalize_evidence(evidence)?;
    }
    lesson.evidence.sort_by_key(EvidenceRef::stable_key);
    if lesson.evidence.len() > MAX_EVIDENCE_PER_RECORD {
        bail!(
            "lesson `{}` exceeds {MAX_EVIDENCE_PER_RECORD} evidence refs",
            lesson.id
        );
    }
    normalize_optional_string(&mut lesson.superseded_by, MAX_ID_BYTES, "superseded lesson")?;
    Ok(())
}

fn validate_lesson(lesson: &LessonRecord) -> Result<()> {
    validate_id(&lesson.id, "lesson id")?;
    validate_summary(&lesson.summary)?;
    validate_optional_string(
        lesson.capability.as_deref(),
        MAX_STRING_CHARS,
        "lesson capability",
    )?;
    validate_optional_string(
        lesson.component.as_deref(),
        MAX_STRING_CHARS,
        "lesson component",
    )?;
    validate_source_ref(lesson.source_ref.as_deref())?;
    validate_optional_string(lesson.agent.as_deref(), MAX_STRING_CHARS, "lesson agent")?;
    validate_optional_string(lesson.model.as_deref(), MAX_STRING_CHARS, "lesson model")?;
    validate_optional_string(
        lesson.version.as_deref(),
        MAX_STRING_CHARS,
        "lesson version",
    )?;
    validate_provenance(&lesson.observed)?;
    validate_evidence_list(&lesson.evidence, &format!("lesson `{}`", lesson.id))?;
    if lesson.status == LessonStatus::Superseded {
        let target = lesson
            .superseded_by
            .as_ref()
            .context("superseded lesson requires superseded_by")?;
        validate_id(target, "superseded lesson")?;
    } else if lesson.superseded_by.is_some() {
        bail!("only superseded lessons may set superseded_by");
    }
    Ok(())
}

fn normalize_observation(observation: &mut FailureObservation) -> Result<()> {
    if observation.attempt == 0 {
        observation.attempt = 1;
    }
    observation.outcome =
        bounded_string(&observation.outcome, MAX_ID_BYTES, "observation outcome")?;
    observation.summary = sanitize_summary(&observation.summary)?;
    normalize_optional_string(
        &mut observation.agent,
        MAX_STRING_CHARS,
        "observation agent",
    )?;
    normalize_optional_string(
        &mut observation.model,
        MAX_STRING_CHARS,
        "observation model",
    )?;
    normalize_optional_string(
        &mut observation.version,
        MAX_STRING_CHARS,
        "observation version",
    )?;
    normalize_provenance(&mut observation.observed)?;
    for evidence in &mut observation.evidence {
        normalize_evidence(evidence)?;
    }
    observation.evidence.sort_by_key(EvidenceRef::stable_key);
    if observation.evidence.len() > MAX_EVIDENCE_PER_RECORD {
        bail!("observation exceeds {MAX_EVIDENCE_PER_RECORD} evidence refs");
    }
    Ok(())
}

fn validate_observation(observation: &FailureObservation) -> Result<()> {
    if observation.attempt == 0 {
        bail!("failure observation attempt must be positive");
    }
    validate_id(&observation.outcome, "observation outcome")?;
    validate_summary(&observation.summary)?;
    validate_optional_string(
        observation.agent.as_deref(),
        MAX_STRING_CHARS,
        "observation agent",
    )?;
    validate_optional_string(
        observation.model.as_deref(),
        MAX_STRING_CHARS,
        "observation model",
    )?;
    validate_optional_string(
        observation.version.as_deref(),
        MAX_STRING_CHARS,
        "observation version",
    )?;
    validate_provenance(&observation.observed)?;
    validate_evidence_list(&observation.evidence, "failure observation")
}

fn normalize_resolution(resolution: &mut FailureResolution) -> Result<()> {
    resolution.summary = sanitize_summary(&resolution.summary)?;
    normalize_optional_string(
        &mut resolution.resolved_by,
        MAX_STRING_CHARS,
        "resolution actor",
    )?;
    normalize_provenance(&mut resolution.observed)?;
    for evidence in &mut resolution.evidence {
        normalize_evidence(evidence)?;
    }
    resolution.evidence.sort_by_key(EvidenceRef::stable_key);
    Ok(())
}

fn validate_resolution(resolution: &FailureResolution) -> Result<()> {
    validate_summary(&resolution.summary)?;
    validate_optional_string(
        resolution.resolved_by.as_deref(),
        MAX_STRING_CHARS,
        "resolution actor",
    )?;
    validate_provenance(&resolution.observed)?;
    validate_evidence_list(&resolution.evidence, "failure resolution")
}

fn normalize_evidence(evidence: &mut EvidenceRef) -> Result<()> {
    normalize_optional_string(&mut evidence.sha256, MAX_STRING_CHARS, "evidence sha256")?;
    normalize_optional_string(
        &mut evidence.legacy_ref,
        MAX_STRING_CHARS,
        "legacy evidence ref",
    )?;
    normalize_optional_string(&mut evidence.kind, MAX_STRING_CHARS, "evidence kind")?;
    normalize_source_ref(&mut evidence.path)?;
    let has_sha = evidence.sha256.is_some();
    let has_legacy = evidence.legacy_ref.is_some();
    if has_sha == has_legacy {
        bail!("evidence must contain exactly one of sha256 or legacy_ref");
    }
    Ok(())
}

fn validate_evidence_list(list: &[EvidenceRef], owner: &str) -> Result<()> {
    if list.len() > MAX_EVIDENCE_PER_RECORD {
        bail!("{owner} exceeds {MAX_EVIDENCE_PER_RECORD} evidence refs");
    }
    let mut keys = BTreeSet::new();
    for evidence in list {
        let has_sha = evidence.sha256.is_some();
        let has_legacy = evidence.legacy_ref.is_some();
        if has_sha == has_legacy {
            bail!("{owner} evidence must contain exactly one of sha256 or legacy_ref");
        }
        validate_optional_string(
            evidence.sha256.as_deref(),
            MAX_STRING_CHARS,
            "evidence sha256",
        )?;
        validate_optional_string(
            evidence.legacy_ref.as_deref(),
            MAX_STRING_CHARS,
            "legacy evidence ref",
        )?;
        validate_optional_string(evidence.kind.as_deref(), MAX_STRING_CHARS, "evidence kind")?;
        validate_source_ref(evidence.path.as_deref())?;
        if let Some(sha256) = evidence.sha256.as_deref() {
            validate_sha256(sha256)?;
        }
        if let Some(legacy) = evidence.legacy_ref.as_deref() {
            validate_legacy_ref(legacy)?;
        }
        if !keys.insert(evidence.stable_key()) {
            bail!("{owner} contains duplicate evidence refs");
        }
    }
    Ok(())
}

fn normalize_provenance(provenance: &mut GraphGitProvenance) -> Result<()> {
    normalize_optional_string(
        &mut provenance.graph_hash,
        MAX_STRING_CHARS,
        "observed graph hash",
    )?;
    normalize_optional_string(&mut provenance.git_commit, MAX_STRING_CHARS, "git commit")?;
    normalize_optional_string(&mut provenance.git_branch, MAX_STRING_CHARS, "git branch")?;
    normalize_optional_string(
        &mut provenance.source_repo,
        MAX_SOURCE_REF_CHARS,
        "source repo",
    )?;
    Ok(())
}

fn validate_provenance(provenance: &GraphGitProvenance) -> Result<()> {
    validate_optional_string(
        provenance.graph_hash.as_deref(),
        MAX_STRING_CHARS,
        "observed graph hash",
    )?;
    validate_optional_string(
        provenance.git_commit.as_deref(),
        MAX_STRING_CHARS,
        "git commit",
    )?;
    validate_optional_string(
        provenance.git_branch.as_deref(),
        MAX_STRING_CHARS,
        "git branch",
    )?;
    validate_optional_string(
        provenance.source_repo.as_deref(),
        MAX_SOURCE_REF_CHARS,
        "source repo",
    )
}

fn normalize_link(link: &mut CrossProjectLink) -> Result<()> {
    validate_id(&link.id, "cross-project link id")?;
    link.project = bounded_string(&link.project, MAX_ID_BYTES, "cross-project project")?;
    normalize_optional_string(&mut link.failure_id, MAX_ID_BYTES, "linked failure")?;
    normalize_optional_string(&mut link.lesson_id, MAX_ID_BYTES, "linked lesson")?;
    normalize_source_ref(&mut link.source_ref)?;
    if let Some(evidence) = link.evidence.as_mut() {
        normalize_evidence(evidence)?;
    }
    Ok(())
}

fn validate_link(link: &CrossProjectLink, graph: &FailureGraph) -> Result<()> {
    validate_id(&link.id, "cross-project link id")?;
    validate_id(&link.project, "cross-project project")?;
    if link.failure_id.is_some() == link.lesson_id.is_some() {
        bail!("cross-project link must target exactly one failure or lesson");
    }
    if let Some(id) = &link.failure_id {
        validate_id(id, "linked failure")?;
        if !graph.failures.contains_key(id) {
            bail!("cross-project link references unknown failure `{id}`");
        }
    }
    if let Some(id) = &link.lesson_id {
        validate_id(id, "linked lesson")?;
        if !graph.lessons.contains_key(id) {
            bail!("cross-project link references unknown lesson `{id}`");
        }
    }
    validate_source_ref(link.source_ref.as_deref())?;
    if let Some(evidence) = &link.evidence {
        validate_evidence_list(std::slice::from_ref(evidence), "cross-project link")?;
    }
    Ok(())
}

fn validate_edge(edge: &EdgeRecord, graph: &FailureGraph) -> Result<()> {
    validate_id(&edge.id, "failure edge id")?;
    validate_id(&edge.from, "failure edge source")?;
    validate_id(&edge.to, "failure edge target")?;
    let from_known =
        graph.failures.contains_key(&edge.from) || graph.lessons.contains_key(&edge.from);
    let to_known = graph.failures.contains_key(&edge.to) || graph.lessons.contains_key(&edge.to);
    let endpoint_can_be_external = matches!(
        edge.edge_type,
        FailureEdgeType::AppliesTo | FailureEdgeType::RelatedComponent | FailureEdgeType::ReusedIn
    );
    if !from_known {
        bail!(
            "failure edge `{}` references unknown source `{}`",
            edge.id,
            edge.from
        );
    }
    if !to_known && !endpoint_can_be_external {
        bail!(
            "failure edge `{}` references unknown target `{}`",
            edge.id,
            edge.to
        );
    }
    if edge.edge_type == FailureEdgeType::RetryOf
        && (!graph.failures.contains_key(&edge.from) || !graph.failures.contains_key(&edge.to))
    {
        bail!("retry_of edge must connect two failures");
    }
    if let Some(evidence) = &edge.evidence {
        validate_evidence_list(std::slice::from_ref(evidence), "failure edge")?;
    }
    Ok(())
}

fn validate_summary(summary: &str) -> Result<()> {
    if summary.trim().is_empty() {
        bail!("failure/lesson summary must not be empty");
    }
    if summary.chars().count() > MAX_SUMMARY_CHARS {
        bail!("summary exceeds {MAX_SUMMARY_CHARS} characters");
    }
    if summary.chars().any(char::is_control) {
        bail!("summary contains control characters");
    }
    let lower = summary.to_ascii_lowercase();
    if lower.contains("system prompt")
        || lower.contains("assistant prompt")
        || lower.contains("chain of thought")
        || lower.contains("stack trace")
        || lower.contains("```")
    {
        bail!("summary resembles a prompt, transcript, or log payload");
    }
    Ok(())
}

fn bounded_string(value: &str, limit: usize, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{label} must not be empty");
    }
    if value.chars().count() > limit {
        bail!("{label} exceeds {limit} characters");
    }
    if value.chars().any(|character| character.is_control()) {
        bail!("{label} contains control characters");
    }
    Ok(value.to_owned())
}

fn normalize_optional_string(value: &mut Option<String>, limit: usize, label: &str) -> Result<()> {
    if let Some(current) = value {
        *current = bounded_string(current, limit, label)?;
    }
    Ok(())
}

fn validate_optional_string(value: Option<&str>, limit: usize, label: &str) -> Result<()> {
    if let Some(value) = value {
        bounded_string(value, limit, label)?;
    }
    Ok(())
}

fn validate_id(value: &str, label: &str) -> Result<()> {
    bounded_string(value, MAX_ID_BYTES, label)
        .map(|_| ())
        .and_then(|_| {
            if value.starts_with('/') || value.contains("../") || value.contains("\\") {
                bail!("{label} must not be absolute or traversal-shaped")
            }
            Ok(())
        })
}

fn normalize_source_ref(value: &mut Option<String>) -> Result<()> {
    if let Some(current) = value {
        *current = bounded_string(current, MAX_SOURCE_REF_CHARS, "source reference")?;
        validate_source_ref(Some(current.as_str()))?;
    }
    Ok(())
}

fn validate_source_ref(value: Option<&str>) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    bounded_string(value, MAX_SOURCE_REF_CHARS, "source reference")?;
    if value.starts_with('/')
        || value.starts_with('~')
        || value.contains("../")
        || value.contains("\\")
        || value.contains("://")
    {
        bail!("source reference must be repo-relative and must not traverse or use a URL");
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    let digest = value.strip_prefix("sha256:").unwrap_or(value);
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("evidence sha256 must be a 64-hex digest, optionally prefixed sha256:");
    }
    Ok(())
}

fn validate_legacy_ref(value: &str) -> Result<()> {
    if value.starts_with('/')
        || value.starts_with('~')
        || value.contains("../")
        || value.contains('\\')
    {
        bail!("legacy evidence ref must not contain absolute or traversal paths");
    }
    Ok(())
}

fn slug_key(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character.to_ascii_lowercase());
        } else if !result.ends_with('-') {
            result.push('-');
        }
        if result.len() >= 80 {
            break;
        }
    }
    result.trim_matches('-').to_owned()
}

/// Build a lossless-enough, reference-only projection of existing learning
/// failures.  This function is pure: callers can use it on `load` without
/// writing a newly materialized `failure_graph` key.
pub fn project_legacy_failures(
    learning: &crate::learning_data::LearningData,
    graph_hash: Option<&str>,
) -> FailureGraph {
    let mut graph = FailureGraph::empty();
    for record in learning.nodes.values() {
        let Some(code) = record.failure_code else {
            continue;
        };
        let Some(outcome) = record.outcome else {
            continue;
        };
        let outcome = serde_json::to_string(&outcome)
            .unwrap_or_else(|_| "\"failed\"".to_owned())
            .trim_matches('"')
            .to_owned();
        let code = serde_json::to_string(&code)
            .unwrap_or_else(|_| "\"unknown\"".to_owned())
            .trim_matches('"')
            .to_owned();
        let id = failure_id(&record.node_id, &code);
        let summary = redact_summary(record.notes.as_deref().unwrap_or(record.objective.as_str()));
        let summary = if validate_summary(&summary).is_ok() {
            summary
        } else {
            format!("legacy failure on {}", redact_summary(&record.node_id))
        };
        let mut failure = FailureRecord {
            id: id.clone(),
            node_id: record.node_id.clone(),
            attempt: record.attempt_count.max(1),
            failure_code: code.clone(),
            outcome: outcome.clone(),
            state: FailureState::Unresolved,
            summary: if summary.is_empty() {
                record.node_id.clone()
            } else {
                summary
            },
            observed: GraphGitProvenance {
                graph_hash: graph_hash.map(str::to_owned),
                ..GraphGitProvenance::default()
            },
            ..FailureRecord::default()
        };
        failure.observations.push(FailureObservation {
            attempt: failure.attempt,
            outcome,
            summary: failure.summary.clone(),
            evidence: record
                .verification
                .as_ref()
                .map(|verification| {
                    verification
                        .evidence_refs
                        .iter()
                        .filter(|reference| validate_legacy_ref(reference).is_ok())
                        .map(|reference| EvidenceRef::legacy(reference.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            agent: record
                .executor
                .as_ref()
                .and_then(|executor| executor.agent.clone()),
            model: record
                .executor
                .as_ref()
                .and_then(|executor| executor.model.clone()),
            version: record
                .executor
                .as_ref()
                .and_then(|executor| executor.version.clone()),
            observed: GraphGitProvenance {
                graph_hash: graph_hash.map(str::to_owned),
                ..GraphGitProvenance::default()
            },
            ..FailureObservation::default()
        });
        graph.failures.insert(id, failure);
    }
    if normalize(&mut graph).is_err() {
        // Legacy learning data is historical input. If an old free-form field
        // cannot satisfy the new bounded contract, return an empty-but-valid
        // envelope rather than failing a read or fabricating facts.
        graph = FailureGraph::empty();
        let _ = normalize(&mut graph);
    }
    graph
}

fn legacy_value_has_secret(value: &Value) -> Result<()> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let normalized = key.to_ascii_lowercase().replace('-', "_");
                if matches!(
                    normalized.as_str(),
                    "access_token"
                        | "api_key"
                        | "authorization"
                        | "credentials"
                        | "password"
                        | "private_key"
                        | "refresh_token"
                        | "secret"
                        | "secrets"
                        | "token"
                ) {
                    bail!("failure graph contains forbidden credential field `{key}`");
                }
                legacy_value_has_secret(child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                legacy_value_has_secret(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Validate flattened extension fields for secrets without restricting their
/// shape; unknown fields remain round-trippable and forward-compatible.
pub fn validate_unknown_fields(graph: &FailureGraph) -> Result<()> {
    let encoded = serde_json::to_value(graph).context("encode failure graph unknown fields")?;
    legacy_value_has_secret(&encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn failure() -> FailureRecord {
        FailureRecord {
            node_id: "build".to_owned(),
            attempt: 1,
            failure_code: "tool_failure".to_owned(),
            outcome: "failed_execution".to_owned(),
            summary: "compiler failed".to_owned(),
            ..FailureRecord::default()
        }
    }

    #[test]
    fn normalization_assigns_stable_ids_and_hash() -> Result<()> {
        let mut graph = FailureGraph {
            schema: FAILURE_GRAPH_SCHEMA.to_owned(),
            failures: [("ignored".to_owned(), failure())].into_iter().collect(),
            ..FailureGraph::empty()
        };
        normalize(&mut graph)?;
        assert_eq!(graph.failures.len(), 1);
        assert_eq!(
            graph.failures.keys().next().unwrap(),
            "failure:build:tool_failure"
        );
        assert!(graph.failure_graph_hash.starts_with("sha256:"));
        assert_eq!(graph.failure_graph_hash, failure_graph_hash(&graph)?);
        Ok(())
    }

    #[test]
    fn resolution_requires_success_evidence() {
        let mut graph = FailureGraph {
            schema: FAILURE_GRAPH_SCHEMA.to_owned(),
            failures: [(
                failure_id("build", "tool_failure"),
                FailureRecord {
                    id: failure_id("build", "tool_failure"),
                    state: FailureState::Resolved,
                    resolution: Some(FailureResolution {
                        success: true,
                        summary: "fixed".to_owned(),
                        ..FailureResolution::default()
                    }),
                    ..failure()
                },
            )]
            .into_iter()
            .collect(),
            ..FailureGraph::empty()
        };
        assert!(normalize(&mut graph).is_err());
    }

    #[test]
    fn source_refs_reject_absolute_and_traversal_paths() {
        assert!(validate_source_ref(Some("/tmp/log")).is_err());
        assert!(validate_source_ref(Some("src/../secret")).is_err());
        assert!(validate_source_ref(Some("src/main.rs")).is_ok());
    }

    #[test]
    fn timestamp_fields_do_not_change_hash() -> Result<()> {
        let mut graph = FailureGraph {
            schema: FAILURE_GRAPH_SCHEMA.to_owned(),
            failures: [(failure_id("build", "tool_failure"), failure())]
                .into_iter()
                .collect(),
            extra: [(
                "future".to_owned(),
                json!({"generated_at": "one", "keep": true}),
            )]
            .into_iter()
            .collect(),
            ..FailureGraph::empty()
        };
        normalize(&mut graph)?;
        let before = graph.failure_graph_hash.clone();
        graph.extra.insert(
            "future".to_owned(),
            json!({"generated_at": "two", "keep": true}),
        );
        assert_eq!(before, failure_graph_hash(&graph)?);
        Ok(())
    }
}
