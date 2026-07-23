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

/// Outcome of driving one graph.
pub(crate) struct RunOutcome {
    pub built: bool,
    pub verified: Option<bool>,
    pub detail: String,
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
    capability.contains("code.generate") || capability.ends_with(".edit") || capability.contains("code.write")
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
        let id = node.get("id").and_then(Value::as_str).context("node missing id")?;
        by_id.insert(id.to_owned(), node.clone());
        indegree.entry(id.to_owned()).or_insert(0);
    }
    let mut adjacency: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in graph.get("edges").and_then(Value::as_array).into_iter().flatten() {
        let (from, to) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        );
        if let (Some(from), Some(to)) = (from, to) {
            if by_id.contains_key(from) && by_id.contains_key(to) {
                adjacency.entry(from.to_owned()).or_default().push(to.to_owned());
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
    std::env::var(key).ok().filter(|value| !value.trim().is_empty())
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
            c.arg("--dangerously-bypass-approvals-and-sandbox").arg(prompt);
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

/// Run the configured single worker on `instruction` in `workspace`.
fn run_worker(instruction: &str, workspace: &Path) -> Result<bool> {
    run_worker_as(&worker_kind(), instruction, workspace)
}

/// Run a specific `kind` of worker on `instruction` in `workspace`, streaming.
fn run_worker_as(kind: &str, instruction: &str, workspace: &Path) -> Result<bool> {
    let prompt = format!(
        "{instruction}\n\nWork entirely in the current directory. Create or edit the files \
         needed and make any tests pass. Do not ask questions; make reasonable choices."
    );
    let status = worker_command(kind, &prompt)?
        .current_dir(workspace)
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
                candidate.is_file()
                    || candidate.with_extension("exe").is_file()
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

/// Detect and run the workspace's tests. Returns `None` when there is nothing to
/// run (cannot verify, but not a failure).
fn verify_workspace(workspace: &Path) -> Result<Option<bool>> {
    let has = |name: &str| workspace.join(name).exists();
    let python_tests = std::fs::read_dir(workspace)
        .map(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("test_") && name.ends_with(".py")
                    || name.ends_with("_test.py")
            })
        })
        .unwrap_or(false);

    let mut command = if has("Cargo.toml") {
        let mut c = Command::new("cargo");
        c.arg("test");
        c
    } else if python_tests {
        // Prefer pytest when it is importable; otherwise fall back to unittest
        // discovery so a missing pytest is not mistaken for a test failure.
        let pytest_available = Command::new("python3")
            .args(["-c", "import pytest"])
            .current_dir(workspace)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        let mut c = Command::new("python3");
        if pytest_available {
            c.args(["-m", "pytest", "-q"]);
        } else {
            c.args(["-m", "unittest", "discover", "-q"]);
        }
        c
    } else if has("package.json") {
        let mut c = Command::new("npm");
        c.args(["test", "--silent"]);
        c
    } else {
        return Ok(None);
    };
    match command.current_dir(workspace).status() {
        Ok(status) => Ok(Some(status.success())),
        // The runner itself could not launch — unverifiable, not a failure.
        Err(_) => Ok(None),
    }
}

/// Execute one node with a given agent: build nodes run the worker; verify nodes
/// run the tests; passive nodes (analyze/control) are no-ops. Returns
/// `(node_ok, verified)`.
fn run_node(node: &Value, agent: &str, workspace: &Path) -> Result<(bool, Option<bool>)> {
    let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
    let instruction = node.get("instruction").and_then(Value::as_str).unwrap_or("");
    if is_build(capability) {
        Ok((run_worker_as(agent, instruction, workspace)?, None))
    } else if is_verify(capability) {
        let verified = verify_workspace(workspace)?;
        // A verify node only fails on an actual test failure.
        Ok((!matches!(verified, Some(false)), verified))
    } else {
        Ok((true, None))
    }
}

/// Shared scheduler state for the multi-agent pull-queue.
#[derive(Default)]
struct Schedule {
    completed: BTreeSet<String>,
    in_progress: BTreeSet<String>,
    failed: Option<String>,
    built: bool,
    verified: Option<bool>,
}

/// Drive the whole graph with several agents: each agent repeatedly checks out a
/// dependency-ready node no one else holds, runs it, and marks it complete —
/// until the entire graph is done (or a node fails). Independent branches run in
/// parallel across agents.
pub(crate) fn run_multi_agent(
    graph: &Value,
    workspace: &Path,
    agents: &[String],
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
    for edge in graph.get("edges").and_then(Value::as_array).into_iter().flatten() {
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

    // The primary agent (first in the roster) is the lead: only it takes the
    // build/coding nodes; the others handle analyze / verify / control.
    let primary: &str = agents.first().map(String::as_str).unwrap_or("");
    std::thread::scope(|scope| {
        for agent in agents {
            let agent = agent.clone();
            let is_primary = agent.as_str() == primary;
            let (schedule, ids, node_by_id, predecessors) =
                (&schedule, &ids, &node_by_id, &predecessors);
            scope.spawn(move || {
              let mut mine: u64 = 0;
              loop {
                // Fairness: an agent that has already done more work waits a
                // little longer before grabbing the next node, so work rotates
                // across the team instead of one agent winning every race.
                // Bounded so large graphs are not slowed materially.
                if mine > 0 {
                    std::thread::sleep(Duration::from_millis(mine.min(3) * 20));
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
                    // How many build nodes are ready right now: with >1 the team
                    // parallelizes; with exactly 1 the lead takes it.
                    let ready_builds = ids
                        .iter()
                        .filter(|id| is_ready(id, &state) && is_build(&capability_of(id)))
                        .count();
                    let next = ids.iter().find(|id| {
                        is_ready(id, &state)
                            && (is_primary
                                || !is_build(&capability_of(id))
                                || ready_builds >= 2)
                    });
                    match next {
                        Some(id) => {
                            state.in_progress.insert(id.clone());
                            Some(id.clone())
                        }
                        None => None,
                    }
                };
                let Some(id) = claimed else {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };

                let node = node_by_id.get(&id).expect("claimed node exists");
                let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
                println!("  [{agent}] ▸ checked out {id} ({capability})");
                let result = run_node(node, &agent, workspace);

                let mut state = schedule.lock().expect("schedule lock");
                state.in_progress.remove(&id);
                match result {
                    Ok((ok, verified)) => {
                        if is_build(capability) && ok {
                            state.built = true;
                        }
                        if let Some(value) = verified {
                            state.verified = Some(value);
                        }
                        if ok {
                            state.completed.insert(id.clone());
                            mine += 1;
                            println!("  [{agent}] ✓ {id}");
                        } else {
                            state.failed = Some(id.clone());
                            println!("  [{agent}] ✗ {id}");
                        }
                    }
                    Err(error) => {
                        state.failed = Some(id.clone());
                        eprintln!("  [{agent}] ✗ {id}: {error:#}");
                    }
                }
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
    })
}

/// Drive the whole graph: run each node, streaming progress.
pub(crate) fn run_with_workers(graph: &Value, workspace: &Path) -> Result<RunOutcome> {
    let ordered = topo_order(graph)?;
    let mut built = false;
    let mut verified = None;
    for node in &ordered {
        let id = node.get("id").and_then(Value::as_str).unwrap_or("node");
        let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
        let instruction = node
            .get("instruction")
            .and_then(Value::as_str)
            .unwrap_or("");

        if is_build(capability) {
            println!("  ▸ {id} ({capability}) — building with {}…", worker_kind());
            let ok = run_worker(instruction, workspace)?;
            println!("    {} {id}", if ok { "✓" } else { "✗" });
            built = ok;
            if !ok {
                return Ok(RunOutcome {
                    built: false,
                    verified: None,
                    detail: format!("build node `{id}` failed"),
                });
            }
        } else if is_verify(capability) {
            println!("  ▸ {id} ({capability}) — verifying…");
            verified = verify_workspace(workspace)?;
            match verified {
                Some(true) => println!("    ✓ {id} — tests pass"),
                Some(false) => {
                    println!("    ✗ {id} — tests failed");
                    return Ok(RunOutcome {
                        built,
                        verified,
                        detail: "verification failed".to_owned(),
                    });
                }
                None => println!("    · {id} — no tests detected to run"),
            }
        } else {
            // analyze / control: a plan or marker step, no worker spend.
            println!("  · {id} ({capability})");
        }
    }
    let detail = match verified {
        Some(true) => "built and verified".to_owned(),
        Some(false) => "built but verification failed".to_owned(),
        None if built => "built (no tests to verify)".to_owned(),
        None => "nothing to build".to_owned(),
    };
    Ok(RunOutcome { built, verified, detail })
}
