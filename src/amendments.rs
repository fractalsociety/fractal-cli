//! Safe mid-build graph amendment queue and lead-planner expansion.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PendingAmendment {
    pub(crate) command_id: String,
    pub(crate) task_ref: String,
    pub(crate) instruction: String,
    pub(crate) source: String,
}

#[derive(Debug, Deserialize)]
struct PlannerDocument {
    tasks: Vec<PlannerTask>,
}

#[derive(Debug, Deserialize)]
struct PlannerTask {
    id: String,
    title: String,
    #[serde(default)]
    capability: String,
    instruction: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

pub(crate) struct AppliedAmendment {
    pub(crate) command_id: String,
    pub(crate) graph: Value,
    pub(crate) graph_hash: String,
}

fn queue_path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal").join("pending-amendments.jsonl")
}

pub(crate) fn queue(
    workspace: &Path,
    command_id: impl Into<String>,
    task_ref: &str,
    instruction: &str,
    source: &str,
) -> Result<()> {
    let task_ref = task_ref.trim();
    let instruction = instruction.trim();
    if !valid_task_ref(task_ref) {
        bail!("task reference must look like 0.1 or 2.3");
    }
    if instruction.is_empty() || instruction.len() > 4_000 {
        bail!("branch instruction must be 1-4000 characters");
    }
    let path = queue_path(workspace);
    fs::create_dir_all(path.parent().expect("amendment queue has parent"))?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .with_context(|| format!("open amendment queue {}", path.display()))?;
    serde_json::to_writer(
        &mut file,
        &PendingAmendment {
            command_id: command_id.into(),
            task_ref: task_ref.to_owned(),
            instruction: instruction.to_owned(),
            source: source.to_owned(),
        },
    )?;
    file.write_all(b"\n")?;
    file.sync_data().ok();
    Ok(())
}

pub(crate) fn apply_pending(
    mut graph: Value,
    mut graph_hash: String,
    workspace: &Path,
    lead_agent: &str,
) -> (Value, String) {
    for request in drain(workspace) {
        println!(
            "  ✦ [{}] planning a new branch from task {}…",
            lead_agent, request.task_ref
        );
        match apply_one(&graph, &graph_hash, workspace, lead_agent, &request) {
            Ok(applied) => {
                graph = applied.graph;
                graph_hash = applied.graph_hash;
                if let Err(error) = crate::project_file::persist_evolved(workspace, &graph) {
                    eprintln!("  branch graph persist note: {error:#}");
                } else {
                    crate::project_sync::maybe_sync_runtime(workspace);
                }
                if request.command_id.starts_with("amend_") {
                    crate::project_sync::mark_amendment_result(
                        workspace,
                        &applied.command_id,
                        true,
                        None,
                    )
                    .ok();
                }
                println!(
                    "  ✓ accepted branch {} — graph now includes the planner's new tasks",
                    request.task_ref
                );
            }
            Err(error) => {
                eprintln!(
                    "  branch request {} could not be applied: {error:#}",
                    request.task_ref
                );
                if request.command_id.starts_with("amend_") {
                    crate::project_sync::mark_amendment_result(
                        workspace,
                        &request.command_id,
                        false,
                        Some(&format!("{error:#}")),
                    )
                    .ok();
                }
            }
        }
    }
    (graph, graph_hash)
}

fn drain(workspace: &Path) -> Vec<PendingAmendment> {
    let path = queue_path(workspace);
    let processing = path.with_extension(format!("processing-{}", std::process::id()));
    if fs::rename(&path, &processing).is_err() {
        return Vec::new();
    }
    let requests = fs::read_to_string(&processing)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    fs::remove_file(processing).ok();
    requests
}

fn apply_one(
    graph: &Value,
    parent_hash: &str,
    workspace: &Path,
    lead_agent: &str,
    request: &PendingAmendment,
) -> Result<AppliedAmendment> {
    let anchor = resolve_task(graph, &request.task_ref)
        .with_context(|| format!("task {} is not in the current graph", request.task_ref))?;
    let output_path = workspace.join(".fractal").join("fractal-amendment.json");
    fs::remove_file(&output_path).ok();
    let prompt = format!(
        "You are the lead planner amending a live execution graph. The user requested a new \
         dependency branch from task {task_ref} (internal node `{anchor}`):\n\n{instruction}\n\n\
         Write only `.fractal/fractal-amendment.json`. It must be JSON shaped as \
         {{\"tasks\":[{{\"id\":\"short_id\",\"title\":\"...\",\"capability\":\"code.generate\",\
         \"instruction\":\"concrete standalone implementation instruction with files and acceptance behavior\",\
         \"depends_on\":[\"anchor\"]}}]}}. Produce 1-6 bounded tasks. `depends_on` may use `anchor` \
         or an earlier id in this new task list. Include a final project.tests.execute task when \
         the feature changes behavior. Maximize dependency-safe parallelism. Do not edit product files now.",
        task_ref = request.task_ref,
        instruction = request.instruction,
    );
    let timeout = std::env::var("FRACTAL_AGENT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(900_000);
    let run = crate::execute::run_agent_prompt(lead_agent, &prompt, workspace, timeout)
        .with_context(|| format!("launch lead planner `{lead_agent}`"))?;
    if !run.ok {
        bail!(
            "lead planner {}",
            if run.timed_out { "timed out" } else { "failed" }
        );
    }
    let raw = fs::read_to_string(&output_path)
        .context("lead planner did not write .fractal/fractal-amendment.json")?;
    let document: PlannerDocument =
        serde_json::from_str(&raw).context("lead planner wrote invalid amendment JSON")?;
    validate_tasks(&document.tasks)?;

    let (mut harness, work, target) = crate::graph_store::load_source(parent_hash)
        .context("current graph has no recompilable source genome")?;
    let prefix = amendment_prefix(&request.command_id);
    let id_map: BTreeMap<String, String> = document
        .tasks
        .iter()
        .map(|task| {
            (
                task.id.clone(),
                format!("{prefix}.{}", sanitize_id(&task.id)),
            )
        })
        .collect();
    let mut local_dependents = BTreeSet::new();
    let mut new_ids = Vec::new();
    for task in &document.tasks {
        let id = id_map[&task.id].clone();
        let dependencies = if task.depends_on.is_empty() {
            vec![anchor.clone()]
        } else {
            task.depends_on
                .iter()
                .map(|dependency| {
                    if dependency == "anchor" {
                        Ok(anchor.clone())
                    } else {
                        local_dependents.insert(dependency.clone());
                        id_map
                            .get(dependency)
                            .cloned()
                            .ok_or_else(|| anyhow!("unknown amendment dependency `{dependency}`"))
                    }
                })
                .collect::<Result<Vec<_>>>()?
        };
        append_harness_task(&mut harness, &id, task, &dependencies)?;
        new_ids.push((task.id.clone(), id));
    }
    let sinks: Vec<String> = new_ids
        .iter()
        .filter(|(local, _)| !local_dependents.contains(local))
        .map(|(_, id)| id.clone())
        .collect();
    connect_sinks_to_closeout(&mut harness, &sinks);

    let mut child =
        crate::compile::recompile(&work, &harness, &target).context("compile planner amendment")?;
    child["parent_graph"] = json!(parent_hash);
    child["evolution_arm"] = json!("user_branch");
    crate::graph_store::rehash_graph(&mut child)?;
    let record = crate::graph_store::commit_graph(&child)?;
    crate::graph_store::persist_source(&record.graph_hash, &harness, &work, &target).ok();
    Ok(AppliedAmendment {
        command_id: request.command_id.clone(),
        graph: child,
        graph_hash: record.graph_hash,
    })
}

fn resolve_task(graph: &Value, task_ref: &str) -> Option<String> {
    graph
        .get("nodes")?
        .as_array()?
        .iter()
        .find(|node| {
            node.get("id").and_then(Value::as_str) == Some(task_ref)
                || node
                    .get("execution")
                    .and_then(|execution| execution.get("task_number"))
                    .and_then(Value::as_str)
                    == Some(task_ref)
        })?
        .get("id")?
        .as_str()
        .map(str::to_owned)
}

fn append_harness_task(
    harness: &mut Value,
    id: &str,
    task: &PlannerTask,
    dependencies: &[String],
) -> Result<()> {
    let ready = |value: &str| format!("{value}.ready");
    let capability = normalize_capability(&task.capability);
    let nodes = harness
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .context("harness nodes are missing")?;
    nodes.push(json!({
        "id": id,
        "title": task.title.trim(),
        "capability": capability,
        "memory_scopes": ["work:goal", "workspace:root"],
        "preconditions": dependencies.iter().map(|dependency| ready(dependency)).collect::<Vec<_>>(),
        "produced_state": [ready(id)],
        "instruction": task.instruction.trim(),
        "budget": {"timeout_ms": if capability.ends_with("tests.execute") { 120_000 } else { 180_000 }},
    }));
    let edges = harness
        .get_mut("edges")
        .and_then(Value::as_array_mut)
        .context("harness edges are missing")?;
    for dependency in dependencies {
        edges.push(json!({"from": dependency, "to": id, "condition": "success"}));
    }
    Ok(())
}

fn connect_sinks_to_closeout(harness: &mut Value, sinks: &[String]) {
    if let Some(nodes) = harness.get_mut("nodes").and_then(Value::as_array_mut) {
        if let Some(closeout) = nodes
            .iter_mut()
            .find(|node| node.get("id").and_then(Value::as_str) == Some("lead_closeout"))
        {
            if let Some(preconditions) = closeout
                .get_mut("preconditions")
                .and_then(Value::as_array_mut)
            {
                preconditions.extend(sinks.iter().map(|sink| json!(format!("{sink}.ready"))));
            }
        }
    }
    if let Some(edges) = harness.get_mut("edges").and_then(Value::as_array_mut) {
        edges.extend(
            sinks
                .iter()
                .map(|sink| json!({"from": sink, "to": "lead_closeout", "condition": "success"})),
        );
    }
}

fn validate_tasks(tasks: &[PlannerTask]) -> Result<()> {
    if tasks.is_empty() || tasks.len() > 6 {
        bail!("amendment planner must produce 1-6 tasks");
    }
    let mut seen = BTreeSet::new();
    for task in tasks {
        if sanitize_id(&task.id).is_empty()
            || task.title.trim().is_empty()
            || task.instruction.trim().is_empty()
            || !seen.insert(task.id.clone())
        {
            bail!("amendment tasks require unique ids, titles, and instructions");
        }
        if task
            .depends_on
            .iter()
            .any(|dependency| dependency != "anchor" && !seen.contains(dependency))
        {
            bail!("amendment dependencies must reference anchor or an earlier new task");
        }
    }
    Ok(())
}

fn normalize_capability(value: &str) -> &'static str {
    let lower = value.to_ascii_lowercase();
    if lower.contains("test") || lower.contains("verif") {
        "project.tests.execute"
    } else if lower.contains("edit") || lower.contains("review") {
        "code.edit"
    } else if lower.contains("analy") || lower.contains("plan") {
        "content.analyze"
    } else {
        "code.generate"
    }
}

fn amendment_prefix(command_id: &str) -> String {
    let clean = sanitize_id(command_id);
    if clean.is_empty() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or_default();
        format!("branch.{now}")
    } else {
        format!("branch.{clean}")
    }
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .chars()
        .take(64)
        .collect()
}

fn valid_task_ref(value: &str) -> bool {
    let Some((wave, position)) = value.split_once('.') else {
        return false;
    };
    !wave.is_empty()
        && !position.is_empty()
        && wave.bytes().all(|byte| byte.is_ascii_digit())
        && position.bytes().all(|byte| byte.is_ascii_digit())
        && position != "0"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_references_are_wave_dot_position() {
        assert!(valid_task_ref("0.1"));
        assert!(valid_task_ref("12.3"));
        assert!(!valid_task_ref("task-1"));
        assert!(!valid_task_ref("1.0"));
    }
}
