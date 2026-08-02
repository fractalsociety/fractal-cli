//! Standardized, portable per-project execution graph.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// Included here so catalog persistence can ship without clap/main wiring yet.
// Later command registration may re-declare this as a crate-root module.
#[path = "project_catalog.rs"]
pub(crate) mod project_catalog;

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct FractalProject {
    pub(crate) schema: String,
    pub(crate) project: ProjectIdentity,
    pub(crate) graph_hash: String,
    pub(crate) graph: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) execution: Option<ExecutionState>,
    #[serde(default)]
    pub(crate) learning: crate::learning_data::LearningData,
    /// Optional efficiency ledger stored beside execution and learning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) efficiency: Option<crate::efficiency::EfficiencyData>,
    /// Additive failure/lesson graph. This field is intentionally outside the
    /// immutable execution graph and therefore never contributes to
    /// `graph_hash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) failure_graph: Option<crate::failure_graph::FailureGraph>,
    pub(crate) updated_at: String,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ProjectIdentity {
    pub(crate) slug: String,
    pub(crate) title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prompt: Option<String>,
    pub(crate) visibility: String,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ExecutionState {
    pub(crate) schema: String,
    pub(crate) phase: String,
    pub(crate) assignments: BTreeMap<String, ExecutionAssignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) progress: Option<PlanningProgress>,
    pub(crate) updated_at: String,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PlanningProgress {
    pub(crate) schema: String,
    pub(crate) message: String,
    pub(crate) step: u32,
    pub(crate) elapsed_seconds: u64,
    pub(crate) agent_label: String,
    pub(crate) source: String,
    pub(crate) updated_at: String,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ExecutionAssignment {
    pub(crate) agent_id: String,
    pub(crate) agent_label: String,
    pub(crate) state: String,
    pub(crate) checked_out_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) released_at: Option<String>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

static PROJECT_FILE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
const MANAGED_IDENTITY_SCHEMA: &str = "fractal.managed-project-identity.v1";
// A writer creates the lock and writes its PID immediately.  An empty or
// malformed lock therefore indicates a crash during creation, but it must not
// be removed immediately: another process could still be between create and
// write.  Keep the recovery window deliberately conservative.
const STALE_LOCK_AGE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

struct ProjectWriteGuard {
    path: PathBuf,
}

impl ProjectWriteGuard {
    fn acquire(workspace: &Path) -> Result<Self> {
        let directory = workspace.join(".fractal");
        fs::create_dir_all(&directory)
            .with_context(|| format!("create {}", directory.display()))?;
        let path = directory.join("project.fractal.lock");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    file.sync_all()?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if stale_lock(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        bail!(
                            "timed out waiting for canonical project lock {}",
                            path.display()
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("lock {}", path.display()))
                }
            }
        }
    }
}

fn stale_lock(path: &Path) -> bool {
    stale_lock_at(path, SystemTime::now())
}

fn stale_lock_at(path: &Path, now: SystemTime) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    if let Ok(pid) = contents.trim().parse::<i32>() {
        if pid > 0 {
            #[cfg(unix)]
            {
                let result = unsafe { libc::kill(pid, 0) };
                if result == 0 {
                    // A live owner always wins, even when the lock file is
                    // old.  This also protects against PID reuse races.
                    return false;
                }
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
                    // The process exists but cannot be inspected by us.
                    return false;
                }
                let errno = std::io::Error::last_os_error().raw_os_error();
                if errno == Some(libc::ESRCH) {
                    return true;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = pid;
            }
        }
    }
    // Invalid/empty/non-positive PID contents are only recoverable after a
    // conservative age threshold.  If metadata or the clock is unavailable,
    // fail closed and retain the lock.
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= STALE_LOCK_AGE)
}

impl Drop for ProjectWriteGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ManagedProjectIdentity {
    schema: String,
    slug: String,
    title: String,
    #[serde(default)]
    prompt: Option<String>,
}

pub(crate) fn path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal").join("project.fractal")
}

/// Pin the user-confirmed name for a managed voice project. Every later
/// planning/execution persist reads this record, so lead request text cannot
/// replace the dashboard title or hosted URL slug.
pub(crate) fn configure_managed_identity(workspace: &Path, name: &str, prompt: &str) -> Result<()> {
    let title = clean_title(name, workspace);
    let slug = slug_from(&title);
    let identity = ManagedProjectIdentity {
        schema: MANAGED_IDENTITY_SCHEMA.to_owned(),
        slug,
        title,
        prompt: Some(prompt.trim().to_owned()),
    };
    let destination = managed_identity_path(workspace);
    let directory = destination.parent().expect("managed identity has parent");
    fs::create_dir_all(directory).with_context(|| format!("create {}", directory.display()))?;
    atomic_write(&destination, &serde_json::to_vec_pretty(&identity)?)
}

pub(crate) fn persist(workspace: &Path, graph: &Value, title: &str) -> Result<PathBuf> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let graph_hash = graph
        .get("graph_hash")
        .and_then(Value::as_str)
        .context("execution graph is missing graph_hash")?;
    if graph.get("schema").and_then(Value::as_str) != Some("fractal.execution_graph.v1") {
        bail!("only fractal.execution_graph.v1 can be stored in a project.fractal file");
    }
    crate::graph_store::verify_graph_document(graph)
        .context("refuse to persist an execution graph with an invalid hash")?;
    reject_secret_fields(graph)?;
    let managed_identity = load_managed_identity(workspace)?;
    let slug = managed_identity
        .as_ref()
        .map(|identity| identity.slug.clone())
        .unwrap_or_else(|| slug_for(workspace));
    let title = managed_identity
        .as_ref()
        .map(|identity| identity.title.clone())
        .unwrap_or_else(|| clean_title(title, workspace));
    let prompt = managed_identity
        .as_ref()
        .and_then(|identity| identity.prompt.clone())
        .unwrap_or_else(|| title.clone());
    let now = timestamp();
    let current = load(workspace).ok();
    let execution = current
        .as_ref()
        .filter(|current| current.graph_hash == graph_hash)
        .and_then(|current| current.execution.clone())
        .or_else(|| {
            current
                .as_ref()
                .and_then(|current| execution_from_parent(current, graph, &now))
        })
        .or_else(|| {
            Some(ExecutionState {
                schema: "fractal.execution_state.v1".to_owned(),
                phase: if is_planning_preview(graph) {
                    "planning"
                } else {
                    "executing"
                }
                .to_owned(),
                assignments: BTreeMap::new(),
                progress: None,
                updated_at: now.clone(),
                extra: BTreeMap::new(),
            })
        });
    let mut learning = current
        .as_ref()
        .map(|document| merge_learning(&document.learning, graph, &now))
        .unwrap_or_else(|| learning_from_graph(graph, &now));
    if let Some(criteria) = load_acceptance_criteria(workspace) {
        learning.extra.insert(
            "acceptance_criteria".to_owned(),
            Value::Array(criteria.into_iter().map(Value::String).collect()),
        );
    }
    let efficiency = current
        .as_ref()
        .and_then(|document| document.efficiency.clone())
        .or_else(|| {
            let config = crate::efficiency_config::EfficiencyConfig::default();
            Some(crate::efficiency::EfficiencyData::for_config(
                config.mode,
                &config.config_hash(),
            ))
        });
    let document = FractalProject {
        schema: "fractal.project.v1".to_owned(),
        project: ProjectIdentity {
            slug,
            title,
            prompt: Some(prompt),
            visibility: current
                .as_ref()
                .map(|document| document.project.visibility.clone())
                .unwrap_or_else(|| "private".to_owned()),
            extra: current
                .as_ref()
                .map(|document| document.project.extra.clone())
                .unwrap_or_default(),
        },
        graph_hash: graph_hash.to_owned(),
        graph: graph.clone(),
        execution,
        learning,
        efficiency,
        failure_graph: current
            .as_ref()
            .and_then(|document| document.failure_graph.clone()),
        updated_at: now,
        extra: current
            .as_ref()
            .map(|document| document.extra.clone())
            .unwrap_or_default(),
    };
    let destination = path(workspace);
    let directory = destination.parent().expect("project file has parent");
    fs::create_dir_all(directory).with_context(|| format!("create {}", directory.display()))?;
    let bytes = serde_json::to_vec_pretty(&document)?;
    atomic_write(&destination, &bytes)?;
    Ok(destination)
}

/// Repoint an existing portable project at a committed evolved child graph while
/// retaining its human-facing title. Lineage-compatible execution attribution is
/// preserved by `persist`, so completed parent nodes do not disappear from the
/// cloud graph when a verifier or repair node is grafted.
pub(crate) fn persist_evolved(workspace: &Path, graph: &Value) -> Result<PathBuf> {
    let title = load(workspace)
        .map(|document| document.project.title)
        .unwrap_or_else(|_| slug_for(workspace));
    persist(workspace, graph, &title)
}

fn execution_from_parent(
    current: &FractalProject,
    graph: &Value,
    updated_at: &str,
) -> Option<ExecutionState> {
    if graph.get("parent_graph").and_then(Value::as_str) != Some(current.graph_hash.as_str()) {
        return None;
    }
    let mut execution = current.execution.clone()?;
    let node_ids = graph
        .get("nodes")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|node| node.get("id").and_then(Value::as_str))
        .collect::<std::collections::BTreeSet<_>>();
    execution
        .assignments
        .retain(|node, _| node_ids.contains(node.as_str()));
    execution.phase = "executing".to_owned();
    execution.progress = None;
    execution.updated_at = updated_at.to_owned();
    Some(execution)
}

#[allow(dead_code)]
pub(crate) fn mark_node_ready(workspace: &Path, node: &str) -> Result<()> {
    mutate_execution_document(workspace, |document, now| {
        ensure_known_node(document, node)?;
        let record = document
            .learning
            .nodes
            .get_mut(node)
            .context("learning data is missing graph node after normalization")?;
        if record.started_at.is_some() || record.finished_at.is_some() {
            bail!("graph node `{node}` is already started or terminal");
        }
        record.ready_at.get_or_insert_with(|| now.to_owned());
        Ok(())
    })
}

#[allow(dead_code)]
pub(crate) fn checkout_start_node(
    workspace: &Path,
    node: &str,
    agent_id: &str,
    agent_label: &str,
) -> Result<()> {
    mutate_execution_document(workspace, |document, now| {
        checkout_start_node_in_document(document, node, agent_id, agent_label, now)
    })
}

#[allow(dead_code)]
pub(crate) fn finish_node(
    workspace: &Path,
    node: &str,
    agent_id: &str,
    outcome: crate::learning_data::NodeOutcome,
) -> Result<()> {
    mutate_execution_document(workspace, |document, now| {
        finish_node_in_document(document, node, agent_id, outcome, now)
    })
}

#[allow(dead_code)]
pub(crate) fn release_node(
    workspace: &Path,
    node: &str,
    agent_id: &str,
    failure: Option<(
        crate::learning_data::NodeOutcome,
        crate::learning_data::FailureCode,
    )>,
) -> Result<()> {
    mutate_execution_document(workspace, |document, now| {
        release_node_in_document(document, node, agent_id, failure, now)
    })
}

#[allow(dead_code)]
pub(crate) fn reopen_node(workspace: &Path, node: &str) -> Result<()> {
    mutate_execution_document(workspace, |document, now| {
        ensure_known_node(document, node)?;
        if let Some(assignment) = document
            .execution
            .as_mut()
            .and_then(|e| e.assignments.get_mut(node))
        {
            if assignment.state == "checked_out" {
                bail!("graph node `{node}` cannot be reopened while checked out");
            }
            assignment.state = "released".to_owned();
            assignment.completed_at = None;
            assignment.released_at = Some(now.to_owned());
        }
        let record = document
            .learning
            .nodes
            .get_mut(node)
            .context("learning node missing")?;
        record.outcome = None;
        record.failure_code = None;
        record.verification = None;
        record.finished_at = None;
        record.reopen_count += 1;
        Ok(())
    })
}

#[allow(dead_code)]
pub(crate) fn record_verification_result(
    workspace: &Path,
    node: &str,
    passed: bool,
    evidence_refs: Vec<String>,
) -> Result<()> {
    mutate_execution_document(workspace, |document, _now| {
        ensure_known_node(document, node)?;
        let record = document
            .learning
            .nodes
            .get_mut(node)
            .context("learning node missing")?;
        record.verification = Some(crate::learning_data::Verification {
            kind: Some("automated".to_owned()),
            passed: Some(passed),
            evidence_refs,
            ..crate::learning_data::Verification::default()
        });
        Ok(())
    })
}

#[allow(dead_code)]
pub(crate) fn record_artifact_produced(
    workspace: &Path,
    node: &str,
    reference: &str,
) -> Result<()> {
    mutate_execution_document(workspace, |document, _now| {
        ensure_known_node(document, node)?;
        let record = document
            .learning
            .nodes
            .get_mut(node)
            .context("learning node missing")?;
        push_unique(&mut record.artifacts_produced, reference);
        Ok(())
    })
}

#[allow(dead_code)]
pub(crate) fn record_artifact_consumed(
    workspace: &Path,
    node: &str,
    reference: &str,
) -> Result<()> {
    mutate_execution_document(workspace, |document, _now| {
        ensure_known_node(document, node)?;
        let record = document
            .learning
            .nodes
            .get_mut(node)
            .context("learning node missing")?;
        push_unique(&mut record.consumed_by, reference);
        Ok(())
    })
}

#[allow(dead_code)]
pub(crate) fn record_human_intervention(
    workspace: &Path,
    node: &str,
    note: Option<&str>,
) -> Result<()> {
    mutate_execution_document(workspace, |document, _now| {
        ensure_known_node(document, node)?;
        let record = document
            .learning
            .nodes
            .get_mut(node)
            .context("learning node missing")?;
        record.human_intervention = true;
        if let Some(note) = note {
            record.notes = Some(note.chars().take(1_000).collect());
        }
        Ok(())
    })
}

#[allow(dead_code)]
pub(crate) fn set_node_costs(
    workspace: &Path,
    node: &str,
    estimated_cost: Option<f64>,
    actual_cost: Option<f64>,
) -> Result<()> {
    mutate_execution_document(workspace, |document, _now| {
        ensure_known_node(document, node)?;
        let record = document
            .learning
            .nodes
            .get_mut(node)
            .context("learning node missing")?;
        record.estimated_cost = estimated_cost;
        record.actual_cost = actual_cost;
        Ok(())
    })
}

#[allow(dead_code)]
pub(crate) fn append_graph_edit_event(
    workspace: &Path,
    event: crate::learning_data::GraphEditEvent,
) -> Result<()> {
    mutate_execution_document(workspace, |document, now| {
        let mut event = event;
        if event.timestamp.trim().is_empty() || event.timestamp.as_str() < now {
            event.timestamp = now.to_owned();
        }
        document.learning.graph_edits.push(event);
        Ok(())
    })
}

#[allow(dead_code)]
pub(crate) fn update_graph_edit_event_effect(
    workspace: &Path,
    index: usize,
    effect: crate::learning_data::EventualEffect,
) -> Result<()> {
    mutate_execution_document(workspace, |document, _now| {
        let event = document
            .learning
            .graph_edits
            .get_mut(index)
            .context("graph edit event index is out of range")?;
        event.eventual_effect = effect;
        Ok(())
    })
}

#[allow(dead_code)]
pub(crate) fn store_graph_outcome(
    workspace: &Path,
    outcome: crate::learning_data::GraphOutcome,
) -> Result<()> {
    mutate_execution_document(workspace, |document, _now| {
        document.learning.outcome = Some(outcome);
        Ok(())
    })
}

fn mutate_execution_document(
    workspace: &Path,
    update: impl FnOnce(&mut FractalProject, &str) -> Result<()>,
) -> Result<()> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    let now = monotonic_timestamp(&document, timestamp());
    update(&mut document, &now)?;
    refresh_terminal_outcome(&mut document);
    if let Some(execution) = document.execution.as_mut() {
        execution.updated_at = now.clone();
    }
    document.updated_at = now;
    write_document(workspace, &document)
}

#[allow(dead_code)]
fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|current| current == value) {
        values.push(value.to_owned());
    }
}

fn ensure_known_node(document: &FractalProject, node: &str) -> Result<()> {
    let known_node = document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.get("id").and_then(Value::as_str) == Some(node));
    if !known_node {
        bail!("execution mutation references unknown graph node `{node}`");
    }
    Ok(())
}

fn execution_state<'a>(document: &'a mut FractalProject, now: &str) -> &'a mut ExecutionState {
    document.execution.get_or_insert_with(|| ExecutionState {
        schema: "fractal.execution_state.v1".to_owned(),
        phase: "executing".to_owned(),
        assignments: BTreeMap::new(),
        progress: None,
        updated_at: now.to_owned(),
        extra: BTreeMap::new(),
    })
}

#[allow(dead_code)]
fn checkout_start_node_in_document(
    document: &mut FractalProject,
    node: &str,
    agent_id: &str,
    agent_label: &str,
    now: &str,
) -> Result<()> {
    ensure_known_node(document, node)?;
    let blocked = dependency_blockers(document, node);
    if !blocked.is_empty() {
        bail!(
            "graph node `{node}` is not dependency-ready; incomplete: {}",
            blocked.join(", ")
        );
    }
    let execution = execution_state(document, now);
    execution.phase = "executing".to_owned();
    execution.progress = None;
    if let Some(current) = execution.assignments.get(node) {
        if current.state == "completed" {
            bail!("graph node `{node}` is already completed");
        }
        if current.state == "checked_out" && current.agent_id != agent_id {
            bail!(
                "graph node `{node}` is checked out by {} ({})",
                current.agent_label,
                current.agent_id
            );
        }
    }
    let checked_out_at = execution
        .assignments
        .get(node)
        .map(|assignment| assignment.checked_out_at.clone())
        .unwrap_or_else(|| now.to_owned());
    execution.assignments.insert(
        node.to_owned(),
        ExecutionAssignment {
            agent_id: agent_id.to_owned(),
            agent_label: agent_label.to_owned(),
            state: "checked_out".to_owned(),
            checked_out_at,
            completed_at: None,
            released_at: None,
            extra: BTreeMap::new(),
        },
    );
    if let Some(record) = document.learning.nodes.get_mut(node) {
        if record.outcome.take().is_some() {
            record.reopen_count += 1;
            record.finished_at = None;
            record.failure_code = None;
            record.verification = None;
        }
        record.ready_at.get_or_insert_with(|| now.to_owned());
        record.started_at = Some(now.to_owned());
        record.attempt_count += 1;
        record.executor = Some(crate::learning_data::Executor {
            agent: Some(agent_label.to_owned()),
            model: std::env::var("FRACTAL_MODEL").ok(),
            version: option_env!("CARGO_PKG_VERSION").map(str::to_owned),
            ..crate::learning_data::Executor::default()
        });
    }
    Ok(())
}

#[allow(dead_code)]
fn finish_node_in_document(
    document: &mut FractalProject,
    node: &str,
    agent_id: &str,
    outcome: crate::learning_data::NodeOutcome,
    now: &str,
) -> Result<()> {
    if matches!(
        outcome,
        crate::learning_data::NodeOutcome::FailedExecution
            | crate::learning_data::NodeOutcome::FailedVerification
    ) {
        bail!("finish_node requires a non-failure terminal outcome");
    }
    ensure_known_node(document, node)?;
    let execution = execution_state(document, now);
    let current = execution
        .assignments
        .get(node)
        .context("graph node must be checked out before completion")?;
    if current.state != "checked_out" {
        bail!("graph node `{node}` is not checked out");
    }
    if current.agent_id != agent_id {
        bail!(
            "graph node `{node}` is owned by {} ({})",
            current.agent_label,
            current.agent_id
        );
    }
    let checked_out_at = current.checked_out_at.clone();
    let agent_label = current.agent_label.clone();
    execution.assignments.insert(
        node.to_owned(),
        ExecutionAssignment {
            agent_id: agent_id.to_owned(),
            agent_label,
            state: "completed".to_owned(),
            checked_out_at,
            completed_at: Some(now.to_owned()),
            released_at: None,
            extra: BTreeMap::new(),
        },
    );
    if let Some(record) = document.learning.nodes.get_mut(node) {
        record.finished_at = Some(now.to_owned());
        record.outcome = Some(outcome);
    }
    complete_graph_if_terminal(document);
    Ok(())
}

fn release_node_in_document(
    document: &mut FractalProject,
    node: &str,
    agent_id: &str,
    failure: Option<(
        crate::learning_data::NodeOutcome,
        crate::learning_data::FailureCode,
    )>,
    now: &str,
) -> Result<()> {
    ensure_known_node(document, node)?;
    let execution = execution_state(document, now);
    let current = execution
        .assignments
        .get(node)
        .context("graph node must be checked out before release")?;
    if current.state != "checked_out" {
        bail!("graph node `{node}` is not checked out");
    }
    if current.agent_id != agent_id {
        bail!(
            "graph node `{node}` is owned by {} ({})",
            current.agent_label,
            current.agent_id
        );
    }
    let checked_out_at = current.checked_out_at.clone();
    let agent_label = current.agent_label.clone();
    execution.assignments.insert(
        node.to_owned(),
        ExecutionAssignment {
            agent_id: agent_id.to_owned(),
            agent_label,
            state: "released".to_owned(),
            checked_out_at,
            completed_at: None,
            released_at: Some(now.to_owned()),
            extra: BTreeMap::new(),
        },
    );
    if let Some((outcome, failure_code)) = failure {
        if !matches!(
            outcome,
            crate::learning_data::NodeOutcome::FailedExecution
                | crate::learning_data::NodeOutcome::FailedVerification
                | crate::learning_data::NodeOutcome::Cancelled
        ) {
            bail!("release_node failure outcome must be controlled terminal failure/cancel state");
        }
        if let Some(record) = document.learning.nodes.get_mut(node) {
            record.finished_at = Some(now.to_owned());
            record.outcome = Some(outcome);
            record.failure_code = Some(failure_code);
            if outcome == crate::learning_data::NodeOutcome::FailedVerification {
                record.verification = Some(crate::learning_data::Verification {
                    kind: Some("automated".to_owned()),
                    passed: Some(false),
                    evidence_refs: Vec::new(),
                    ..crate::learning_data::Verification::default()
                });
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn dependency_blockers(document: &FractalProject, node: &str) -> Vec<String> {
    document
        .graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|edge| edge.get("to").and_then(Value::as_str) == Some(node))
        .filter(|edge| {
            edge.get("condition")
                .and_then(Value::as_str)
                .is_none_or(|condition| condition != "failure")
        })
        .filter_map(|edge| edge.get("from").and_then(Value::as_str))
        .filter(|dependency| {
            document
                .execution
                .as_ref()
                .and_then(|execution| execution.assignments.get(*dependency))
                .is_none_or(|assignment| assignment.state != "completed")
        })
        .map(str::to_owned)
        .collect()
}

#[allow(dead_code)]
fn complete_graph_if_terminal(document: &mut FractalProject) {
    let node_count = document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let completed = document.execution.as_ref().is_some_and(|execution| {
        execution.assignments.len() == node_count
            && execution
                .assignments
                .values()
                .all(|assignment| assignment.state == "completed")
    });
    if completed {
        if let Some(execution) = document.execution.as_mut() {
            execution.phase = "completed".to_owned();
        }
    }
    refresh_terminal_outcome(document);
}

/// Recompute the graph-level outcome whenever a terminal state is observed.
/// Replacing the prior value is intentional: event eventual effects and late
/// lifecycle facts may arrive after the first terminal write, and aggregation
/// reads only source records so it cannot double-count a refresh.
fn refresh_terminal_outcome(document: &mut FractalProject) {
    let Some(execution) = document.execution.as_ref() else {
        return;
    };
    let node_count = document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let all_terminal_assignments = node_count > 0
        && execution.assignments.len() == node_count
        && execution
            .assignments
            .values()
            .all(|assignment| matches!(assignment.state.as_str(), "completed" | "released"));
    let terminal_phase = matches!(execution.phase.as_str(), "completed" | "halted");
    if terminal_phase || all_terminal_assignments {
        if all_terminal_assignments
            && !terminal_phase
            && execution
                .assignments
                .values()
                .any(|assignment| assignment.state == "released")
        {
            if let Some(execution) = document.execution.as_mut() {
                execution.phase = "halted".to_owned();
            }
        }
        document.learning.outcome = Some(crate::learning_data::aggregate_for_graph(
            &document.learning,
            &document.graph,
        ));
    }
}

pub(crate) fn transition(
    workspace: &Path,
    node: &str,
    action: &str,
    agent_id: &str,
    agent_label: &str,
) -> Result<()> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    let known_node = document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.get("id").and_then(Value::as_str) == Some(node));
    if !known_node {
        bail!("execution transition references unknown graph node `{node}`");
    }
    let now = timestamp();
    let execution = document.execution.get_or_insert_with(|| ExecutionState {
        schema: "fractal.execution_state.v1".to_owned(),
        phase: "executing".to_owned(),
        assignments: BTreeMap::new(),
        progress: None,
        updated_at: now.clone(),
        extra: BTreeMap::new(),
    });
    execution.phase = "executing".to_owned();
    execution.progress = None;
    match action {
        "checkout" => {
            if let Some(current) = execution.assignments.get(node) {
                if current.state == "completed" {
                    bail!("graph node `{node}` is already completed");
                }
                if current.state == "checked_out" && current.agent_id != agent_id {
                    bail!(
                        "graph node `{node}` is checked out by {} ({})",
                        current.agent_label,
                        current.agent_id
                    );
                }
            }
            let blocked: Vec<String> = document
                .graph
                .get("edges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|edge| edge.get("to").and_then(Value::as_str) == Some(node))
                .filter(|edge| {
                    edge.get("condition")
                        .and_then(Value::as_str)
                        .is_none_or(|condition| condition != "failure")
                })
                .filter_map(|edge| edge.get("from").and_then(Value::as_str))
                .filter(|dependency| {
                    execution
                        .assignments
                        .get(*dependency)
                        .is_none_or(|assignment| assignment.state != "completed")
                })
                .map(str::to_owned)
                .collect();
            if !blocked.is_empty() {
                bail!(
                    "graph node `{node}` is not dependency-ready; incomplete: {}",
                    blocked.join(", ")
                );
            }
            let checked_out_at = execution
                .assignments
                .get(node)
                .map(|assignment| assignment.checked_out_at.clone())
                .unwrap_or_else(|| now.clone());
            execution.assignments.insert(
                node.to_owned(),
                ExecutionAssignment {
                    agent_id: agent_id.to_owned(),
                    agent_label: agent_label.to_owned(),
                    state: "checked_out".to_owned(),
                    checked_out_at,
                    completed_at: None,
                    released_at: None,
                    extra: BTreeMap::new(),
                },
            );
            if let Some(record) = document.learning.nodes.get_mut(node) {
                if record.outcome.take().is_some() {
                    record.reopen_count += 1;
                    record.finished_at = None;
                    record.failure_code = None;
                    record.verification = None;
                }
                record.started_at = Some(now.clone());
                record.attempt_count += 1;
                record.executor = Some(crate::learning_data::Executor {
                    agent: Some(agent_label.to_owned()),
                    model: std::env::var("FRACTAL_MODEL").ok(),
                    version: option_env!("CARGO_PKG_VERSION").map(str::to_owned),
                    ..crate::learning_data::Executor::default()
                });
            }
        }
        "complete" => {
            let current = execution
                .assignments
                .get(node)
                .context("graph node must be checked out before completion")?;
            if current.state != "checked_out" {
                bail!("graph node `{node}` is not checked out");
            }
            if current.agent_id != agent_id {
                bail!(
                    "graph node `{node}` is owned by {} ({})",
                    current.agent_label,
                    current.agent_id
                );
            }
            let checked_out_at = execution
                .assignments
                .get(node)
                .map(|assignment| assignment.checked_out_at.clone())
                .unwrap_or_else(|| now.clone());
            execution.assignments.insert(
                node.to_owned(),
                ExecutionAssignment {
                    agent_id: agent_id.to_owned(),
                    agent_label: agent_label.to_owned(),
                    state: "completed".to_owned(),
                    checked_out_at,
                    completed_at: Some(now.clone()),
                    released_at: None,
                    extra: BTreeMap::new(),
                },
            );
            if let Some(record) = document.learning.nodes.get_mut(node) {
                let is_verifier = record.node_type == "verification";
                record.finished_at = Some(now.clone());
                record.outcome = Some(if is_verifier {
                    crate::learning_data::NodeOutcome::VerifiedSuccess
                } else {
                    crate::learning_data::NodeOutcome::UnverifiedSuccess
                });
                if is_verifier {
                    record.verification = Some(crate::learning_data::Verification {
                        kind: Some("automated".to_owned()),
                        passed: Some(true),
                        evidence_refs: Vec::new(),
                        ..crate::learning_data::Verification::default()
                    });
                }
            }
        }
        "release" | "failed_execution" | "failed_verification" => {
            let current = execution
                .assignments
                .get(node)
                .context("graph node must be checked out before release")?;
            if current.state != "checked_out" {
                bail!("graph node `{node}` is not checked out");
            }
            if current.agent_id != agent_id {
                bail!(
                    "graph node `{node}` is owned by {} ({})",
                    current.agent_label,
                    current.agent_id
                );
            }
            let checked_out_at = execution
                .assignments
                .get(node)
                .map(|assignment| assignment.checked_out_at.clone())
                .unwrap_or_else(|| now.clone());
            execution.assignments.insert(
                node.to_owned(),
                ExecutionAssignment {
                    agent_id: agent_id.to_owned(),
                    agent_label: agent_label.to_owned(),
                    state: "released".to_owned(),
                    checked_out_at,
                    completed_at: None,
                    released_at: Some(now.clone()),
                    extra: BTreeMap::new(),
                },
            );
            if action != "release" {
                if let Some(record) = document.learning.nodes.get_mut(node) {
                    record.finished_at = Some(now.clone());
                    record.outcome = Some(if action == "failed_verification" {
                        crate::learning_data::NodeOutcome::FailedVerification
                    } else {
                        crate::learning_data::NodeOutcome::FailedExecution
                    });
                    record.failure_code = Some(if action == "failed_verification" {
                        crate::learning_data::FailureCode::WeakVerifier
                    } else {
                        crate::learning_data::FailureCode::ToolFailure
                    });
                    if action == "failed_verification" {
                        record.verification = Some(crate::learning_data::Verification {
                            kind: Some("automated".to_owned()),
                            passed: Some(false),
                            evidence_refs: Vec::new(),
                            ..crate::learning_data::Verification::default()
                        });
                    }
                }
            }
        }
        other => bail!("unsupported execution transition `{other}`"),
    }
    let node_count = document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let completed = execution
        .assignments
        .values()
        .filter(|assignment| assignment.state == "completed")
        .count();
    if node_count > 0 && completed == node_count {
        execution.phase = "completed".to_owned();
    } else if node_count > 0
        && execution.assignments.len() == node_count
        && execution
            .assignments
            .values()
            .all(|assignment| matches!(assignment.state.as_str(), "completed" | "released"))
    {
        execution.phase = "halted".to_owned();
    }
    execution.updated_at = now.clone();
    refresh_terminal_outcome(&mut document);
    document.updated_at = now;
    write_document(workspace, &document)
}

pub(crate) fn assignment(workspace: &Path, node: &str) -> Result<Option<ExecutionAssignment>> {
    let document = load(workspace)?;
    let known = document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.get("id").and_then(Value::as_str) == Some(node));
    if !known {
        bail!("unknown graph node `{node}`");
    }
    Ok(document
        .execution
        .and_then(|execution| execution.assignments.get(node).cloned()))
}

pub(crate) fn import_legacy_assignments(
    workspace: &Path,
    assignments: BTreeMap<String, ExecutionAssignment>,
) -> Result<usize> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    let known: BTreeSet<String> = document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let execution = document.execution.get_or_insert_with(|| ExecutionState {
        schema: "fractal.execution_state.v1".to_owned(),
        phase: "executing".to_owned(),
        assignments: BTreeMap::new(),
        progress: None,
        updated_at: timestamp(),
        extra: BTreeMap::new(),
    });
    let mut imported = 0;
    for (node, assignment) in assignments {
        if known.contains(&node) && !execution.assignments.contains_key(&node) {
            execution.assignments.insert(node, assignment);
            imported += 1;
        }
    }
    let now = timestamp();
    execution.updated_at = now.clone();
    document.updated_at = now;
    write_document(workspace, &document)?;
    Ok(imported)
}

/// Completed assignments persisted by workers themselves. This is the recovery
/// source of truth when a coordinator disappears after dispatching a wave: those
/// workers may finish after the last checkpoint was written.
pub(crate) fn completed_nodes(workspace: &Path) -> BTreeSet<String> {
    load(workspace)
        .ok()
        .and_then(|document| document.execution)
        .map(|execution| {
            execution
                .assignments
                .into_iter()
                .filter_map(|(node, assignment)| (assignment.state == "completed").then_some(node))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn backfill_execution(workspace: &Path) -> Result<bool> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    if document.execution.is_some() {
        return Ok(false);
    }
    let now = timestamp();
    document.execution = Some(ExecutionState {
        schema: "fractal.execution_state.v1".to_owned(),
        phase: "executing".to_owned(),
        assignments: BTreeMap::new(),
        progress: None,
        updated_at: now.clone(),
        extra: BTreeMap::new(),
    });
    document.updated_at = now;
    write_document(workspace, &document)?;
    Ok(true)
}

pub(crate) fn release_stale_assignments(workspace: &Path) -> Result<bool> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    let Some(execution) = document.execution.as_mut() else {
        return Ok(false);
    };
    let now = timestamp();
    let mut changed = false;
    for assignment in execution.assignments.values_mut() {
        if assignment.state == "checked_out" {
            assignment.state = "released".to_owned();
            assignment.released_at = Some(now.clone());
            changed = true;
        }
    }
    if !changed {
        return Ok(false);
    }
    execution.phase = "halted".to_owned();
    execution.progress = None;
    execution.updated_at = now.clone();
    refresh_terminal_outcome(&mut document);
    document.updated_at = now;
    write_document(workspace, &document)?;
    Ok(true)
}

pub(crate) fn set_execution_phase(workspace: &Path, phase: &str) -> Result<()> {
    if !matches!(phase, "planning" | "executing" | "halted" | "completed") {
        bail!("unsupported execution phase `{phase}`");
    }
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    let now = timestamp();
    let execution = document.execution.get_or_insert_with(|| ExecutionState {
        schema: "fractal.execution_state.v1".to_owned(),
        phase: phase.to_owned(),
        assignments: BTreeMap::new(),
        progress: None,
        updated_at: now.clone(),
        extra: BTreeMap::new(),
    });
    execution.phase = phase.to_owned();
    if phase != "planning" {
        execution.progress = None;
    }
    execution.updated_at = now.clone();
    refresh_terminal_outcome(&mut document);
    document.updated_at = now;
    write_document(workspace, &document)
}

pub(crate) fn record_graph_edit(
    workspace: &Path,
    graph_before_hash: &str,
    action: &str,
    target: Option<&str>,
    created_nodes: Vec<String>,
    trigger: &str,
    actor: &str,
) -> Result<()> {
    if !matches!(
        action,
        "split_node"
            | "reroute_node"
            | "cancel_node"
            | "add_dependency"
            | "remove_dependency"
            | "add_branch"
            | "add_wave_task"
            | "evolve_graph"
    ) {
        bail!("unsupported graph edit action `{action}`");
    }
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    document
        .learning
        .graph_edits
        .push(crate::learning_data::GraphEditEvent {
            graph_before_hash: graph_before_hash.to_owned(),
            action: crate::learning_data::GraphEditAction {
                kind: action.to_owned(),
                target: target.map(str::to_owned),
                created_nodes,
                ..crate::learning_data::GraphEditAction::default()
            },
            trigger: trigger.chars().take(240).collect(),
            actor: actor.chars().take(120).collect(),
            timestamp: timestamp(),
            eventual_effect: crate::learning_data::EventualEffect::default(),
            ..crate::learning_data::GraphEditEvent::default()
        });
    if let Some(record) = target.and_then(|id| document.learning.nodes.get_mut(id)) {
        record.human_intervention = true;
    }
    refresh_terminal_outcome(&mut document);
    document.updated_at = timestamp();
    write_document(workspace, &document)
}

pub(crate) fn update_planning_progress(
    workspace: &Path,
    message: &str,
    step: u32,
    elapsed_seconds: u64,
    agent_label: &str,
    source: &str,
) -> Result<()> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    let now = timestamp();
    let execution = document.execution.get_or_insert_with(|| ExecutionState {
        schema: "fractal.execution_state.v1".to_owned(),
        phase: "planning".to_owned(),
        assignments: BTreeMap::new(),
        progress: None,
        updated_at: now.clone(),
        extra: BTreeMap::new(),
    });
    if execution.phase != "planning" {
        return Ok(());
    }
    execution.progress = Some(PlanningProgress {
        schema: "fractal.planning_progress.v1".to_owned(),
        message: message.chars().take(500).collect(),
        step,
        elapsed_seconds,
        agent_label: agent_label.chars().take(120).collect(),
        source: source.chars().take(240).collect(),
        updated_at: now.clone(),
        extra: BTreeMap::new(),
    });
    execution.updated_at = now.clone();
    document.updated_at = now;
    write_document(workspace, &document)
}

pub(crate) fn load(workspace: &Path) -> Result<FractalProject> {
    let path = path(workspace);
    let mut document: FractalProject = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("decode {}", path.display()))?;
    document.learning =
        crate::learning_data::normalize(document.learning, &document.graph, &document.updated_at);
    validate(&document)?;
    Ok(document)
}

pub(crate) fn set_visibility(workspace: &Path, visibility: &str) -> Result<()> {
    if !matches!(visibility, "public" | "private" | "unlisted") {
        bail!("unsupported project visibility `{visibility}`");
    }
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    document.project.visibility = visibility.to_owned();
    document.updated_at = timestamp();
    write_document(workspace, &document)
}

fn validate(document: &FractalProject) -> Result<()> {
    if document.schema != "fractal.project.v1"
        || document.graph_hash.is_empty()
        || document.project.slug.is_empty()
        || document.graph.get("graph_hash").and_then(Value::as_str)
            != Some(document.graph_hash.as_str())
    {
        bail!("invalid fractal.project.v1 document");
    }
    crate::graph_store::verify_graph_document(&document.graph)
        .context("embedded execution graph hash is invalid")?;
    reject_secret_fields(&document.graph)?;
    crate::learning_data::validate(&document.learning)
        .map_err(|error| anyhow::anyhow!("invalid fractal.learning.v1 document: {error}"))?;
    let learning = serde_json::to_value(&document.learning)
        .map_err(|error| anyhow::anyhow!("encode fractal.learning.v1: {error}"))?;
    reject_secret_fields(&learning)
        .context("learning envelope contains forbidden credential-shaped fields")?;
    if let Some(efficiency) = &document.efficiency {
        crate::efficiency::validate(efficiency)
            .map_err(|error| anyhow::anyhow!("invalid fractal.efficiency.v1 document: {error}"))?;
        let encoded = serde_json::to_value(efficiency)
            .map_err(|error| anyhow::anyhow!("encode fractal.efficiency.v1: {error}"))?;
        reject_secret_fields(&encoded)
            .context("efficiency envelope contains forbidden credential-shaped fields")?;
    }
    if let Some(failure_graph) = &document.failure_graph {
        crate::failure_graph::validate(failure_graph)
            .context("invalid fractal.failure_graph.v1 document")?;
        let encoded =
            serde_json::to_value(failure_graph).context("encode fractal.failure_graph.v1")?;
        reject_secret_fields(&encoded)
            .context("failure graph contains forbidden credential-shaped fields")?;
        crate::failure_graph::validate_unknown_fields(failure_graph)
            .context("failure graph unknown fields contain forbidden credentials")?;
    }
    if let Some(catalog) = document.extra.get("catalog") {
        let schema = catalog.get("schema").and_then(Value::as_str).unwrap_or("");
        if schema == project_catalog::CATALOG_SCHEMA {
            project_catalog::validate_value(catalog)
                .map_err(|error| anyhow::anyhow!("invalid fractal.catalog.v1 document: {error}"))?;
        } else if schema.is_empty() {
            bail!("catalog envelope is missing schema");
        }
        // Unsupported future catalog schemas stay opaque; do not partial-parse.
    }
    if let Some(execution) = &document.execution {
        if execution.schema != "fractal.execution_state.v1"
            || !matches!(
                execution.phase.as_str(),
                "planning" | "executing" | "halted" | "completed"
            )
        {
            bail!("invalid fractal.execution_state.v1 document");
        }
        if let Some(progress) = &execution.progress {
            if progress.schema != "fractal.planning_progress.v1"
                || execution.phase != "planning"
                || progress.message.trim().is_empty()
                || progress.message.chars().count() > 500
                || progress.step == 0
                || progress.agent_label.trim().is_empty()
                || progress.agent_label.chars().count() > 120
                || progress.source.chars().count() > 240
            {
                bail!("invalid fractal.planning_progress.v1 document");
            }
        }
        let node_ids = document
            .graph
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|node| node.get("id").and_then(Value::as_str))
            .collect::<std::collections::BTreeSet<_>>();
        for (node, assignment) in &execution.assignments {
            if !node_ids.contains(node.as_str())
                || !matches!(
                    assignment.state.as_str(),
                    "checked_out" | "completed" | "released"
                )
                || assignment.agent_id.trim().is_empty()
                || assignment.agent_label.trim().is_empty()
            {
                bail!("invalid execution assignment for node `{node}`");
            }
        }
    }
    Ok(())
}

fn learning_from_graph(graph: &Value, now: &str) -> crate::learning_data::LearningData {
    crate::learning_data::normalize(crate::learning_data::LearningData::default(), graph, now)
}

fn merge_learning(
    current: &crate::learning_data::LearningData,
    graph: &Value,
    now: &str,
) -> crate::learning_data::LearningData {
    let mut merged = learning_from_graph(graph, now);
    for (id, record) in &mut merged.nodes {
        if let Some(previous) = current.nodes.get(id) {
            let depends_on = record.depends_on.clone();
            *record = previous.clone();
            record.depends_on = depends_on;
        }
    }
    merged.graph_edits = current.graph_edits.clone();
    merged.outcome = current.outcome.clone();
    merged.extra = current.extra.clone();
    merged
}

fn is_planning_preview(graph: &Value) -> bool {
    let nodes = graph.get("nodes").and_then(Value::as_array);
    nodes.is_some_and(|nodes| {
        nodes.len() == 1
            && nodes[0].get("capability").and_then(Value::as_str) == Some("control.plan")
    })
}

/// Keep the lead PRD's compact criterion IDs beside learning data so the
/// terminal aggregator can report per-criterion results without embedding the
/// full PRD or any large evidence payload in the portable project file.
fn load_acceptance_criteria(workspace: &Path) -> Option<Vec<String>> {
    let path = workspace.join(".fractal").join("lead-prd.json");
    let value: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let mut ids = value
        .get("acceptance_criteria")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|criterion| {
            criterion
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| criterion.as_str())
        })
        .filter(|id| !id.trim().is_empty())
        .map(|id| id.trim().chars().take(120).collect::<String>())
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    (!ids.is_empty()).then_some(ids)
}

fn write_document(workspace: &Path, document: &FractalProject) -> Result<()> {
    validate(document)?;
    let destination = path(workspace);
    let directory = destination.parent().expect("project file has parent");
    fs::create_dir_all(directory).with_context(|| format!("create {}", directory.display()))?;
    atomic_write(&destination, &serde_json::to_vec_pretty(document)?)
}

/// Atomically load, mutate, validate, and rewrite the portable project file under
/// the shared project-file lock. Used by efficiency accounting and similar append
/// paths that must not race execution/learning updates.
pub(crate) fn mutate_document(
    workspace: &Path,
    update: impl FnOnce(&mut FractalProject) -> Result<()>,
) -> Result<()> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    update(&mut document)?;
    document.updated_at = timestamp();
    write_document(workspace, &document)
}

/// Guarded catalog write: validate a typed `fractal.catalog.v1` envelope, then
/// atomically replace only `extra["catalog"]`. Sibling identity, graph,
/// graph_hash, execution, learning, efficiency, and other unknown top-level
/// fields are preserved. This is not a generic JSON mutation API.
pub(crate) fn replace_catalog(
    workspace: &Path,
    catalog: &project_catalog::CatalogV1,
) -> Result<()> {
    project_catalog::validate(catalog)
        .map_err(|error| anyhow::anyhow!("invalid fractal.catalog.v1 document: {error}"))?;
    let encoded = serde_json::to_value(catalog)
        .map_err(|error| anyhow::anyhow!("encode fractal.catalog.v1: {error}"))?;
    reject_secret_fields(&encoded)
        .context("catalog envelope contains forbidden credential-shaped fields")?;
    mutate_document(workspace, |document| {
        document.extra.insert("catalog".to_owned(), encoded.clone());
        Ok(())
    })
}

/// Return the stored failure graph, or a pure projection of legacy learning
/// failures when older projects do not yet contain the additive key. Reading
/// never materializes or rewrites the projection.
#[allow(dead_code)]
pub(crate) fn load_failure_graph(workspace: &Path) -> Result<crate::failure_graph::FailureGraph> {
    let document = load(workspace)?;
    Ok(document.failure_graph.unwrap_or_else(|| {
        crate::failure_graph::project_legacy_failures(
            &document.learning,
            Some(document.graph_hash.as_str()),
        )
    }))
}

/// Alias used by display/runtime consumers that already hold a decoded
/// project document. This is intentionally pure and does not write.
#[allow(dead_code)]
pub(crate) fn failure_graph(document: &FractalProject) -> crate::failure_graph::FailureGraph {
    document.failure_graph.clone().unwrap_or_else(|| {
        crate::failure_graph::project_legacy_failures(
            &document.learning,
            Some(document.graph_hash.as_str()),
        )
    })
}

/// Append one failure observation under the canonical project lock. A stable
/// node/code key groups retries into one record while retaining every
/// observation in append order.
#[allow(dead_code)]
pub(crate) fn append_failure(
    workspace: &Path,
    failure: crate::failure_graph::FailureRecord,
) -> Result<String> {
    let mut incoming = crate::failure_graph::FailureGraph::empty();
    incoming.failures.insert(failure.id.clone(), failure);
    crate::failure_graph::normalize(&mut incoming).context("normalize failure before append")?;
    let incoming = incoming
        .failures
        .into_values()
        .next()
        .context("normalized failure was missing")?;
    let mut incoming = incoming;
    if incoming.observations.is_empty() {
        incoming
            .observations
            .push(failure_observation_from_record(&incoming));
    }
    let id = incoming.id.clone();
    mutate_document(workspace, |document| {
        let mut graph = document.failure_graph.clone().unwrap_or_else(|| {
            crate::failure_graph::project_legacy_failures(
                &document.learning,
                Some(document.graph_hash.as_str()),
            )
        });
        crate::failure_graph::normalize(&mut graph).context("normalize existing failure graph")?;
        if let Some(existing) = graph.failures.get_mut(&id) {
            if existing.state != crate::failure_graph::FailureState::Unresolved {
                bail!(
                    "cannot append a retry to {} failure `{id}`",
                    format_state(existing.state)
                );
            }
            existing.attempt = existing.attempt.max(incoming.attempt);
            existing.outcome = incoming.outcome.clone();
            existing.summary = incoming.summary.clone();
            existing.capability = incoming.capability.clone().or(existing.capability.clone());
            existing.component = incoming.component.clone().or(existing.component.clone());
            existing.source_ref = incoming.source_ref.clone().or(existing.source_ref.clone());
            existing.agent = incoming.agent.clone().or(existing.agent.clone());
            existing.model = incoming.model.clone().or(existing.model.clone());
            existing.version = incoming.version.clone().or(existing.version.clone());
            existing.observed = incoming.observed.clone();
            existing.evidence = incoming.evidence.clone();
            if incoming.observations.is_empty() {
                existing
                    .observations
                    .push(failure_observation_from_record(&incoming));
            } else {
                existing.observations.extend(incoming.observations.clone());
            }
        } else {
            graph.failures.insert(id.clone(), incoming.clone());
        }
        crate::failure_graph::normalize(&mut graph).context("normalize appended failure graph")?;
        document.failure_graph = Some(graph);
        Ok(())
    })
    .map(|_| id)
}

/// Resolve a failure only when the caller supplies an explicit successful
/// resolution and compact evidence. Existing retry observations remain intact.
#[allow(dead_code)]
pub(crate) fn resolve_failure(
    workspace: &Path,
    failure_id: &str,
    resolution: crate::failure_graph::FailureResolution,
) -> Result<()> {
    let failure_id = failure_id.to_owned();
    mutate_document(workspace, |document| {
        let mut graph = document.failure_graph.clone().unwrap_or_else(|| {
            crate::failure_graph::project_legacy_failures(
                &document.learning,
                Some(document.graph_hash.as_str()),
            )
        });
        let failure = graph
            .failures
            .get_mut(&failure_id)
            .with_context(|| format!("unknown failure `{failure_id}`"))?;
        if !resolution.success || resolution.evidence.is_empty() {
            bail!("resolving failure requires successful resolution evidence");
        }
        failure.state = crate::failure_graph::FailureState::Resolved;
        failure.superseded_by = None;
        failure.resolution = Some(resolution);
        crate::failure_graph::normalize(&mut graph).context("normalize resolved failure graph")?;
        document.failure_graph = Some(graph);
        Ok(())
    })
}

/// Mark a failure superseded by another stable failure key. The target must
/// already exist so status and references cannot drift into a dangling state.
#[allow(dead_code)]
pub(crate) fn supersede_failure(
    workspace: &Path,
    failure_id: &str,
    superseded_by: &str,
) -> Result<()> {
    let failure_id = failure_id.to_owned();
    let superseded_by = superseded_by.to_owned();
    mutate_document(workspace, |document| {
        let mut graph = document.failure_graph.clone().unwrap_or_else(|| {
            crate::failure_graph::project_legacy_failures(
                &document.learning,
                Some(document.graph_hash.as_str()),
            )
        });
        if !graph.failures.contains_key(&superseded_by) {
            bail!("superseding failure `{superseded_by}` does not exist");
        }
        let failure = graph
            .failures
            .get_mut(&failure_id)
            .with_context(|| format!("unknown failure `{failure_id}`"))?;
        failure.state = crate::failure_graph::FailureState::Superseded;
        failure.resolution = None;
        failure.superseded_by = Some(superseded_by);
        crate::failure_graph::normalize(&mut graph)
            .context("normalize superseded failure graph")?;
        document.failure_graph = Some(graph);
        Ok(())
    })
}

/// Insert or replace one deterministic lesson record. This is the sole lesson
/// mutation seam; arbitrary JSON updates are intentionally unavailable.
#[allow(dead_code)]
pub(crate) fn upsert_lesson(
    workspace: &Path,
    lesson: crate::failure_graph::LessonRecord,
) -> Result<String> {
    let mut incoming = crate::failure_graph::FailureGraph::empty();
    incoming.lessons.insert(lesson.id.clone(), lesson);
    crate::failure_graph::normalize(&mut incoming).context("normalize lesson before upsert")?;
    let lesson = incoming
        .lessons
        .into_values()
        .next()
        .context("normalized lesson was missing")?;
    let id = lesson.id.clone();
    mutate_document(workspace, |document| {
        let mut graph = document.failure_graph.clone().unwrap_or_else(|| {
            crate::failure_graph::project_legacy_failures(
                &document.learning,
                Some(document.graph_hash.as_str()),
            )
        });
        graph.lessons.insert(id.clone(), lesson.clone());
        crate::failure_graph::normalize(&mut graph).context("normalize lesson graph")?;
        document.failure_graph = Some(graph);
        Ok(())
    })
    .map(|_| id)
}

/// Add or replace one typed edge. Endpoint and status invariants are checked
/// by the failure graph validator before the atomic write.
#[allow(dead_code)]
pub(crate) fn add_failure_edge(
    workspace: &Path,
    mut edge: crate::failure_graph::EdgeRecord,
) -> Result<String> {
    if edge.id.trim().is_empty() {
        edge.id = crate::failure_graph::edge_id(edge.edge_type, &edge.from, &edge.to);
    }
    let id = edge.id.clone();
    mutate_document(workspace, |document| {
        let mut graph = document.failure_graph.clone().unwrap_or_else(|| {
            crate::failure_graph::project_legacy_failures(
                &document.learning,
                Some(document.graph_hash.as_str()),
            )
        });
        graph.edges.insert(id.clone(), edge.clone());
        crate::failure_graph::normalize(&mut graph).context("normalize edge graph")?;
        document.failure_graph = Some(graph);
        Ok(())
    })
    .map(|_| id)
}

/// Replace the complete typed failure graph through the guarded atomic seam.
/// Identity, graph, graph_hash, execution, learning, catalog, efficiency,
/// and every unknown sibling field remain untouched.
#[allow(dead_code)]
pub(crate) fn replace_failure_graph(
    workspace: &Path,
    mut graph: crate::failure_graph::FailureGraph,
) -> Result<()> {
    crate::failure_graph::normalize(&mut graph).context("normalize replacement failure graph")?;
    mutate_document(workspace, |document| {
        document.failure_graph = Some(graph.clone());
        Ok(())
    })
}

#[allow(dead_code)]
fn failure_observation_from_record(
    record: &crate::failure_graph::FailureRecord,
) -> crate::failure_graph::FailureObservation {
    crate::failure_graph::FailureObservation {
        attempt: record.attempt,
        outcome: record.outcome.clone(),
        summary: record.summary.clone(),
        evidence: record.evidence.clone(),
        agent: record.agent.clone(),
        model: record.model.clone(),
        version: record.version.clone(),
        observed: record.observed.clone(),
        ..crate::failure_graph::FailureObservation::default()
    }
}

#[allow(dead_code)]
fn format_state(state: crate::failure_graph::FailureState) -> &'static str {
    match state {
        crate::failure_graph::FailureState::Unresolved => "unresolved",
        crate::failure_graph::FailureState::Resolved => "resolved",
        crate::failure_graph::FailureState::Superseded => "superseded",
    }
}

pub(crate) fn project_timestamp() -> String {
    timestamp()
}

fn project_file_lock() -> std::sync::MutexGuard<'static, ()> {
    PROJECT_FILE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("project file lock")
}

fn reject_secret_fields(value: &Value) -> Result<()> {
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
                    bail!("execution graph contains forbidden credential field `{key}`");
                }
                reject_secret_fields(child)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                reject_secret_fields(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn slug_for(workspace: &Path) -> String {
    let raw = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_owned());
    slug_from(&raw)
}

pub(crate) fn slug_from(raw: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in raw.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            separator = false;
        } else if !separator && !slug.is_empty() {
            slug.push('-');
            separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "project".to_owned()
    } else {
        slug
    }
}

fn managed_identity_path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal").join("managed-project.json")
}

fn load_managed_identity(workspace: &Path) -> Result<Option<ManagedProjectIdentity>> {
    let path = managed_identity_path(workspace);
    if !path.is_file() {
        return Ok(None);
    }
    let identity: ManagedProjectIdentity = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("decode {}", path.display()))?;
    if identity.schema != MANAGED_IDENTITY_SCHEMA
        || identity.slug.is_empty()
        || identity.title.is_empty()
        || slug_from(&identity.slug) != identity.slug
    {
        bail!("managed project identity is malformed");
    }
    Ok(Some(identity))
}

fn clean_title(title: &str, workspace: &Path) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        slug_for(workspace)
    } else {
        title.chars().take(240).collect()
    }
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    rfc3339_utc(seconds)
}

fn monotonic_timestamp(document: &FractalProject, candidate: String) -> String {
    let mut maximum = candidate;
    maximum = maximum.max(document.updated_at.clone());
    if let Some(execution) = &document.execution {
        maximum = maximum.max(execution.updated_at.clone());
        for assignment in execution.assignments.values() {
            maximum = maximum.max(assignment.checked_out_at.clone());
            if let Some(value) = &assignment.completed_at {
                maximum = maximum.max(value.clone());
            }
            if let Some(value) = &assignment.released_at {
                maximum = maximum.max(value.clone());
            }
        }
    }
    for record in document.learning.nodes.values() {
        for value in [
            record.created_at.as_ref(),
            record.ready_at.as_ref(),
            record.started_at.as_ref(),
            record.finished_at.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            maximum = maximum.max(value.clone());
        }
    }
    for event in &document.learning.graph_edits {
        maximum = maximum.max(event.timestamp.clone());
    }
    maximum
}

fn rfc3339_utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    // Howard Hinnant's civil-from-days algorithm, with Unix epoch adjustment.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn atomic_write(destination: &Path, bytes: &[u8]) -> Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temporary = destination.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_workspace() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "My Expense App {}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn temp_lock_path() -> PathBuf {
        let workspace = temp_workspace();
        let directory = workspace.join(".fractal");
        fs::create_dir_all(&directory).unwrap();
        directory.join("project.fractal.lock")
    }

    #[test]
    fn empty_old_lock_is_recoverable_after_the_conservative_age_window() {
        let path = temp_lock_path();
        fs::write(&path, b"").unwrap();
        let old_now = SystemTime::now() + STALE_LOCK_AGE + std::time::Duration::from_secs(1);
        assert!(stale_lock_at(&path, old_now));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fresh_empty_lock_is_retained() {
        let path = temp_lock_path();
        fs::write(&path, b"").unwrap();
        assert!(!stale_lock_at(&path, SystemTime::now()));
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_owned_by_a_dead_pid_is_recoverable() -> Result<()> {
        let path = temp_lock_path();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()?;
        let pid = child.id();
        child.wait()?;
        fs::write(&path, pid.to_string())?;
        assert!(stale_lock_at(&path, SystemTime::now()));
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn lock_owned_by_a_live_pid_is_never_recovered() {
        let path = temp_lock_path();
        fs::write(&path, std::process::id().to_string()).unwrap();
        let old_now = SystemTime::now() + STALE_LOCK_AGE + std::time::Duration::from_secs(1);
        assert!(!stale_lock_at(&path, old_now));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn persists_portable_standard_document() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".fractal"))?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [],
            "edges": []
        });
        graph["graph_hash"] = Value::String(
            fractal_contracts::canonical_sha256(&graph)
                .map_err(|error| anyhow::anyhow!("hash fixture: {error}"))?,
        );
        let stored = persist(&workspace, &graph, "Build an expense tracker")?;
        assert_eq!(stored, workspace.join(".fractal/project.fractal"));
        let document = load(&workspace)?;
        assert_eq!(document.schema, "fractal.project.v1");
        assert!(document.project.slug.starts_with("my-expense-app-"));
        assert_eq!(document.graph, graph);
        assert_eq!(
            document
                .execution
                .as_ref()
                .map(|state| state.phase.as_str()),
            Some("executing")
        );
        let encoded = fs::read_to_string(stored)?;
        assert!(!encoded.contains(workspace.to_string_lossy().as_ref()));
        set_visibility(&workspace, "public")?;
        persist(&workspace, &graph, "Build an expense tracker")?;
        assert_eq!(
            load(&workspace)?.project.visibility,
            "public",
            "later graph updates must preserve an explicitly selected visibility"
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn managed_voice_name_controls_dashboard_title_and_url_slug() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        configure_managed_identity(
            &workspace,
            "Pocket Ledger",
            "Build me a personal expense tracker",
        )?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [],
            "edges": []
        });
        graph["graph_hash"] = Value::String(
            fractal_contracts::canonical_sha256(&graph)
                .map_err(|error| anyhow::anyhow!("hash fixture: {error}"))?,
        );

        persist(&workspace, &graph, "Build me a personal expense tracker")?;
        let document = load(&workspace)?;

        assert_eq!(document.project.title, "Pocket Ledger");
        assert_eq!(document.project.slug, "pocket-ledger");
        assert_eq!(
            document.project.prompt.as_deref(),
            Some("Build me a personal expense tracker")
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn records_portable_agent_assignments_without_changing_graph_hash() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [
                {"id": "build", "capability": "code.generate", "instruction": "Build it."},
                {"id": "test", "capability": "project.tests.execute", "instruction": "Test it."}
            ],
            "edges": [{"from": "build", "to": "test"}]
        });
        graph["graph_hash"] = Value::String(
            fractal_contracts::canonical_sha256(&graph)
                .map_err(|error| anyhow::anyhow!("hash fixture: {error}"))?,
        );
        persist(&workspace, &graph, "Build app")?;
        let original_hash = load(&workspace)?.graph_hash;
        transition(&workspace, "build", "checkout", "cursor", "Cursor")?;
        transition(&workspace, "build", "complete", "cursor", "Cursor")?;
        transition(&workspace, "test", "checkout", "codex", "Codex")?;
        let document = load(&workspace)?;
        assert_eq!(document.graph_hash, original_hash);
        let execution = document.execution.expect("execution state");
        assert_eq!(execution.phase, "executing");
        assert_eq!(execution.assignments["build"].state, "completed");
        assert_eq!(execution.assignments["test"].state, "checked_out");
        assert!(release_stale_assignments(&workspace)?);
        let execution = load(&workspace)?.execution.expect("execution state");
        assert_eq!(execution.phase, "halted");
        assert_eq!(execution.assignments["test"].state, "released");
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn planning_progress_updates_portable_state_without_changing_graph_hash() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [{
                "id": "lead_planning",
                "capability": "control.plan",
                "instruction": "Plan it."
            }],
            "edges": []
        });
        graph["graph_hash"] = Value::String(
            fractal_contracts::canonical_sha256(&graph)
                .map_err(|error| anyhow::anyhow!("hash fixture: {error}"))?,
        );
        persist(&workspace, &graph, "Plan app")?;
        let original_hash = load(&workspace)?.graph_hash;

        update_planning_progress(
            &workspace,
            "⏳ [claude] is selecting the architecture",
            2,
            30,
            "claude",
            "APP_PRD.md",
        )?;

        let document = load(&workspace)?;
        assert_eq!(document.graph_hash, original_hash);
        let execution = document.execution.expect("execution state");
        assert_eq!(execution.phase, "planning");
        let progress = execution.progress.expect("planning progress");
        assert_eq!(progress.schema, "fractal.planning_progress.v1");
        assert_eq!(progress.step, 2);
        assert_eq!(progress.elapsed_seconds, 30);
        assert!(progress.message.contains("selecting the architecture"));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn evolved_child_preserves_parent_execution_and_has_a_valid_hash() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut parent = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [
                {"id": "build", "capability": "code.generate", "instruction": "Build it."},
                {"id": "test", "capability": "project.tests.execute", "instruction": "Test it."}
            ],
            "edges": [{"from": "build", "to": "test"}]
        });
        parent["graph_hash"] = Value::String(
            fractal_contracts::canonical_sha256(&parent)
                .map_err(|error| anyhow::anyhow!("hash fixture: {error}"))?,
        );
        persist(&workspace, &parent, "Build app")?;
        transition(&workspace, "build", "checkout", "cursor", "Cursor")?;
        transition(&workspace, "build", "complete", "cursor", "Cursor")?;

        let parent_hash = parent["graph_hash"].as_str().unwrap().to_owned();
        let mut child = parent.clone();
        child["parent_graph"] = Value::String(parent_hash);
        child["nodes"].as_array_mut().unwrap().push(json!({
            "id": "verify.build.harness",
            "capability": "project.tests.execute",
            "instruction": "Verify it."
        }));
        child["edges"].as_array_mut().unwrap().push(json!({
            "from": "build",
            "to": "verify.build.harness"
        }));
        child.as_object_mut().unwrap().remove("graph_hash");
        child["graph_hash"] = Value::String(
            fractal_contracts::canonical_sha256(&child)
                .map_err(|error| anyhow::anyhow!("hash fixture: {error}"))?,
        );

        persist_evolved(&workspace, &child)?;
        let document = load(&workspace)?;
        crate::graph_store::verify_graph_document(&document.graph)?;
        assert_eq!(document.graph_hash, child["graph_hash"]);
        let execution = document.execution.expect("execution state");
        assert_eq!(execution.phase, "executing");
        assert_eq!(execution.assignments["build"].state, "completed");
        assert!(!execution.assignments.contains_key("verify.build.harness"));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn renders_unix_epoch_as_rfc3339() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_722_470_400), "2024-08-01T00:00:00Z");
    }

    #[test]
    fn refuses_credential_fields() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [],
            "edges": [],
            "access_token": "must-not-leak"
        });
        graph["graph_hash"] = Value::String(
            fractal_contracts::canonical_sha256(&graph)
                .map_err(|error| anyhow::anyhow!("hash fixture: {error}"))?,
        );
        let error = persist(&workspace, &graph, "unsafe")
            .expect_err("credential-shaped fields must be refused");
        assert!(error.to_string().contains("forbidden credential field"));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn legacy_project_without_learning_is_normalized_on_load() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".fractal"))?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_legacy",
            "nodes": [
                {"id": "plan", "capability": "control.plan", "title": "Plan"},
                {"id": "build", "capability": "code.generate", "instruction": "Build"}
            ],
            "edges": [{"from": "plan", "to": "build"}],
            "future_graph_field": {"kept": true}
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        let legacy = json!({
            "schema": "fractal.project.v1",
            "project": {"slug": "legacy", "title": "Legacy", "visibility": "private"},
            "graph_hash": graph["graph_hash"],
            "graph": graph,
            "updated_at": "2024-01-01T00:00:00Z",
            "future_project_field": {"kept": true}
        });
        fs::write(path(&workspace), serde_json::to_vec_pretty(&legacy)?)?;

        let document = load(&workspace)?;

        assert_eq!(document.learning.schema, "fractal.learning.v1");
        assert_eq!(document.learning.nodes["build"].depends_on, vec!["plan"]);
        assert_eq!(
            document.extra["future_project_field"],
            json!({"kept": true})
        );
        assert_eq!(document.graph["future_graph_field"], json!({"kept": true}));
        crate::graph_store::verify_graph_document(&document.graph)?;
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn central_mutation_apis_round_trip_enriched_learning() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_enriched",
            "nodes": [{"id": "build", "capability": "code.generate", "instruction": "Build"}],
            "edges": []
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Build")?;
        mark_node_ready(&workspace, "build")?;
        checkout_start_node(&workspace, "build", "agent-1", "Agent One")?;
        record_artifact_produced(&workspace, "build", "artifact:build-log")?;
        record_artifact_consumed(&workspace, "build", "artifact:input-spec")?;
        record_human_intervention(&workspace, "build", Some("operator approved repair"))?;
        set_node_costs(&workspace, "build", Some(1.25), Some(1.50))?;
        record_verification_result(
            &workspace,
            "build",
            true,
            vec!["artifact:build-log".to_owned()],
        )?;
        finish_node(
            &workspace,
            "build",
            "agent-1",
            crate::learning_data::NodeOutcome::VerifiedSuccess,
        )?;
        store_graph_outcome(
            &workspace,
            crate::learning_data::aggregate(&load(&workspace)?.learning),
        )?;

        let document = load(&workspace)?;
        let record = &document.learning.nodes["build"];
        assert_eq!(
            record.outcome,
            Some(crate::learning_data::NodeOutcome::VerifiedSuccess)
        );
        assert_eq!(record.artifacts_produced, vec!["artifact:build-log"]);
        assert_eq!(record.consumed_by, vec!["artifact:input-spec"]);
        assert!(record.human_intervention);
        assert_eq!(record.estimated_cost, Some(1.25));
        assert_eq!(record.actual_cost, Some(1.50));
        assert_eq!(document.graph_hash, graph["graph_hash"]);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn terminal_transitions_persist_and_refresh_graph_outcomes() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".fractal"))?;
        fs::write(
            workspace.join(".fractal").join("lead-prd.json"),
            serde_json::to_vec(&json!({
                "schema": "fractal.prd.v1",
                "acceptance_criteria": [{"id": "AC-1"}]
            }))?,
        )?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [{"id": "build", "capability": "project.tests.execute", "instruction": "Build"}],
            "edges": []
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Build")?;

        checkout_start_node(&workspace, "build", "agent-1", "Agent One")?;
        record_verification_result(&workspace, "build", true, vec!["evidence:ac-1".to_owned()])?;
        finish_node(
            &workspace,
            "build",
            "agent-1",
            crate::learning_data::NodeOutcome::HumanCompleted,
        )?;
        let completed = load(&workspace)?;
        assert_eq!(
            completed
                .execution
                .as_ref()
                .map(|execution| execution.phase.as_str()),
            Some("completed")
        );
        assert_eq!(
            completed
                .learning
                .outcome
                .as_ref()
                .map(|outcome| outcome.acceptance_criteria.len()),
            Some(1)
        );
        assert_eq!(
            completed
                .learning
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.final_verified_success),
            Some(true)
        );

        append_graph_edit_event(
            &workspace,
            crate::learning_data::GraphEditEvent {
                graph_before_hash: completed.graph_hash.clone(),
                action: crate::learning_data::GraphEditAction {
                    kind: "add_branch".to_owned(),
                    ..crate::learning_data::GraphEditAction::default()
                },
                trigger: "late repair".to_owned(),
                actor: "operator".to_owned(),
                timestamp: String::new(),
                eventual_effect: crate::learning_data::EventualEffect::default(),
                ..crate::learning_data::GraphEditEvent::default()
            },
        )?;
        update_graph_edit_event_effect(
            &workspace,
            0,
            crate::learning_data::EventualEffect {
                success: Some(false),
                rework_reduced: Some(false),
                ..crate::learning_data::EventualEffect::default()
            },
        )?;
        assert_eq!(
            load(&workspace)?
                .learning
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.expanded_unnecessarily),
            Some(true)
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn halted_cancellation_persists_a_negative_terminal_outcome() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [{"id": "build", "capability": "code.generate", "instruction": "Build"}],
            "edges": []
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Build")?;

        checkout_start_node(&workspace, "build", "agent-1", "Agent One")?;
        release_node(
            &workspace,
            "build",
            "agent-1",
            Some((
                crate::learning_data::NodeOutcome::Cancelled,
                crate::learning_data::FailureCode::PrematureCompletion,
            )),
        )?;
        let halted = load(&workspace)?;
        assert_eq!(
            halted
                .execution
                .as_ref()
                .map(|execution| execution.phase.as_str()),
            Some("halted")
        );
        let outcome = halted.learning.outcome.expect("halted outcome");
        assert_eq!(outcome.final_verified_success, Some(false));
        assert_eq!(outcome.stopped_too_early, Some(true));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn invalid_learning_records_and_secret_content_are_rejected() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({"schema": "fractal.execution_graph.v1", "nodes": [{"id": "build"}], "edges": []});
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Build")?;
        let mut raw: Value = serde_json::from_slice(&fs::read(path(&workspace))?)?;
        raw["learning"]["nodes"]["build"]["outcome"] = json!("verified_success");
        raw["learning"]["nodes"]["build"]["finished_at"] = json!("2024-01-01T00:00:00Z");
        raw["learning"]["nodes"]["build"]["notes"] = json!("x".repeat(1001));
        fs::write(path(&workspace), serde_json::to_vec_pretty(&raw)?)?;
        assert!(load(&workspace)
            .expect_err("oversized notes must fail")
            .to_string()
            .contains("notes exceed"));

        raw["learning"]["nodes"]["build"]["notes"] = json!("ok");
        raw["learning"]["nodes"]["build"]["api_key"] = json!("must-not-leak");
        fs::write(path(&workspace), serde_json::to_vec_pretty(&raw)?)?;
        assert!(
            load(&workspace).is_err(),
            "secret learning fields must fail"
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn evolved_child_preserves_unknown_fields_and_learning_attribution() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut parent = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_parent",
            "nodes": [{"id": "build", "capability": "code.generate", "instruction": "Build", "future_node_field": true}],
            "edges": [],
            "future_topology_field": {"must": "survive"}
        });
        parent["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&parent).unwrap());
        persist(&workspace, &parent, "Build")?;
        checkout_start_node(&workspace, "build", "agent-1", "Agent One")?;
        finish_node(
            &workspace,
            "build",
            "agent-1",
            crate::learning_data::NodeOutcome::UnverifiedSuccess,
        )?;

        let mut child = parent.clone();
        child["parent_graph"] = parent["graph_hash"].clone();
        child["nodes"].as_array_mut().unwrap().push(json!({"id": "repair", "capability": "code.generate", "instruction": "Repair", "future_child_field": 7}));
        child["edges"]
            .as_array_mut()
            .unwrap()
            .push(json!({"from": "build", "to": "repair"}));
        child.as_object_mut().unwrap().remove("graph_hash");
        child["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&child).unwrap());
        persist_evolved(&workspace, &child)?;

        let document = load(&workspace)?;
        assert_eq!(
            document.learning.nodes["build"]
                .executor
                .as_ref()
                .unwrap()
                .agent
                .as_deref(),
            Some("Agent One")
        );
        assert_eq!(
            document.graph["future_topology_field"],
            json!({"must": "survive"})
        );
        assert_eq!(document.graph["nodes"][1]["future_child_field"], json!(7));
        assert_eq!(document.learning.nodes["repair"].depends_on, vec!["build"]);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    fn sample_catalog_for(workspace_label: &str) -> project_catalog::CatalogV1 {
        use project_catalog::*;
        let canonical = format!("/tmp/{workspace_label}");
        let evidence = CatalogEvidence {
            path: "src/project_file.rs".to_owned(),
            sha256: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_owned(),
            kind: CatalogEvidenceKind::Source,
            observed_commit: Some("56df19ed4dd0f19b56fc2c10faaa40278dc07936".to_owned()),
            spans: None,
            note: None,
            extra: BTreeMap::new(),
        };
        let mut catalog = CatalogV1 {
            schema: CATALOG_SCHEMA.to_owned(),
            project_key: project_key(&canonical),
            generated_at: "2026-08-02T14:05:00Z".to_owned(),
            catalog_hash: String::new(),
            source: CatalogSource {
                canonical_workspace: canonical.clone(),
                workspace_fingerprint: workspace_fingerprint(&canonical),
                registry_numbers: vec![1],
                labels: vec![workspace_label.to_owned()],
                git: CatalogGit {
                    is_git_repository: true,
                    commit: Some("56df19ed4dd0f19b56fc2c10faaa40278dc07936".to_owned()),
                    dirty: Some(false),
                    dirty_fingerprint: None,
                    unavailable_reason: None,
                    remotes: vec![],
                    extra: BTreeMap::new(),
                },
                extra: BTreeMap::new(),
            },
            audit: CatalogAudit {
                auditor: "fractal graph audit".to_owned(),
                cli_version: Some("0.9.4".to_owned()),
                inventory_hash:
                    "sha256:a0bbf8551226effda0186e95c0c2a0ae7efb5edc67d77b992f2b4ec5342b7baa"
                        .to_owned(),
                started_at: "2026-08-02T14:03:12Z".to_owned(),
                finished_at: "2026-08-02T14:05:00Z".to_owned(),
                bounds: CatalogBounds {
                    max_catalog_bytes: Some(DEFAULT_MAX_CATALOG_BYTES as u64),
                    max_evidence_per_claim: Some(20),
                    max_log_excerpt_chars: Some(1024),
                    max_string_chars: Some(2048),
                    test_timeout_ms: Some(600_000),
                    extra: BTreeMap::new(),
                },
                truncated: false,
                evidence_counts: None,
                extra: BTreeMap::new(),
            },
            capabilities: vec![CatalogCapability {
                key: "canonical-project-persistence".to_owned(),
                title: "Persistence".to_owned(),
                description: None,
                status: CatalogStatus::Verified,
                evidence: vec![evidence.clone()],
                test_keys: vec!["cargo-test".to_owned()],
                component_keys: vec!["fractal-cli-bin".to_owned()],
                extra: BTreeMap::new(),
            }],
            components: vec![CatalogComponent {
                key: "fractal-cli-bin".to_owned(),
                name: "fractal-cli".to_owned(),
                kind: CatalogComponentKind::Binary,
                paths: vec!["src".to_owned()],
                description: None,
                status: CatalogStatus::Verified,
                evidence: vec![evidence.clone()],
                extra: BTreeMap::new(),
            }],
            dependencies: vec![],
            tests: vec![CatalogTest {
                key: "cargo-test".to_owned(),
                command: "cargo test --no-fail-fast".to_owned(),
                classification: CatalogTestClassification::Pass,
                exit_code: Some(0),
                duration_ms: Some(10),
                log_sha256: Some(
                    "sha256:1a46b67449e33a32d4f3335cc7072442d774a058db25255a3240579d45c9a0e1"
                        .to_owned(),
                ),
                log_excerpt: None,
                evidence: vec![evidence.clone()],
                extra: BTreeMap::new(),
            }],
            decisions: vec![],
            cross_graph_links: vec![],
            diagnostics: vec![],
            extra: BTreeMap::new(),
        };
        normalize(&mut catalog).expect("normalize catalog fixture");
        catalog
    }

    #[test]
    fn replace_catalog_preserves_sibling_fields_and_unknown_extras() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_catalog",
            "nodes": [{"id": "build", "capability": "code.generate", "instruction": "Build"}],
            "edges": [],
            "future_graph_field": {"kept": true}
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Catalog Preserve")?;
        mutate_document(&workspace, |document| {
            document.extra.insert(
                "future_project_field".to_owned(),
                json!({"must": "survive"}),
            );
            document.efficiency = Some(crate::efficiency::EfficiencyData::for_config(
                crate::efficiency::EfficiencyMode::Observe,
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ));
            Ok(())
        })?;
        checkout_start_node(&workspace, "build", "agent-1", "Agent One")?;

        let before = load(&workspace)?;
        let before_graph = before.graph.clone();
        let before_hash = before.graph_hash.clone();
        let before_execution = serde_json::to_value(&before.execution)?;
        let before_learning = serde_json::to_value(&before.learning)?;
        let before_efficiency = serde_json::to_value(&before.efficiency)?;
        let before_project = serde_json::to_value(&before.project)?;
        let before_future = before.extra.get("future_project_field").cloned();

        let catalog = sample_catalog_for("catalog-preserve-fixture");
        replace_catalog(&workspace, &catalog)?;

        let after = load(&workspace)?;
        assert_eq!(after.graph, before_graph);
        assert_eq!(after.graph_hash, before_hash);
        assert_eq!(serde_json::to_value(&after.execution)?, before_execution);
        assert_eq!(serde_json::to_value(&after.learning)?, before_learning);
        assert_eq!(serde_json::to_value(&after.efficiency)?, before_efficiency);
        assert_eq!(serde_json::to_value(&after.project)?, before_project);
        assert_eq!(
            after.extra.get("future_project_field").cloned(),
            before_future
        );
        assert_eq!(
            after.extra["catalog"]["project_key"],
            json!(catalog.project_key)
        );
        assert_eq!(after.graph["future_graph_field"], json!({"kept": true}));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn invalid_catalog_replace_leaves_on_disk_bytes_unchanged() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [{"id": "build", "capability": "code.generate", "instruction": "Build"}],
            "edges": []
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Catalog Reject")?;
        let before_bytes = fs::read(path(&workspace))?;

        let mut catalog = sample_catalog_for("catalog-reject-fixture");
        catalog.catalog_hash =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned();
        let error = replace_catalog(&workspace, &catalog)
            .expect_err("invalid catalog hash must be refused");
        assert!(error.to_string().contains("catalog_hash"));

        let after_bytes = fs::read(path(&workspace))?;
        assert_eq!(before_bytes, after_bytes);
        assert!(!load(&workspace)?.extra.contains_key("catalog"));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn secret_catalog_replace_is_rejected_without_mutation() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [],
            "edges": []
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Catalog Secret")?;
        let before_bytes = fs::read(path(&workspace))?;

        let mut catalog = sample_catalog_for("catalog-secret-fixture");
        catalog
            .extra
            .insert("token".to_owned(), json!("must-not-leak"));
        project_catalog::normalize(&mut catalog).unwrap();
        let error =
            replace_catalog(&workspace, &catalog).expect_err("secret catalog must be refused");
        assert!(error.to_string().contains("forbidden credential field"));
        assert_eq!(before_bytes, fs::read(path(&workspace))?);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn failure_graph_apis_append_retry_resolve_and_preserve_siblings() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [{"id": "build", "capability": "code.generate", "instruction": "Build"}],
            "edges": [],
            "future_graph_field": {"keep": true}
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Failure graph")?;
        mutate_document(&workspace, |document| {
            document
                .extra
                .insert("future_sibling".to_owned(), json!({"keep": true}));
            Ok(())
        })?;
        let original_hash = load(&workspace)?.graph_hash;
        let first = crate::failure_graph::FailureRecord {
            node_id: "build".to_owned(),
            attempt: 1,
            failure_code: "tool_failure".to_owned(),
            outcome: "failed_execution".to_owned(),
            summary: "compiler failed".to_owned(),
            ..crate::failure_graph::FailureRecord::default()
        };
        let failure_id = append_failure(&workspace, first)?;
        let retry = crate::failure_graph::FailureRecord {
            node_id: "build".to_owned(),
            attempt: 2,
            failure_code: "tool_failure".to_owned(),
            outcome: "failed_execution".to_owned(),
            summary: "compiler failed again".to_owned(),
            ..crate::failure_graph::FailureRecord::default()
        };
        append_failure(&workspace, retry)?;
        let document = load(&workspace)?;
        let stored = document.failure_graph.as_ref().expect("failure graph");
        assert_eq!(stored.failures[&failure_id].observations.len(), 2);
        assert_eq!(stored.failures[&failure_id].attempt, 2);
        assert_eq!(document.graph_hash, original_hash);
        assert_eq!(document.extra["future_sibling"], json!({"keep": true}));
        assert_eq!(document.graph["future_graph_field"], json!({"keep": true}));
        resolve_failure(
            &workspace,
            &failure_id,
            crate::failure_graph::FailureResolution {
                success: true,
                summary: "compiler fixed".to_owned(),
                evidence: vec![crate::failure_graph::EvidenceRef::legacy("test:build")],
                ..crate::failure_graph::FailureResolution::default()
            },
        )?;
        assert_eq!(
            load(&workspace)?.failure_graph.unwrap().failures[&failure_id].state,
            crate::failure_graph::FailureState::Resolved
        );
        let lesson_id = upsert_lesson(
            &workspace,
            crate::failure_graph::LessonRecord {
                summary: "Use a focused compiler check".to_owned(),
                status: crate::failure_graph::LessonStatus::Adopted,
                ..crate::failure_graph::LessonRecord::default()
            },
        )?;
        let edge_id = add_failure_edge(
            &workspace,
            crate::failure_graph::EdgeRecord {
                edge_type: crate::failure_graph::FailureEdgeType::ResolvedBy,
                from: failure_id.clone(),
                to: lesson_id.clone(),
                ..crate::failure_graph::EdgeRecord::default()
            },
        )?;
        let final_graph = load(&workspace)?.failure_graph.unwrap();
        assert!(final_graph.edges.contains_key(&edge_id));
        assert_eq!(final_graph.edges[&edge_id].to, lesson_id);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn legacy_failure_projection_is_read_only() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [{"id": "build", "capability": "code.generate", "instruction": "Build"}],
            "edges": []
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Legacy failure")?;
        let before = fs::read(path(&workspace))?;
        mutate_document(&workspace, |document| {
            let record = document.learning.nodes.get_mut("build").unwrap();
            record.attempt_count = 1;
            record.failure_code = Some(crate::learning_data::FailureCode::ToolFailure);
            record.outcome = Some(crate::learning_data::NodeOutcome::FailedExecution);
            record.finished_at = Some("2024-01-01T00:00:00Z".to_owned());
            Ok(())
        })?;
        let before_read = fs::read(path(&workspace))?;
        let projection = load_failure_graph(&workspace)?;
        assert_eq!(projection.failures.len(), 1);
        assert!(load(&workspace)?.failure_graph.is_none());
        assert_eq!(before_read, fs::read(path(&workspace))?);
        assert_ne!(before, before_read);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }
}
