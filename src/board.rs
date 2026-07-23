use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::graph_store;

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
    let status = Command::new("open")
        .arg(DEFAULT_BOARD_URL)
        .status()
        .context("failed to launch the macOS `open` command")?;
    if status.success() {
        println!("Opened {DEFAULT_BOARD_URL}");
        Ok(())
    } else {
        Err(anyhow!(
            "macOS `open` could not open {DEFAULT_BOARD_URL} (exit status {status})"
        ))
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
    let viewer_dir = resolve_exec_graph_dir(exec_graph_dir)?;
    let server_path = viewer_dir.join("server.py");
    if !server_path.is_file() {
        bail!("execution-graph viewer server.py is missing: {}", server_path.display());
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

    // In a source checkout CARGO_MANIFEST_DIR is `<repo>/fractal-cli`, so the
    // sibling viewer is stable regardless of the caller's working directory.
    // Installed builds can override this source-tree fallback with the flag or
    // FRACTAL_EXEC_GRAPH_DIR.
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
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
}
