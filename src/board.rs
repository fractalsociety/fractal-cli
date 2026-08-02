use std::env;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Response, Server, StatusCode};

const DEFAULT_BOARD_URL: &str = "http://127.0.0.1:8091/";
const START_COMMAND: &str = "fractal graph board GRAPH_HASH";
const BOARD_START_TIMEOUT: Duration = Duration::from_secs(6);
const BOARD_STOP_TIMEOUT: Duration = Duration::from_secs(3);

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

fn free_board_port(port: u16) -> Result<()> {
    let listing = Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}")])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let pids: Vec<String> = listing.split_whitespace().map(str::to_owned).collect();
    let mut board_pids = Vec::new();
    for pid in &pids {
        let command = Command::new("ps")
            .args(["-p", pid, "-o", "command="])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        if command.contains("fractal graph serve") || command.contains("execution-graph/server.py")
        {
            board_pids.push(pid.clone());
            let _ = Command::new("kill").args(["-TERM", pid]).status();
        }
    }
    // A short fixed sleep is not enough here: the old board can still own the
    // socket while the replacement is spawned.  In that race the replacement
    // exits on bind and the health probe below falsely succeeds against the
    // old project's board.  Wait for the actual socket to become bindable and
    // escalate only for the board processes we identified above.
    if wait_for_port_release(port, BOARD_STOP_TIMEOUT) {
        return Ok(());
    }
    for pid in board_pids {
        let _ = Command::new("kill").args(["-KILL", &pid]).status();
    }
    if wait_for_port_release(port, BOARD_STOP_TIMEOUT) {
        Ok(())
    } else {
        bail!("board port {port} is still in use after its previous server was stopped")
    }
}

fn wait_for_port_release(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn board_identity_matches(port: u16, workspace: &Path, graph_hash: &str) -> bool {
    let endpoint = format!("http://127.0.0.1:{port}/api/identity");
    let Ok(response) = ureq::get(&endpoint)
        .timeout(Duration::from_millis(300))
        .call()
    else {
        return false;
    };
    let Ok(body) = response.into_string() else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&body) else {
        return false;
    };
    identity_matches_value(&value, workspace, graph_hash)
}

fn wait_until_board_identity(
    port: u16,
    workspace: &Path,
    graph_hash: &str,
    child: &mut std::process::Child,
) -> Result<()> {
    let endpoint = format!("http://127.0.0.1:{port}/api/identity");
    let deadline = Instant::now() + BOARD_START_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().context("check execution board process")? {
            bail!(
                "execution board exited before serving {} (status {status})",
                workspace.display()
            );
        }
        if let Ok(response) = ureq::get(&endpoint)
            .timeout(Duration::from_millis(300))
            .call()
        {
            if let Ok(body) = response.into_string() {
                if let Ok(value) = serde_json::from_str::<Value>(&body) {
                    if identity_matches_value(&value, workspace, graph_hash) {
                        return Ok(());
                    }
                    // A listener answering with another project's identity is not
                    // a successful startup.  Keep polling briefly in case this is
                    // the old process completing its shutdown, then fail closed.
                }
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "execution board on 127.0.0.1:{port} did not publish the expected project identity"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn identity_matches_value(value: &Value, workspace: &Path, graph_hash: &str) -> bool {
    let expected_workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .to_string_lossy()
        .into_owned();
    value.get("schema").and_then(Value::as_str) == Some("fractal.board_identity.v1")
        && value.get("workspace").and_then(Value::as_str) == Some(expected_workspace.as_str())
        && value.get("graph_hash").and_then(Value::as_str) == Some(graph_hash)
}

fn board_identity(workspace: &Path) -> Result<Value> {
    let project = crate::project_file::load(workspace)?;
    let canonical_workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    Ok(json!({
        "schema": "fractal.board_identity.v1",
        "backend": "rust",
        "workspace": canonical_workspace.to_string_lossy(),
        "project": project.project.slug,
        "graph_hash": project.graph_hash,
    }))
}

pub(crate) fn open() -> Result<()> {
    open_url(DEFAULT_BOARD_URL)?;
    println!("Opened {DEFAULT_BOARD_URL}");
    Ok(())
}

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

pub(crate) fn browser_target(project_url: Option<&str>, port: u16) -> (String, bool) {
    match project_url {
        Some(url) => (url.to_owned(), true),
        None => (format!("http://127.0.0.1:{port}/"), false),
    }
}

pub(crate) fn serve_graph_file(
    graph_file: &Path,
    port: u16,
    exec_graph_dir: Option<&Path>,
    no_open: bool,
) -> Result<()> {
    let graph: Value = serde_json::from_slice(
        &fs::read(graph_file)
            .with_context(|| format!("read execution graph {}", graph_file.display()))?,
    )
    .with_context(|| format!("parse execution graph {}", graph_file.display()))?;
    let workspace = env::current_dir()?;
    let title = graph
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Fractal project");
    crate::project_file::persist(&workspace, &graph, title)?;
    spawn_project_server(&workspace, port, exec_graph_dir, no_open)
}

pub(crate) fn serve_graph(
    graph_hash: &str,
    port: u16,
    exec_graph_dir: Option<&Path>,
    no_open: bool,
    _preseed_completed: Option<&std::collections::BTreeSet<String>>,
) -> Result<()> {
    let workspace = crate::run_control::current_workspace().unwrap_or(env::current_dir()?);
    let project = crate::project_file::load(&workspace).with_context(|| {
        format!(
            "canonical project state is required before serving a board: {}",
            crate::project_file::path(&workspace).display()
        )
    })?;
    if project.graph_hash != graph_hash {
        bail!(
            "requested graph {graph_hash} does not match canonical project graph {}",
            project.graph_hash
        );
    }
    spawn_project_server(&workspace, port, exec_graph_dir, no_open)
}

fn spawn_project_server(
    workspace: &Path,
    port: u16,
    exec_graph_dir: Option<&Path>,
    no_open: bool,
) -> Result<()> {
    let project = crate::project_file::load(workspace)?;
    // A board is keyed by both its repository and graph.  Reusing a listener
    // by port alone can display a different project's graph (especially when
    // a previous server is still shutting down), so only reuse an existing
    // server after an authenticated local identity check.
    if board_identity_matches(port, workspace, &project.graph_hash) {
        let url = format!("http://127.0.0.1:{port}/");
        println!("Reusing {url} for {}", workspace.display());
        if !no_open {
            open_url(&url)?;
        }
        return Ok(());
    }
    let viewer_dir = resolve_exec_graph_dir(exec_graph_dir)?;
    let executable = env::current_exe().context("resolve Fractal executable")?;
    free_board_port(port)?;
    let mut command = Command::new(&executable);
    command
        .args(["graph", "serve", "--repo"])
        .arg(workspace)
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if !viewer_dir.as_os_str().is_empty() {
        command.arg("--exec-graph-dir").arg(&viewer_dir);
    }
    let mut child = command.spawn().with_context(|| {
        format!(
            "launch Rust execution board for {}",
            crate::project_file::path(workspace).display()
        )
    })?;
    wait_until_board_identity(port, workspace, &project.graph_hash, &mut child)?;
    let url = format!("http://127.0.0.1:{port}/");
    println!("Serving {url}");
    println!(
        "Project graph: {}",
        crate::project_file::path(workspace).display()
    );
    if !no_open {
        open_url(&url)?;
    }
    Ok(())
}

pub(crate) fn serve_project_foreground(
    workspace: &Path,
    port: u16,
    exec_graph_dir: Option<&Path>,
) -> Result<()> {
    let viewer_dir = resolve_exec_graph_dir(exec_graph_dir)?;
    crate::project_file::load(workspace).with_context(|| {
        format!(
            "canonical project state is missing: {}",
            crate::project_file::path(workspace).display()
        )
    })?;
    let server = Server::http(format!("127.0.0.1:{port}"))
        .map_err(|error| anyhow!("bind Rust graph board on 127.0.0.1:{port}: {error}"))?;
    let token = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    );
    for request in server.incoming_requests() {
        if let Err(error) = respond(request, workspace, &viewer_dir, &token) {
            eprintln!("board request failed: {error:#}");
        }
    }
    Ok(())
}

fn respond(
    request: tiny_http::Request,
    workspace: &Path,
    viewer_dir: &Path,
    token: &str,
) -> Result<()> {
    let route = request.url().split('?').next().unwrap_or("/");
    if request.method() == &Method::Get && route == "/api/identity" {
        return send_json(request, StatusCode(200), &board_identity(workspace)?);
    }
    if request.method() == &Method::Get && route == "/api/health" {
        let identity = board_identity(workspace)?;
        return send_json(
            request,
            StatusCode(200),
            &json!({
                "ok": true,
                "backend": "rust",
                "workspace": identity.get("workspace"),
                "graph_hash": identity.get("graph_hash"),
                "project": identity.get("project"),
            }),
        );
    }
    if request.method() == &Method::Get && route == "/api/graph" {
        return send_json(request, StatusCode(200), &project_view(workspace, token)?);
    }
    if request.method() == &Method::Post && route == "/api/run/pause" {
        let authorized = request.headers().iter().any(|header| {
            header.field.equiv("X-Fractal-Control-Token") && header.value.as_str() == token
        });
        if !authorized {
            return send_json(
                request,
                StatusCode(403),
                &json!({"error": "invalid local control token"}),
            );
        }
        let output = Command::new(env::current_exe()?)
            .args(["pause", "--project"])
            .arg(workspace)
            .output()
            .context("run Rust pause command")?;
        if output.status.success() {
            return send_json(request, StatusCode(200), &json!({"ok": true}));
        }
        return send_json(
            request,
            StatusCode(409),
            &json!({"error": String::from_utf8_lossy(&output.stderr)}),
        );
    }
    if request.method() != &Method::Get {
        return send_json(
            request,
            StatusCode(405),
            &json!({"error": "the Rust board API is read-only; use `fractal node` for transitions"}),
        );
    }
    let relative = match route {
        "/" => "index.html",
        "/app.js" => "app.js",
        "/styles.css" => "styles.css",
        "/assets/favicon.svg" => "assets/favicon.svg",
        "/assets/fractal-graph-field.png" => "assets/fractal-graph-field.png",
        _ => {
            return send_json(request, StatusCode(404), &json!({"error": "not found"}));
        }
    };
    let path = viewer_dir.join(relative);
    let bytes = fs::read(&path).unwrap_or_else(|_| embedded_asset(relative).to_vec());
    let content_type = match Path::new(relative)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    };
    let response = Response::from_data(bytes).with_header(
        Header::from_bytes("Content-Type", content_type).expect("valid content type header"),
    );
    request.respond(response).context("send board asset")?;
    Ok(())
}

fn embedded_asset(relative: &str) -> &'static [u8] {
    match relative {
        "index.html" => include_bytes!("../execution-graph/index.html"),
        "app.js" => include_bytes!("../execution-graph/app.js"),
        "styles.css" => include_bytes!("../execution-graph/styles.css"),
        "assets/favicon.svg" => include_bytes!("../execution-graph/assets/favicon.svg"),
        "assets/fractal-graph-field.png" => {
            include_bytes!("../execution-graph/assets/fractal-graph-field.png")
        }
        _ => &[],
    }
}

fn send_json(request: tiny_http::Request, status: StatusCode, value: &Value) -> Result<()> {
    let response = Response::from_string(serde_json::to_string(value)?)
        .with_status_code(status)
        .with_header(
            Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                .expect("valid JSON header"),
        );
    request.respond(response).context("send board JSON")?;
    Ok(())
}

fn project_view(workspace: &Path, token: &str) -> Result<Value> {
    let project = crate::project_file::load(workspace)?;
    let assignments = project
        .execution
        .as_ref()
        .map(|execution| &execution.assignments);
    let nodes = project
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .context("canonical project graph has no nodes")?;
    let tasks: Vec<Value> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let id = node
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("node-{}", index + 1));
            let assignment = assignments.and_then(|items| items.get(&id));
            let status = match assignment.map(|item| item.state.as_str()) {
                Some("completed") => "complete",
                Some("checked_out") => "active",
                _ => "incomplete",
            };
            json!({
                "id": id,
                "title": node.get("title").or_else(|| node.get("objective")).and_then(Value::as_str).unwrap_or(&id),
                "kind": if node.get("capability").and_then(Value::as_str) == Some("control.verify") { "gate" } else { "task" },
                "status": status,
                "checked": status == "complete",
                "line": 0,
                "instruction": node.get("instruction").and_then(Value::as_str).unwrap_or(""),
                "gate": node.get("verification_plan").and_then(Value::as_str).unwrap_or(""),
                "assignment": assignment,
                "execution": node.get("execution").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    let complete = tasks
        .iter()
        .filter(|task| task.get("status").and_then(Value::as_str) == Some("complete"))
        .count();
    let active = tasks
        .iter()
        .filter(|task| task.get("status").and_then(Value::as_str) == Some("active"))
        .count();
    let total = tasks.len();
    let incomplete = total.saturating_sub(complete + active);
    let percent = (complete * 100).checked_div(total).unwrap_or(0);
    let group_status = if active > 0 {
        "active"
    } else if total > 0 && complete == total {
        "complete"
    } else {
        "incomplete"
    };
    let edges: Vec<Value> = project
        .graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|edge| {
            Some(json!({
                "from": edge.get("from")?.as_str()?,
                "to": edge.get("to")?.as_str()?,
                "condition": edge.get("condition").and_then(Value::as_str).unwrap_or("predecessor_complete"),
            }))
        })
        .collect();
    let phase = project
        .execution
        .as_ref()
        .map(|execution| execution.phase.as_str())
        .unwrap_or("planning");
    Ok(json!({
        "schema": "fractal.execution_graph_view.v1",
        "title": project.project.title,
        "graph": project.graph,
        "efficiency": project.efficiency,
        "work_id": project.project.slug,
        "source": ".fractal/project.fractal",
        "source_mtime": project.updated_at,
        "development": {"visible": false, "steps": []},
        "run_control": {"available": true, "phase": phase, "token": token},
        "totals": {
            "complete": complete,
            "active": active,
            "incomplete": incomplete,
            "all": total,
            "percent": percent,
        },
        "overview": {
            "nodes": [{
                "id": "G0",
                "title": project.project.title,
                "status": group_status,
                "completed": complete,
                "total": total,
                "progress": percent,
                "gate": ""
            }],
            "edges": []
        },
        "groups": [{
            "id": "G0",
            "title": project.project.title,
            "status": group_status,
            "completed": complete,
            "total": total,
            "progress": percent,
            "tasks": tasks,
            "edges": edges
        }]
    }))
}

fn resolve_exec_graph_dir(override_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(directory) = override_dir {
        if directory.join("index.html").is_file() {
            return Ok(directory.to_path_buf());
        }
        bail!("board frontend is missing from {}", directory.display());
    }
    if let Some(directory) = env::var_os("FRACTAL_EXEC_GRAPH_DIR") {
        let directory = PathBuf::from(directory);
        if directory.join("index.html").is_file() {
            return Ok(directory);
        }
        bail!("FRACTAL_EXEC_GRAPH_DIR has no board frontend");
    }
    let manifest = Path::new(option_env!("FRACTAL_BUILD_SOURCE_ROOT").unwrap_or("."));
    let standalone = manifest.join("execution-graph");
    if standalone.join("index.html").is_file() {
        return Ok(standalone);
    }
    let repository = manifest.parent().unwrap_or_else(|| Path::new(""));
    let historical = repository.join("execution-graph");
    if historical.join("index.html").is_file() {
        return Ok(historical);
    }
    // Installed and packaged binaries carry the frontend as compile-time assets,
    // so no Python runtime or source checkout is required.
    Ok(PathBuf::new())
}

pub(crate) fn status(base_url: &str, json: bool) -> Result<()> {
    let endpoint = format!("{}/api/graph", base_url.trim_end_matches('/'));
    let response = ureq::get(&endpoint).call().map_err(|error| {
        anyhow!("board not running at {base_url}: {error}\nStart it with:\n  {START_COMMAND}")
    })?;
    let body = response.into_string().context("read board response")?;
    let (_, summary) = parse_payload(&body)
        .with_context(|| format!("board at {base_url} returned an invalid /api/graph payload"))?;
    if json {
        println!("{body}");
    } else {
        print!("{}", render_summary(&summary.title, &summary));
    }
    Ok(())
}

fn parse_payload(body: &str) -> Result<(Value, StatusSummary)> {
    let value: Value = serde_json::from_str(body).context("response is not valid JSON")?;
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
            } else {
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
            .unwrap_or(0);
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
      "groups": [{
        "id": "G0",
        "title": "Pipeline",
        "tasks": [
          {"id":"P0.1","kind":"task","status":"complete"},
          {"id":"P0.2","kind":"task","status":"active"},
          {"id":"P0.G1","kind":"gate","status":"incomplete"}
        ]
      }]
    }"#;

    #[test]
    fn parses_api_graph_fixture_without_network() {
        let (_, summary) = parse_payload(FIXTURE).unwrap();
        assert_eq!(summary.milestone_count, 1);
        assert_eq!(summary.task_counts.total, 2);
        assert_eq!(summary.gate_counts.total, 1);
    }

    #[test]
    fn authenticated_projects_prefer_the_cloud_browser_target() {
        let (cloud, is_cloud) = browser_target(Some("https://fractalsociety.com/james/app"), 8092);
        assert_eq!(cloud, "https://fractalsociety.com/james/app");
        assert!(is_cloud);
        let (local, is_cloud) = browser_target(None, 8092);
        assert_eq!(local, "http://127.0.0.1:8092/");
        assert!(!is_cloud);
    }

    #[test]
    fn embedded_board_exposes_the_efficiency_counter_without_source_assets() {
        let html = String::from_utf8_lossy(embedded_asset("index.html"));
        let script = String::from_utf8_lossy(embedded_asset("app.js"));
        assert!(html.contains("id=\"efficiency-counter\""));
        assert!(html.contains("Estimated 0 tokens saved"));
        assert!(script.contains("renderEfficiency"));
        assert!(script.contains("realized_tokens_saved"));
    }

    #[test]
    fn board_identity_requires_the_expected_workspace_and_graph() {
        let workspace = std::env::current_dir().unwrap();
        let canonical = workspace.canonicalize().unwrap();
        let graph_hash = "sha256:test-graph";
        let identity = json!({
            "schema": "fractal.board_identity.v1",
            "workspace": canonical.to_string_lossy(),
            "graph_hash": graph_hash,
        });
        assert!(identity_matches_value(&identity, &workspace, graph_hash));

        let mut wrong_graph = identity.clone();
        wrong_graph["graph_hash"] = json!("sha256:another-graph");
        assert!(!identity_matches_value(
            &wrong_graph,
            &workspace,
            graph_hash
        ));

        let mut wrong_workspace = identity;
        wrong_workspace["workspace"] = json!("/tmp/another-project");
        assert!(!identity_matches_value(
            &wrong_workspace,
            &workspace,
            graph_hash
        ));
    }

    #[test]
    fn board_identity_payload_is_derived_from_the_canonical_project() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let identity = board_identity(workspace).unwrap();
        let canonical_workspace = workspace.canonicalize().unwrap();
        let canonical_workspace = canonical_workspace.to_string_lossy();
        assert_eq!(
            identity.get("schema").and_then(Value::as_str),
            Some("fractal.board_identity.v1")
        );
        assert_eq!(
            identity.get("backend").and_then(Value::as_str),
            Some("rust")
        );
        assert_eq!(
            identity.get("workspace").and_then(Value::as_str),
            Some(canonical_workspace.as_ref())
        );
        let graph_hash = identity.get("graph_hash").and_then(Value::as_str).unwrap();
        assert!(identity_matches_value(&identity, workspace, graph_hash));
    }

    #[test]
    fn board_port_release_waits_for_socket_not_a_fixed_sleep() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!wait_for_port_release(port, Duration::from_millis(30)));
        drop(listener);
        assert!(wait_for_port_release(port, Duration::from_millis(100)));
    }
}
