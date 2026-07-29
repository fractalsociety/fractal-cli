//! Safe mid-build graph amendment queue and lead-planner expansion.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::compile::{baseline_node_efficiency, node_efficiency_to_graph_value};
use crate::efficiency::{validate_node_metadata, NodeEfficiencyMetadata};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PendingAmendment {
    pub(crate) command_id: String,
    #[serde(default = "default_action")]
    pub(crate) action: String,
    #[serde(default)]
    pub(crate) task_ref: String,
    #[serde(default)]
    pub(crate) wave: Option<u32>,
    pub(crate) instruction: String,
    pub(crate) source: String,
}

fn default_action() -> String {
    "add_branch".to_owned()
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
    /// Planning efficiency metadata; older planners may omit it and a
    /// deterministic baseline is synthesized so every amended node exposes it.
    #[serde(default)]
    efficiency: Option<NodeEfficiencyMetadata>,
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
    action: &str,
    task_ref: &str,
    wave: Option<u32>,
    instruction: &str,
    source: &str,
) -> Result<()> {
    if !matches!(action, "add_branch" | "add_wave_task") {
        bail!("unsupported graph amendment action `{action}`");
    }
    let task_ref = task_ref.trim();
    let instruction = instruction.trim();
    if action == "add_branch" && !valid_task_ref(task_ref) {
        bail!("task reference must look like 0.1 or 2.3");
    }
    if action == "add_wave_task" && !matches!(wave, Some(1..)) {
        bail!("wave task amendments require wave 1 or later");
    }
    if instruction.is_empty() || instruction.len() > 4_000 {
        bail!("amendment instruction must be 1-4000 characters");
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
            action: action.to_owned(),
            task_ref: task_ref.to_owned(),
            wave,
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
        if request.action == "add_wave_task" {
            println!(
                "  ✦ [{}] planning a new peer task for wave {}…",
                lead_agent,
                request.wave.unwrap_or_default()
            );
        } else {
            println!(
                "  ✦ [{}] planning a complete build branch from task {}…",
                lead_agent, request.task_ref
            );
        }
        match apply_one(&graph, &graph_hash, workspace, lead_agent, &request) {
            Ok(applied) => {
                let created_nodes = applied
                    .graph
                    .get("nodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|node| node.get("id").and_then(Value::as_str))
                    .filter(|id| {
                        !graph
                            .get("nodes")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .any(|node| node.get("id").and_then(Value::as_str) == Some(*id))
                    })
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let graph_before_hash = graph_hash.clone();
                graph = applied.graph;
                graph_hash = applied.graph_hash;
                if let Err(error) = crate::project_file::persist_evolved(workspace, &graph) {
                    eprintln!("  branch graph persist note: {error:#}");
                } else {
                    crate::project_file::record_graph_edit(
                        workspace,
                        &graph_before_hash,
                        &request.action,
                        (!request.task_ref.is_empty()).then_some(request.task_ref.as_str()),
                        created_nodes,
                        "human_amendment",
                        &request.source,
                    )
                    .ok();
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
                if request.action == "add_wave_task" {
                    println!(
                        "  ✓ added a task to wave {} — later waves now wait for it",
                        request.wave.unwrap_or_default()
                    );
                } else {
                    println!(
                        "  ✓ accepted branch {} — graph now includes the complete build branch",
                        request.task_ref
                    );
                }
            }
            Err(error) => {
                eprintln!(
                    "  {} request could not be applied: {error:#}",
                    if request.action == "add_wave_task" {
                        format!("wave {}", request.wave.unwrap_or_default())
                    } else {
                        format!("branch {}", request.task_ref)
                    }
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
    let (anchor, wave_dependencies, wave_downstream) = if request.action == "add_wave_task" {
        let wave = request
            .wave
            .context("wave task request is missing its wave")?;
        let (dependencies, downstream) = resolve_wave_flow(graph, wave)?;
        (None, dependencies, downstream)
    } else {
        let anchor = resolve_task(graph, &request.task_ref)
            .with_context(|| format!("task {} is not in the current graph", request.task_ref))?;
        (Some(anchor), Vec::new(), Vec::new())
    };
    let output_path = workspace.join(".fractal").join("fractal-amendment.json");
    fs::remove_file(&output_path).ok();
    let prompt = if request.action == "add_wave_task" {
        format!(
            "You are the lead planner adding one peer task to wave {wave} of a live execution \
             graph. The user requested:\n\n{instruction}\n\nWrite only \
             `.fractal/fractal-amendment.json` as \
             {{\"tasks\":[{{\"id\":\"short_id\",\"title\":\"...\",\"capability\":\"code.generate\",\
             \"instruction\":\"concrete standalone implementation instruction with files and \
             acceptance behavior\",\"depends_on\":[],\"efficiency\":{{\
             \"estimated_remaining_tokens\":12000,\"dependencies\":[],\
             \"expected_artifact\":\"the concrete artifact produced\",\
             \"files_or_systems_affected\":[\"path/to/file\"],\
             \"verification_plan\":\"how the result is verified\",\"current_assumptions\":[],\
             \"similarity_to_other_active_nodes\":{{}},\"confidence_still_useful\":0.9}}}}]}}. \
             Produce exactly one bounded task that \
             can execute alongside the existing work in wave {wave}. Scores and confidence live \
             in 0..=1 and file references contain no whitespace. Do not create a new feature \
             branch and do not edit product files now.",
            wave = request.wave.unwrap_or_default(),
            instruction = request.instruction,
        )
    } else {
        format!(
            "You are the lead planner amending a live execution graph. The user requested a \
             complete new build branch from task {task_ref} (internal node `{anchor}`):\n\n\
             {instruction}\n\nWrite only `.fractal/fractal-amendment.json`. It must be JSON shaped \
             as {{\"tasks\":[{{\"id\":\"short_id\",\"title\":\"...\",\
             \"capability\":\"code.generate\",\"instruction\":\"concrete standalone implementation \
             instruction with files and acceptance behavior\",\"depends_on\":[\"anchor\"],\
             \"efficiency\":{{\"estimated_remaining_tokens\":12000,\"dependencies\":[\"anchor\"],\
             \"expected_artifact\":\"the concrete artifact produced\",\
             \"files_or_systems_affected\":[\"path/to/file\"],\
             \"verification_plan\":\"how the result is verified\",\"current_assumptions\":[],\
             \"similarity_to_other_active_nodes\":{{}},\"confidence_still_useful\":0.9}}}}]}}. \
             Produce 2-8 bounded tasks forming a complete feature branch: implementation, any \
             supporting integration work, and a final project.tests.execute verification task. \
             `depends_on` may use `anchor` or an earlier id in this new task list; each task's \
             `efficiency.dependencies` repeats its `depends_on`, scores and confidence live in \
             0..=1, and file references contain no whitespace. Maximize \
             dependency-safe parallelism inside the branch. Do not edit product files now.",
            task_ref = request.task_ref,
            anchor = anchor.as_deref().unwrap_or_default(),
            instruction = request.instruction,
        )
    };
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
    validate_tasks(&document.tasks, &request.action)?;

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
        let dependencies = if request.action == "add_wave_task" {
            wave_dependencies.clone()
        } else if task.depends_on.is_empty() {
            vec![anchor.clone().expect("branch amendment has an anchor")]
        } else {
            task.depends_on
                .iter()
                .map(|dependency| {
                    if dependency == "anchor" {
                        Ok(anchor.clone().expect("branch amendment has an anchor"))
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
        let efficiency = resolve_task_efficiency(task, &dependencies, &id_map)?;
        append_harness_task(&mut harness, &id, task, &dependencies, &efficiency)?;
        new_ids.push((task.id.clone(), id));
    }
    let sinks: Vec<String> = new_ids
        .iter()
        .filter(|(local, _)| !local_dependents.contains(local))
        .map(|(_, id)| id.clone())
        .collect();
    let branch_depth = anchor
        .as_deref()
        .map(|anchor| branch_depth(graph, anchor) + 1)
        .unwrap_or(0);
    record_amendment_metadata(
        &mut harness,
        &new_ids.iter().map(|(_, id)| id.clone()).collect::<Vec<_>>(),
        &prefix,
        anchor.as_deref(),
        branch_depth,
        if request.action == "add_wave_task" {
            "wave_task"
        } else {
            "branch"
        },
    )?;
    if request.action == "add_wave_task" && !wave_downstream.is_empty() {
        connect_sinks_to_nodes(&mut harness, &sinks, &wave_downstream);
    } else {
        connect_sinks_to_closeout(&mut harness, &sinks);
    }

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

fn resolve_wave_flow(graph: &Value, wave: u32) -> Result<(Vec<String>, Vec<String>)> {
    if wave == 0 {
        bail!("new build tasks cannot be added to the planning wave");
    }
    let nodes = graph
        .get("nodes")
        .and_then(Value::as_array)
        .context("current graph nodes are missing")?;
    let target_ids: BTreeSet<String> = nodes
        .iter()
        .filter(|node| {
            node.get("execution")
                .and_then(|execution| execution.get("wave"))
                .and_then(Value::as_u64)
                == Some(u64::from(wave))
        })
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    if target_ids.is_empty() {
        bail!("wave {wave} is not in the current graph");
    }
    let edges = graph
        .get("edges")
        .and_then(Value::as_array)
        .context("current graph edges are missing")?;
    let mut dependencies = BTreeSet::new();
    let mut downstream = BTreeSet::new();
    for edge in edges {
        if edge.get("condition").and_then(Value::as_str) == Some("failure") {
            continue;
        }
        let Some(from) = edge.get("from").and_then(Value::as_str) else {
            continue;
        };
        let Some(to) = edge.get("to").and_then(Value::as_str) else {
            continue;
        };
        if target_ids.contains(to) && !target_ids.contains(from) {
            dependencies.insert(from.to_owned());
        }
        if target_ids.contains(from) && !target_ids.contains(to) {
            downstream.insert(to.to_owned());
        }
    }
    Ok((
        dependencies.into_iter().collect(),
        downstream.into_iter().collect(),
    ))
}

fn branch_depth(graph: &Value, node_id: &str) -> u32 {
    graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(node_id))
        .and_then(|node| node.get("execution"))
        .and_then(|execution| execution.get("branch_depth"))
        .and_then(Value::as_u64)
        .and_then(|depth| u32::try_from(depth).ok())
        .unwrap_or(0)
}

fn record_amendment_metadata(
    harness: &mut Value,
    node_ids: &[String],
    branch_id: &str,
    branch_parent: Option<&str>,
    branch_depth: u32,
    amendment_kind: &str,
) -> Result<()> {
    let harness = harness
        .as_object_mut()
        .context("harness document must be an object")?;
    let metadata = harness
        .entry("fractal_amendments")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("harness fractal_amendments must be an object")?;
    for id in node_ids {
        metadata.insert(
            id.clone(),
            json!({
                "amendment_kind": amendment_kind,
                "branch_id": if amendment_kind == "branch" {
                    Value::String(branch_id.to_owned())
                } else {
                    Value::Null
                },
                "branch_parent": branch_parent,
                "branch_depth": branch_depth,
            }),
        );
    }
    Ok(())
}

/// Resolve the planning efficiency metadata an amended node will expose. The
/// declared metadata (already range-validated) has its similarity peers remapped
/// into the amendment's namespaced ids; a missing block gets a deterministic
/// baseline. Either way the exposed `dependencies` are the node's ACTUAL
/// resolved graph dependencies, so the compiler's consistency gate holds.
fn resolve_task_efficiency(
    task: &PlannerTask,
    dependencies: &[String],
    id_map: &BTreeMap<String, String>,
) -> Result<NodeEfficiencyMetadata> {
    let mut meta = match &task.efficiency {
        Some(declared) => {
            let mut meta = declared.clone();
            meta.similarity_to_other_active_nodes = declared
                .similarity_to_other_active_nodes
                .iter()
                .map(|(peer, score)| {
                    (
                        id_map.get(peer).cloned().unwrap_or_else(|| peer.clone()),
                        *score,
                    )
                })
                .collect();
            meta
        }
        None => baseline_node_efficiency(
            12_000,
            Vec::new(),
            task.title.trim(),
            Vec::new(),
            "Verified by the amendment's gating project.tests.execute task.",
        ),
    };
    meta.dependencies = dependencies.to_vec();
    validate_node_metadata(&meta)
        .map_err(|error| anyhow!("amendment task `{}` efficiency metadata: {error}", task.id))?;
    Ok(meta)
}

fn append_harness_task(
    harness: &mut Value,
    id: &str,
    task: &PlannerTask,
    dependencies: &[String],
    efficiency: &NodeEfficiencyMetadata,
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
        "efficiency": node_efficiency_to_graph_value(efficiency),
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

fn connect_sinks_to_nodes(harness: &mut Value, sinks: &[String], downstream: &[String]) {
    if let Some(nodes) = harness.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes.iter_mut().filter(|node| {
            node.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| downstream.iter().any(|candidate| candidate == id))
        }) {
            if let Some(preconditions) = node.get_mut("preconditions").and_then(Value::as_array_mut)
            {
                preconditions.extend(sinks.iter().map(|sink| json!(format!("{sink}.ready"))));
            }
        }
    }
    if let Some(edges) = harness.get_mut("edges").and_then(Value::as_array_mut) {
        edges.extend(downstream.iter().flat_map(|target| {
            sinks
                .iter()
                .map(move |sink| json!({"from": sink, "to": target, "condition": "success"}))
        }));
    }
}

fn validate_tasks(tasks: &[PlannerTask], action: &str) -> Result<()> {
    if action == "add_wave_task" && tasks.len() != 1 {
        bail!("wave task planner must produce exactly one task");
    }
    if action == "add_branch" && !(2..=8).contains(&tasks.len()) {
        bail!("branch planner must produce 2-8 tasks");
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
        if let Some(meta) = &task.efficiency {
            validate_node_metadata(meta).map_err(|error| {
                anyhow!("amendment task `{}` efficiency metadata: {error}", task.id)
            })?;
            if meta.dependencies.iter().any(|dependency| {
                dependency != "anchor" && (dependency == &task.id || !seen.contains(dependency))
            }) {
                bail!(
                    "amendment task `{}` efficiency dependencies must reference anchor or an earlier new task",
                    task.id
                );
            }
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

    #[test]
    fn wave_flow_reuses_predecessors_and_blocks_downstream_work() {
        let graph = json!({
            "nodes": [
                {"id":"plan","execution":{"wave":0}},
                {"id":"shell","execution":{"wave":1}},
                {"id":"model","execution":{"wave":1}},
                {"id":"verify","execution":{"wave":2}}
            ],
            "edges": [
                {"from":"plan","to":"shell","condition":"success"},
                {"from":"plan","to":"model","condition":"success"},
                {"from":"shell","to":"verify","condition":"success"},
                {"from":"model","to":"verify","condition":"success"}
            ]
        });
        let (dependencies, downstream) = resolve_wave_flow(&graph, 1).unwrap();
        assert_eq!(dependencies, vec!["plan"]);
        assert_eq!(downstream, vec!["verify"]);
        assert!(resolve_wave_flow(&graph, 0).is_err());
        assert!(resolve_wave_flow(&graph, 9).is_err());
    }

    fn planner_task(
        id: &str,
        depends_on: Vec<String>,
        efficiency: Option<NodeEfficiencyMetadata>,
    ) -> PlannerTask {
        PlannerTask {
            id: id.to_owned(),
            title: format!("{id} title"),
            capability: "code.generate".to_owned(),
            instruction: "do the work".to_owned(),
            depends_on,
            efficiency,
        }
    }

    #[test]
    fn declared_amendment_efficiency_is_range_and_reference_checked() {
        let meta = baseline_node_efficiency(
            5_000,
            vec!["anchor".to_owned()],
            "the new module",
            vec![],
            "verified by the branch tests task",
        );
        let tasks = vec![
            planner_task("impl", vec!["anchor".to_owned()], Some(meta.clone())),
            planner_task("verify", vec!["impl".to_owned()], None),
        ];
        validate_tasks(&tasks, "add_branch").expect("valid declared metadata");

        let mut bad_range = meta.clone();
        bad_range.confidence_still_useful = 2.0;
        let tasks = vec![
            planner_task("impl", vec!["anchor".to_owned()], Some(bad_range)),
            planner_task("verify", vec!["impl".to_owned()], None),
        ];
        assert!(validate_tasks(&tasks, "add_branch")
            .unwrap_err()
            .to_string()
            .contains("confidence_still_useful"));

        let mut unknown_dependency = meta;
        unknown_dependency.dependencies = vec!["ghost".to_owned()];
        let tasks = vec![
            planner_task("impl", vec!["anchor".to_owned()], Some(unknown_dependency)),
            planner_task("verify", vec!["impl".to_owned()], None),
        ];
        assert!(validate_tasks(&tasks, "add_branch")
            .unwrap_err()
            .to_string()
            .contains("efficiency dependencies"));
    }

    #[test]
    fn resolved_amendment_efficiency_tracks_graph_dependencies_and_remaps_peers() {
        let mut meta = baseline_node_efficiency(
            5_000,
            vec!["anchor".to_owned()],
            "the new module",
            vec![],
            "verified by the branch tests task",
        );
        meta.similarity_to_other_active_nodes
            .insert("other".to_owned(), 0.5);
        let task = planner_task("impl", vec!["anchor".to_owned()], Some(meta));
        let id_map = BTreeMap::from([
            ("impl".to_owned(), "branch.cmd.impl".to_owned()),
            ("other".to_owned(), "branch.cmd.other".to_owned()),
        ]);
        let resolved =
            resolve_task_efficiency(&task, &["build".to_owned()], &id_map).expect("resolved");
        assert_eq!(resolved.dependencies, vec!["build".to_owned()]);
        assert_eq!(
            resolved
                .similarity_to_other_active_nodes
                .get("branch.cmd.other"),
            Some(&0.5)
        );

        // A legacy planner without the block gets a deterministic baseline.
        let legacy = planner_task("impl", vec!["anchor".to_owned()], None);
        let resolved =
            resolve_task_efficiency(&legacy, &["build".to_owned()], &id_map).expect("baseline");
        assert_eq!(resolved.dependencies, vec!["build".to_owned()]);
        assert_eq!(resolved.expected_artifact, "impl title");
        assert!((resolved.confidence_still_useful - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn appended_amendment_nodes_expose_efficiency_metadata() {
        let mut harness = json!({"nodes": [], "edges": []});
        let task = planner_task("impl", vec![], None);
        let meta = resolve_task_efficiency(&task, &["build".to_owned()], &BTreeMap::new())
            .expect("baseline metadata");
        append_harness_task(
            &mut harness,
            "branch.cmd.impl",
            &task,
            &["build".to_owned()],
            &meta,
        )
        .expect("append");
        let node = &harness["nodes"][0];
        assert_eq!(node["efficiency"]["dependencies"], json!(["build"]));
        assert_eq!(
            node["efficiency"]["estimated_remaining_tokens"],
            json!(12_000)
        );
        assert_eq!(node["efficiency"]["confidence_still_useful"], json!("1"));
        assert_eq!(
            harness["edges"][0],
            json!({"from": "build", "to": "branch.cmd.impl", "condition": "success"})
        );
    }

    #[test]
    fn amendment_metadata_preserves_nested_branch_depth() {
        let mut harness = json!({"nodes":[],"edges":[]});
        record_amendment_metadata(
            &mut harness,
            &["branch.feature".to_owned()],
            "branch.amend_1",
            Some("build"),
            2,
            "branch",
        )
        .unwrap();
        assert_eq!(
            harness["fractal_amendments"]["branch.feature"]["branch_depth"],
            json!(2)
        );
        assert_eq!(
            harness["fractal_amendments"]["branch.feature"]["branch_parent"],
            json!("build")
        );
    }
}
