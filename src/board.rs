use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::graph_store;

/// Prepare the board-state file for a graph about to be served. A graph with no
/// lineage starts clean (so progress shows from zero). An **evolved child** —
/// grown / repaired / differentiated mid-run, identified by its `parent_graph`
/// field — inherits its parent's board state (the completed / in-progress
/// assignments for the tasks it still contains), so re-serving it mid-run keeps
/// the progress already made instead of resetting every task to pending.
fn seed_board_state(
    graph: &serde_json::Value,
    state_file: &Path,
    preseed_completed: Option<&std::collections::BTreeSet<String>>,
) {
    let node_ids: std::collections::BTreeSet<&str> = graph
        .get("nodes")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.get("id").and_then(serde_json::Value::as_str))
        .collect();

    // Resume: mark the already-completed tasks so the board shows prior progress
    // (green) instead of restarting them from pending.
    if let Some(completed) = preseed_completed {
        let assignments: serde_json::Map<String, serde_json::Value> = completed
            .iter()
            .filter(|id| node_ids.contains(id.as_str()))
            .map(|id| {
                (
                    id.clone(),
                    serde_json::json!({
                        "agent_id": "resumed",
                        "agent_label": "resumed",
                        "state": "completed",
                    }),
                )
            })
            .collect();
        let graph_id = graph.get("graph_id").cloned().unwrap_or_default();
        let state = serde_json::json!({ "graph_id": graph_id, "assignments": assignments });
        if let Ok(serialized) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(state_file, serialized);
        }
        return;
    }

    if let Some(parent) = graph
        .get("parent_graph")
        .and_then(serde_json::Value::as_str)
    {
        let parent_state = graph_store::graph_path(parent).with_extension("board-state.json");
        if let Ok(text) = std::fs::read_to_string(&parent_state) {
            if let Ok(mut state) = serde_json::from_str::<serde_json::Value>(&text) {
                // Keep only the assignments for tasks the child still has.
                if let Some(map) = state
                    .get_mut("assignments")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    map.retain(|node_id, _| node_ids.contains(node_id.as_str()));
                }
                if let Ok(serialized) = serde_json::to_string_pretty(&state) {
                    if std::fs::write(state_file, serialized).is_ok() {
                        return; // inherited the parent's progress
                    }
                }
            }
        }
    }
    // No lineage (or the parent state was unreadable): start clean.
    let _ = std::fs::remove_file(state_file);
}

/// Terminate any *leftover fractal board server* still listening on `port` from
/// a previous run. Without this, the new server cannot bind the port and the
/// browser connects to the stale (already-completed, all-green) server instead
/// of the fresh graph. Only processes whose command is the execution-graph
/// `server.py` are killed, never unrelated processes on the port.
fn free_port(port: u16) {
    use std::collections::BTreeSet;
    // PIDs currently listening on the port.
    let Ok(listing) = Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}")])
        .output()
    else {
        return;
    };
    let on_port: BTreeSet<String> = String::from_utf8_lossy(&listing.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if on_port.is_empty() {
        return;
    }
    // PIDs that are execution-graph board servers (matched on the full command
    // line via pgrep, which — unlike `ps -o command=` — does not truncate the
    // long macOS `python3` path before the server.py argument).
    let board_servers: BTreeSet<String> = Command::new("pgrep")
        .args(["-f", "execution-graph/server.py"])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut killed = false;
    for pid in on_port.intersection(&board_servers) {
        let _ = Command::new("kill").arg("-9").arg(pid).status();
        killed = true;
    }
    if killed {
        // Give the OS a moment to release the socket before the new bind.
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Block until the board's `/api/health` responds, so execution progress posted
/// immediately after does not race a not-yet-listening server (which would drop
/// the first node's checkout/complete and leave it stuck).
pub(crate) fn wait_until_listening(port: u16) {
    let url = format!("http://127.0.0.1:{port}/api/health");
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if ureq::get(&url)
            .timeout(Duration::from_millis(300))
            .call()
            .is_ok()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

const DEFAULT_BOARD_URL: &str = "http://127.0.0.1:8091/";
const START_COMMAND: &str = "python3 execution-graph/server.py --prd FRACTAL_PIPELINE_TASKS.md --state execution-graph/graph-state-pipeline.json --port 8091";

#[derive(Debug, Deserialize)]
struct BoardPayload {
    title: String,
    groups: Vec<Milestone>,
}

#[derive(Debug, Deserialize)]
struct Milestone {
    id: String,
    title: String,
    tasks: Vec<Task>,
}

#[derive(Debug, Deserialize)]
struct Task {
    kind: String,
    status: String,
}

#[derive(Debug, Eq, PartialEq)]
struct StatusSummary {
    title: String,
    milestone_count: usize,
    task_counts: Counts,
    gate_counts: Counts,
    milestones: Vec<MilestoneProgress>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Counts {
    total: usize,
    complete: usize,
    active: usize,
    incomplete: usize,
}

#[derive(Debug, Eq, PartialEq)]
struct MilestoneProgress {
    id: String,
    title: String,
    complete: usize,
    total: usize,
}

impl Counts {
    fn add(&mut self, status: &str) {
        self.total += 1;
        match status {
            "complete" => self.complete += 1,
            "active" => self.active += 1,
            _ => self.incomplete += 1,
        }
    }
}

/// Open the default execution-graph board in the macOS browser.
pub(crate) fn open() -> Result<()> {
    open_url(DEFAULT_BOARD_URL)?;
    println!("Opened {DEFAULT_BOARD_URL}");
    Ok(())
}

/// Open one already-validated board or project URL in the macOS browser.
pub(crate) fn open_url(url: &str) -> Result<()> {
    let status = Command::new("open")
        .arg(url)
        .status()
        .context("failed to launch the macOS `open` command")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "macOS `open` could not open {url} (exit status {status})"
        ))
    }
}

/// Select the page shown after graph publication. The localhost server remains
/// the execution-status backend, while an authenticated cloud URL becomes the
/// browser destination.
pub(crate) fn browser_target(project_url: Option<&str>, port: u16) -> (String, bool) {
    match project_url {
        Some(url) => (url.to_owned(), true),
        None => (format!("http://127.0.0.1:{port}/"), false),
    }
}

/// Launch a board backed by a raw execution-graph JSON file (fresh state).
pub(crate) fn serve_graph_file(
    graph_file: &Path,
    port: u16,
    exec_graph_dir: Option<&Path>,
    no_open: bool,
) -> Result<()> {
    if !graph_file.is_file() {
        bail!("execution graph file is missing: {}", graph_file.display());
    }
    let state_file = graph_file.with_extension("board-state.json");
    let _ = std::fs::remove_file(&state_file); // start clean so progress shows
    free_port(port); // replace any leftover board server holding this port
    let viewer_dir = resolve_exec_graph_dir(exec_graph_dir)?;
    let server_path = viewer_dir.join("server.py");
    if !server_path.is_file() {
        bail!(
            "execution-graph viewer server.py is missing: {}",
            server_path.display()
        );
    }
    Command::new("python3")
        .arg(&server_path)
        .arg("--graph")
        .arg(graph_file)
        .arg("--state")
        .arg(&state_file)
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch board server {}", server_path.display()))?;
    wait_until_listening(port);
    let url = format!("http://127.0.0.1:{port}/");
    println!("Board: {url}");
    if !no_open {
        let _ = Command::new("open").arg(&url).status();
    }
    Ok(())
}

/// Launch a board backed by one committed execution graph.
pub(crate) fn serve_graph(
    graph_hash: &str,
    port: u16,
    exec_graph_dir: Option<&Path>,
    no_open: bool,
    preseed_completed: Option<&std::collections::BTreeSet<String>>,
) -> Result<()> {
    let graph = graph_store::load_graph(graph_hash)?;
    let graph_file = graph_store::graph_path(graph_hash);
    if !graph_file.is_file() {
        bail!(
            "committed execution graph file is missing: {}",
            graph_file.display()
        );
    }
    let graph_id = graph
        .get("graph_id")
        .and_then(serde_json::Value::as_str)
        .context("stored execution graph is missing graph_id")?;
    let state_file = graph_file.with_extension("board-state.json");
    // Seed the board state. A brand-new graph starts clean so progress shows from
    // zero; an evolved child (grown/repaired/differentiated mid-run) INHERITS its
    // parent's progress so re-serving it does not reset every completed task to
    // pending — which, combined with the planning reveal, made the whole graph
    // vanish behind "planning…" each time evolution fired.
    seed_board_state(&graph, &state_file, preseed_completed);
    free_port(port); // replace any leftover board server holding this port

    let viewer_dir = resolve_exec_graph_dir(exec_graph_dir)?;
    let server_path = viewer_dir.join("server.py");
    if !server_path.is_file() {
        bail!(
            "execution-graph viewer server.py is missing: {}",
            server_path.display()
        );
    }

    Command::new("python3")
        .arg(&server_path)
        .arg("--graph")
        .arg(&graph_file)
        .arg("--state")
        .arg(&state_file)
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to launch python3 execution-graph viewer {}",
                server_path.display()
            )
        })?;
    wait_until_listening(port);

    let url = format!("http://127.0.0.1:{port}/");
    println!("Serving {url}");
    println!("Graph id: {graph_id}");
    println!("Graph hash: {graph_hash}");
    println!("Board state: {}", state_file.display());

    if !no_open {
        let status = Command::new("open")
            .arg(&url)
            .status()
            .context("failed to launch the macOS `open` command")?;
        if !status.success() {
            return Err(anyhow!(
                "macOS `open` could not open {url} (exit status {status})"
            ));
        }
    }
    Ok(())
}

fn resolve_exec_graph_dir(override_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(directory) = override_dir {
        return Ok(directory.to_path_buf());
    }
    if let Some(directory) = env::var_os("FRACTAL_EXEC_GRAPH_DIR") {
        if directory.is_empty() {
            bail!("FRACTAL_EXEC_GRAPH_DIR is set but empty");
        }
        return Ok(PathBuf::from(directory));
    }

    // The standalone repository owns its viewer. Keep the parent-directory
    // fallback for binaries built from the historical Fractalmaster layout.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let standalone = manifest.join("execution-graph");
    if standalone.join("server.py").is_file() {
        return Ok(standalone);
    }
    let repository = manifest
        .parent()
        .context("cannot resolve repository root from the fractal-cli manifest")?;
    Ok(repository.join("execution-graph"))
}

/// Fetch, parse, and print current execution-graph status.
pub(crate) fn status(base_url: &str, json: bool) -> Result<()> {
    let endpoint = format!("{}/api/graph", base_url.trim_end_matches('/'));
    let response = ureq::get(&endpoint).call().map_err(|error| {
        anyhow!("board not running at {base_url}: {error}\nStart it with:\n  {START_COMMAND}")
    })?;
    let body = response.into_string().map_err(|error| {
        anyhow!(
            "board at {base_url} returned an unreadable response: {error}\nStart it with:\n  {START_COMMAND}"
        )
    })?;
    let (_, summary) = parse_payload(&body)
        .with_context(|| format!("board at {base_url} returned an invalid /api/graph payload"))?;

    if json {
        println!("{body}");
    } else {
        print!("{}", render_summary(&summary.title, &summary));
    }
    Ok(())
}

fn parse_payload(body: &str) -> Result<(serde_json::Value, StatusSummary)> {
    let value: serde_json::Value =
        serde_json::from_str(body).context("response is not valid JSON")?;
    let payload: BoardPayload =
        serde_json::from_value(value.clone()).context("response has an unexpected graph schema")?;

    let mut task_counts = Counts::default();
    let mut gate_counts = Counts::default();
    let mut milestones = Vec::with_capacity(payload.groups.len());
    for milestone in payload.groups {
        let mut complete = 0;
        for task in &milestone.tasks {
            if task.status == "complete" {
                complete += 1;
            }
            if task.kind == "gate" {
                gate_counts.add(&task.status);
            } else if task.kind == "task" {
                task_counts.add(&task.status);
            }
        }
        milestones.push(MilestoneProgress {
            id: milestone.id,
            title: milestone.title,
            complete,
            total: milestone.tasks.len(),
        });
    }

    Ok((
        value,
        StatusSummary {
            title: payload.title,
            milestone_count: milestones.len(),
            task_counts,
            gate_counts,
            milestones,
        },
    ))
}

fn render_summary(title: &str, summary: &StatusSummary) -> String {
    let mut lines = vec![
        format!("Board: {title}"),
        format!("Milestones: {}", summary.milestone_count),
        format!(
            "Tasks: {} total, {} complete, {} active, {} incomplete",
            summary.task_counts.total,
            summary.task_counts.complete,
            summary.task_counts.active,
            summary.task_counts.incomplete
        ),
        format!(
            "Gates: {} total, {} complete, {} active, {} incomplete",
            summary.gate_counts.total,
            summary.gate_counts.complete,
            summary.gate_counts.active,
            summary.gate_counts.incomplete
        ),
        "Progress:".to_owned(),
    ];
    lines.extend(summary.milestones.iter().map(|milestone| {
        let percent = (milestone.complete * 100)
            .checked_div(milestone.total)
            .map_or(0, std::convert::identity);
        format!(
            "  {} — {}: {}/{} ({}%)",
            milestone.id, milestone.title, milestone.complete, milestone.total, percent
        )
    }));
    format!("{}\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "schema": "fractal.execution_graph_view.v1",
      "title": "Fractal Pipeline",
      "groups": [
        {
          "id": "P0",
          "title": "Front door",
          "tasks": [
            {"id":"P0.1","title":"CLI","kind":"task","status":"complete"},
            {"id":"P0.2","title":"Intent","kind":"task","status":"active"},
            {"id":"P0.G1","title":"Stable hash","kind":"gate","status":"incomplete"}
          ]
        },
        {
          "id": "P1",
          "title": "Compile",
          "tasks": [
            {"id":"P1.1","title":"Compile","kind":"task","status":"incomplete"},
            {"id":"P1.G1","title":"Same hash","kind":"gate","status":"complete"}
          ]
        }
      ]
    }"#;

    #[test]
    fn parses_api_graph_fixture_without_network() {
        let (value, summary) = parse_payload(FIXTURE).unwrap();
        assert_eq!(value["title"], "Fractal Pipeline");
        assert_eq!(summary.milestone_count, 2);
        assert_eq!(
            summary.task_counts,
            Counts {
                total: 3,
                complete: 1,
                active: 1,
                incomplete: 1
            }
        );
        assert_eq!(
            summary.gate_counts,
            Counts {
                total: 2,
                complete: 1,
                active: 0,
                incomplete: 1
            }
        );
        assert_eq!(
            summary.milestones,
            vec![
                MilestoneProgress {
                    id: "P0".to_owned(),
                    title: "Front door".to_owned(),
                    complete: 1,
                    total: 3
                },
                MilestoneProgress {
                    id: "P1".to_owned(),
                    title: "Compile".to_owned(),
                    complete: 1,
                    total: 2
                }
            ]
        );
    }

    #[test]
    fn renders_counts_and_milestone_progress() {
        let (_, summary) = parse_payload(FIXTURE).unwrap();
        let output = render_summary("Fractal Pipeline", &summary);
        assert!(output.contains("Milestones: 2"));
        assert!(output.contains("Tasks: 3 total, 1 complete, 1 active, 1 incomplete"));
        assert!(output.contains("Gates: 2 total, 1 complete, 0 active, 1 incomplete"));
        assert!(output.contains("P0 — Front door: 1/3 (33%)"));
    }

    #[test]
    fn authenticated_projects_prefer_the_cloud_browser_target() {
        let (cloud, is_cloud) =
            browser_target(Some("https://fractalsociety.com/james/app"), 8092);
        assert_eq!(cloud, "https://fractalsociety.com/james/app");
        assert!(is_cloud);

        let (local, is_cloud) = browser_target(None, 8092);
        assert_eq!(local, "http://127.0.0.1:8092/");
        assert!(!is_cloud);
    }
}
