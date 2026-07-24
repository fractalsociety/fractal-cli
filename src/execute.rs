//! Drive a committed execution graph to a built, verified result by handing
//! each build node to a real headless worker (claude / codex / cursor) in the
//! trusted workspace, then running the graph's verification node against what it
//! produced. This is what turns a typed request into an actual artifact.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// One node's execution record, for signed receipts + evidence roots.
#[derive(Clone)]
pub(crate) struct NodeRun {
    pub node: String,
    pub agent: String,
    pub is_verify: bool,
    pub ok: bool,
    /// Content-addressed evidence digest (hex) of the workspace after the node.
    pub evidence_hex: String,
}

/// Outcome of driving one graph.
pub(crate) struct RunOutcome {
    pub built: bool,
    pub verified: Option<bool>,
    pub detail: String,
    /// The node whose verification failed (drives governed evolution), if any.
    pub failed_node: Option<String>,
    /// Per-node execution log for chain receipts + the sanitized export.
    pub log: Vec<NodeRun>,
}
/// The headless worker to use, from `$FRACTAL_WORKER` (default `claude`).
fn worker_kind() -> String {
    std::env::var("FRACTAL_WORKER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "claude".to_owned())
}

/// Human label for the configured worker.
pub(crate) fn worker_label() -> String {
    worker_kind()
}

fn is_build(capability: &str) -> bool {
    capability.contains("code.generate")
        || capability.ends_with(".edit")
        || capability.contains("code.write")
}

fn is_verify(capability: &str) -> bool {
    capability.contains("tests") || capability.contains("verify")
}

/// Order node ids so every edge `from → to` runs `from` first (Kahn).
fn topo_order(graph: &Value) -> Result<Vec<Value>> {
    let nodes = graph
        .get("nodes")
        .and_then(Value::as_array)
        .context("graph has no nodes")?;
    let mut by_id: BTreeMap<String, Value> = BTreeMap::new();
    let mut indegree: BTreeMap<String, usize> = BTreeMap::new();
    for node in nodes {
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .context("node missing id")?;
        by_id.insert(id.to_owned(), node.clone());
        indegree.entry(id.to_owned()).or_insert(0);
    }
    let mut adjacency: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (from, to) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        );
        if let (Some(from), Some(to)) = (from, to) {
            if by_id.contains_key(from) && by_id.contains_key(to) {
                adjacency
                    .entry(from.to_owned())
                    .or_default()
                    .push(to.to_owned());
                *indegree.get_mut(to).unwrap() += 1;
            }
        }
    }
    let mut ready: VecDeque<String> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut ordered = Vec::new();
    while let Some(id) = ready.pop_front() {
        if let Some(node) = by_id.get(&id) {
            ordered.push(node.clone());
        }
        for next in adjacency.get(&id).cloned().unwrap_or_default() {
            let degree = indegree.get_mut(&next).unwrap();
            *degree -= 1;
            if *degree == 0 {
                ready.push_back(next);
            }
        }
    }
    if ordered.len() != by_id.len() {
        bail!("execution graph has a cycle");
    }
    Ok(ordered)
}

/// An optional pinned model for `kind`, from `$FRACTAL_<KIND>_MODEL`
/// (e.g. `FRACTAL_CLAUDE_MODEL=fable`).
fn model_for(kind: &str) -> Option<String> {
    let key = format!(
        "FRACTAL_{}_MODEL",
        kind.to_ascii_uppercase().replace('-', "_")
    );
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Build the headless worker command for `kind`, honoring a pinned model.
fn worker_command(kind: &str, prompt: &str) -> Result<Command> {
    let mut command = match kind {
        "claude" => {
            let mut c = Command::new("claude");
            c.arg("-p");
            if let Some(model) = model_for("claude") {
                c.arg("--model").arg(model);
            }
            c.arg("--dangerously-skip-permissions").arg(prompt);
            c
        }
        "codex" => {
            let mut c = Command::new("codex");
            c.arg("exec");
            if let Some(model) = model_for("codex") {
                c.arg("--model").arg(model);
            }
            c.arg("--dangerously-bypass-approvals-and-sandbox")
                .arg(prompt);
            c
        }
        "cursor" | "cursor-agent" => {
            let mut c = Command::new("cursor-agent");
            c.arg("-p");
            if let Some(model) = model_for("cursor") {
                c.arg("--model").arg(model);
            }
            c.arg("--force").arg(prompt);
            c
        }
        other => bail!("unknown worker: {other} (use claude|codex|cursor)"),
    };
    command.env("FRACTAL_WORKER", kind);
    Ok(command)
}

/// Run a specific `kind` of worker on `instruction` in `workspace`, streaming.
fn run_worker_as(kind: &str, instruction: &str, workspace: &Path) -> Result<bool> {
    let prompt = format!(
        "You are one agent on a coordinated team; a lead has planned the project (read INTERFACE.md \
         if it exists). Do exactly this assigned task and nothing else:\n\n{instruction}\n\nWork \
         entirely in the current directory. Create or edit only the files this task needs and make \
         any tests pass. Do not ask questions; make reasonable choices."
    );
    // Detach the worker's stdin: headless agents (e.g. `claude -p`) otherwise
    // inherit the CLI's piped stdin and block reading it instead of exiting.
    let status = worker_command(kind, &prompt)?
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .status()
        .with_context(|| format!("failed to launch worker `{kind}` (is it on PATH?)"))?;
    Ok(status.success())
}

/// The binary that provides a given logical agent.
fn agent_binary(kind: &str) -> &str {
    match kind {
        "cursor" | "cursor-agent" => "cursor-agent",
        other => other,
    }
}

/// Whether `binary` is resolvable on `PATH`.
fn binary_on_path(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(binary);
                candidate.is_file() || candidate.with_extension("exe").is_file()
            })
        })
        .unwrap_or(false)
}

/// Every supported agent whose binary is on `PATH` (ignores env config).
pub(crate) fn available_agents() -> Vec<String> {
    ["claude", "codex", "cursor"]
        .into_iter()
        .filter(|kind| binary_on_path(agent_binary(kind)))
        .map(str::to_owned)
        .collect()
}

/// The agents to run, from `$FRACTAL_AGENTS` (comma-separated) or auto-detected
/// among claude / codex / cursor on `PATH`.
pub(crate) fn detect_agents() -> Vec<String> {
    if let Ok(list) = std::env::var("FRACTAL_AGENTS") {
        let chosen: Vec<String> = list
            .split(',')
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect();
        if !chosen.is_empty() {
            return chosen;
        }
    }
    ["codex", "cursor", "claude"]
        .into_iter()
        .filter(|kind| binary_on_path(agent_binary(kind)))
        .map(str::to_owned)
        .collect()
}

/// Result of executing one node.
struct NodeOutcome {
    ok: bool,
    verified: Option<bool>,
    /// A human-readable note (e.g. the evidence-floor verdict) to surface.
    note: Option<String>,
}

/// Execute one node with a given agent: build nodes run the worker; verify nodes
/// run the tests and are judged by the genuine `fractal-verify` evidence floor;
/// passive nodes (analyze/control) are no-ops.
fn run_node(node: &Value, agent: &str, workspace: &Path) -> Result<NodeOutcome> {
    let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
    let id = node.get("id").and_then(Value::as_str).unwrap_or("node");
    let instruction = node
        .get("instruction")
        .and_then(Value::as_str)
        .unwrap_or("");
    if is_build(capability) {
        Ok(NodeOutcome {
            ok: run_worker_as(agent, instruction, workspace)?,
            verified: None,
            note: None,
        })
    } else if is_verify(capability) {
        // Genuine governance: judge the suite with the real deny-by-default floor.
        match crate::verify::evaluate_workspace(workspace, id, agent)? {
            Some(verdict) => Ok(NodeOutcome {
                ok: verdict.complete,
                verified: Some(verdict.complete),
                note: Some(verdict.detail),
            }),
            // Nothing to run: unverifiable, but not a failure.
            None => Ok(NodeOutcome {
                ok: true,
                verified: None,
                note: None,
            }),
        }
    } else {
        Ok(NodeOutcome {
            ok: true,
            verified: None,
            note: None,
        })
    }
}

/// Best-effort report of a node transition to the live board so the dashboard
/// turns yellow (checkout) then green (complete) as agents work.
fn report_node(board: Option<&str>, node: &str, action: &str, agent: &str) {
    if let Some(base) = board {
        let url = format!(
            "{}/api/tasks/{}/{}",
            base.trim_end_matches('/'),
            node,
            action
        );
        let body = serde_json::json!({ "agent_id": agent, "agent_label": agent }).to_string();
        let _ = ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_string(&body);
    }
}

/// A content-addressed digest of the workspace's top-level files (sorted names +
/// contents), used as a node's evidence after it runs.
fn workspace_digest(workspace: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut names: Vec<String> = std::fs::read_dir(workspace)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut hasher = Sha256::new();
    for name in &names {
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        if let Ok(bytes) = std::fs::read(workspace.join(name)) {
            hasher.update(&bytes);
        }
    }
    let mut hex = String::from("sha256:");
    for byte in hasher.finalize() {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Shared scheduler state for the multi-agent pull-queue.
#[derive(Default)]
struct Schedule {
    completed: BTreeSet<String>,
    in_progress: BTreeSet<String>,
    failed: Option<String>,
    built: bool,
    verified: Option<bool>,
    log: Vec<NodeRun>,
}

/// Drive the whole graph with several agents: each agent repeatedly checks out a
/// dependency-ready node no one else holds, runs it, and marks it complete —
/// until the entire graph is done (or a node fails). Independent branches run in
/// parallel across agents.
pub(crate) fn run_multi_agent(
    graph: &Value,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
) -> Result<RunOutcome> {
    let ordered = topo_order(graph)?; // validates acyclic
    let ids: Vec<String> = ordered
        .iter()
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let node_by_id: BTreeMap<String, Value> = ordered
        .iter()
        .filter_map(|node| {
            node.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_owned(), node.clone()))
        })
        .collect();
    let mut predecessors: BTreeMap<String, Vec<String>> =
        ids.iter().map(|id| (id.clone(), Vec::new())).collect();
    for edge in graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(from), Some(to)) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        ) {
            if let Some(list) = predecessors.get_mut(to) {
                list.push(from.to_owned());
            }
        }
    }
    let total = ids.len();
    let schedule = Mutex::new(Schedule::default());

    // The lead (first agent) is the ORCHESTRATOR: it plans the project (the root
    // node) and closes it out (control), then assigns + monitors — it does not do
    // the coding tasks. Every other agent is a WORKER that pulls ready coding
    // tasks in parallel and steals another when it finishes early. A solo agent
    // does everything itself.
    let lead: &str = agents.first().map(String::as_str).unwrap_or("");
    let has_workers = agents.len() > 1;
    std::thread::scope(|scope| {
        for agent in agents {
            let agent = agent.clone();
            let is_lead = agent.as_str() == lead;
            let (schedule, ids, node_by_id, predecessors) =
                (&schedule, &ids, &node_by_id, &predecessors);
            scope.spawn(move || {
              let mut mine: u64 = 0;
              loop {
                // Fairness: an agent that has already done more work waits a
                // little longer before grabbing the next node, so work rotates
                // across the team instead of one agent winning every race.
                // Bounded so large graphs are not slowed materially.
                // The yield for an agent that has already worked must exceed the
                // idle poll below, so idle agents win the next (parallel) node
                // instead of one fast agent grabbing them all.
                if mine > 0 {
                    std::thread::sleep(Duration::from_millis(mine.min(3) * 120));
                }
                // Atomically check out a ready, unclaimed node.
                let claimed = {
                    let mut state = schedule.lock().expect("schedule lock");
                    if state.failed.is_some() || state.completed.len() == total {
                        break;
                    }
                    let capability_of = |id: &String| {
                        node_by_id
                            .get(id)
                            .and_then(|node| node.get("capability"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned()
                    };
                    let is_ready = |id: &String, state: &Schedule| {
                        !state.completed.contains(id)
                            && !state.in_progress.contains(id)
                            && predecessors[id].iter().all(|pred| state.completed.contains(pred))
                    };
                    let is_root = |id: &String| predecessors[id].is_empty();
                    let is_control = |id: &String| capability_of(id).starts_with("control.");
                    // Role split: the lead plans (root) + closes out (control);
                    // workers do the middle coding/verify tasks in parallel.
                    let for_this_agent = |id: &String| {
                        if !has_workers {
                            true
                        } else if is_lead {
                            is_root(id) || is_control(id)
                        } else {
                            !is_root(id) && !is_control(id)
                        }
                    };
                    let next = ids
                        .iter()
                        .find(|id| is_ready(id, &state) && for_this_agent(id));
                    match next {
                        Some(id) => {
                            state.in_progress.insert(id.clone());
                            Some(id.clone())
                        }
                        None => None,
                    }
                };
                let Some(id) = claimed else {
                    std::thread::sleep(Duration::from_millis(30));
                    continue;
                };

                let node = node_by_id.get(&id).expect("claimed node exists");
                let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
                let is_planning =
                    has_workers && is_lead && predecessors[&id].is_empty() && is_build(capability);
                let clr = crate::ui::CLEAR_LINE;
                if is_planning {
                    println!("{clr}  🧠 [{agent}] planning the project & interface, then assigning tasks to the team…");
                } else if has_workers && is_lead {
                    println!("{clr}  [{agent}] (orchestrator) ▸ {id}");
                } else {
                    println!("{clr}  [{agent}] ▸ checked out {id} ({capability})");
                }
                report_node(board, &id, "checkout", &agent);
                let result = run_node(node, &agent, workspace);

                let evidence_hex = workspace_digest(workspace);
                let node_is_verify = is_verify(capability);
                let mut state = schedule.lock().expect("schedule lock");
                state.in_progress.remove(&id);
                let node_ok = match result {
                    Ok(NodeOutcome { ok, verified, note }) => {
                        if is_build(capability) && ok {
                            state.built = true;
                        }
                        if let Some(value) = verified {
                            state.verified = Some(value);
                        }
                        let suffix = note
                            .as_deref()
                            .map(|note| format!(" — {note}"))
                            .unwrap_or_default();
                        if ok {
                            state.completed.insert(id.clone());
                            mine += 1;
                            report_node(board, &id, "complete", &agent);
                            if is_planning {
                                println!("{clr}  [{agent}] ✓ plan ready — dispatching tasks to the workers.");
                            } else {
                                println!("{clr}  [{agent}] ✓ {id}{suffix}");
                            }
                        } else {
                            state.failed = Some(id.clone());
                            println!("{clr}  [{agent}] ✗ {id}{suffix}");
                        }
                        ok
                    }
                    Err(error) => {
                        state.failed = Some(id.clone());
                        eprintln!("  [{agent}] ✗ {id}: {error:#}");
                        false
                    }
                };
                state.log.push(NodeRun {
                    node: id.clone(),
                    agent: agent.clone(),
                    is_verify: node_is_verify,
                    ok: node_ok,
                    evidence_hex,
                });
              }
            });
        }
    });

    let state = schedule.into_inner().expect("schedule");
    let detail = if let Some(failed) = &state.failed {
        format!("stopped at node `{failed}`")
    } else {
        match state.verified {
            Some(true) => "built and verified by the agent team".to_owned(),
            Some(false) => "built but verification failed".to_owned(),
            None if state.built => "built (no tests to verify)".to_owned(),
            None => "graph completed".to_owned(),
        }
    };
    Ok(RunOutcome {
        built: state.built,
        verified: state.verified,
        detail,
        failed_node: state.failed.clone(),
        log: state.log.clone(),
    })
}
