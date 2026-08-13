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
    /// Optional dependency node/task ref for add/remove dependency edits.
    #[serde(default)]
    pub(crate) dependency: Option<String>,
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
    /// Existing nodes whose structural lifecycle ended in this edit.
    pub(crate) retired_nodes: Vec<String>,
}

fn queue_path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal").join("pending-amendments.jsonl")
}

fn failure_path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal").join("failed-amendments.jsonl")
}

pub(crate) fn has_pending(workspace: &Path) -> bool {
    pending_files(workspace).into_iter().any(|path| {
        fs::read_to_string(path)
            .ok()
            .is_some_and(|raw| raw.lines().any(|line| !line.trim().is_empty()))
    })
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
    if !matches!(action, "add_branch" | "add_wave_task" | "add_team_wave") {
        bail!("unsupported graph amendment action `{action}`");
    }
    let task_ref = task_ref.trim();
    let instruction = instruction.trim();
    if action == "add_branch" && !valid_task_ref(task_ref) {
        bail!("task reference must look like 0.1 or 2.3");
    }
    if is_wave_action(action) && !matches!(wave, Some(1..)) {
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
            dependency: None,
        },
    )?;
    file.write_all(b"\n")?;
    file.sync_data().ok();
    Ok(())
}

/// Queue a controlled human graph edit that applies without invoking a planner.
#[allow(dead_code)]
pub(crate) fn queue_edit(
    workspace: &Path,
    command_id: impl Into<String>,
    action: &str,
    task_ref: &str,
    dependency: Option<&str>,
    instruction: &str,
    source: &str,
) -> Result<()> {
    if !is_direct_edit(action) {
        bail!("unsupported direct graph edit action `{action}`");
    }
    let task_ref = task_ref.trim();
    if !valid_task_ref(task_ref) && resolve_task_id_only(task_ref).is_none() {
        // Accept wave.position refs; node ids are validated at apply time.
        if task_ref.is_empty() || task_ref.len() > 120 {
            bail!("direct edit target must be a non-empty task reference");
        }
    }
    if matches!(action, "add_dependency" | "remove_dependency")
        && dependency
            .map(str::trim)
            .is_none_or(|value| value.is_empty())
    {
        bail!("dependency edits require a dependency reference");
    }
    if action == "reroute_node" && instruction.trim().is_empty() {
        bail!("reroute edits require a replacement instruction");
    }
    if instruction.len() > 4_000 {
        bail!("amendment instruction must be at most 4000 characters");
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
            wave: None,
            instruction: instruction.to_owned(),
            source: source.to_owned(),
            dependency: dependency.map(|value| value.trim().to_owned()),
        },
    )?;
    file.write_all(b"\n")?;
    file.sync_data().ok();
    Ok(())
}

#[allow(dead_code)]
fn resolve_task_id_only(task_ref: &str) -> Option<&str> {
    (!task_ref.is_empty() && !task_ref.contains('.')).then_some(task_ref)
}

fn is_direct_edit(action: &str) -> bool {
    matches!(
        action,
        "split_node" | "reroute_node" | "cancel_node" | "add_dependency" | "remove_dependency"
    )
}

pub(crate) fn apply_pending(
    graph: Value,
    graph_hash: String,
    workspace: &Path,
    lead_agent: &str,
) -> (Value, String) {
    apply_pending_limit(graph, graph_hash, workspace, lead_agent, usize::MAX)
}

/// Apply one actionable amendment and preserve the rest of the claimed batch.
/// Coordinators use this to return to worker joins between slow planner calls.
pub(crate) fn apply_next_pending(
    graph: Value,
    graph_hash: String,
    workspace: &Path,
    lead_agent: &str,
) -> (Value, String) {
    apply_pending_limit(graph, graph_hash, workspace, lead_agent, 1)
}

fn apply_pending_limit(
    mut graph: Value,
    mut graph_hash: String,
    workspace: &Path,
    lead_agent: &str,
    max_actionable: usize,
) -> (Value, String) {
    if graph.get("graph_hash").and_then(Value::as_str) != Some(graph_hash.as_str())
        || crate::graph_store::verify_graph_document(&graph).is_err()
    {
        eprintln!("  amendment note: refusing to mutate a graph with an invalid parent hash");
        return (graph, graph_hash);
    }
    let (pending, claimed_files) = match claim_pending(workspace) {
        Ok(claimed) => claimed,
        Err(error) => {
            eprintln!("  amendment note: could not claim pending queue: {error:#}");
            return (graph, graph_hash);
        }
    };
    if pending.is_empty() {
        return (graph, graph_hash);
    }
    let mut remaining = Vec::new();
    let mut retryable_failures = Vec::new();
    let mut seen = BTreeSet::new();
    let mut attempted = 0usize;
    for request in pending {
        if request.command_id.trim().is_empty() || !seen.insert(request.command_id.clone()) {
            continue;
        }
        if amendment_already_applied(&graph, &request.command_id) {
            continue;
        }
        if attempted >= max_actionable {
            remaining.push(request);
            continue;
        }
        attempted = attempted.saturating_add(1);
        if is_wave_action(&request.action) {
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
                    remaining.push(request);
                    continue;
                }
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
                if !applied.retired_nodes.is_empty() {
                    mark_retired_nodes(workspace, &applied.retired_nodes, &request.action).ok();
                }
                crate::project_sync::maybe_sync_runtime(workspace);
                if request.command_id.starts_with("amend_") {
                    crate::project_sync::mark_amendment_result(
                        workspace,
                        &applied.command_id,
                        true,
                        None,
                    )
                    .ok();
                }
                if is_wave_action(&request.action) {
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
                let error_text = format!("{error:#}");
                let retryable = !is_permanent_failure(&request, &error_text);
                // Rotate a failed request behind untouched work. Otherwise
                // `apply_next_pending` retries the same head item forever and
                // starves every valid amendment queued after it.
                if retryable {
                    retryable_failures.push(request.clone());
                }
                if let Err(persist_error) =
                    record_failed(workspace, &request, &error_text, retryable)
                {
                    eprintln!("  amendment failure persistence note: {persist_error:#}");
                }
                eprintln!(
                    "  {} request could not be applied: {error:#}",
                    if is_wave_action(&request.action) {
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
    remaining.extend(retryable_failures);
    if let Err(error) = finish_claim(workspace, &claimed_files, &remaining) {
        eprintln!("  amendment queue rewrite note: {error:#}");
    }
    (graph, graph_hash)
}

fn is_permanent_failure(request: &PendingAmendment, error: &str) -> bool {
    request.action == "add_team_wave"
        && request.source == "master_architect"
        && error.starts_with("wave ")
        && error.ends_with(" is not in the current graph")
}

fn record_failed(
    workspace: &Path,
    request: &PendingAmendment,
    error: &str,
    retryable: bool,
) -> Result<()> {
    let path = failure_path(workspace);
    fs::create_dir_all(path.parent().expect("amendment failure queue has parent"))?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .with_context(|| format!("open amendment failure queue {}", path.display()))?;
    serde_json::to_writer(
        &mut file,
        &json!({
            "request": request,
            "error": error,
            "retryable": retryable,
        }),
    )?;
    file.write_all(b"\n")?;
    file.sync_data().ok();
    Ok(())
}

fn pending_files(workspace: &Path) -> Vec<PathBuf> {
    let path = queue_path(workspace);
    let mut paths = Vec::new();
    if path.is_file() {
        paths.push(path.clone());
    }
    if let Some(directory) = path.parent() {
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.flatten() {
                let candidate = entry.path();
                let Some(name) = candidate.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if name.starts_with("pending-amendments.processing") {
                    paths.push(candidate);
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Atomically moves the live append queue aside before reading it. New
/// amendments can then append to a fresh queue while a slow planner runs;
/// finishing this claim must never rewrite or delete those concurrent writes.
fn claim_pending(workspace: &Path) -> Result<(Vec<PendingAmendment>, Vec<PathBuf>)> {
    let mut requests = Vec::new();
    let path = queue_path(workspace);
    fs::create_dir_all(path.parent().expect("amendment queue has parent"))?;
    if path.is_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let processing = path.with_extension(format!("processing-{}-{nonce}", std::process::id()));
        fs::rename(&path, &processing).with_context(|| {
            format!(
                "claim amendment queue {} as {}",
                path.display(),
                processing.display()
            )
        })?;
    }
    let claimed = pending_files(workspace)
        .into_iter()
        .filter(|candidate| candidate != &path)
        .collect::<Vec<_>>();
    for claimed_path in &claimed {
        read_pending_file(claimed_path, &mut requests);
    }
    Ok((requests, claimed))
}

fn read_pending_file(path: &Path, requests: &mut Vec<PendingAmendment>) {
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    requests.extend(
        raw.lines()
            .filter_map(|line| serde_json::from_str::<PendingAmendment>(line).ok()),
    );
}

fn finish_claim(
    workspace: &Path,
    claimed_files: &[PathBuf],
    requests: &[PendingAmendment],
) -> Result<()> {
    let path = queue_path(workspace);
    fs::create_dir_all(path.parent().expect("amendment queue has parent"))?;
    if !requests.is_empty() {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&path)
            .with_context(|| format!("requeue amendments in {}", path.display()))?;
        for request in requests {
            serde_json::to_writer(&mut file, request)?;
            file.write_all(b"\n")?;
        }
        file.sync_data().ok();
    }
    for claimed in claimed_files {
        let _ = fs::remove_file(claimed);
    }
    Ok(())
}

fn amendment_already_applied(graph: &Value, command_id: &str) -> bool {
    let recorded = graph
        .get("applied_amendment_command_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|value| value.as_str() == Some(command_id));
    if recorded {
        return true;
    }
    let prefix = format!("{}.", amendment_prefix(command_id));
    graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.get("id").and_then(Value::as_str))
        .any(|node_id| node_id.starts_with(&prefix))
}

fn mark_amendment_applied(graph: &mut Value, command_id: &str) {
    let Some(object) = graph.as_object_mut() else {
        return;
    };
    let entry = object
        .entry("applied_amendment_command_ids".to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entry.is_array() {
        *entry = Value::Array(Vec::new());
    }
    let values = entry.as_array_mut().expect("array after initialization");
    if !values
        .iter()
        .any(|value| value.as_str() == Some(command_id))
    {
        values.push(Value::String(command_id.to_owned()));
    }
}

fn apply_one(
    graph: &Value,
    parent_hash: &str,
    workspace: &Path,
    lead_agent: &str,
    request: &PendingAmendment,
) -> Result<AppliedAmendment> {
    if is_direct_edit(&request.action) {
        return apply_direct_edit(graph, parent_hash, request);
    }
    let (anchor, wave_dependencies, wave_downstream) = if is_wave_action(&request.action) {
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
    let prompt = if request.action == "add_team_wave" {
        format!(
            "You are the master architect forming one specialist team mission in wave {wave}. \
             The mission request is:\n\n{instruction}\n\nWrite only \
             `.fractal/fractal-amendment.json` using the standard amendment task schema. \
             Produce exactly five independent, artifact-disjoint implementation or verification \
             tasks for one coherent specialization. Every task must include bounded efficiency \
             metadata, a concrete owned path, measurable acceptance behavior, and empty \
             `depends_on` plus empty `efficiency.dependencies`; the controller resolves canonical \
             wave dependencies. These five tasks will later be delegated by one team leader to \
             five workers. This invocation is planning-only: do not join collaboration sessions, \
             start receive loops, or wait for messages. Bound reconnaissance to twelve read-only \
             commands and prefer the authoritative status/index documents over broad repository \
             scans. Keep every efficiency text field credential-neutral: do not use the words \
             authorization, api_key, apikey, password, private_key, private-key, secret, cookie, \
             bearer, or token=. Do not edit product files or create branches now.",
            wave = request.wave.unwrap_or_default(),
            instruction = request.instruction,
        )
    } else if request.action == "add_wave_task" {
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
             in 0..=1 and file references contain no whitespace. Keep `expected_artifact`, \
             `verification_plan`, and each assumption at or below 480 UTF-8 bytes; use no more \
             than 64 affected paths and 32 assumptions. Leave both `depends_on` and \
             `efficiency.dependencies` empty because the coordinator resolves the live wave's \
             canonical dependencies after planning. Do not create a new feature branch and do \
             not edit product files now.",
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
    let run = crate::execute::run_lead_agent_prompt(lead_agent, &prompt, workspace, timeout)
        .with_context(|| format!("launch lead planner `{lead_agent}`"))?;
    if !run.ok {
        bail!(
            "lead planner {}",
            if run.timed_out { "timed out" } else { "failed" }
        );
    }
    let raw = fs::read_to_string(&output_path)
        .context("lead planner did not write .fractal/fractal-amendment.json")?;
    let mut document: PlannerDocument =
        serde_json::from_str(&raw).context("lead planner wrote invalid amendment JSON")?;
    normalize_planner_metadata(&mut document.tasks);
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
    let efficiency_id_map = similarity_peer_map(graph, &id_map);
    let mut local_dependents = BTreeSet::new();
    let mut new_ids = Vec::new();
    for task in &document.tasks {
        let id = id_map[&task.id].clone();
        let dependencies = if is_wave_action(&request.action) {
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
        let efficiency = resolve_task_efficiency(task, &dependencies, &efficiency_id_map)?;
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
        if request.action == "add_team_wave" {
            "team_wave"
        } else if request.action == "add_wave_task" {
            "wave_task"
        } else {
            "branch"
        },
    )?;
    if is_wave_action(&request.action) && !wave_downstream.is_empty() {
        connect_sinks_to_nodes(&mut harness, &sinks, &wave_downstream);
    } else {
        connect_sinks_to_closeout(&mut harness, &sinks);
    }

    let mut child =
        crate::compile::recompile(&work, &harness, &target).context("compile planner amendment")?;
    child["parent_graph"] = json!(parent_hash);
    child["evolution_arm"] = json!("user_branch");
    for applied in graph
        .get("applied_amendment_command_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        mark_amendment_applied(&mut child, applied);
    }
    mark_amendment_applied(&mut child, &request.command_id);
    crate::graph_store::rehash_graph(&mut child)?;
    let record = crate::graph_store::commit_graph(&child)?;
    crate::graph_store::persist_source(&record.graph_hash, &harness, &work, &target).ok();
    Ok(AppliedAmendment {
        command_id: request.command_id.clone(),
        graph: child,
        graph_hash: record.graph_hash,
        retired_nodes: Vec::new(),
    })
}

fn similarity_peer_map(
    graph: &Value,
    amendment_ids: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut resolved = amendment_ids.clone();
    let mut suffixes: BTreeMap<String, Option<String>> = BTreeMap::new();
    for node in graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        resolved
            .entry(id.to_owned())
            .or_insert_with(|| id.to_owned());
        if let Some(task_number) = node
            .pointer("/execution/task_number")
            .and_then(Value::as_str)
        {
            resolved
                .entry(task_number.to_owned())
                .or_insert_with(|| id.to_owned());
        }
        let suffix = id.rsplit('.').next().unwrap_or(id).to_owned();
        suffixes
            .entry(suffix)
            .and_modify(|candidate| *candidate = None)
            .or_insert_with(|| Some(id.to_owned()));
    }
    for (suffix, candidate) in suffixes {
        if let Some(id) = candidate {
            resolved.entry(suffix).or_insert(id);
        }
    }
    resolved
}

fn normalize_planner_metadata(tasks: &mut [PlannerTask]) {
    for task in tasks {
        let Some(meta) = task.efficiency.as_mut() else {
            continue;
        };
        meta.expected_artifact = bounded_planner_text(&meta.expected_artifact);
        meta.verification_plan = bounded_planner_text(&meta.verification_plan);
        for assumption in &mut meta.current_assumptions {
            *assumption = bounded_planner_text(assumption);
        }
    }
}

fn bounded_planner_text(text: &str) -> String {
    let text = text.trim();
    let mut cut = text.len().min(crate::efficiency::MAX_BASIS_BYTES);
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_owned()
}

/// Apply a controlled human edit directly to the immutable graph. This path
/// never invokes a planner: it verifies the parent's hash, rejects no-ops and
/// cycles, records structural/controlled fields, and commits a rehashed child.
fn apply_direct_edit(
    graph: &Value,
    parent_hash: &str,
    request: &PendingAmendment,
) -> Result<AppliedAmendment> {
    if graph.get("graph_hash").and_then(Value::as_str) != Some(parent_hash) {
        bail!("direct edit parent hash does not match the current graph");
    }
    crate::graph_store::verify_graph_document(graph).context("current graph hash is invalid")?;
    let target = resolve_task(graph, &request.task_ref)
        .with_context(|| format!("task {} is not in the current graph", request.task_ref))?;
    let dependency = request
        .dependency
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| resolve_task(graph, value).or_else(|| Some(value.to_owned())));
    let mut child = graph.clone();
    child["parent_graph"] = json!(parent_hash);
    child["evolution_arm"] = json!(format!("human_{}", request.action));
    let mut retired_nodes = Vec::new();

    match request.action.as_str() {
        "split_node" => {
            let suffix = sanitize_id(&request.command_id);
            let created = format!(
                "{}.split.{}",
                target,
                if suffix.is_empty() { "human" } else { &suffix }
            );
            if node_exists(&child, &created) {
                bail!("split node `{created}` already exists");
            }
            let mut created_node = node_mut(&mut child, &target)?.clone();
            let instruction = if request.instruction.trim().is_empty() {
                format!("Complete the human-requested split follow-up for `{target}`.")
            } else {
                request.instruction.trim().to_owned()
            };
            initialize_direct_node(&mut created_node, &created, &instruction, "created")?;
            child
                .get_mut("nodes")
                .and_then(Value::as_array_mut)
                .context("graph nodes are missing")?
                .push(created_node);
            let target_node = node_mut(&mut child, &target)?
                .as_object_mut()
                .context("graph node must be an object")?;
            target_node.insert("structural_outcome".to_owned(), json!("superseded"));
            target_node.insert("controlled_outcome".to_owned(), json!("accepted"));
            target_node.insert("human_intervention".to_owned(), json!(true));
            child
                .get_mut("edges")
                .and_then(Value::as_array_mut)
                .context("graph edges are missing")?
                .push(json!({"from": target, "to": created, "condition": "success"}));
            retired_nodes.push(target.clone());
        }
        "reroute_node" => {
            let instruction = request.instruction.trim();
            if instruction.is_empty() {
                bail!("reroute edits require a replacement instruction");
            }
            let object = node_mut(&mut child, &target)?
                .as_object_mut()
                .context("graph node must be an object")?;
            if object.get("instruction").and_then(Value::as_str) == Some(instruction) {
                bail!("reroute is a no-op");
            }
            object.insert("instruction".to_owned(), json!(instruction));
            object.insert("human_intervention".to_owned(), json!(true));
            object.insert("structural_outcome".to_owned(), json!("rerouted"));
            object.insert("controlled_outcome".to_owned(), json!("accepted"));
        }
        "cancel_node" => {
            let object = node_mut(&mut child, &target)?
                .as_object_mut()
                .context("graph node must be an object")?;
            if object.get("controlled_outcome").and_then(Value::as_str) == Some("cancelled") {
                bail!("cancel is a no-op");
            }
            object.insert("structural_outcome".to_owned(), json!("cancelled"));
            object.insert("controlled_outcome".to_owned(), json!("cancelled"));
            object.insert("human_intervention".to_owned(), json!(true));
            object.insert("capability".to_owned(), json!("control.cancelled"));
            object.insert(
                "instruction".to_owned(),
                json!("Cancelled by accepted human graph edit."),
            );
            retired_nodes.push(target.clone());
        }
        "add_dependency" => {
            let dependency = dependency.context("add_dependency requires dependency")?;
            if dependency == target {
                bail!("a node cannot depend on itself");
            }
            if edge_exists(&child, &dependency, &target) {
                bail!("dependency edit is a no-op");
            }
            if path_exists(&child, &target, &dependency) {
                bail!("dependency edit would create a cycle");
            }
            child
                .get_mut("edges")
                .and_then(Value::as_array_mut)
                .context("graph edges are missing")?
                .push(json!({"from": dependency, "to": target, "condition": "success"}));
        }
        "remove_dependency" => {
            let dependency = dependency.context("remove_dependency requires dependency")?;
            let edges = child
                .get_mut("edges")
                .and_then(Value::as_array_mut)
                .context("graph edges are missing")?;
            let before = edges.len();
            edges.retain(|edge| {
                !(edge.get("from").and_then(Value::as_str) == Some(dependency.as_str())
                    && edge.get("to").and_then(Value::as_str) == Some(target.as_str())
                    && edge
                        .get("condition")
                        .and_then(Value::as_str)
                        .is_none_or(|condition| condition != "failure"))
            });
            if edges.len() == before {
                bail!("dependency edit is a no-op");
            }
        }
        _ => bail!("unsupported graph edit action `{}`", request.action),
    }
    rebuild_dependencies(&mut child)?;
    mark_amendment_applied(&mut child, &request.command_id);
    crate::graph_store::rehash_graph(&mut child)?;
    let record = crate::graph_store::commit_graph(&child)?;
    Ok(AppliedAmendment {
        command_id: request.command_id.clone(),
        graph: child,
        graph_hash: record.graph_hash,
        retired_nodes,
    })
}

fn node_exists(graph: &Value, id: &str) -> bool {
    graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|node| node.get("id").and_then(Value::as_str) == Some(id))
}

fn node_mut<'a>(graph: &'a mut Value, id: &str) -> Result<&'a mut Value> {
    graph
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(id))
        .with_context(|| format!("graph node `{id}` is missing"))
}

fn edge_exists(graph: &Value, from: &str, to: &str) -> bool {
    graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|edge| {
            edge.get("from").and_then(Value::as_str) == Some(from)
                && edge.get("to").and_then(Value::as_str) == Some(to)
                && edge
                    .get("condition")
                    .and_then(Value::as_str)
                    .is_none_or(|condition| condition != "failure")
        })
}

fn path_exists(graph: &Value, from: &str, to: &str) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![from.to_owned()];
    while let Some(current) = stack.pop() {
        if current == to {
            return true;
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        for edge in graph
            .get("edges")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if edge.get("from").and_then(Value::as_str) == Some(current.as_str())
                && edge
                    .get("condition")
                    .and_then(Value::as_str)
                    .is_none_or(|condition| condition != "failure")
            {
                if let Some(next) = edge.get("to").and_then(Value::as_str) {
                    stack.push(next.to_owned());
                }
            }
        }
    }
    false
}

fn initialize_direct_node(
    node: &mut Value,
    id: &str,
    instruction: &str,
    structural_outcome: &str,
) -> Result<()> {
    let object = node
        .as_object_mut()
        .context("graph node must be an object")?;
    object.insert("id".to_owned(), json!(id));
    object.insert("instruction".to_owned(), json!(instruction));
    object.insert("structural_outcome".to_owned(), json!(structural_outcome));
    object.insert("controlled_outcome".to_owned(), json!("accepted"));
    object.insert("human_intervention".to_owned(), json!(true));
    object.remove("started_at");
    object.remove("finished_at");
    object.remove("outcome");
    object.remove("failure_code");
    object.remove("verification");
    Ok(())
}

fn rebuild_dependencies(graph: &mut Value) -> Result<()> {
    let mut dependencies: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if edge.get("condition").and_then(Value::as_str) == Some("failure") {
            continue;
        }
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
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .context("graph nodes are missing")?
    {
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let depends_on = dependencies.remove(&id).unwrap_or_default();
        if let Some(object) = node.as_object_mut() {
            object.insert(
                "depends_on".to_owned(),
                Value::Array(depends_on.into_iter().map(Value::String).collect()),
            );
        }
    }
    Ok(())
}

fn mark_retired_nodes(workspace: &Path, nodes: &[String], action: &str) -> Result<()> {
    let outcome = if action == "cancel_node" {
        crate::learning_data::NodeOutcome::Cancelled
    } else {
        crate::learning_data::NodeOutcome::Superseded
    };
    crate::project_file::mutate_document(workspace, |document| {
        let now = crate::project_file::project_timestamp();
        for id in nodes {
            if let Some(record) = document.learning.nodes.get_mut(id) {
                record.finished_at = Some(now.clone());
                record.outcome = Some(outcome);
                record.failure_code = None;
                record.verification = None;
                record.human_intervention = true;
            } else {
                document.learning.nodes.insert(
                    id.clone(),
                    crate::learning_data::NodeRecord {
                        node_id: id.clone(),
                        node_type: "implementation".to_owned(),
                        objective: format!("Human {action} target `{id}`"),
                        created_at: Some(now.clone()),
                        finished_at: Some(now.clone()),
                        outcome: Some(outcome),
                        human_intervention: true,
                        ..crate::learning_data::NodeRecord::default()
                    },
                );
            }
        }
        Ok(())
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
    let dependency_states = dependencies
        .iter()
        .map(|dependency| {
            harness
                .get("nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|node| node.get("id").and_then(Value::as_str) == Some(dependency))
                .and_then(|node| node.get("produced_state"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find_map(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| ready(dependency))
        })
        .collect::<Vec<_>>();
    let nodes = harness
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .context("harness nodes are missing")?;
    nodes.push(json!({
        "id": id,
        "title": task.title.trim(),
        "capability": capability,
        "memory_scopes": ["work:goal", "workspace:root"],
        "preconditions": dependency_states,
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
    let mut has_closeout = false;
    if let Some(nodes) = harness.get_mut("nodes").and_then(Value::as_array_mut) {
        if let Some(closeout) = nodes
            .iter_mut()
            .find(|node| node.get("id").and_then(Value::as_str) == Some("lead_closeout"))
        {
            has_closeout = true;
            if let Some(preconditions) = closeout
                .get_mut("preconditions")
                .and_then(Value::as_array_mut)
            {
                preconditions.extend(sinks.iter().map(|sink| json!(format!("{sink}.ready"))));
            }
        }
    }
    // Recompiled source genomes for completed/evolved projects may no longer
    // contain the original lead closeout node. In that case the amendment's
    // sinks are valid terminal nodes; emitting edges to a missing closeout
    // makes harness compilation fail and prevents completed graphs reopening.
    if !has_closeout {
        return;
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
    if action == "add_team_wave" && tasks.len() != 5 {
        bail!("team wave planner must produce exactly five tasks");
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
        if !is_wave_action(action)
            && task
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
            if !is_wave_action(action)
                && meta.dependencies.iter().any(|dependency| {
                    dependency != "anchor" && (dependency == &task.id || !seen.contains(dependency))
                })
            {
                bail!(
                    "amendment task `{}` efficiency dependencies must reference anchor or an earlier new task",
                    task.id
                );
            }
        }
    }
    Ok(())
}

fn is_wave_action(action: &str) -> bool {
    matches!(action, "add_wave_task" | "add_team_wave")
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn editable_graph() -> Value {
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "edit-test",
            "nodes": [
                {"id":"plan","capability":"content.analyze","instruction":"plan","execution":{"task_number":"0.1"}},
                {"id":"build","capability":"code.generate","instruction":"build","execution":{"task_number":"1.1"}},
                {"id":"verify","capability":"project.tests.execute","instruction":"verify","execution":{"task_number":"2.1"}}
            ],
            "edges": [
                {"from":"plan","to":"build","condition":"success"},
                {"from":"build","to":"verify","condition":"success"}
            ]
        });
        crate::graph_store::rehash_graph(&mut graph).unwrap();
        graph
    }

    fn temp_workspace(name: &str) -> std::path::PathBuf {
        let workspace = std::env::temp_dir().join(format!(
            "fractal-amendments-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        workspace
    }

    #[test]
    fn completed_genome_without_closeout_accepts_terminal_amendment_sinks() {
        let mut harness = json!({
            "nodes": [{"id": "completed", "produced_state": ["done"]}],
            "edges": []
        });

        connect_sinks_to_closeout(&mut harness, &["branch.new_lane".to_owned()]);

        assert_eq!(harness["edges"], json!([]));
        assert!(harness["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|node| { node.get("id").and_then(Value::as_str) != Some("lead_closeout") }));
    }

    #[test]
    fn active_genome_still_connects_amendment_sinks_to_closeout() {
        let mut harness = json!({
            "nodes": [{"id": "lead_closeout", "preconditions": []}],
            "edges": []
        });

        connect_sinks_to_closeout(&mut harness, &["branch.new_lane".to_owned()]);

        assert_eq!(
            harness["nodes"][0]["preconditions"],
            json!(["branch.new_lane.ready"])
        );
        assert_eq!(
            harness["edges"],
            json!([{"from":"branch.new_lane","to":"lead_closeout","condition":"success"}])
        );
    }

    #[test]
    fn peer_task_uses_the_existing_producers_declared_ready_state() {
        let mut harness = json!({
            "nodes": [{
                "id": "plan",
                "produced_state": ["plan_ready"]
            }],
            "edges": []
        });
        let task = PlannerTask {
            id: "recovery".to_owned(),
            title: "Recovery".to_owned(),
            capability: "code.generate".to_owned(),
            instruction: "Implement recovery".to_owned(),
            depends_on: Vec::new(),
            efficiency: None,
        };
        let efficiency = baseline_node_efficiency(
            12_000,
            vec!["plan".to_owned()],
            "recovery artifact",
            Vec::new(),
            "run tests",
        );

        append_harness_task(
            &mut harness,
            "branch.recovery",
            &task,
            &["plan".to_owned()],
            &efficiency,
        )
        .unwrap();

        assert_eq!(harness["nodes"][1]["preconditions"], json!(["plan_ready"]));
    }

    #[test]
    fn direct_amendment_queue_is_crash_safe_and_exactly_once_by_command_id() {
        let workspace = temp_workspace("amend-exactly-once");
        let graph = editable_graph();
        crate::graph_store::commit_graph(&graph).unwrap();
        crate::project_file::persist(&workspace, &graph, "Amend").unwrap();
        let command_id = "cmd_exactly_once";
        queue_edit(
            &workspace,
            command_id,
            "reroute_node",
            "build",
            None,
            "Build with the recovered instruction.",
            "test",
        )
        .unwrap();
        queue_edit(
            &workspace,
            command_id,
            "reroute_node",
            "build",
            None,
            "Build with the recovered instruction.",
            "test",
        )
        .unwrap();

        let previous = graph["graph_hash"].as_str().unwrap().to_owned();
        let (first_graph, first_hash) = apply_pending(graph, previous, &workspace, "lead");
        assert!(
            first_graph["applied_amendment_command_ids"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|value| value.as_str() == Some(command_id))
                .count()
                == 1
        );
        assert!(!has_pending(&workspace));
        let applied_nodes = first_graph["nodes"].as_array().unwrap().len();

        let (second_graph, second_hash) =
            apply_pending(first_graph.clone(), first_hash.clone(), &workspace, "lead");
        assert_eq!(second_hash, first_hash);
        assert_eq!(
            second_graph["nodes"].as_array().unwrap().len(),
            applied_nodes
        );
        assert_eq!(
            second_graph["applied_amendment_command_ids"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|value| value.as_str() == Some(command_id))
                .count(),
            1
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn committed_node_prefix_recovers_missing_amendment_history() {
        let graph = json!({
            "nodes": [
                {"id": "branch.local-1234-99.task", "instruction": "already committed"}
            ],
            "applied_amendment_command_ids": []
        });
        assert!(amendment_already_applied(&graph, "local-1234-99"));
        assert!(!amendment_already_applied(&graph, "local-1234-100"));
    }

    #[test]
    fn processing_file_left_by_crash_is_recovered() {
        let workspace = temp_workspace("amend-processing");
        let request = PendingAmendment {
            command_id: "cmd_processing".to_owned(),
            action: "reroute_node".to_owned(),
            task_ref: "build".to_owned(),
            wave: None,
            instruction: "Recovered from processing file.".to_owned(),
            source: "test".to_owned(),
            dependency: None,
        };
        let queue = queue_path(&workspace);
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        let processing = queue.with_extension(format!("processing-{}", std::process::id()));
        let mut raw = Vec::new();
        serde_json::to_writer(&mut raw, &request).unwrap();
        raw.write_all(
            b"
",
        )
        .unwrap();
        std::fs::write(&processing, raw).unwrap();
        let (pending, claimed) = claim_pending(&workspace).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].command_id, "cmd_processing");
        finish_claim(&workspace, &claimed, &[]).unwrap();
        assert!(!processing.exists());
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn amendments_queued_during_a_claim_are_not_deleted() {
        let workspace = temp_workspace("amend-concurrent-append");
        queue_edit(
            &workspace,
            "claimed",
            "reroute_node",
            "build",
            None,
            "First instruction.",
            "test",
        )
        .unwrap();

        let (claimed_requests, claimed_files) = claim_pending(&workspace).unwrap();
        assert_eq!(claimed_requests.len(), 1);
        queue_edit(
            &workspace,
            "arrived_during_planning",
            "reroute_node",
            "build",
            None,
            "Second instruction.",
            "test",
        )
        .unwrap();

        finish_claim(&workspace, &claimed_files, &[]).unwrap();
        assert!(has_pending(&workspace));
        let (remaining, remaining_files) = claim_pending(&workspace).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].command_id, "arrived_during_planning");
        finish_claim(&workspace, &remaining_files, &[]).unwrap();
        assert!(!has_pending(&workspace));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn incremental_apply_returns_to_the_coordinator_between_requests() {
        let workspace = temp_workspace("amend-incremental");
        let graph = editable_graph();
        crate::graph_store::commit_graph(&graph).unwrap();
        crate::project_file::persist(&workspace, &graph, "Incremental").unwrap();
        for (id, instruction) in [("first", "First route."), ("second", "Second route.")] {
            queue_edit(
                &workspace,
                id,
                "reroute_node",
                "build",
                None,
                instruction,
                "test",
            )
            .unwrap();
        }

        let before = graph["graph_hash"].as_str().unwrap().to_owned();
        let (first, first_hash) = apply_next_pending(graph, before, &workspace, "lead");
        assert!(amendment_already_applied(&first, "first"));
        assert!(!amendment_already_applied(&first, "second"));
        assert!(has_pending(&workspace));

        let (second, _) = apply_next_pending(first, first_hash, &workspace, "lead");
        assert!(amendment_already_applied(&second, "second"));
        assert!(!has_pending(&workspace));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn incremental_apply_rotates_a_failed_head_behind_unattempted_work() {
        let workspace = temp_workspace("amend-failed-head-fairness");
        let graph = editable_graph();
        crate::graph_store::commit_graph(&graph).unwrap();
        crate::project_file::persist(&workspace, &graph, "Fairness").unwrap();
        queue_edit(
            &workspace,
            "bad-head",
            "reroute_node",
            "missing-node",
            None,
            "This target does not exist.",
            "test",
        )
        .unwrap();
        queue_edit(
            &workspace,
            "valid-next",
            "reroute_node",
            "build",
            None,
            "Valid replacement instruction.",
            "test",
        )
        .unwrap();

        let before = graph["graph_hash"].as_str().unwrap().to_owned();
        let (unchanged, unchanged_hash) = apply_next_pending(graph, before, &workspace, "lead");
        assert!(!amendment_already_applied(&unchanged, "valid-next"));

        let (applied, _) = apply_next_pending(unchanged, unchanged_hash, &workspace, "lead");
        assert!(amendment_already_applied(&applied, "valid-next"));
        let (remaining, claimed) = claim_pending(&workspace).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].command_id, "bad-head");
        finish_claim(&workspace, &claimed, &[]).unwrap();
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn direct_human_edits_preserve_hashes_and_reject_noops() {
        let _lock = crate::graph_store::ENV_LOCK.lock().unwrap();
        let _home = crate::graph_store::TestHome::new("direct-human-edits").unwrap();
        let graph = editable_graph();
        crate::graph_store::commit_graph(&graph).unwrap();
        let before = graph["graph_hash"].as_str().unwrap().to_owned();
        let split = PendingAmendment {
            command_id: "cmd_split".to_owned(),
            action: "split_node".to_owned(),
            task_ref: "1.1".to_owned(),
            wave: None,
            instruction: "split build".to_owned(),
            source: "human".to_owned(),
            dependency: None,
        };
        let applied = apply_direct_edit(&graph, &before, &split).unwrap();
        assert_eq!(applied.retired_nodes, vec!["build"]);
        assert!(applied.graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| {
                node["id"] == "build.split.cmd_split" && node["structural_outcome"] == "created"
            }));
        crate::graph_store::verify_graph_document(&applied.graph).unwrap();

        let reroute = PendingAmendment {
            command_id: "cmd_reroute".to_owned(),
            action: "reroute_node".to_owned(),
            task_ref: "1.1".to_owned(),
            wave: None,
            instruction: "new build route".to_owned(),
            source: "human".to_owned(),
            dependency: None,
        };
        assert!(apply_direct_edit(&applied.graph, &applied.graph_hash, &reroute).is_ok());
        assert!(apply_direct_edit(
            &applied.graph,
            &applied.graph_hash,
            &PendingAmendment {
                instruction: "build".to_owned(),
                ..reroute.clone()
            }
        )
        .is_err());

        let add = PendingAmendment {
            command_id: "cmd_add".to_owned(),
            action: "add_dependency".to_owned(),
            task_ref: "2.1".to_owned(),
            wave: None,
            instruction: String::new(),
            source: "human".to_owned(),
            dependency: Some("0.1".to_owned()),
        };
        let with_dependency = apply_direct_edit(&applied.graph, &applied.graph_hash, &add).unwrap();
        assert!(edge_exists(&with_dependency.graph, "plan", "verify"));
        let remove = PendingAmendment {
            command_id: "cmd_remove".to_owned(),
            action: "remove_dependency".to_owned(),
            ..add.clone()
        };
        let removed =
            apply_direct_edit(&with_dependency.graph, &with_dependency.graph_hash, &remove)
                .unwrap();
        assert!(!edge_exists(&removed.graph, "plan", "verify"));
        let cancel = PendingAmendment {
            command_id: "cmd_cancel".to_owned(),
            action: "cancel_node".to_owned(),
            task_ref: "2.1".to_owned(),
            wave: None,
            instruction: String::new(),
            source: "human".to_owned(),
            dependency: None,
        };
        let cancelled = apply_direct_edit(&removed.graph, &removed.graph_hash, &cancel).unwrap();
        assert!(cancelled.graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| { node["id"] == "verify" && node["controlled_outcome"] == "cancelled" }));
        crate::graph_store::verify_graph_document(&cancelled.graph).unwrap();
    }

    #[test]
    fn human_edit_events_are_ordered_and_keep_verified_before_hashes() {
        let _lock = crate::graph_store::ENV_LOCK.lock().unwrap();
        let _home = crate::graph_store::TestHome::new("human-event-order").unwrap();
        std::env::set_var("FRACTAL_OFFLINE", "1");
        let workspace = std::env::temp_dir().join(format!(
            "fractal-amend-events-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut graph = editable_graph();
        crate::graph_store::commit_graph(&graph).unwrap();
        crate::project_file::persist(&workspace, &graph, "Human Events").unwrap();
        let mut hashes = Vec::new();
        let edits = [
            ("split_node", "1.1", None, "split"),
            ("reroute_node", "1.1", None, "reroute"),
            ("cancel_node", "2.1", None, ""),
            ("add_dependency", "2.1", Some("0.1"), ""),
            ("remove_dependency", "2.1", Some("0.1"), ""),
        ];
        for (index, (action, target, dependency, instruction)) in edits.iter().enumerate() {
            hashes.push(graph["graph_hash"].as_str().unwrap().to_owned());
            queue_edit(
                &workspace,
                format!("cmd-{index}"),
                action,
                target,
                *dependency,
                if *action == "reroute_node" {
                    "new route"
                } else {
                    instruction
                },
                "human",
            )
            .unwrap();
            let before = graph["graph_hash"].as_str().unwrap().to_owned();
            let (next_graph, next_hash) = apply_pending(graph, before, &workspace, "lead");
            graph = next_graph;
            assert_eq!(graph["graph_hash"].as_str(), Some(next_hash.as_str()));
            crate::graph_store::verify_graph_document(&graph).unwrap();
        }
        let project = crate::project_file::load(&workspace).unwrap();
        assert_eq!(project.learning.graph_edits.len(), edits.len());
        for (event, before) in project.learning.graph_edits.iter().zip(hashes) {
            assert_eq!(event.graph_before_hash, before);
            assert!(!event.timestamp.is_empty());
            assert!(event.eventual_effect.success.is_none());
            assert_eq!(event.trigger, "human_amendment");
            assert_eq!(event.actor, "human");
        }
        assert_eq!(
            project.learning.graph_edits[0].action.created_nodes,
            vec!["build.split.cmd-0"]
        );
        assert_eq!(
            project
                .learning
                .graph_edits
                .iter()
                .map(|event| event.action.kind.as_str())
                .collect::<Vec<_>>(),
            vec![
                "split_node",
                "reroute_node",
                "cancel_node",
                "add_dependency",
                "remove_dependency"
            ]
        );
        crate::project_file::update_graph_edit_event_effect(
            &workspace,
            0,
            crate::learning_data::EventualEffect {
                success: Some(true),
                rework_reduced: Some(true),
                ..crate::learning_data::EventualEffect::default()
            },
        )
        .unwrap();
        let updated = crate::project_file::load(&workspace).unwrap();
        assert_eq!(
            updated.learning.graph_edits[0].eventual_effect.success,
            Some(true)
        );
        assert_eq!(
            updated.learning.graph_edits[0]
                .eventual_effect
                .rework_reduced,
            Some(true)
        );
        let noop = PendingAmendment {
            command_id: "noop".to_owned(),
            action: "remove_dependency".to_owned(),
            task_ref: "2.1".to_owned(),
            wave: None,
            instruction: String::new(),
            source: "human".to_owned(),
            dependency: Some("0.1".to_owned()),
        };
        assert!(apply_direct_edit(&graph, graph["graph_hash"].as_str().unwrap(), &noop).is_err());
        assert_eq!(
            crate::project_file::load(&workspace)
                .unwrap()
                .learning
                .graph_edits
                .len(),
            edits.len()
        );
        std::fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn cross_boundary_human_edits_round_trip_learning_events() {
        let _lock = crate::graph_store::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _home = crate::graph_store::TestHome::new("cross-boundary-edits").unwrap();
        std::env::set_var("FRACTAL_OFFLINE", "1");
        let workspace = std::env::temp_dir().join(format!(
            "fractal-amend-e2e-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let graph = editable_graph();
        crate::graph_store::commit_graph(&graph).unwrap();
        crate::project_file::persist(&workspace, &graph, "E2E Edits").unwrap();
        let before = graph["graph_hash"].as_str().unwrap().to_owned();
        queue_edit(
            &workspace,
            "e2e-split",
            "split_node",
            "1.1",
            None,
            "split for e2e",
            "operator",
        )
        .unwrap();
        let (graph, hash) = apply_pending(graph, before.clone(), &workspace, "lead");
        assert_ne!(hash, before);
        crate::graph_store::verify_graph_document(&graph).unwrap();

        let raw = std::fs::read(crate::project_file::path(&workspace)).unwrap();
        let encoded: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(
            encoded["learning"]["graph_edits"][0]["action"]["type"],
            json!("split_node")
        );
        assert_eq!(
            encoded["learning"]["graph_edits"][0]["graph_before_hash"],
            json!(before)
        );
        assert_eq!(
            encoded["learning"]["graph_edits"][0]["action"]["created_nodes"],
            json!(["build.split.e2e-split"])
        );
        let reloaded = crate::project_file::load(&workspace).unwrap();
        assert_eq!(reloaded.graph_hash, hash);
        assert_eq!(reloaded.learning.graph_edits.len(), 1);
        assert_eq!(
            reloaded.learning.nodes["build"].outcome,
            Some(crate::learning_data::NodeOutcome::Superseded)
        );
        assert!(reloaded.learning.nodes["build"].human_intervention);
        std::fs::remove_dir_all(workspace).ok();
    }

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
    fn wave_task_ignores_planner_dependency_names_and_uses_canonical_wave_flow() {
        let meta = baseline_node_efficiency(
            5_000,
            vec!["plan".to_owned()],
            "the refreshed graph artifacts",
            vec![],
            "parse and validate both artifacts",
        );
        let tasks = vec![planner_task(
            "refresh_master_graph",
            vec!["plan".to_owned()],
            Some(meta),
        )];

        validate_tasks(&tasks, "add_wave_task")
            .expect("wave dependencies are replaced by canonical graph dependencies");
    }

    #[test]
    fn team_wave_requires_exactly_five_peer_tasks() {
        let tasks: Vec<PlannerTask> = (0..5)
            .map(|index| planner_task(&format!("team_{index}"), Vec::new(), None))
            .collect();
        validate_tasks(&tasks, "add_team_wave").expect("one leader delegates five tasks");
        assert!(validate_tasks(&tasks[..4], "add_team_wave")
            .unwrap_err()
            .to_string()
            .contains("exactly five"));
    }

    #[test]
    fn planner_metadata_is_bounded_before_contract_validation() {
        let unicode = "é".repeat(crate::efficiency::MAX_BASIS_BYTES);
        let mut meta = baseline_node_efficiency(5_000, Vec::new(), &unicode, vec![], &unicode);
        meta.current_assumptions = vec![unicode];
        let mut tasks = vec![planner_task("bounded", Vec::new(), Some(meta))];

        normalize_planner_metadata(&mut tasks);
        validate_tasks(&tasks, "add_wave_task").expect("normalized metadata is valid");
        let meta = tasks[0].efficiency.as_ref().unwrap();
        assert!(meta.expected_artifact.len() <= crate::efficiency::MAX_BASIS_BYTES);
        assert!(meta.verification_plan.len() <= crate::efficiency::MAX_BASIS_BYTES);
        assert!(meta.current_assumptions[0].len() <= crate::efficiency::MAX_BASIS_BYTES);
        assert!(meta
            .expected_artifact
            .is_char_boundary(meta.expected_artifact.len()));
    }

    #[test]
    fn failed_amendments_are_preserved_with_their_retry_context() {
        let workspace = std::env::temp_dir().join(format!(
            "fractal_failed_amendment_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let request = PendingAmendment {
            command_id: "local-1".to_owned(),
            action: "add_wave_task".to_owned(),
            task_ref: String::new(),
            wave: Some(2),
            instruction: "bounded retry".to_owned(),
            source: "explicit_amendment".to_owned(),
            dependency: None,
        };

        record_failed(&workspace, &request, "metadata was too large", true).unwrap();
        let line = fs::read_to_string(failure_path(&workspace)).unwrap();
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["request"]["command_id"], "local-1");
        assert_eq!(value["error"], "metadata was too large");
        assert_eq!(value["retryable"], true);
        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn stale_architect_team_wave_is_quarantined_instead_of_retried_forever() {
        let request = PendingAmendment {
            command_id: "architect-team-0009".to_owned(),
            action: "add_team_wave".to_owned(),
            task_ref: String::new(),
            wave: Some(6),
            instruction: "form another bounded team".to_owned(),
            source: "master_architect".to_owned(),
            dependency: None,
        };

        assert!(is_permanent_failure(
            &request,
            "wave 6 is not in the current graph"
        ));
        assert!(!is_permanent_failure(
            &request,
            "planner process temporarily unavailable"
        ));

        let mut explicit = request;
        explicit.source = "explicit_amendment".to_owned();
        assert!(!is_permanent_failure(
            &explicit,
            "wave 6 is not in the current graph"
        ));
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
    fn similarity_peer_map_resolves_only_unique_canonical_suffixes() {
        let graph = json!({"nodes": [
            {"id": "branch.first.rpc_adapter", "execution": {"task_number": "5.20"}},
            {"id": "branch.first.shared"},
            {"id": "branch.second.shared"}
        ]});
        let amendment_ids =
            BTreeMap::from([("new_task".to_owned(), "branch.command.new_task".to_owned())]);
        let peers = similarity_peer_map(&graph, &amendment_ids);
        assert_eq!(peers["rpc_adapter"], "branch.first.rpc_adapter");
        assert_eq!(peers["5.20"], "branch.first.rpc_adapter");
        assert_eq!(
            peers["branch.first.rpc_adapter"],
            "branch.first.rpc_adapter"
        );
        assert_eq!(peers["new_task"], "branch.command.new_task");
        assert!(!peers.contains_key("shared"));
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
