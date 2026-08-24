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
    /// Additive external-review approval/revocation audit. This field is
    /// deliberately outside the immutable execution graph and never changes
    /// graph_hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) external_gate_ledger: Option<crate::external_gates::ExternalGateLedger>,
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

/// Exact, deterministic effect of replacing a halted project's execution
/// graph. The preview is also the optimistic-concurrency token used by apply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct GraphMigrationReport {
    pub(crate) old_graph_hash: String,
    pub(crate) new_graph_hash: String,
    pub(crate) preserved: Vec<String>,
    pub(crate) reopened: Vec<String>,
    pub(crate) removed: Vec<String>,
}

/// Compute a fail-closed migration without writing. Only completed assignments
/// with an unchanged semantic node projection enter the initial preserve set;
/// the fixed-point pass then removes nodes whose new dependencies are not also
/// preserved.
pub(crate) fn preview_halted_graph_migration(
    workspace: &Path,
    graph: &Value,
    forced_reopen: &BTreeSet<String>,
) -> Result<GraphMigrationReport> {
    let current = load(workspace).context("preserve-execution requires an existing project")?;
    plan_halted_graph_migration(&current, graph, forced_reopen)
}

/// Atomically replace the canonical project with the migration preview, after
/// rechecking it under the project lock. Immutable graph-store writes may occur
/// before this call, but canonical project state is never partially updated.
pub(crate) fn apply_halted_graph_migration(
    workspace: &Path,
    graph: &Value,
    forced_reopen: &BTreeSet<String>,
    expected: &GraphMigrationReport,
) -> Result<PathBuf> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut current = load(workspace)?;
    let actual = plan_halted_graph_migration(&current, graph, forced_reopen)?;
    if &actual != expected {
        bail!(
            "halted graph migration preview is stale: expected {} -> {}, found {} -> {}",
            expected.old_graph_hash,
            expected.new_graph_hash,
            actual.old_graph_hash,
            actual.new_graph_hash
        );
    }

    let now = monotonic_timestamp(&current, timestamp());
    let old_learning = current.learning.clone();
    let old_assignments = current
        .execution
        .as_ref()
        .map(|execution| execution.assignments.clone())
        .unwrap_or_default();
    let mut learning = learning_from_graph(graph, &now);
    learning.graph_edits = old_learning.graph_edits.clone();
    learning.extra = old_learning.extra.clone();

    for node in &actual.preserved {
        if let (Some(previous), Some(fresh)) =
            (old_learning.nodes.get(node), learning.nodes.get_mut(node))
        {
            let dependencies = fresh.depends_on.clone();
            *fresh = previous.clone();
            fresh.depends_on = dependencies;
        }
    }
    for node in &actual.reopened {
        if let (Some(previous), Some(fresh)) =
            (old_learning.nodes.get(node), learning.nodes.get_mut(node))
        {
            fresh.attempt_count = previous.attempt_count;
            fresh.reopen_count = previous.reopen_count.saturating_add(1);
        }
    }
    append_bounded_migration_history(&mut learning, &old_learning, &actual, &now)?;
    learning.outcome = None;

    let mut assignments = BTreeMap::new();
    for node in &actual.preserved {
        if let Some(assignment) = old_assignments.get(node) {
            assignments.insert(node.clone(), assignment.clone());
        }
    }
    for node in &actual.reopened {
        if let Some(previous) = old_assignments.get(node) {
            let mut released = previous.clone();
            released.state = "released".to_owned();
            released.completed_at = None;
            released.released_at = Some(now.clone());
            assignments.insert(node.clone(), released);
        }
    }

    let execution_extra = current
        .execution
        .as_ref()
        .map(|execution| execution.extra.clone())
        .unwrap_or_default();
    current.graph_hash = actual.new_graph_hash.clone();
    current.graph = graph.clone();
    current.execution = Some(ExecutionState {
        schema: "fractal.execution_state.v1".to_owned(),
        phase: "halted".to_owned(),
        assignments,
        progress: None,
        updated_at: now.clone(),
        extra: execution_extra,
    });
    current.learning = learning;
    current.updated_at = now;
    write_document(workspace, &current)?;
    Ok(path(workspace))
}

fn plan_halted_graph_migration(
    current: &FractalProject,
    graph: &Value,
    forced_reopen: &BTreeSet<String>,
) -> Result<GraphMigrationReport> {
    let phase = current
        .execution
        .as_ref()
        .map(|execution| execution.phase.as_str())
        .unwrap_or("<missing>");
    if phase != "halted" {
        bail!("preserve-execution requires a halted project; current phase is `{phase}`");
    }
    validate_migration_history(&current.learning.extra)?;
    crate::graph_store::verify_graph_document(graph)
        .context("refuse to migrate to an execution graph with an invalid hash")?;
    reject_secret_fields(graph)?;
    let new_graph_hash = graph
        .get("graph_hash")
        .and_then(Value::as_str)
        .context("replacement execution graph is missing graph_hash")?
        .to_owned();
    let old_nodes = graph_nodes_by_id(&current.graph)?;
    let new_nodes = graph_nodes_by_id(graph)?;
    for node in forced_reopen {
        if node.trim().is_empty() || !new_nodes.contains_key(node) {
            bail!("--reopen references unknown replacement graph node `{node}`");
        }
    }

    let completed = current
        .execution
        .as_ref()
        .map(|execution| {
            execution
                .assignments
                .iter()
                .filter_map(|(node, assignment)| {
                    (assignment.state == "completed").then_some(node.clone())
                })
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut preserved = BTreeSet::new();
    for (id, new_node) in &new_nodes {
        let Some(old_node) = old_nodes.get(id) else {
            continue;
        };
        if completed.contains(id)
            && !forced_reopen.contains(id)
            && semantic_node_projection(old_node) == semantic_node_projection(new_node)
        {
            preserved.insert(id.clone());
        }
    }

    let dependencies = graph_dependencies(graph, new_nodes.keys().cloned().collect())?;
    loop {
        let invalid = preserved
            .iter()
            .filter(|node| {
                dependencies
                    .get(*node)
                    .is_some_and(|values| values.iter().any(|dep| !preserved.contains(dep)))
            })
            .cloned()
            .collect::<Vec<_>>();
        if invalid.is_empty() {
            break;
        }
        for node in invalid {
            preserved.remove(&node);
        }
    }

    let new_ids = new_nodes.keys().cloned().collect::<BTreeSet<_>>();
    let old_ids = old_nodes.keys().cloned().collect::<BTreeSet<_>>();
    Ok(GraphMigrationReport {
        old_graph_hash: current.graph_hash.clone(),
        new_graph_hash,
        preserved: preserved.iter().cloned().collect(),
        reopened: new_ids.difference(&preserved).cloned().collect(),
        removed: old_ids.difference(&new_ids).cloned().collect(),
    })
}

fn graph_nodes_by_id(graph: &Value) -> Result<BTreeMap<String, Value>> {
    let nodes = graph
        .get("nodes")
        .and_then(Value::as_array)
        .context("execution graph nodes must be an array")?;
    let mut indexed = BTreeMap::new();
    for node in nodes {
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .context("execution graph node is missing id")?;
        if indexed.insert(id.to_owned(), node.clone()).is_some() {
            bail!("execution graph contains duplicate node `{id}`");
        }
    }
    Ok(indexed)
}

fn graph_dependencies(
    graph: &Value,
    node_ids: BTreeSet<String>,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut dependencies = node_ids
        .iter()
        .map(|id| (id.clone(), BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for edge in graph
        .get("edges")
        .and_then(Value::as_array)
        .context("execution graph edges must be an array")?
    {
        let from = edge
            .get("from")
            .and_then(Value::as_str)
            .context("execution graph edge is missing from")?;
        let to = edge
            .get("to")
            .and_then(Value::as_str)
            .context("execution graph edge is missing to")?;
        if !node_ids.contains(from) || !node_ids.contains(to) {
            bail!("execution graph edge `{from}` -> `{to}` references an unknown node");
        }
        dependencies
            .get_mut(to)
            .expect("known dependency target")
            .insert(from.to_owned());
    }
    Ok(dependencies)
}

fn semantic_node_projection(node: &Value) -> Value {
    let mut projected = node.clone();
    if let Some(object) = projected.as_object_mut() {
        for field in [
            "execution",
            "depends_on",
            "preconditions",
            "created_at",
            "ready_at",
            "started_at",
            "finished_at",
            "attempt_count",
            "artifacts_produced",
            "consumed_by",
            "human_intervention",
            "outcome",
            "failure_code",
            "verification",
            "actual_cost",
            "notes",
            "reopen_count",
        ] {
            object.remove(field);
        }
        if let Some(efficiency) = object.get_mut("efficiency").and_then(Value::as_object_mut) {
            efficiency.remove("dependencies");
        }
    }
    projected
}

fn append_bounded_migration_history(
    learning: &mut crate::learning_data::LearningData,
    old_learning: &crate::learning_data::LearningData,
    report: &GraphMigrationReport,
    now: &str,
) -> Result<()> {
    const MAX_MIGRATIONS: usize = 16;
    const MAX_RETIRED_NODES: usize = 128;
    let retired_nodes = report
        .removed
        .iter()
        .take(MAX_RETIRED_NODES)
        .map(|id| {
            let record = old_learning.nodes.get(id);
            serde_json::json!({
                "node_id": id,
                "outcome": record.and_then(|record| record.outcome),
                "failure_code": record.and_then(|record| record.failure_code),
                "attempt_count": record.map_or(0, |record| record.attempt_count),
                "finished_at": record.and_then(|record| record.finished_at.as_deref()),
            })
        })
        .collect::<Vec<_>>();
    let entry = serde_json::json!({
        "old_graph_hash": report.old_graph_hash,
        "new_graph_hash": report.new_graph_hash,
        "migrated_at": now,
        "preserved": report.preserved,
        "reopened": report.reopened,
        "removed": report.removed,
        "retired_nodes": retired_nodes,
    });
    let history = learning
        .extra
        .entry("plan_migrations".to_owned())
        .or_insert_with(|| {
            serde_json::json!({
                "schema": "fractal.plan_migrations.v1",
                "records": []
            })
        });
    if history.get("schema").and_then(Value::as_str) != Some("fractal.plan_migrations.v1") {
        bail!("existing plan_migrations history has an unsupported schema");
    }
    let records = history
        .get_mut("records")
        .and_then(Value::as_array_mut)
        .context("existing plan_migrations history records must be an array")?;
    records.push(entry);
    if records.len() > MAX_MIGRATIONS {
        records.drain(0..records.len() - MAX_MIGRATIONS);
    }
    Ok(())
}

fn validate_migration_history(extra: &BTreeMap<String, Value>) -> Result<()> {
    let Some(history) = extra.get("plan_migrations") else {
        return Ok(());
    };
    if history.get("schema").and_then(Value::as_str) != Some("fractal.plan_migrations.v1") {
        bail!("existing plan_migrations history has an unsupported schema");
    }
    if !history.get("records").is_some_and(Value::is_array) {
        bail!("existing plan_migrations history records must be an array");
    }
    Ok(())
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
    persist_locked(workspace, graph, title, None)
}

/// Persist an evolved graph only when the project still points at the expected
/// parent. The parent check and document write happen under the same in-process
/// and canonical project-file locks, so a newer writer cannot slip between the
/// check and the replacement.
pub(crate) fn persist_evolved_if_parent(
    workspace: &Path,
    graph: &Value,
    expected_parent_hash: &str,
) -> Result<PathBuf> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let expected_parent_hash = expected_parent_hash.trim();
    let current = load(workspace)?;
    if current.graph_hash != expected_parent_hash {
        bail!(
            "current project graph hash mismatch: expected {expected_parent_hash}, found {}",
            current.graph_hash
        );
    }
    let graph_parent = graph.get("parent_graph").and_then(Value::as_str);
    if graph_parent != Some(expected_parent_hash) {
        let found = graph_parent.unwrap_or("<missing>");
        bail!("evolved graph parent hash mismatch: expected {expected_parent_hash}, found {found}");
    }
    let title = current.project.title.clone();
    persist_locked(workspace, graph, &title, Some(current))
}

fn persist_locked(
    workspace: &Path,
    graph: &Value,
    title: &str,
    current: Option<FractalProject>,
) -> Result<PathBuf> {
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
    let current = current.or_else(|| load(workspace).ok());
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
    let same_graph = current
        .as_ref()
        .is_some_and(|document| document.graph_hash == graph_hash);
    let mut learning = current
        .as_ref()
        .map(|document| merge_learning(&document.learning, graph, &now, same_graph))
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
    let mut document = FractalProject {
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
        external_gate_ledger: current
            .as_ref()
            .and_then(|document| document.external_gate_ledger.clone()),
        updated_at: now,
        extra: current
            .as_ref()
            .map(|document| document.extra.clone())
            .unwrap_or_default(),
    };
    // A verifier may have completed before a coordinator persisted the next
    // lifecycle snapshot. Reconcile those durable records before deciding
    // whether this graph is terminal so old verifier completions participate
    // in the next aggregate without replaying the node.
    refresh_terminal_outcome(&mut document);
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
        checkout_start_node_in_document(workspace, document, node, agent_id, agent_label, now)
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

pub(crate) fn record_integration_failure(
    workspace: &Path,
    node: &str,
    detail: crate::learning_data::IntegrationFailureDetail,
) -> Result<()> {
    mutate_execution_document(workspace, |document, _now| {
        ensure_known_node(document, node)?;
        let record = document
            .learning
            .nodes
            .get_mut(node)
            .context("learning node missing")?;
        record.integration_failure = Some(detail);
        Ok(())
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
    workspace: &Path,
    document: &mut FractalProject,
    node: &str,
    agent_id: &str,
    agent_label: &str,
    now: &str,
) -> Result<()> {
    ensure_known_node(document, node)?;
    crate::external_gates::enforce_checkout(workspace, document, node, agent_id)?;
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
        record.integration_failure = None;
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
            record.integration_failure = None;
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

/// Reconcile successful verification records to their one intended
/// implementation target. The relation is deliberately conservative: an
/// explicit `verifies`/target field wins, otherwise graph incoming topology
/// must identify exactly one implementation predecessor. Ambiguous, missing,
/// malformed, or non-implementation targets are ignored (fail closed).
fn reconcile_successful_verifications(document: &mut FractalProject) {
    let verifier_ids = document
        .learning
        .nodes
        .iter()
        .filter(|(id, record)| {
            is_verification_graph_node(document, id)
                && record.outcome == Some(crate::learning_data::NodeOutcome::VerifiedSuccess)
                && record.verification.as_ref().and_then(|v| v.passed) == Some(true)
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();

    for verifier_id in verifier_ids {
        let Some(target_id) = verification_target(document, &verifier_id) else {
            continue;
        };
        if !is_implementation_graph_node(document, &target_id) {
            continue;
        }
        let Some(verifier) = document.learning.nodes.get(&verifier_id) else {
            continue;
        };
        let Some(verifier_check) = verifier.verification.as_ref() else {
            continue;
        };
        let evidence_refs = verifier_check.evidence_refs.clone();
        let verifier_finished_at = verifier.finished_at.clone();
        let fallback_finished_at = document.updated_at.clone();
        let Some(target) = document.learning.nodes.get_mut(&target_id) else {
            continue;
        };

        // A successful automated verifier adds verification evidence, but it
        // must not erase a stronger explicit human decision or a superseded
        // lifecycle state already recorded for the implementation target.
        let upgrade_outcome = !matches!(
            target.outcome,
            Some(crate::learning_data::NodeOutcome::HumanCompleted)
                | Some(crate::learning_data::NodeOutcome::Superseded)
        );
        if upgrade_outcome {
            target.outcome = Some(crate::learning_data::NodeOutcome::VerifiedSuccess);
            target.failure_code = None;
        }
        if target.finished_at.is_none() {
            target.finished_at = verifier_finished_at
                .or(Some(fallback_finished_at))
                .or_else(|| Some(timestamp()));
        }
        let mut verification = target.verification.take().unwrap_or_default();
        verification.kind = Some("automated".to_owned());
        verification.passed = Some(true);
        verification.evidence_refs = evidence_refs;
        target.verification = Some(verification);
    }
}

/// Resolve a verifier's target from an explicit relation or a uniquely
/// identifying incoming graph dependency. The boolean distinguishes an
/// explicitly declared but malformed/ambiguous relation from an absent one so
/// malformed declarations cannot silently fall back to guessed topology.
fn verification_target(document: &FractalProject, verifier_id: &str) -> Option<String> {
    let verifier = graph_node(document, verifier_id)?;
    let (explicit, target) = explicit_verification_target(verifier);
    if explicit {
        return target.filter(|target| is_implementation_graph_node(document, target));
    }

    let mut dependencies = BTreeSet::new();
    let mut saw_graph_edge = false;
    if let Some(edges) = document.graph.get("edges").and_then(Value::as_array) {
        for edge in edges {
            if edge.get("to").and_then(Value::as_str) != Some(verifier_id) {
                continue;
            }
            saw_graph_edge = true;
            if edge.get("condition").and_then(Value::as_str) == Some("failure") {
                continue;
            }
            if let Some(from) = edge.get("from").and_then(Value::as_str) {
                dependencies.insert(from.to_owned());
            }
        }
    }
    if let Some(values) = verifier.get("depends_on").and_then(Value::as_array) {
        dependencies.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    if dependencies.is_empty() && !saw_graph_edge {
        dependencies.extend(
            document
                .learning
                .nodes
                .get(verifier_id)
                .into_iter()
                .flat_map(|record| record.depends_on.iter().cloned()),
        );
    }
    // A dangling dependency makes the relation untrustworthy even if another
    // dependency happens to look like an implementation.
    if dependencies
        .iter()
        .any(|dependency| graph_node(document, dependency).is_none())
    {
        return None;
    }
    let implementations = dependencies
        .iter()
        .filter(|dependency| is_implementation_graph_node(document, dependency))
        .cloned()
        .collect::<Vec<_>>();
    (implementations.len() == 1).then(|| implementations[0].clone())
}

fn graph_node<'a>(document: &'a FractalProject, id: &str) -> Option<&'a Value> {
    document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(id))
}

fn is_verification_graph_node(document: &FractalProject, id: &str) -> bool {
    let record_type = document
        .learning
        .nodes
        .get(id)
        .map(|record| record.node_type.as_str());
    if record_type == Some("verification") {
        return true;
    }
    let Some(node) = graph_node(document, id) else {
        return false;
    };
    node.get("node_type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "verification")
        || node
            .get("capability")
            .and_then(Value::as_str)
            .is_some_and(|capability| {
                capability.starts_with("project.tests")
                    || capability.starts_with("python.tests")
                    || capability.contains("verify")
                    || capability.contains("test")
            })
}

fn is_implementation_graph_node(document: &FractalProject, id: &str) -> bool {
    let record_type = document
        .learning
        .nodes
        .get(id)
        .map(|record| record.node_type.as_str());
    if record_type == Some("control") || record_type == Some("verification") {
        return false;
    }
    let Some(node) = graph_node(document, id) else {
        return false;
    };
    if node.get("node_type").and_then(Value::as_str) == Some("control")
        || node.get("node_type").and_then(Value::as_str) == Some("verification")
    {
        return false;
    }
    !is_verification_graph_node(document, id)
}

fn explicit_verification_target(node: &Value) -> (bool, Option<String>) {
    const KEYS: [&str; 8] = [
        "verifies",
        "verifies_node_ids",
        "verifies_node_id",
        "verifies_node",
        "verification_target",
        "target_implementation",
        "target_node",
        "target",
    ];
    let mut saw_declaration = false;
    let mut malformed_declaration = false;
    let mut candidates = BTreeSet::new();
    let mut inspect = |container: &Value| {
        let Some(object) = container.as_object() else {
            return;
        };
        for key in KEYS {
            let Some(value) = object.get(key) else {
                continue;
            };
            saw_declaration = true;
            let Some(values) = relation_values(value) else {
                malformed_declaration = true;
                continue;
            };
            candidates.extend(values);
        }
    };
    inspect(node);
    for key in ["verification", "efficiency", "execution"] {
        if let Some(value) = node.get(key) {
            inspect(value);
        }
    }
    if !saw_declaration || malformed_declaration || candidates.len() != 1 {
        (saw_declaration, None)
    } else {
        (true, candidates.into_iter().next())
    }
}

fn relation_values(value: &Value) -> Option<Vec<String>> {
    if let Some(id) = value.as_str() {
        return (!id.trim().is_empty()).then(|| vec![id.to_owned()]);
    }
    if let Some(values) = value.as_array() {
        let mut ids = Vec::with_capacity(values.len());
        for value in values {
            ids.push(value.as_str()?.to_owned());
        }
        return Some(ids);
    }
    value.as_object().and_then(|object| {
        ["node_id", "target_node", "id"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(Value::as_str))
            .map(|id| vec![id.to_owned()])
    })
}

/// Recompute the graph-level outcome whenever a terminal state is observed.
/// Replacing the prior value is intentional: event eventual effects and late
/// lifecycle facts may arrive after the first terminal write, and aggregation
/// reads only source records so it cannot double-count a refresh.
fn refresh_terminal_outcome(document: &mut FractalProject) {
    // Reconcile every durable successful verifier, not just the verifier that
    // triggered this write. This makes the lifecycle seam self-healing for
    // records written by an older binary before verifier→implementation
    // propagation existed.
    reconcile_successful_verifications(document);
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

#[derive(Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct RecoveryReconciliation {
    pub(crate) adopted: Vec<String>,
    pub(crate) released: Vec<String>,
    pub(crate) frontier: Vec<String>,
    pub(crate) completed: Vec<String>,
    pub(crate) phase: String,
}

/// Reconcile derived run/catalog/checkpoint views back to the canonical portable
/// project file. Completed nodes are never rewritten. Checked-out ownership is
/// adopted when its worker is still known-active and expired exactly once when it
/// is not; already released nodes stay released on repeated passes.
#[allow(dead_code)]
pub(crate) fn reconcile_recovery(
    workspace: &Path,
    active_agent_ids: &BTreeSet<String>,
) -> Result<RecoveryReconciliation> {
    let _guard = project_file_lock();
    let _file_guard = ProjectWriteGuard::acquire(workspace)?;
    let mut document = load(workspace)?;
    let now = timestamp();
    let mut adopted = Vec::new();
    let mut released = Vec::new();
    let execution = document.execution.get_or_insert_with(|| ExecutionState {
        schema: "fractal.execution_state.v1".to_owned(),
        phase: "executing".to_owned(),
        assignments: BTreeMap::new(),
        progress: None,
        updated_at: now.clone(),
        extra: BTreeMap::new(),
    });
    for (node, assignment) in &mut execution.assignments {
        if assignment.state == "checked_out" {
            if active_agent_ids.contains(&assignment.agent_id) {
                adopted.push(node.clone());
            } else {
                assignment.state = "released".to_owned();
                assignment.released_at.get_or_insert_with(|| now.clone());
                released.push(node.clone());
            }
        }
    }
    let completed = execution
        .assignments
        .iter()
        .filter_map(|(node, assignment)| (assignment.state == "completed").then_some(node.clone()))
        .collect::<Vec<_>>();
    let node_count = document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let all_completed = node_count > 0
        && execution.assignments.len() == node_count
        && execution
            .assignments
            .values()
            .all(|assignment| assignment.state == "completed");
    let all_terminal = node_count > 0
        && execution.assignments.len() == node_count
        && execution
            .assignments
            .values()
            .all(|assignment| matches!(assignment.state.as_str(), "completed" | "released"));
    execution.phase = if all_completed {
        "completed".to_owned()
    } else if all_terminal || !released.is_empty() {
        "halted".to_owned()
    } else {
        "executing".to_owned()
    };
    execution.progress = None;
    execution.updated_at = now.clone();
    let phase = execution.phase.clone();
    refresh_terminal_outcome(&mut document);
    let frontier = dependency_ready_frontier_in_document(&document);
    document.updated_at = now;
    write_document(workspace, &document)?;
    Ok(RecoveryReconciliation {
        adopted,
        released,
        frontier,
        completed,
        phase,
    })
}

#[allow(dead_code)]
pub(crate) fn dependency_ready_frontier(workspace: &Path) -> Result<Vec<String>> {
    let document = load(workspace)?;
    Ok(dependency_ready_frontier_in_document(&document))
}

#[allow(dead_code)]
fn dependency_ready_frontier_in_document(document: &FractalProject) -> Vec<String> {
    let assignments = document
        .execution
        .as_ref()
        .map(|execution| &execution.assignments);
    document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.get("id").and_then(Value::as_str))
        .filter(|id| {
            !assignments
                .and_then(|values| values.get(*id))
                .is_some_and(|assignment| {
                    assignment.state == "checked_out" || assignment.state == "completed"
                })
        })
        .filter(|id| dependency_blockers(document, id).is_empty())
        .map(str::to_owned)
        .collect()
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
    let is_verifier_node = is_verification_graph_node(&document, node);
    if action == "checkout" {
        // This is the final TOCTOU authority. Scheduler/frontier filters are
        // advisory; verify the gate ledger while the canonical project lock is
        // held immediately before ownership is written.
        crate::external_gates::enforce_checkout(workspace, &document, node, agent_id)?;
    }
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
                let existing_evidence = record
                    .verification
                    .as_ref()
                    .map(|verification| verification.evidence_refs.clone())
                    .unwrap_or_default();
                record.finished_at = Some(now.clone());
                record.outcome = Some(if is_verifier_node {
                    crate::learning_data::NodeOutcome::VerifiedSuccess
                } else {
                    crate::learning_data::NodeOutcome::UnverifiedSuccess
                });
                if is_verifier_node {
                    record.verification = Some(crate::learning_data::Verification {
                        kind: Some("automated".to_owned()),
                        passed: Some(true),
                        evidence_refs: existing_evidence,
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
    if let Some(ledger) = &document.external_gate_ledger {
        crate::external_gates::validate_ledger(ledger)
            .context("invalid fractal.external_gate_ledger.v1 document")?;
        let encoded =
            serde_json::to_value(ledger).context("encode fractal.external_gate_ledger.v1")?;
        reject_secret_fields(&encoded)
            .context("external gate ledger contains forbidden credential-shaped fields")?;
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
    same_graph: bool,
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
    // Graph outcomes summarize one immutable graph. Carrying a terminal
    // parent's result into an evolved child makes a nonterminal graph appear
    // complete. A same-graph rewrite may retain it; a changed graph starts
    // without an outcome and is recomputed only after terminal assignments.
    merged.outcome = same_graph.then(|| current.outcome.clone()).flatten();
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
        child["parent_graph"] = Value::String(parent_hash.clone());
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

        persist_evolved_if_parent(&workspace, &child, &parent_hash)?;
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
    fn evolved_parent_guard_rejects_stale_writer_without_overwrite() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut parent = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_parent_guard",
            "nodes": [{"id": "build", "capability": "code.generate", "instruction": "Build"}],
            "edges": []
        });
        parent["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&parent)?);
        persist(&workspace, &parent, "Build")?;
        let parent_hash = parent["graph_hash"].as_str().unwrap().to_owned();

        let mut first_child = parent.clone();
        first_child["parent_graph"] = Value::String(parent_hash.clone());
        first_child["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "verify-first", "capability": "project.tests.execute", "instruction": "Verify first"}));
        first_child.as_object_mut().unwrap().remove("graph_hash");
        first_child["graph_hash"] =
            Value::String(fractal_contracts::canonical_sha256(&first_child)?);

        // A newer writer wins before this stale writer reaches the guarded
        // boundary. The stale child must not replace that newer graph.
        let workspace_for_writer = workspace.clone();
        let first_child_for_writer = first_child.clone();
        let parent_hash_for_writer = parent_hash.clone();
        let writer = std::thread::spawn(move || {
            persist_evolved_if_parent(
                &workspace_for_writer,
                &first_child_for_writer,
                &parent_hash_for_writer,
            )
        });
        writer.join().expect("newer writer thread must not panic")?;
        let before = fs::read(path(&workspace))?;

        let error = persist_evolved_if_parent(&workspace, &first_child, &parent_hash)
            .expect_err("stale parent must be rejected atomically");
        assert!(error
            .to_string()
            .contains("current project graph hash mismatch"));
        assert_eq!(fs::read(path(&workspace))?, before);
        assert_eq!(load(&workspace)?.graph_hash, first_child["graph_hash"]);

        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn reconciliation_preserves_completed_releases_stale_once_and_restores_frontier() -> Result<()>
    {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_recovery",
            "nodes": [
                {"id": "a", "capability": "code.generate", "instruction": "A"},
                {"id": "b", "capability": "code.generate", "instruction": "B"},
                {"id": "c", "capability": "code.generate", "instruction": "C"},
                {"id": "d", "capability": "project.tests.execute", "instruction": "D"}
            ],
            "edges": [
                {"from": "a", "to": "c"},
                {"from": "b", "to": "c"},
                {"from": "c", "to": "d"}
            ]
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Recovery")?;
        transition(&workspace, "a", "checkout", "live", "Live")?;
        transition(&workspace, "a", "complete", "live", "Live")?;
        transition(&workspace, "b", "checkout", "dead", "Dead")?;
        transition(&workspace, "c", "checkout", "live", "Live")
            .expect_err("c must not be ready while b is incomplete");

        let first = reconcile_recovery(&workspace, &BTreeSet::new())?;
        assert_eq!(first.completed, vec!["a".to_owned()]);
        assert_eq!(first.released, vec!["b".to_owned()]);
        assert_eq!(first.frontier, vec!["b".to_owned()]);
        assert_eq!(first.phase, "halted");
        let released_at = load(&workspace)?.execution.unwrap().assignments["b"]
            .released_at
            .clone();

        let second = reconcile_recovery(&workspace, &BTreeSet::new())?;
        assert!(second.released.is_empty());
        assert_eq!(
            load(&workspace)?.execution.unwrap().assignments["b"].released_at,
            released_at
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn reconciliation_adopts_valid_checkout_and_exposes_parallel_frontier() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_parallel",
            "nodes": [
                {"id": "root", "capability": "code.generate", "instruction": "Root"},
                {"id": "left", "capability": "code.generate", "instruction": "Left"},
                {"id": "right", "capability": "code.generate", "instruction": "Right"},
                {"id": "join", "capability": "project.tests.execute", "instruction": "Join"}
            ],
            "edges": [
                {"from": "root", "to": "left"},
                {"from": "root", "to": "right"},
                {"from": "left", "to": "join"},
                {"from": "right", "to": "join"}
            ]
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        persist(&workspace, &graph, "Parallel")?;
        transition(&workspace, "root", "checkout", "worker", "Worker")?;
        let active = BTreeSet::from(["worker".to_owned()]);
        let adopted = reconcile_recovery(&workspace, &active)?;
        assert_eq!(adopted.adopted, vec!["root".to_owned()]);
        assert!(adopted.frontier.is_empty());
        transition(&workspace, "root", "complete", "worker", "Worker")?;
        assert_eq!(
            dependency_ready_frontier(&workspace)?,
            vec!["left".to_owned(), "right".to_owned()]
        );
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

    #[test]
    fn evolved_nonterminal_child_discards_parent_graph_outcome() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut parent = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_terminal_parent",
            "nodes": [
                {"id": "build", "capability": "code.generate", "instruction": "Build"},
                {"id": "verify.build", "capability": "project.tests.execute", "instruction": "Verify", "verifies": ["build"]}
            ],
            "edges": [{"from": "build", "to": "verify.build", "condition": "success"}]
        });
        parent["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&parent)?);
        persist(&workspace, &parent, "Terminal parent")?;
        transition(&workspace, "build", "checkout", "worker", "Worker")?;
        transition(&workspace, "build", "complete", "worker", "Worker")?;
        transition(&workspace, "verify.build", "checkout", "worker", "Worker")?;
        transition(&workspace, "verify.build", "complete", "worker", "Worker")?;
        let parent_document = load(&workspace)?;
        assert_eq!(
            parent_document
                .learning
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.final_verified_success),
            Some(true)
        );

        let mut child = parent.clone();
        child["parent_graph"] = parent["graph_hash"].clone();
        child["nodes"]
            .as_array_mut()
            .expect("parent nodes")
            .push(json!({
                "id": "repair",
                "capability": "code.generate",
                "instruction": "Repair"
            }));
        child["edges"]
            .as_array_mut()
            .expect("parent edges")
            .push(json!({"from": "verify.build", "to": "repair", "condition": "success"}));
        child
            .as_object_mut()
            .expect("child object")
            .remove("graph_hash");
        child["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&child)?);
        persist_evolved(&workspace, &child)?;

        let evolved = load(&workspace)?;
        assert!(
            evolved.learning.outcome.is_none(),
            "an evolved graph with an unassigned repair node must not retain the parent's terminal outcome"
        );
        assert_eq!(
            evolved.learning.nodes["verify.build"].outcome,
            Some(crate::learning_data::NodeOutcome::VerifiedSuccess)
        );
        assert!(evolved.learning.nodes["repair"].outcome.is_none());
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn successful_verification_propagates_to_one_explicit_target() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_pair",
            "nodes": [
                {"id": "build", "capability": "code.generate", "instruction": "Build"},
                {"id": "verify.build", "capability": "project.tests.execute", "instruction": "Verify", "verifies": ["build"]}
            ],
            "edges": [{"from": "build", "to": "verify.build", "condition": "success"}]
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph)?);
        persist(&workspace, &graph, "Pair")?;
        transition(&workspace, "build", "checkout", "worker", "Worker")?;
        transition(&workspace, "build", "complete", "worker", "Worker")?;
        transition(&workspace, "verify.build", "checkout", "worker", "Worker")?;
        transition(&workspace, "verify.build", "complete", "worker", "Worker")?;

        let document = load(&workspace)?;
        let build = &document.learning.nodes["build"];
        assert_eq!(
            build.outcome,
            Some(crate::learning_data::NodeOutcome::VerifiedSuccess)
        );
        assert_eq!(
            build
                .verification
                .as_ref()
                .and_then(|verification| verification.passed),
            Some(true)
        );
        assert_eq!(
            build
                .verification
                .as_ref()
                .and_then(|verification| verification.kind.as_deref()),
            Some("automated")
        );
        assert_eq!(
            document.learning.nodes["verify.build"].outcome,
            Some(crate::learning_data::NodeOutcome::VerifiedSuccess)
        );
        assert_eq!(
            document
                .learning
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.final_verified_success),
            Some(true)
        );
        assert_eq!(
            document
                .learning
                .outcome
                .as_ref()
                .map(|outcome| outcome.verification_coverage_denominator),
            Some(2)
        );
        assert_eq!(
            document
                .learning
                .outcome
                .as_ref()
                .unwrap()
                .verification_coverage,
            1.0
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn successful_verification_preserves_human_and_superseded_target_outcomes() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_preserve_target_outcome",
            "nodes": [
                {"id": "build", "capability": "code.generate", "instruction": "Build"},
                {"id": "verify.build", "capability": "project.tests.execute", "instruction": "Verify", "verifies": ["build"]}
            ],
            "edges": [{"from": "build", "to": "verify.build", "condition": "success"}]
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph)?);
        persist(&workspace, &graph, "Preserve target")?;
        transition(&workspace, "build", "checkout", "worker", "Worker")?;
        record_human_intervention(&workspace, "build", Some("human acceptance"))?;
        finish_node(
            &workspace,
            "build",
            "worker",
            crate::learning_data::NodeOutcome::HumanCompleted,
        )?;
        transition(&workspace, "verify.build", "checkout", "worker", "Worker")?;
        transition(&workspace, "verify.build", "complete", "worker", "Worker")?;
        let document = load(&workspace)?;
        assert_eq!(
            document.learning.nodes["build"].outcome,
            Some(crate::learning_data::NodeOutcome::HumanCompleted)
        );
        assert_eq!(
            document.learning.nodes["build"]
                .verification
                .as_ref()
                .and_then(|verification| verification.passed),
            Some(true)
        );
        fs::remove_dir_all(&workspace)?;

        // Repeat the verifier flow with a target explicitly marked
        // superseded. Successful verification should attach evidence while
        // preserving that lifecycle outcome too.
        let superseded_workspace = temp_workspace();
        fs::create_dir_all(&superseded_workspace)?;
        persist(&superseded_workspace, &graph, "Preserve superseded target")?;
        transition(
            &superseded_workspace,
            "build",
            "checkout",
            "worker",
            "Worker",
        )?;
        finish_node(
            &superseded_workspace,
            "build",
            "worker",
            crate::learning_data::NodeOutcome::Superseded,
        )?;
        transition(
            &superseded_workspace,
            "verify.build",
            "checkout",
            "worker",
            "Worker",
        )?;
        transition(
            &superseded_workspace,
            "verify.build",
            "complete",
            "worker",
            "Worker",
        )?;
        let superseded_document = load(&superseded_workspace)?;
        assert_eq!(
            superseded_document.learning.nodes["build"].outcome,
            Some(crate::learning_data::NodeOutcome::Superseded)
        );
        assert_eq!(
            superseded_document.learning.nodes["build"]
                .verification
                .as_ref()
                .and_then(|verification| verification.passed),
            Some(true)
        );
        fs::remove_dir_all(superseded_workspace)?;
        Ok(())
    }

    #[test]
    fn verification_target_resolution_fails_closed_when_missing_or_ambiguous() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_pair_fail_closed",
            "nodes": [
                {"id": "build-a", "capability": "code.generate", "instruction": "Build A"},
                {"id": "build-b", "capability": "code.generate", "instruction": "Build B"},
                {"id": "build-c", "capability": "code.generate", "instruction": "Build C"},
                {"id": "verify-ambiguous", "capability": "project.tests.execute", "instruction": "Verify A or B"},
                {"id": "verify-missing", "capability": "project.tests.execute", "instruction": "Verify without target"}
            ],
            "edges": [
                {"from": "build-a", "to": "verify-ambiguous", "condition": "success"},
                {"from": "build-b", "to": "verify-ambiguous", "condition": "success"}
            ]
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph)?);
        persist(&workspace, &graph, "Fail closed")?;
        for node in ["build-a", "build-b"] {
            transition(&workspace, node, "checkout", "worker", "Worker")?;
            transition(&workspace, node, "complete", "worker", "Worker")?;
        }
        transition(
            &workspace,
            "verify-ambiguous",
            "checkout",
            "worker",
            "Worker",
        )?;
        transition(
            &workspace,
            "verify-ambiguous",
            "complete",
            "worker",
            "Worker",
        )?;
        transition(&workspace, "verify-missing", "checkout", "worker", "Worker")?;
        transition(&workspace, "verify-missing", "complete", "worker", "Worker")?;

        let document = load(&workspace)?;
        for node in ["build-a", "build-b", "build-c"] {
            assert_ne!(
                document.learning.nodes[node].outcome,
                Some(crate::learning_data::NodeOutcome::VerifiedSuccess),
                "{node} must not be guessed as an ambiguous/missing verifier target"
            );
        }
        assert_eq!(
            document.learning.nodes["verify-ambiguous"].outcome,
            Some(crate::learning_data::NodeOutcome::VerifiedSuccess)
        );
        assert_eq!(
            document.learning.nodes["verify-missing"].outcome,
            Some(crate::learning_data::NodeOutcome::VerifiedSuccess)
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn historical_successful_verifier_is_reconciled_on_next_refresh() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_historical_pair",
            "nodes": [
                {"id": "build", "capability": "code.generate", "instruction": "Build"},
                {"id": "verify.build", "capability": "project.tests.execute", "instruction": "Verify"}
            ],
            "edges": [{"from": "build", "to": "verify.build", "condition": "success"}]
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph)?);
        persist(&workspace, &graph, "Historical")?;
        transition(&workspace, "build", "checkout", "worker", "Worker")?;
        transition(&workspace, "build", "complete", "worker", "Worker")?;
        transition(&workspace, "verify.build", "checkout", "worker", "Worker")?;
        transition(&workspace, "verify.build", "complete", "worker", "Worker")?;

        // Emulate a project written by the old binary: the verifier is
        // terminal, but its implementation target is still unverified.
        let mut raw: Value = serde_json::from_slice(&fs::read(path(&workspace))?)?;
        raw["learning"]["nodes"]["build"]["outcome"] = json!("unverified_success");
        raw["learning"]["nodes"]["build"]
            .as_object_mut()
            .expect("build record")
            .remove("verification");
        fs::write(path(&workspace), serde_json::to_vec_pretty(&raw)?)?;
        assert_eq!(
            load(&workspace)?.learning.nodes["build"].outcome,
            Some(crate::learning_data::NodeOutcome::UnverifiedSuccess)
        );

        set_execution_phase(&workspace, "completed")?;
        let reconciled = load(&workspace)?;
        assert_eq!(
            reconciled.learning.nodes["build"].outcome,
            Some(crate::learning_data::NodeOutcome::VerifiedSuccess)
        );
        assert_eq!(
            reconciled.learning.nodes["build"]
                .verification
                .as_ref()
                .and_then(|verification| verification.passed),
            Some(true)
        );
        assert_eq!(
            reconciled
                .learning
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.final_verified_success),
            Some(true)
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn fully_paired_graph_aggregates_every_implementation_and_verifier() -> Result<()> {
        const PAIRS: usize = 54;
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut nodes = Vec::with_capacity(PAIRS * 2);
        let mut edges = Vec::with_capacity(PAIRS);
        for index in 0..PAIRS {
            let implementation = format!("implementation-{index}");
            let verifier = format!("verification-{index}");
            nodes.push(json!({"id": implementation, "capability": "code.generate", "instruction": "Implement"}));
            nodes.push(json!({"id": verifier, "capability": "project.tests.execute", "instruction": "Verify"}));
            edges.push(json!({"from": implementation, "to": verifier, "condition": "success"}));
        }
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_fifty_four_pairs",
            "nodes": nodes,
            "edges": edges
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph)?);
        persist(&workspace, &graph, "54 pairs")?;
        for index in 0..PAIRS {
            let implementation = format!("implementation-{index}");
            let verifier = format!("verification-{index}");
            transition(&workspace, &implementation, "checkout", "worker", "Worker")?;
            transition(&workspace, &implementation, "complete", "worker", "Worker")?;
            transition(&workspace, &verifier, "checkout", "worker", "Worker")?;
            transition(&workspace, &verifier, "complete", "worker", "Worker")?;
        }
        let document = load(&workspace)?;
        let outcome = document.learning.outcome.expect("terminal aggregate");
        assert_eq!(
            outcome.verification_coverage_denominator,
            (PAIRS * 2) as u32
        );
        assert_eq!(outcome.verification_coverage, 1.0);
        assert_eq!(outcome.final_verified_success, Some(true));
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

    fn migration_graph(revision: &str, nodes: &[(&str, &str)], edges: &[(&str, &str)]) -> Value {
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": revision,
            "nodes": nodes.iter().map(|(id, instruction)| json!({
                "id": id,
                "title": id,
                "capability": "code.generate",
                "instruction": instruction,
                "execution": {"wave": 7, "task_number": "7.1"},
                "depends_on": edges.iter()
                    .filter(|(_, to)| to == id)
                    .map(|(from, _)| Value::String((*from).to_owned()))
                    .collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "edges": edges.iter().map(|(from, to)| json!({
                "from": from,
                "to": to,
                "condition": "success"
            })).collect::<Vec<_>>(),
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        graph
    }

    fn seed_halted_migration_project(
        workspace: &Path,
        graph: &Value,
        completed: &[&str],
    ) -> Result<()> {
        fs::create_dir_all(workspace)?;
        persist(workspace, graph, "Migration fixture")?;
        mutate_document(workspace, |document| {
            let now = "2026-01-01T00:00:00Z".to_owned();
            let execution = document.execution.as_mut().expect("execution");
            execution.phase = "halted".to_owned();
            for node in completed {
                execution.assignments.insert(
                    (*node).to_owned(),
                    ExecutionAssignment {
                        agent_id: format!("agent-{node}"),
                        agent_label: format!("Agent {node}"),
                        state: "completed".to_owned(),
                        checked_out_at: now.clone(),
                        completed_at: Some(now.clone()),
                        released_at: None,
                        extra: BTreeMap::new(),
                    },
                );
            }
            Ok(())
        })
    }

    #[test]
    fn halted_graph_migration_requires_existing_project() {
        let workspace = temp_workspace();
        let graph = migration_graph("new", &[("a", "same")], &[]);
        let error = preview_halted_graph_migration(&workspace, &graph, &BTreeSet::new())
            .expect_err("missing project must fail closed");
        assert!(format!("{error:#}").contains("requires an existing project"));
        assert!(!path(&workspace).exists());
    }

    #[test]
    fn halted_graph_migration_refuses_active_project_without_write() -> Result<()> {
        let workspace = temp_workspace();
        let old = migration_graph("old", &[("a", "same")], &[]);
        fs::create_dir_all(&workspace)?;
        persist(&workspace, &old, "Active")?;
        let before = fs::read(path(&workspace))?;
        let new = migration_graph("new", &[("a", "same")], &[]);
        let error = preview_halted_graph_migration(&workspace, &new, &BTreeSet::new())
            .expect_err("executing project must be refused");
        assert!(format!("{error:#}").contains("requires a halted project"));
        assert_eq!(fs::read(path(&workspace))?, before);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn halted_graph_migration_preserves_unchanged_completed_assignment() -> Result<()> {
        let workspace = temp_workspace();
        let old = migration_graph("old", &[("a", "same")], &[]);
        seed_halted_migration_project(&workspace, &old, &["a"])?;
        let new = migration_graph("new", &[("a", "same")], &[]);
        let preview = preview_halted_graph_migration(&workspace, &new, &BTreeSet::new())?;
        assert_eq!(preview.preserved, ["a"]);
        assert!(preview.reopened.is_empty());
        apply_halted_graph_migration(&workspace, &new, &BTreeSet::new(), &preview)?;
        assert_eq!(
            load(&workspace)?.execution.unwrap().assignments["a"].state,
            "completed"
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn halted_graph_migration_reopens_semantically_changed_node() -> Result<()> {
        let workspace = temp_workspace();
        let old = migration_graph("old", &[("a", "old instruction")], &[]);
        seed_halted_migration_project(&workspace, &old, &["a"])?;
        let new = migration_graph("new", &[("a", "corrected instruction")], &[]);
        let preview = preview_halted_graph_migration(&workspace, &new, &BTreeSet::new())?;
        assert!(preview.preserved.is_empty());
        assert_eq!(preview.reopened, ["a"]);
        apply_halted_graph_migration(&workspace, &new, &BTreeSet::new(), &preview)?;
        let assignment = load(&workspace)?
            .execution
            .unwrap()
            .assignments
            .remove("a")
            .unwrap();
        assert_eq!(assignment.state, "released");
        assert!(assignment.completed_at.is_none());
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn halted_graph_migration_enforces_preserved_dependency_closure() -> Result<()> {
        let workspace = temp_workspace();
        let old = migration_graph("old", &[("a", "old"), ("b", "same")], &[("a", "b")]);
        seed_halted_migration_project(&workspace, &old, &["a", "b"])?;
        let new = migration_graph("new", &[("a", "changed"), ("b", "same")], &[("a", "b")]);
        let preview = preview_halted_graph_migration(&workspace, &new, &BTreeSet::new())?;
        assert!(preview.preserved.is_empty());
        assert_eq!(preview.reopened, ["a", "b"]);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn halted_graph_migration_honors_forced_reopen() -> Result<()> {
        let workspace = temp_workspace();
        let old = migration_graph("old", &[("companion_export_contract_tests", "same")], &[]);
        seed_halted_migration_project(&workspace, &old, &["companion_export_contract_tests"])?;
        let new = migration_graph("new", &[("companion_export_contract_tests", "same")], &[]);
        let forced = BTreeSet::from(["companion_export_contract_tests".to_owned()]);
        let preview = preview_halted_graph_migration(&workspace, &new, &forced)?;
        assert!(preview.preserved.is_empty());
        assert_eq!(preview.reopened, ["companion_export_contract_tests"]);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn halted_graph_migration_removes_active_node_and_keeps_bounded_history() -> Result<()> {
        let workspace = temp_workspace();
        let old = migration_graph(
            "old",
            &[("keep", "same"), ("dynamic_verifier", "verify")],
            &[],
        );
        seed_halted_migration_project(&workspace, &old, &["keep", "dynamic_verifier"])?;
        let new = migration_graph("new", &[("keep", "same")], &[]);
        let preview = preview_halted_graph_migration(&workspace, &new, &BTreeSet::new())?;
        assert_eq!(preview.removed, ["dynamic_verifier"]);
        apply_halted_graph_migration(&workspace, &new, &BTreeSet::new(), &preview)?;
        let migrated = load(&workspace)?;
        assert!(!migrated
            .execution
            .unwrap()
            .assignments
            .contains_key("dynamic_verifier"));
        assert_eq!(
            migrated.learning.extra["plan_migrations"]["records"][0]["retired_nodes"][0]["node_id"],
            "dynamic_verifier"
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn halted_graph_migration_invalid_replacement_is_atomic() -> Result<()> {
        let workspace = temp_workspace();
        let old = migration_graph("old", &[("a", "same")], &[]);
        seed_halted_migration_project(&workspace, &old, &["a"])?;
        let before = fs::read(path(&workspace))?;
        let replacement = migration_graph("new", &[("a", "same")], &[]);
        let mut stale = preview_halted_graph_migration(&workspace, &replacement, &BTreeSet::new())?;
        stale.preserved.clear();
        assert!(
            apply_halted_graph_migration(&workspace, &replacement, &BTreeSet::new(), &stale)
                .is_err()
        );
        assert_eq!(fs::read(path(&workspace))?, before);

        let mut invalid = migration_graph("new", &[("a", "same")], &[]);
        invalid["nodes"][0]["instruction"] = Value::String("tampered".to_owned());
        assert!(preview_halted_graph_migration(&workspace, &invalid, &BTreeSet::new()).is_err());
        assert_eq!(fs::read(path(&workspace))?, before);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }
}
