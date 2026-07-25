//! Standardized, portable per-project execution graph.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct FractalProject {
    pub(crate) schema: String,
    pub(crate) project: ProjectIdentity,
    pub(crate) graph_hash: String,
    pub(crate) graph: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) execution: Option<ExecutionState>,
    pub(crate) updated_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ProjectIdentity {
    pub(crate) slug: String,
    pub(crate) title: String,
    pub(crate) visibility: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ExecutionState {
    pub(crate) schema: String,
    pub(crate) phase: String,
    pub(crate) assignments: BTreeMap<String, ExecutionAssignment>,
    pub(crate) updated_at: String,
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
}

static PROJECT_FILE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal").join("project.fractal")
}

pub(crate) fn persist(workspace: &Path, graph: &Value, title: &str) -> Result<PathBuf> {
    let _guard = project_file_lock();
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
    let slug = slug_for(workspace);
    let now = timestamp();
    let execution = load(workspace)
        .ok()
        .filter(|current| current.graph_hash == graph_hash)
        .and_then(|current| current.execution)
        .or_else(|| execution_from_local_board(graph_hash, graph, &now))
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
                updated_at: now.clone(),
            })
        });
    let document = FractalProject {
        schema: "fractal.project.v1".to_owned(),
        project: ProjectIdentity {
            slug,
            title: clean_title(title, workspace),
            visibility: "private".to_owned(),
        },
        graph_hash: graph_hash.to_owned(),
        graph: graph.clone(),
        execution,
        updated_at: now,
    };
    let destination = path(workspace);
    let directory = destination.parent().expect("project file has parent");
    fs::create_dir_all(directory).with_context(|| format!("create {}", directory.display()))?;
    let bytes = serde_json::to_vec_pretty(&document)?;
    atomic_write(&destination, &bytes)?;
    Ok(destination)
}

fn execution_from_local_board(
    graph_hash: &str,
    graph: &Value,
    updated_at: &str,
) -> Option<ExecutionState> {
    let hash = graph_hash.strip_prefix("sha256:")?;
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let home = std::env::var_os("FRACTAL_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".fractal")))
        .unwrap_or_else(|| PathBuf::from(".fractal"));
    let board_path = home.join("graphs").join(format!("{hash}.board-state.json"));
    let value: Value = serde_json::from_slice(&fs::read(board_path).ok()?).ok()?;
    let assignments: BTreeMap<String, ExecutionAssignment> =
        serde_json::from_value(value.get("assignments")?.clone()).ok()?;
    if assignments.is_empty() {
        return None;
    }
    let node_count = graph
        .get("nodes")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let completed = assignments
        .values()
        .filter(|assignment| assignment.state == "completed")
        .count();
    Some(ExecutionState {
        schema: "fractal.execution_state.v1".to_owned(),
        phase: if node_count > 0 && completed == node_count {
            "completed"
        } else {
            "executing"
        }
        .to_owned(),
        assignments,
        updated_at: updated_at.to_owned(),
    })
}

pub(crate) fn transition(
    workspace: &Path,
    node: &str,
    action: &str,
    agent_id: &str,
    agent_label: &str,
) -> Result<()> {
    let _guard = project_file_lock();
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
        updated_at: now.clone(),
    });
    execution.phase = "executing".to_owned();
    match action {
        "checkout" => {
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
                },
            );
        }
        "complete" => {
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
                },
            );
        }
        "release" => {
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
                },
            );
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
    }
    execution.updated_at = now.clone();
    document.updated_at = now;
    write_document(workspace, &document)
}

pub(crate) fn backfill_execution(workspace: &Path) -> Result<bool> {
    let _guard = project_file_lock();
    let mut document = load(workspace)?;
    if document.execution.is_some() {
        return Ok(false);
    }
    let now = timestamp();
    let Some(execution) = execution_from_local_board(&document.graph_hash, &document.graph, &now)
    else {
        return Ok(false);
    };
    document.execution = Some(execution);
    document.updated_at = now;
    write_document(workspace, &document)?;
    Ok(true)
}

pub(crate) fn release_stale_assignments(workspace: &Path) -> Result<bool> {
    let _guard = project_file_lock();
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
    execution.updated_at = now.clone();
    document.updated_at = now;
    write_document(workspace, &document)?;
    Ok(true)
}

pub(crate) fn set_execution_phase(workspace: &Path, phase: &str) -> Result<()> {
    if !matches!(phase, "planning" | "executing" | "halted" | "completed") {
        bail!("unsupported execution phase `{phase}`");
    }
    let _guard = project_file_lock();
    let mut document = load(workspace)?;
    let now = timestamp();
    let execution = document.execution.get_or_insert_with(|| ExecutionState {
        schema: "fractal.execution_state.v1".to_owned(),
        phase: phase.to_owned(),
        assignments: BTreeMap::new(),
        updated_at: now.clone(),
    });
    execution.phase = phase.to_owned();
    execution.updated_at = now.clone();
    document.updated_at = now;
    write_document(workspace, &document)
}

pub(crate) fn load(workspace: &Path) -> Result<FractalProject> {
    let path = path(workspace);
    let document: FractalProject = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("decode {}", path.display()))?;
    validate(&document)?;
    Ok(document)
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
    if let Some(execution) = &document.execution {
        if execution.schema != "fractal.execution_state.v1"
            || !matches!(
                execution.phase.as_str(),
                "planning" | "executing" | "halted" | "completed"
            )
        {
            bail!("invalid fractal.execution_state.v1 document");
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

fn is_planning_preview(graph: &Value) -> bool {
    let nodes = graph.get("nodes").and_then(Value::as_array);
    nodes.is_some_and(|nodes| {
        nodes.len() == 1
            && nodes[0].get("capability").and_then(Value::as_str) == Some("control.plan")
    })
}

fn write_document(workspace: &Path, document: &FractalProject) -> Result<()> {
    validate(document)?;
    let destination = path(workspace);
    let directory = destination.parent().expect("project file has parent");
    fs::create_dir_all(directory).with_context(|| format!("create {}", directory.display()))?;
    atomic_write(&destination, &serde_json::to_vec_pretty(document)?)
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
        std::env::temp_dir().join(format!(
            "My Expense App {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn persists_portable_standard_document() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
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
        transition(&workspace, "test", "checkout", "codex", "Codex")?;
        transition(&workspace, "build", "complete", "cursor", "Cursor")?;
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
}
