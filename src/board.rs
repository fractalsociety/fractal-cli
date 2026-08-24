use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::fs;
use std::io::Read;
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
const BOARD_PROJECTS_SCHEMA: &str = "fractal.board_projects.v1";
const FAILURE_GRAPH_VIEW_SCHEMA: &str = "fractal.failure_graph_view.v1";
const GRAPH_SNAPSHOT_SCHEMA: &str = "fractal.graph_snapshot.v1";
const INTELLIGENCE_SNAPSHOT_SCHEMA: &str = "fractal.intelligence.snapshot.v1";
const INTELLIGENCE_QUERY_SCHEMA: &str = "fractal.intelligence.query.v1";
const INTELLIGENCE_QUERY_RESPONSE_SCHEMA: &str = "fractal.intelligence.query_response.v1";
const GRAPH_UI_BUNDLE_ID: &str = "fractal-graph-ui.v1";
const MAX_QUERY_BODY_BYTES: u64 = 16 * 1024;
const MAX_QUERY_CHARS: usize = 512;
const MAX_QUERY_LENSES: usize = 7;
const MAX_QUERY_ROOTS: usize = 32;
const MAX_QUERY_DEPTH: u32 = 32;
const MAX_QUERY_NODES: usize = 1_000;
const MAX_QUERY_EDGES: usize = 2_000;
const LENS_IDS: [&str; 7] = [
    "overview",
    "execution",
    "resource_economic",
    "memory_knowledge",
    "trace_evidence",
    "failure_learning",
    "agent_model_tool_harness",
];
const READ_ONLY_API_ERROR: &str =
    "the Rust board API is read-only; use `fractal node` for transitions";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntelligenceQueryRequest {
    schema: String,
    query: String,
    modality: QueryModality,
    #[serde(default)]
    project_key: Option<String>,
    #[serde(default)]
    lens_ids: Vec<String>,
    #[serde(default)]
    root_ids: Vec<String>,
    #[serde(default)]
    bounds: QueryBounds,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QueryModality {
    Text,
    Voice,
}

impl QueryModality {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Voice => "voice",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryBounds {
    #[serde(default)]
    max_depth: Option<u32>,
    #[serde(default)]
    max_nodes: Option<usize>,
    #[serde(default)]
    max_edges: Option<usize>,
}

#[derive(Debug)]
struct QueryApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl QueryApiError {
    fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode(400),
            code,
            message: message.into(),
        }
    }

    fn payload_too_large() -> Self {
        Self {
            status: StatusCode(413),
            code: "query_too_large",
            message: format!("query body exceeds {MAX_QUERY_BODY_BYTES} bytes"),
        }
    }
}

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

/// Read-only master board over a frozen `fractal.repository_inventory.v1` artifact.
///
/// Opens the browser unless `no_open` is set, then serves in the foreground.
#[allow(dead_code)] // wired by later `fractal graph master` clap integration
pub(crate) fn serve_master(
    inventory_path: &Path,
    port: u16,
    exec_graph_dir: Option<&Path>,
    no_open: bool,
) -> Result<()> {
    // Master mode is explicit in the URL so the same board server can keep
    // bare-root compatibility for individual project graphs.
    let url = master_board_url(port);
    if !no_open {
        let open_url_owned = url.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            let _ = open_url(&open_url_owned);
        });
    }
    println!("Serving master board {url}");
    println!("Inventory: {}", inventory_path.display());
    serve_master_foreground(inventory_path, port, exec_graph_dir)
}

fn master_board_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/?mode=master")
}

/// Foreground master-board server entry point (inventory-backed, GET-only APIs).
#[allow(dead_code)] // wired by later clap/main integration
pub(crate) fn serve_master_foreground(
    inventory_path: &Path,
    port: u16,
    exec_graph_dir: Option<&Path>,
) -> Result<()> {
    let viewer_dir = resolve_exec_graph_dir(exec_graph_dir)?;
    let inventory = crate::master_graph::load_inventory(inventory_path).with_context(|| {
        format!(
            "load frozen repository inventory {}",
            inventory_path.display()
        )
    })?;
    let bound_workspace = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let server = Server::http(format!("127.0.0.1:{port}"))
        .map_err(|error| anyhow!("bind Rust master board on 127.0.0.1:{port}: {error}"))?;
    let token = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    );
    let state = MasterBoardState {
        inventory_path: inventory_path.to_path_buf(),
        inventory,
        viewer_dir,
        token,
        bound_workspace,
    };
    for request in server.incoming_requests() {
        if let Err(error) = respond_master(request, &state) {
            eprintln!("master board request failed: {error:#}");
        }
    }
    Ok(())
}

struct MasterBoardState {
    inventory_path: PathBuf,
    inventory: crate::master_graph::RepositoryInventory,
    viewer_dir: PathBuf,
    token: String,
    bound_workspace: PathBuf,
}

#[derive(Debug)]
struct ApiReply {
    status: StatusCode,
    body: Value,
    etag: Option<String>,
}

fn respond_master(request: tiny_http::Request, state: &MasterBoardState) -> Result<()> {
    let url = request.url().to_owned();
    let route = url.split('?').next().unwrap_or("/");
    let method = request.method().clone();

    // Keep a pasted bare master-board URL in master mode as well.  Individual
    // boards intentionally continue serving `/` without a mode query; this
    // redirect is scoped to the inventory-backed server and avoids the
    // confusing first render where the shared frontend would otherwise pick
    // its individual default.
    if method == Method::Get
        && route == "/"
        && query_param(&url, "mode").as_deref() != Some("master")
    {
        let response = Response::empty(StatusCode(302)).with_header(
            Header::from_bytes("Location", "/?mode=master")
                .expect("valid master mode redirect header"),
        );
        request
            .respond(response)
            .context("redirect bare master board")?;
        return Ok(());
    }

    if method == Method::Get && route == "/api/identity" {
        return send_json(
            request,
            StatusCode(200),
            &board_identity(&state.bound_workspace).unwrap_or_else(|_| {
                json!({
                    "schema": "fractal.board_identity.v1",
                    "backend": "rust",
                    "workspace": state.bound_workspace.to_string_lossy(),
                    "project": Value::Null,
                    "graph_hash": Value::Null,
                })
            }),
        );
    }
    if method == Method::Get && route == "/api/health" {
        return send_json(
            request,
            StatusCode(200),
            &json!({
                "ok": true,
                "backend": "rust",
                "mode": "master",
                "inventory": state.inventory_path.to_string_lossy(),
                "inventory_hash": state.inventory.inventory_hash,
            }),
        );
    }
    if route == "/api/failure-graph" && method != Method::Get {
        return send_json(
            request,
            StatusCode(405),
            &json!({"error": READ_ONLY_API_ERROR, "code": "read_only"}),
        );
    }
    if method == Method::Get && route == "/api/failure-graph" {
        let project_key = query_param(&url, "project").or_else(|| query_param(&url, "project_key"));
        let reply = master_failure_graph_reply(state, project_key.as_deref());
        let if_none_match = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("If-None-Match"))
            .map(|header| header.value.as_str().to_owned());
        if let (Some(etag), Some(inm)) = (&reply.etag, if_none_match.as_deref()) {
            if etag_matches(etag, inm) {
                let response = Response::from_data(Vec::<u8>::new())
                    .with_status_code(StatusCode(304))
                    .with_header(
                        Header::from_bytes("ETag", etag.as_bytes()).expect("valid ETag header"),
                    );
                request.respond(response).context("send 304")?;
                return Ok(());
            }
        }
        return send_json_with_etag(request, reply.status, &reply.body, reply.etag.as_deref());
    }
    if method == Method::Post && route == "/api/run/pause" {
        let authorized = request.headers().iter().any(|header| {
            header.field.equiv("X-Fractal-Control-Token") && header.value.as_str() == state.token
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
            .arg(&state.bound_workspace)
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

    if matches!(
        route,
        "/api/projects" | "/api/master-graph" | "/api/master" | "/api/project-graph"
    ) && method != Method::Get
    {
        return send_json(
            request,
            StatusCode(405),
            &json!({"error": READ_ONLY_API_ERROR, "code": "read_only"}),
        );
    }

    if method == Method::Get
        && (route == "/api/projects"
            || route == "/api/master-graph"
            || route == "/api/master"
            || route == "/api/project-graph"
            || route == "/api/graph")
    {
        let if_none_match = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("If-None-Match"))
            .map(|header| header.value.as_str().to_owned());
        let reply = match route {
            "/api/projects" => master_projects_reply(state),
            "/api/master-graph" | "/api/master" => master_graph_reply(state),
            "/api/project-graph" => {
                let key = query_param(&url, "project_key");
                project_graph_reply(state, key.as_deref())
            }
            "/api/graph" => {
                // Preserve the individual board contract: no query → bound workspace.
                // Optional `?project=` is an inventory-scoped alias for project-graph.
                match query_param(&url, "project").or_else(|| query_param(&url, "project_key")) {
                    Some(key) => project_graph_reply(state, Some(key.as_str())),
                    None => match project_view(&state.bound_workspace, &state.token) {
                        Ok(body) => {
                            let graph_hash = crate::project_file::load(&state.bound_workspace)
                                .ok()
                                .map(|project| project.graph_hash);
                            let etag = graph_hash
                                .as_deref()
                                .or_else(|| {
                                    body.get("graph")
                                        .and_then(|graph| graph.get("hash"))
                                        .and_then(Value::as_str)
                                })
                                .map(|hash| format!("\"{hash}\""));
                            ApiReply {
                                status: StatusCode(200),
                                body,
                                etag,
                            }
                        }
                        Err(error) => ApiReply {
                            status: StatusCode(409),
                            body: json!({
                                "error": format!("{error:#}"),
                                "code": "unavailable_project",
                            }),
                            etag: None,
                        },
                    },
                }
            }
            _ => unreachable!(),
        };
        if let (Some(etag), Some(inm)) = (&reply.etag, if_none_match.as_deref()) {
            if etag_matches(etag, inm) {
                let response = Response::from_data(Vec::<u8>::new())
                    .with_status_code(StatusCode(304))
                    .with_header(
                        Header::from_bytes("ETag", etag.as_bytes()).expect("valid ETag header"),
                    );
                request.respond(response).context("send 304")?;
                return Ok(());
            }
        }
        return send_json_with_etag(request, reply.status, &reply.body, reply.etag.as_deref());
    }

    if method != Method::Get {
        return send_json(
            request,
            StatusCode(405),
            &json!({"error": READ_ONLY_API_ERROR, "code": "read_only"}),
        );
    }

    serve_board_asset(request, route, &state.viewer_dir)
}

fn master_projects_reply(state: &MasterBoardState) -> ApiReply {
    match compose_master_view(state) {
        Ok(view) => {
            let bound_project_key = bound_project_key(state);
            let mut projects: Vec<Value> = view
                .projects
                .iter()
                .map(|project| {
                    let (failure_summary, failure_graph_hash) = if project.available {
                        failure_summary_for_workspace(Path::new(&project.canonical_workspace))
                    } else {
                        (
                            json!({
                                "unresolved": 0,
                                "resolved": 0,
                                "superseded": 0,
                                "lessons": 0,
                                "observations": 0,
                                "total": 0,
                            }),
                            None,
                        )
                    };
                    json!({
                        "project_key": project.project_key,
                        "labels": project.labels,
                        "registry_numbers": project.registry_numbers,
                        "canonical_workspace": project.canonical_workspace,
                        "available": project.available,
                        "catalog_state": project.catalog_state,
                        "graph_hash": project.graph_hash,
                        "failure_summary": failure_summary,
                        "failure_graph_hash": failure_graph_hash,
                        "unavailable_reason": project.git.unavailable_reason,
                    })
                })
                .collect();
            projects.sort_by(|left, right| {
                left.get("project_key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .cmp(
                        right
                            .get("project_key")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    )
            });
            let mut unavailable: Vec<Value> = view
                .unavailable
                .iter()
                .map(|entry| {
                    json!({
                        "canonical_workspace": entry.canonical_workspace,
                        "reason": entry.reason,
                        "registry_numbers": entry.registry_numbers,
                    })
                })
                .collect();
            unavailable.sort_by(|left, right| {
                left.get("canonical_workspace")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .cmp(
                        right
                            .get("canonical_workspace")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    )
            });
            let body = json!({
                "schema": BOARD_PROJECTS_SCHEMA,
                "inventory_hash": view.inventory_hash,
                "bound_project_key": bound_project_key,
                "projects": projects,
                "unavailable": unavailable,
            });
            let etag = format!("\"{}\"", view.inventory_hash);
            ApiReply {
                status: StatusCode(200),
                body,
                etag: Some(etag),
            }
        }
        Err(error) => ApiReply {
            status: StatusCode(500),
            body: json!({
                "error": format!("{error:#}"),
                "code": "compose_failed",
            }),
            etag: None,
        },
    }
}

fn master_graph_reply(state: &MasterBoardState) -> ApiReply {
    match compose_master_view(state) {
        Ok(view) => match serde_json::to_value(&view) {
            Ok(mut body) => {
                if let Some(projects) = body.get_mut("projects").and_then(Value::as_array_mut) {
                    for project in projects {
                        let available = project
                            .get("available")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let (summary, hash) = if available {
                            project
                                .get("canonical_workspace")
                                .and_then(Value::as_str)
                                .map(Path::new)
                                .map(failure_summary_for_workspace)
                                .unwrap_or((
                                    json!({
                                        "unresolved": 0,
                                        "resolved": 0,
                                        "superseded": 0,
                                        "lessons": 0,
                                        "observations": 0,
                                        "total": 0,
                                    }),
                                    None,
                                ))
                        } else {
                            (
                                json!({
                                    "unresolved": 0,
                                    "resolved": 0,
                                    "superseded": 0,
                                    "lessons": 0,
                                    "observations": 0,
                                    "total": 0,
                                }),
                                None,
                            )
                        };
                        if let Some(object) = project.as_object_mut() {
                            object.insert("failure_summary".to_owned(), summary);
                            object.insert(
                                "failure_graph_hash".to_owned(),
                                hash.map(Value::String).unwrap_or(Value::Null),
                            );
                        }
                    }
                }
                let etag = format!("\"{}\"", view.view_hash);
                ApiReply {
                    status: StatusCode(200),
                    body,
                    etag: Some(etag),
                }
            }
            Err(error) => ApiReply {
                status: StatusCode(500),
                body: json!({
                    "error": format!("encode master graph: {error}"),
                    "code": "compose_failed",
                }),
                etag: None,
            },
        },
        Err(error) => ApiReply {
            status: StatusCode(500),
            body: json!({
                "error": format!("{error:#}"),
                "code": "compose_failed",
            }),
            etag: None,
        },
    }
}

fn project_graph_reply(state: &MasterBoardState, project_key: Option<&str>) -> ApiReply {
    let Some(project_key) = project_key.map(str::trim).filter(|key| !key.is_empty()) else {
        return ApiReply {
            status: StatusCode(400),
            body: json!({
                "error": "project_key query parameter is required",
                "code": "bad_request",
            }),
            etag: None,
        };
    };
    if !is_safe_project_key(project_key) {
        return ApiReply {
            status: StatusCode(404),
            body: json!({
                "error": "project_key is not a valid inventory member key",
                "code": "not_in_inventory",
            }),
            etag: None,
        };
    }

    let Some(record) = inventory_record_for_key(&state.inventory, project_key) else {
        return ApiReply {
            status: StatusCode(404),
            body: json!({
                "error": format!("project_key `{project_key}` is not in the frozen inventory"),
                "code": "not_in_inventory",
            }),
            etag: None,
        };
    };

    if !record.exists {
        return ApiReply {
            status: StatusCode(409),
            body: json!({
                "error": format!(
                    "project `{project_key}` is unavailable: {}",
                    record
                        .unavailable_reason
                        .as_deref()
                        .unwrap_or("workspace_path_does_not_exist")
                ),
                "code": "unavailable_project",
                "project_key": project_key,
                "canonical_workspace": record.canonical_workspace,
                "diagnostics": [{
                    "code": "unavailable_workspace",
                    "severity": "warning",
                    "message": record
                        .unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "workspace_path_does_not_exist".to_owned()),
                    "project_key": project_key,
                }],
            }),
            etag: None,
        };
    }

    // Resolve exclusively from the frozen inventory record — never from caller paths.
    let workspace = PathBuf::from(&record.canonical_workspace);
    let composed = compose_master_view(state).ok();
    if let Some(entry) = composed.as_ref().and_then(|view| {
        view.projects
            .iter()
            .find(|project| project.project_key == project_key)
    }) {
        if !entry.available {
            let diagnostics: Vec<Value> = composed
                .as_ref()
                .map(|view| {
                    view.diagnostics
                        .iter()
                        .filter(|diagnostic| diagnostic.project_key.as_deref() == Some(project_key))
                        .take(32)
                        .map(|diagnostic| {
                            json!({
                                "code": diagnostic.code,
                                "severity": diagnostic.severity,
                                "message": diagnostic.message,
                                "project_key": diagnostic.project_key,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            return ApiReply {
                status: StatusCode(409),
                body: json!({
                    "error": format!("project `{project_key}` is unavailable"),
                    "code": "unavailable_project",
                    "project_key": project_key,
                    "catalog_state": entry.catalog_state,
                    "diagnostics": diagnostics,
                }),
                etag: None,
            };
        }
    }

    match project_view(&workspace, &state.token) {
        Ok(mut body) => {
            if let Some(entry) = composed.as_ref().and_then(|view| {
                view.projects
                    .iter()
                    .find(|project| project.project_key == project_key)
            }) {
                if entry.catalog_state == "invalid" {
                    let diagnostics: Vec<Value> = composed
                        .as_ref()
                        .map(|view| {
                            view.diagnostics
                                .iter()
                                .filter(|diagnostic| {
                                    diagnostic.project_key.as_deref() == Some(project_key)
                                })
                                .take(32)
                                .map(|diagnostic| {
                                    json!({
                                        "code": diagnostic.code,
                                        "severity": diagnostic.severity,
                                        "message": diagnostic.message,
                                        "project_key": diagnostic.project_key,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // Execution graph remains usable; surface bounded catalog diagnostics.
                    if let Some(object) = body.as_object_mut() {
                        object.insert(
                            "catalog_diagnostics".to_owned(),
                            json!({
                                "code": "invalid_project",
                                "catalog_state": entry.catalog_state,
                                "diagnostics": diagnostics,
                            }),
                        );
                    }
                }
            }
            let etag = crate::project_file::load(&workspace)
                .ok()
                .map(|project| format!("\"{}\"", project.graph_hash));
            ApiReply {
                status: StatusCode(200),
                body,
                etag,
            }
        }
        Err(error) => {
            let catalog_state = composed.as_ref().and_then(|view| {
                view.projects
                    .iter()
                    .find(|project| project.project_key == project_key)
                    .map(|project| project.catalog_state.as_str())
            });
            let code = if catalog_state == Some("invalid") {
                "invalid_project"
            } else {
                "unavailable_project"
            };
            ApiReply {
                status: StatusCode(409),
                body: json!({
                    "error": format!("{error:#}"),
                    "code": code,
                    "project_key": project_key,
                    "diagnostics": [{
                        "code": "project_load_failed",
                        "severity": "error",
                        "message": format!("{error:#}"),
                        "project_key": project_key,
                    }],
                }),
                etag: None,
            }
        }
    }
}

fn master_failure_graph_reply(state: &MasterBoardState, project_key: Option<&str>) -> ApiReply {
    let workspace = if let Some(project_key) = project_key {
        let project_key = project_key.trim();
        if !is_safe_project_key(project_key) {
            return ApiReply {
                status: StatusCode(404),
                body: json!({
                    "error": "project is not a valid inventory member key",
                    "code": "not_in_inventory",
                    "diagnostics": [safe_failure_diagnostic(
                        "invalid_project_key",
                        "warning",
                        "Failure history requires an inventory project key.",
                    )],
                }),
                etag: None,
            };
        }
        let Some(record) = inventory_record_for_key(&state.inventory, project_key) else {
            return ApiReply {
                status: StatusCode(404),
                body: json!({
                    "error": "project is not present in the frozen inventory",
                    "code": "not_in_inventory",
                    "project_key": project_key,
                    "diagnostics": [safe_failure_diagnostic(
                        "project_not_in_inventory",
                        "warning",
                        "Failure history is available only for frozen inventory members.",
                    )],
                }),
                etag: None,
            };
        };
        if !record.exists {
            return ApiReply {
                status: StatusCode(409),
                body: json!({
                    "error": "project workspace is unavailable",
                    "code": "unavailable_project",
                    "project_key": project_key,
                    "diagnostics": [safe_failure_diagnostic(
                        "unavailable_workspace",
                        "warning",
                        "Failure history is unavailable because the inventory workspace is missing.",
                    )],
                }),
                etag: None,
            };
        }
        PathBuf::from(&record.canonical_workspace)
    } else {
        state.bound_workspace.clone()
    };
    failure_graph_reply(&workspace)
}

fn compose_master_view(state: &MasterBoardState) -> Result<crate::master_graph::MasterGraphView> {
    match crate::master_graph::compose_inventory(
        &state.inventory,
        crate::master_graph::ComposeOptions {
            validate_only: false,
            cache: None,
        },
    )? {
        crate::master_graph::ComposeResult::View(view) => Ok(view),
        crate::master_graph::ComposeResult::ValidateOnly(_) => {
            bail!("compose returned validate-only output unexpectedly")
        }
    }
}

fn bound_project_key(state: &MasterBoardState) -> Option<String> {
    let canonical = state
        .bound_workspace
        .canonicalize()
        .unwrap_or_else(|_| state.bound_workspace.clone());
    let key = crate::master_graph::derive_project_key(&canonical.to_string_lossy());
    inventory_record_for_key(&state.inventory, &key).map(|_| key)
}

fn inventory_record_for_key<'a>(
    inventory: &'a crate::master_graph::RepositoryInventory,
    project_key: &str,
) -> Option<&'a crate::master_graph::InventoryRecord> {
    inventory.records.iter().find(|record| {
        crate::master_graph::derive_project_key(&record.canonical_workspace) == project_key
    })
}

fn is_safe_project_key(project_key: &str) -> bool {
    if project_key.is_empty() || project_key.len() > 96 {
        return false;
    }
    if project_key.contains('/')
        || project_key.contains('\\')
        || project_key.contains("..")
        || project_key.contains('\0')
        || project_key.contains('%')
    {
        return false;
    }
    let mut chars = project_key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next()?;
        if key != name {
            continue;
        }
        let raw = parts.next().unwrap_or("");
        return Some(percent_decode(raw));
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hi = from_hex(bytes[index + 1]);
                let lo = from_hex(bytes[index + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    index += 3;
                    continue;
                }
                out.push(bytes[index]);
                index += 1;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn etag_matches(etag: &str, if_none_match: &str) -> bool {
    if_none_match.split(',').map(str::trim).any(|candidate| {
        let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
        candidate == "*" || candidate == etag || candidate == etag.trim_matches('"')
    })
}

/// A bounded, presentation-only projection of the canonical failure graph.
///
/// The project file remains the authority.  This view deliberately does not
/// serialize the typed graph wholesale: flattened extension fields can contain
/// arbitrary data and must never put logs, credentials, or machine-local paths
/// on the read-only board.
#[derive(Debug)]
struct FailureGraphView {
    body: Value,
    hash: String,
    summary: Value,
}

fn empty_failure_graph() -> crate::failure_graph::FailureGraph {
    let mut graph = crate::failure_graph::FailureGraph::empty();
    let _ = crate::failure_graph::normalize(&mut graph);
    graph
}

fn safe_failure_diagnostic(code: &str, severity: &str, message: &str) -> Value {
    json!({
        "code": code,
        "severity": severity,
        "message": message,
    })
}

fn safe_provenance(value: &crate::failure_graph::GraphGitProvenance) -> Value {
    // source_repo is intentionally omitted: despite the typed contract
    // allowing a reference string, it is frequently an absolute workspace
    // path in historical records.
    let mut object = serde_json::Map::new();
    if let Some(hash) = &value.graph_hash {
        object.insert("graph_hash".to_owned(), json!(hash));
    }
    if let Some(commit) = &value.git_commit {
        object.insert("git_commit".to_owned(), json!(commit));
    }
    if let Some(branch) = &value.git_branch {
        object.insert("git_branch".to_owned(), json!(branch));
    }
    if let Some(dirty) = value.dirty {
        object.insert("dirty".to_owned(), json!(dirty));
    }
    Value::Object(object)
}

fn safe_evidence(value: &crate::failure_graph::EvidenceRef) -> Value {
    let mut object = serde_json::Map::new();
    if let Some(sha256) = &value.sha256 {
        object.insert("sha256".to_owned(), json!(sha256));
    }
    if let Some(legacy_ref) = &value.legacy_ref {
        object.insert("legacy_ref".to_owned(), json!(legacy_ref));
    }
    if let Some(kind) = &value.kind {
        object.insert("kind".to_owned(), json!(kind));
    }
    // `path` and flattened fields are omitted.  Evidence hashes and stable
    // legacy identifiers are sufficient for an inspector without exposing a
    // workspace path or an arbitrary log payload.
    Value::Object(object)
}

fn safe_observation(value: &crate::failure_graph::FailureObservation) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("attempt".to_owned(), json!(value.attempt));
    object.insert("outcome".to_owned(), json!(value.outcome));
    object.insert("summary".to_owned(), json!(value.summary));
    object.insert(
        "evidence".to_owned(),
        Value::Array(value.evidence.iter().map(safe_evidence).collect()),
    );
    if let Some(agent) = &value.agent {
        object.insert("agent".to_owned(), json!(agent));
    }
    if let Some(model) = &value.model {
        object.insert("model".to_owned(), json!(model));
    }
    if let Some(version) = &value.version {
        object.insert("version".to_owned(), json!(version));
    }
    object.insert("observed".to_owned(), safe_provenance(&value.observed));
    Value::Object(object)
}

fn safe_resolution(value: &crate::failure_graph::FailureResolution) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("success".to_owned(), json!(value.success));
    object.insert("summary".to_owned(), json!(value.summary));
    object.insert(
        "evidence".to_owned(),
        Value::Array(value.evidence.iter().map(safe_evidence).collect()),
    );
    if let Some(resolved_by) = &value.resolved_by {
        object.insert("resolved_by".to_owned(), json!(resolved_by));
    }
    object.insert("observed".to_owned(), safe_provenance(&value.observed));
    Value::Object(object)
}

fn safe_failure_record(value: &crate::failure_graph::FailureRecord) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("id".to_owned(), json!(value.id));
    object.insert("node_id".to_owned(), json!(value.node_id));
    object.insert("attempt".to_owned(), json!(value.attempt));
    object.insert("failure_code".to_owned(), json!(value.failure_code));
    object.insert("outcome".to_owned(), json!(value.outcome));
    object.insert(
        "state".to_owned(),
        json!(match value.state {
            crate::failure_graph::FailureState::Unresolved => "unresolved",
            crate::failure_graph::FailureState::Resolved => "resolved",
            crate::failure_graph::FailureState::Superseded => "superseded",
        }),
    );
    object.insert("summary".to_owned(), json!(value.summary));
    if let Some(capability) = &value.capability {
        object.insert("capability".to_owned(), json!(capability));
    }
    if let Some(component) = &value.component {
        object.insert("component".to_owned(), json!(component));
    }
    if let Some(source_ref) = &value.source_ref {
        object.insert("source_ref".to_owned(), json!(source_ref));
    }
    object.insert(
        "evidence".to_owned(),
        Value::Array(value.evidence.iter().map(safe_evidence).collect()),
    );
    object.insert(
        "observations".to_owned(),
        Value::Array(value.observations.iter().map(safe_observation).collect()),
    );
    if let Some(agent) = &value.agent {
        object.insert("agent".to_owned(), json!(agent));
    }
    if let Some(model) = &value.model {
        object.insert("model".to_owned(), json!(model));
    }
    if let Some(version) = &value.version {
        object.insert("version".to_owned(), json!(version));
    }
    object.insert("observed".to_owned(), safe_provenance(&value.observed));
    if let Some(resolution) = &value.resolution {
        object.insert("resolution".to_owned(), safe_resolution(resolution));
    }
    if let Some(superseded_by) = &value.superseded_by {
        object.insert("superseded_by".to_owned(), json!(superseded_by));
    }
    Value::Object(object)
}

fn safe_lesson(value: &crate::failure_graph::LessonRecord) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("id".to_owned(), json!(value.id));
    object.insert("summary".to_owned(), json!(value.summary));
    object.insert(
        "status".to_owned(),
        json!(match value.status {
            crate::failure_graph::LessonStatus::Proposed => "proposed",
            crate::failure_graph::LessonStatus::Adopted => "adopted",
            crate::failure_graph::LessonStatus::Superseded => "superseded",
            crate::failure_graph::LessonStatus::Rejected => "rejected",
        }),
    );
    if let Some(capability) = &value.capability {
        object.insert("capability".to_owned(), json!(capability));
    }
    if let Some(component) = &value.component {
        object.insert("component".to_owned(), json!(component));
    }
    if let Some(source_ref) = &value.source_ref {
        object.insert("source_ref".to_owned(), json!(source_ref));
    }
    object.insert(
        "evidence".to_owned(),
        Value::Array(value.evidence.iter().map(safe_evidence).collect()),
    );
    if let Some(agent) = &value.agent {
        object.insert("agent".to_owned(), json!(agent));
    }
    if let Some(model) = &value.model {
        object.insert("model".to_owned(), json!(model));
    }
    if let Some(version) = &value.version {
        object.insert("version".to_owned(), json!(version));
    }
    object.insert("observed".to_owned(), safe_provenance(&value.observed));
    if let Some(superseded_by) = &value.superseded_by {
        object.insert("superseded_by".to_owned(), json!(superseded_by));
    }
    Value::Object(object)
}

fn safe_failure_edge(value: &crate::failure_graph::EdgeRecord) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("id".to_owned(), json!(value.id));
    object.insert("type".to_owned(), json!(value.edge_type.as_str()));
    object.insert("from".to_owned(), json!(value.from));
    object.insert("to".to_owned(), json!(value.to));
    if let Some(evidence) = &value.evidence {
        object.insert("evidence".to_owned(), safe_evidence(evidence));
    }
    Value::Object(object)
}

fn failure_graph_view_from_graph(
    workspace: &Path,
    mut graph: crate::failure_graph::FailureGraph,
    mut diagnostics: Vec<Value>,
) -> FailureGraphView {
    if crate::failure_graph::normalize(&mut graph).is_err() {
        graph = empty_failure_graph();
        diagnostics.push(safe_failure_diagnostic(
            "invalid_failure_graph",
            "warning",
            "Failure history failed validation; showing an empty read-only history.",
        ));
    }
    let hash = graph.failure_graph_hash.clone();
    let mut unresolved = 0usize;
    let mut resolved = 0usize;
    let mut superseded = 0usize;
    let mut observations = 0usize;
    let records: Vec<Value> = graph
        .failures
        .values()
        .map(|record| {
            observations += record.observations.len();
            match record.state {
                crate::failure_graph::FailureState::Unresolved => unresolved += 1,
                crate::failure_graph::FailureState::Resolved => resolved += 1,
                crate::failure_graph::FailureState::Superseded => superseded += 1,
            }
            safe_failure_record(record)
        })
        .collect();
    let lessons: Vec<Value> = graph.lessons.values().map(safe_lesson).collect();
    let edges: Vec<Value> = graph.edges.values().map(safe_failure_edge).collect();
    let summary = json!({
        "unresolved": unresolved,
        "resolved": resolved,
        "superseded": superseded,
        "lessons": lessons.len(),
        "observations": observations,
        "total": records.len(),
    });
    let project_key = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let project_key = crate::master_graph::derive_project_key(&project_key.to_string_lossy());
    let body = json!({
        "schema": FAILURE_GRAPH_VIEW_SCHEMA,
        "project_key": project_key,
        "failure_graph_hash": hash.clone(),
        "canonical_hash": graph.failure_graph_hash.clone(),
        "summary": summary,
        "records": records,
        "lessons": lessons,
        "edges": edges,
        "diagnostics": diagnostics,
    });
    FailureGraphView {
        body,
        hash,
        summary,
    }
}

/// Read and safely project a canonical project failure graph.  Invalid or
/// unsupported envelopes intentionally degrade to an empty view with a stable
/// diagnostic rather than taking down the execution board.
fn failure_graph_view(workspace: &Path) -> FailureGraphView {
    let path = crate::project_file::path(workspace);
    let mut diagnostics = Vec::new();
    let value = match fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    {
        Some(value) => value,
        None => {
            diagnostics.push(safe_failure_diagnostic(
                "failure_graph_unavailable",
                "warning",
                "Failure history is unavailable for this project.",
            ));
            return failure_graph_view_from_graph(workspace, empty_failure_graph(), diagnostics);
        }
    };
    let Some(raw) = value.get("failure_graph") else {
        match crate::project_file::load_failure_graph(workspace) {
            Ok(graph) => return failure_graph_view_from_graph(workspace, graph, diagnostics),
            Err(_) => {
                diagnostics.push(safe_failure_diagnostic(
                    "legacy_failure_projection_unavailable",
                    "warning",
                    "Legacy failure history could not be projected.",
                ));
                return failure_graph_view_from_graph(
                    workspace,
                    empty_failure_graph(),
                    diagnostics,
                );
            }
        }
    };
    let Some(schema) = raw.get("schema").and_then(Value::as_str) else {
        diagnostics.push(safe_failure_diagnostic(
            "invalid_failure_graph_schema",
            "warning",
            "Failure history has no supported schema; showing an empty read-only history.",
        ));
        return failure_graph_view_from_graph(workspace, empty_failure_graph(), diagnostics);
    };
    if schema != crate::failure_graph::FAILURE_GRAPH_SCHEMA {
        diagnostics.push(safe_failure_diagnostic(
            "unsupported_failure_graph_schema",
            "warning",
            "Failure history uses an unsupported schema; showing an empty read-only history.",
        ));
        return failure_graph_view_from_graph(workspace, empty_failure_graph(), diagnostics);
    }
    match serde_json::from_value::<crate::failure_graph::FailureGraph>(raw.clone()) {
        Ok(graph) => failure_graph_view_from_graph(workspace, graph, diagnostics),
        Err(_) => {
            diagnostics.push(safe_failure_diagnostic(
                "invalid_failure_graph",
                "warning",
                "Failure history failed validation; showing an empty read-only history.",
            ));
            failure_graph_view_from_graph(workspace, empty_failure_graph(), diagnostics)
        }
    }
}

fn failure_graph_reply(workspace: &Path) -> ApiReply {
    let view = failure_graph_view(workspace);
    ApiReply {
        status: StatusCode(200),
        body: view.body,
        etag: Some(format!("\"{}\"", view.hash)),
    }
}

fn failure_summary_for_workspace(workspace: &Path) -> (Value, Option<String>) {
    let view = failure_graph_view(workspace);
    (view.summary, Some(view.hash))
}

fn respond(
    mut request: tiny_http::Request,
    workspace: &Path,
    viewer_dir: &Path,
    token: &str,
) -> Result<()> {
    let url = request.url().to_owned();
    let route = url.split('?').next().unwrap_or("/");
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
    if matches!(route, "/api/snapshot" | "/api/intelligence/snapshot") {
        if request.method() != &Method::Get {
            return send_json(
                request,
                StatusCode(405),
                &json!({"error": "snapshot is read-only", "code": "read_only"}),
            );
        }
        return send_json(request, StatusCode(200), &graph_snapshot(workspace)?);
    }
    if matches!(route, "/api/query" | "/api/intelligence/query") {
        if request.method() != &Method::Post {
            return send_json(
                request,
                StatusCode(405),
                &json!({"error": "query requires POST", "code": "method_not_allowed"}),
            );
        }
        let content_type_ok = request.headers().iter().any(|header| {
            header.field.equiv("Content-Type")
                && header
                    .value
                    .as_str()
                    .to_ascii_lowercase()
                    .starts_with("application/json")
        });
        if !content_type_ok {
            return send_json(
                request,
                StatusCode(415),
                &json!({"error": "Content-Type must be application/json", "code": "unsupported_media_type"}),
            );
        }
        let query = match read_query_request(&mut request) {
            Ok(query) => query,
            Err(error) => {
                return send_json(
                    request,
                    error.status,
                    &json!({"error": error.message, "code": error.code}),
                )
            }
        };
        return match intelligence_query(workspace, &query) {
            Ok(response) => send_json(request, StatusCode(200), &response),
            Err(error) => send_json(
                request,
                error.status,
                &json!({"error": error.message, "code": error.code}),
            ),
        };
    }
    if route == "/api/failure-graph" && request.method() != &Method::Get {
        return send_json(
            request,
            StatusCode(405),
            &json!({"error": READ_ONLY_API_ERROR, "code": "read_only"}),
        );
    }
    if request.method() == &Method::Get && route == "/api/failure-graph" {
        let reply = failure_graph_reply(workspace);
        let if_none_match = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("If-None-Match"))
            .map(|header| header.value.as_str().to_owned());
        if let (Some(etag), Some(inm)) = (&reply.etag, if_none_match.as_deref()) {
            if etag_matches(etag, inm) {
                let response = Response::from_data(Vec::<u8>::new())
                    .with_status_code(StatusCode(304))
                    .with_header(
                        Header::from_bytes("ETag", etag.as_bytes()).expect("valid ETag header"),
                    );
                request.respond(response).context("send 304")?;
                return Ok(());
            }
        }
        return send_json_with_etag(request, reply.status, &reply.body, reply.etag.as_deref());
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
            &json!({"error": READ_ONLY_API_ERROR, "code": "read_only"}),
        );
    }
    serve_board_asset(request, route, viewer_dir)
}

fn serve_board_asset(request: tiny_http::Request, route: &str, viewer_dir: &Path) -> Result<()> {
    let relative = match route {
        "/" => "index.html",
        "/app.js" => "app.js",
        "/styles.css" => "styles.css",
        "/master-graph.js" => "master-graph.js",
        "/master-graph.css" => "master-graph.css",
        "/fractal-graph-ui.js" => "fractal-graph-ui.js",
        "/fractal-graph-ui.css" => "fractal-graph-ui.css",
        "/fractal-graph-ui.manifest.json" => "fractal-graph-ui.manifest.json",
        "/assets/favicon.svg" => "assets/favicon.svg",
        "/assets/fractal-graph-field.png" => "assets/fractal-graph-field.png",
        _ => {
            return send_json(
                request,
                StatusCode(404),
                &json!({"error": "not found", "code": "not_found"}),
            );
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
        Some("json") => "application/json; charset=utf-8",
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
        "master-graph.js" => include_bytes!("../execution-graph/master-graph.js"),
        "master-graph.css" => include_bytes!("../execution-graph/master-graph.css"),
        "fractal-graph-ui.js" => include_bytes!("../execution-graph/fractal-graph-ui.js"),
        "fractal-graph-ui.css" => include_bytes!("../execution-graph/fractal-graph-ui.css"),
        "fractal-graph-ui.manifest.json" => {
            include_bytes!("../execution-graph/fractal-graph-ui.manifest.json")
        }
        "assets/favicon.svg" => include_bytes!("../execution-graph/assets/favicon.svg"),
        "assets/fractal-graph-field.png" => {
            include_bytes!("../execution-graph/assets/fractal-graph-field.png")
        }
        _ => &[],
    }
}

fn send_json(request: tiny_http::Request, status: StatusCode, value: &Value) -> Result<()> {
    send_json_with_etag(request, status, value, None)
}

fn send_json_with_etag(
    request: tiny_http::Request,
    status: StatusCode,
    value: &Value,
    etag: Option<&str>,
) -> Result<()> {
    let mut response = Response::from_string(serde_json::to_string(value)?)
        .with_status_code(status)
        .with_header(
            Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                .expect("valid JSON header"),
        );
    if let Some(etag) = etag {
        response = response
            .with_header(Header::from_bytes("ETag", etag.as_bytes()).expect("valid ETag header"));
        response = response.with_header(
            Header::from_bytes("Cache-Control", "private, no-cache").expect("valid cache header"),
        );
    }
    request.respond(response).context("send board JSON")?;
    Ok(())
}

fn project_view(workspace: &Path, token: &str) -> Result<Value> {
    let project = crate::project_file::load(workspace)?;
    let (failure_summary, failure_graph_hash) = failure_summary_for_workspace(workspace);
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
        // Additive compact projection; detailed records live at
        // `/api/failure-graph` so existing graph consumers remain stable.
        "failure_summary": failure_summary,
        "failure_graph_hash": failure_graph_hash,
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

fn graph_snapshot(workspace: &Path) -> Result<Value> {
    let project = crate::project_file::load(workspace)?;
    let intelligence = project_intelligence(&project);
    Ok(json!({
        "schema": GRAPH_SNAPSHOT_SCHEMA,
        "bundle": GRAPH_UI_BUNDLE_ID,
        "project_key": project.project.slug,
        "project": project.project,
        "graph": project.graph,
        "execution": project.execution,
        "learning": project.learning,
        "efficiency": project.efficiency,
        "intelligence": intelligence,
    }))
}

fn project_intelligence(project: &crate::project_file::FractalProject) -> Value {
    let mut derived = derived_project_intelligence(project);
    let Some(supplied) = project
        .extra
        .get("intelligence")
        .and_then(Value::as_object)
        .filter(|value| {
            value.get("schema").and_then(Value::as_str) == Some(INTELLIGENCE_SNAPSHOT_SCHEMA)
        })
    else {
        return derived;
    };
    let mut merged = supplied.clone();
    let mut lenses = derived
        .get_mut("lenses")
        .and_then(Value::as_object_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    if let Some(authoritative) = supplied.get("lenses").and_then(Value::as_object) {
        for lens_id in LENS_IDS {
            if let Some(lens) = authoritative.get(lens_id).filter(|value| value.is_object()) {
                lenses.insert(lens_id.to_owned(), lens.clone());
            }
        }
    }
    merged.insert(
        "schema".to_owned(),
        Value::String(INTELLIGENCE_SNAPSHOT_SCHEMA.to_owned()),
    );
    merged.insert("lenses".to_owned(), Value::Object(lenses));
    Value::Object(merged)
}

fn lens_record(
    id: impl Into<String>,
    record_type: &str,
    label: impl Into<String>,
    summary: impl Into<String>,
    properties: serde_json::Map<String, Value>,
) -> Value {
    json!({
        "id": id.into(),
        "type": record_type,
        "label": label.into(),
        "summary": summary.into(),
        "properties": properties,
    })
}

fn derived_lens(
    lens_id: &str,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    source_hashes: &[String],
    generated_at: &str,
) -> Value {
    let available = !nodes.is_empty() || !edges.is_empty();
    let mut lens = json!({
        "lens_id": lens_id,
        "label": lens_label(lens_id),
        "summary": lens_summary(lens_id),
        "availability": if available { "available" } else { "unavailable" },
        "nodes": nodes,
        "edges": edges,
        "provenance": {
            "source_hashes": source_hashes,
            "generated_at": generated_at,
            "derivation": "canonical_project_projection",
        },
    });
    if available {
        let node_count = lens["nodes"].as_array().map_or(0, Vec::len);
        let edge_count = lens["edges"].as_array().map_or(0, Vec::len);
        lens.as_object_mut().expect("derived lens object").insert(
            "counts".to_owned(),
            json!({"nodes": node_count, "edges": edge_count}),
        );
    }
    lens
}

fn insert_if_some<T: serde::Serialize>(
    properties: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<T>,
) {
    if let Some(value) = value.and_then(|item| serde_json::to_value(item).ok()) {
        properties.insert(key.to_owned(), value);
    }
}

fn safe_evidence_list(evidence: &[crate::failure_graph::EvidenceRef]) -> Vec<Value> {
    evidence
        .iter()
        .map(safe_evidence)
        .filter(|value| value.as_object().is_some_and(|object| !object.is_empty()))
        .collect()
}

fn economic_lens_nodes(
    project: &crate::project_file::FractalProject,
    graph_nodes: &[Value],
) -> Vec<Value> {
    let mut nodes = Vec::new();
    for node in graph_nodes {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        let mut properties = serde_json::Map::new();
        if let Some(value) = node.pointer("/policy_contract/budgets") {
            properties.insert("declared_policy_budget".to_owned(), value.clone());
        }
        if let Some(value) = node.get("budget") {
            properties.insert("declared_runtime_budget".to_owned(), value.clone());
        }
        if let Some(value) = node.pointer("/efficiency/estimated_remaining_tokens") {
            properties.insert("estimated_remaining_tokens".to_owned(), value.clone());
        }
        if properties.is_empty() {
            continue;
        }
        properties.insert(
            "measurement_state".to_owned(),
            Value::String("declared_estimate".to_owned()),
        );
        properties.insert("task_id".to_owned(), Value::String(id.to_owned()));
        nodes.push(lens_record(
            format!("economic:budget:{id}"),
            "declared_budget",
            node.get("title").and_then(Value::as_str).unwrap_or(id),
            "Declared limits and planning estimates; not observed usage.",
            properties,
        ));
    }
    if let Some(efficiency) = &project.efficiency {
        let realized_evidence = efficiency
            .episodes
            .iter()
            .any(|episode| episode.realized_tokens_saved.is_some());
        for episode in &efficiency.episodes {
            let mut properties = serde_json::Map::new();
            properties.insert(
                "measurement_state".to_owned(),
                Value::String(
                    if episode.realized_tokens_saved.is_some() {
                        "observed_and_estimated"
                    } else {
                        "estimated"
                    }
                    .to_owned(),
                ),
            );
            properties.insert(
                "estimated_tokens_avoided".to_owned(),
                json!(episode.estimated_tokens_avoided),
            );
            properties.insert(
                "confidence_adjusted_tokens_avoided".to_owned(),
                json!(episode.confidence_adjusted_tokens_avoided),
            );
            properties.insert("confidence".to_owned(), json!(episode.confidence));
            properties.insert(
                "estimation_basis".to_owned(),
                Value::String(episode.estimation_basis.clone()),
            );
            properties.insert("accepted".to_owned(), Value::Bool(episode.accepted));
            properties.insert(
                "waste_type".to_owned(),
                Value::String(episode.waste_type.as_str().to_owned()),
            );
            properties.insert(
                "proposed_action".to_owned(),
                Value::String(episode.proposed_action.as_str().to_owned()),
            );
            insert_if_some(
                &mut properties,
                "realized_tokens_saved",
                episode.realized_tokens_saved,
            );
            insert_if_some(
                &mut properties,
                "realization_basis",
                episode.realization_basis.clone(),
            );
            nodes.push(lens_record(
                format!("economic:efficiency:{}", episode.episode_id),
                "efficiency_episode",
                format!("Efficiency estimate for {}", episode.detected_node),
                if episode.realized_tokens_saved.is_some() {
                    "Estimated and observed values are separately named and evidence-linked."
                } else {
                    "Estimate only; no realized savings are claimed."
                },
                properties,
            ));
        }
        for (scope, aggregate) in [
            ("build", &efficiency.build),
            ("lifetime", &efficiency.lifetime),
        ] {
            let mut properties = serde_json::Map::new();
            properties.insert(
                "measurement_state".to_owned(),
                Value::String(
                    if realized_evidence {
                        "observed_and_estimated"
                    } else {
                        "estimated"
                    }
                    .to_owned(),
                ),
            );
            if aggregate.episode_count > 0 {
                properties.insert("episode_count".to_owned(), json!(aggregate.episode_count));
            }
            if aggregate.gross_estimated_tokens_avoided > 0 {
                properties.insert(
                    "gross_estimated_tokens_avoided".to_owned(),
                    json!(aggregate.gross_estimated_tokens_avoided),
                );
            }
            if aggregate.confidence_adjusted_tokens_avoided > 0 {
                properties.insert(
                    "confidence_adjusted_tokens_avoided".to_owned(),
                    json!(aggregate.confidence_adjusted_tokens_avoided),
                );
            }
            if aggregate.estimated_cost_avoided > 0.0 {
                properties.insert(
                    "estimated_cost_avoided".to_owned(),
                    json!(aggregate.estimated_cost_avoided),
                );
            }
            if realized_evidence {
                properties.insert(
                    "realized_tokens_saved".to_owned(),
                    json!(aggregate.realized_tokens_saved),
                );
                properties.insert(
                    "realized_cost_avoided".to_owned(),
                    json!(aggregate.realized_cost_avoided),
                );
            }
            if properties.len() > 1 {
                nodes.push(lens_record(
                    format!("economic:aggregate:{scope}"),
                    "efficiency_aggregate",
                    format!("{scope} efficiency"),
                    if realized_evidence {
                        "Aggregate with separately named estimated and evidence-backed realized values."
                    } else {
                        "Estimate-only aggregate; realized values are intentionally omitted."
                    },
                    properties,
                ));
            }
        }
    }
    for record in project.learning.nodes.values() {
        if record.estimated_cost.is_none() && record.actual_cost.is_none() {
            continue;
        }
        let mut properties = serde_json::Map::new();
        properties.insert("task_id".to_owned(), Value::String(record.node_id.clone()));
        properties.insert(
            "measurement_state".to_owned(),
            Value::String(
                if record.actual_cost.is_some() {
                    "observed_and_estimated"
                } else {
                    "estimated"
                }
                .to_owned(),
            ),
        );
        insert_if_some(&mut properties, "estimated_cost", record.estimated_cost);
        insert_if_some(&mut properties, "observed_cost", record.actual_cost);
        nodes.push(lens_record(
            format!("economic:cost:{}", record.node_id),
            "task_cost",
            record.objective.clone(),
            "Task cost fields retain their declared estimated or observed meaning.",
            properties,
        ));
    }
    nodes
}

fn memory_lens(project: &crate::project_file::FractalProject) -> (Vec<Value>, Vec<Value>) {
    let mut nodes = Vec::new();
    let mut included = BTreeSet::new();
    for record in project.learning.nodes.values() {
        if record.outcome.is_none()
            && record.notes.is_none()
            && record.artifacts_produced.is_empty()
            && record.consumed_by.is_empty()
            && !record.human_intervention
        {
            continue;
        }
        included.insert(record.node_id.clone());
        let mut properties = serde_json::Map::new();
        properties.insert("task_id".to_owned(), Value::String(record.node_id.clone()));
        insert_if_some(&mut properties, "outcome", record.outcome);
        insert_if_some(&mut properties, "notes", record.notes.clone());
        if !record.artifacts_produced.is_empty() {
            properties.insert(
                "artifacts_produced".to_owned(),
                json!(record.artifacts_produced),
            );
        }
        if !record.consumed_by.is_empty() {
            properties.insert("consumed_by".to_owned(), json!(record.consumed_by));
        }
        properties.insert("attempt_count".to_owned(), json!(record.attempt_count));
        properties.insert("reopen_count".to_owned(), json!(record.reopen_count));
        if record.human_intervention {
            properties.insert("human_intervention".to_owned(), Value::Bool(true));
        }
        nodes.push(lens_record(
            format!("memory:node:{}", record.node_id),
            "learning_record",
            record.objective.clone(),
            match record.outcome {
                Some(crate::learning_data::NodeOutcome::VerifiedSuccess) => {
                    "Verified learning outcome recorded by the canonical project."
                }
                Some(_) => "Learning outcome recorded without upgrading its verification status.",
                None => "Learning record with evidence or notes and no claimed outcome.",
            },
            properties,
        ));
    }
    for (index, edit) in project.learning.graph_edits.iter().enumerate() {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "graph_before_hash".to_owned(),
            Value::String(edit.graph_before_hash.clone()),
        );
        properties.insert("actor".to_owned(), Value::String(edit.actor.clone()));
        properties.insert("trigger".to_owned(), Value::String(edit.trigger.clone()));
        properties.insert(
            "timestamp".to_owned(),
            Value::String(edit.timestamp.clone()),
        );
        properties.insert(
            "action".to_owned(),
            serde_json::to_value(&edit.action).unwrap_or(Value::Null),
        );
        nodes.push(lens_record(
            format!("memory:graph-edit:{index}"),
            "graph_edit_learning",
            format!("Graph edit {}", index + 1),
            "Canonical graph-edit memory with its original trigger and actor.",
            properties,
        ));
    }
    let edges = project
        .learning
        .nodes
        .values()
        .flat_map(|record| {
            record
                .depends_on
                .iter()
                .filter(|dependency| {
                    included.contains(&record.node_id) && included.contains(*dependency)
                })
                .map(|dependency| {
                    json!({
                        "id": format!("memory-edge:{dependency}:{}", record.node_id),
                        "source": format!("memory:node:{dependency}"),
                        "target": format!("memory:node:{}", record.node_id),
                        "type": "depends_on",
                    })
                })
        })
        .collect();
    (nodes, edges)
}

fn trace_lens(project: &crate::project_file::FractalProject) -> (Vec<Value>, Vec<Value>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    if let Some(execution) = &project.execution {
        for (node_id, assignment) in &execution.assignments {
            let mut properties = serde_json::Map::new();
            properties.insert("task_id".to_owned(), Value::String(node_id.clone()));
            properties.insert("state".to_owned(), Value::String(assignment.state.clone()));
            properties.insert(
                "agent_id".to_owned(),
                Value::String(assignment.agent_id.clone()),
            );
            properties.insert(
                "agent_label".to_owned(),
                Value::String(assignment.agent_label.clone()),
            );
            properties.insert(
                "checked_out_at".to_owned(),
                Value::String(assignment.checked_out_at.clone()),
            );
            insert_if_some(
                &mut properties,
                "completed_at",
                assignment.completed_at.clone(),
            );
            insert_if_some(
                &mut properties,
                "released_at",
                assignment.released_at.clone(),
            );
            nodes.push(lens_record(
                format!("trace:assignment:{node_id}"),
                "assignment_trace",
                format!("Assignment for {node_id}"),
                "Execution assignment state from the canonical project.",
                properties,
            ));
        }
    }
    for record in project.learning.nodes.values() {
        let Some(verification) = &record.verification else {
            continue;
        };
        let mut properties = serde_json::Map::new();
        properties.insert("task_id".to_owned(), Value::String(record.node_id.clone()));
        insert_if_some(&mut properties, "type", verification.kind.clone());
        insert_if_some(&mut properties, "passed", verification.passed);
        if !verification.evidence_refs.is_empty() {
            properties.insert(
                "evidence_refs".to_owned(),
                json!(verification.evidence_refs),
            );
        }
        nodes.push(lens_record(
            format!("trace:verification:{}", record.node_id),
            "verification_evidence",
            format!("Verification for {}", record.node_id),
            if verification.evidence_refs.is_empty() {
                "Verification state is recorded without inventing proof references."
            } else {
                "Verification state with canonical opaque evidence references."
            },
            properties,
        ));
        if project
            .execution
            .as_ref()
            .is_some_and(|execution| execution.assignments.contains_key(&record.node_id))
        {
            edges.push(json!({
                "id": format!("trace-edge:{}", record.node_id),
                "source": format!("trace:assignment:{}", record.node_id),
                "target": format!("trace:verification:{}", record.node_id),
                "type": "verified_by",
            }));
        }
    }
    (nodes, edges)
}

fn failure_learning_lens(
    project: &crate::project_file::FractalProject,
) -> (Vec<Value>, Vec<Value>, Option<String>) {
    let graph = crate::project_file::failure_graph(project);
    let mut nodes = Vec::new();
    let mut ids = BTreeSet::new();
    for failure in graph.failures.values() {
        ids.insert(failure.id.clone());
        let mut properties = serde_json::Map::new();
        properties.insert("task_id".to_owned(), Value::String(failure.node_id.clone()));
        properties.insert(
            "failure_code".to_owned(),
            Value::String(failure.failure_code.clone()),
        );
        properties.insert("outcome".to_owned(), Value::String(failure.outcome.clone()));
        properties.insert(
            "state".to_owned(),
            serde_json::to_value(failure.state).unwrap_or(Value::Null),
        );
        properties.insert("attempt".to_owned(), json!(failure.attempt));
        let evidence = safe_evidence_list(&failure.evidence);
        if !evidence.is_empty() {
            properties.insert("evidence".to_owned(), Value::Array(evidence));
        }
        if let Some(resolution) = &failure.resolution {
            properties.insert(
                "resolution".to_owned(),
                json!({
                    "success": resolution.success,
                    "summary": resolution.summary,
                    "evidence": safe_evidence_list(&resolution.evidence),
                }),
            );
        }
        nodes.push(lens_record(
            failure.id.clone(),
            "failure",
            failure.summary.clone(),
            "Canonical failure record; resolution and evidence appear only when recorded.",
            properties,
        ));
    }
    for lesson in graph.lessons.values() {
        ids.insert(lesson.id.clone());
        let mut properties = serde_json::Map::new();
        properties.insert(
            "status".to_owned(),
            serde_json::to_value(lesson.status).unwrap_or(Value::Null),
        );
        let evidence = safe_evidence_list(&lesson.evidence);
        if !evidence.is_empty() {
            properties.insert("evidence".to_owned(), Value::Array(evidence));
        }
        insert_if_some(&mut properties, "capability", lesson.capability.clone());
        insert_if_some(&mut properties, "component", lesson.component.clone());
        nodes.push(lens_record(
            lesson.id.clone(),
            "lesson",
            lesson.summary.clone(),
            "Canonical lesson retaining its recorded adoption status.",
            properties,
        ));
    }
    for record in project.learning.nodes.values() {
        let Some(failure_code) = record.failure_code else {
            continue;
        };
        let code = serde_json::to_value(failure_code)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_owned());
        let id = format!("learning-failure:{}:{code}", record.node_id);
        if ids.contains(&id) {
            continue;
        }
        let mut properties = serde_json::Map::new();
        properties.insert("task_id".to_owned(), Value::String(record.node_id.clone()));
        properties.insert("failure_code".to_owned(), Value::String(code));
        insert_if_some(&mut properties, "outcome", record.outcome);
        properties.insert("attempt_count".to_owned(), json!(record.attempt_count));
        ids.insert(id.clone());
        nodes.push(lens_record(
            id,
            "learning_failure",
            record.objective.clone(),
            "Learning record reports a failure without claiming a separate lesson or resolution.",
            properties,
        ));
    }
    let edges = graph
        .edges
        .values()
        .filter(|edge| ids.contains(&edge.from) && ids.contains(&edge.to))
        .map(|edge| {
            json!({
                "id": edge.id,
                "source": edge.from,
                "target": edge.to,
                "type": edge.edge_type.as_str(),
            })
        })
        .collect();
    (
        nodes,
        edges,
        (!graph.failure_graph_hash.is_empty()).then_some(graph.failure_graph_hash),
    )
}

fn agent_harness_lens(
    project: &crate::project_file::FractalProject,
    graph_nodes: &[Value],
) -> (Vec<Value>, Vec<Value>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let assignments = project
        .execution
        .as_ref()
        .map(|execution| &execution.assignments);
    for node in graph_nodes {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        let learned_executor = project
            .learning
            .nodes
            .get(id)
            .and_then(|record| record.executor.as_ref());
        let assignment = assignments.and_then(|items| items.get(id));
        if learned_executor.is_some() || assignment.is_some() {
            let mut properties = serde_json::Map::new();
            properties.insert("task_id".to_owned(), Value::String(id.to_owned()));
            insert_if_some(
                &mut properties,
                "agent",
                learned_executor
                    .and_then(|executor| executor.agent.clone())
                    .or_else(|| assignment.map(|value| value.agent_label.clone())),
            );
            insert_if_some(
                &mut properties,
                "model",
                learned_executor.and_then(|executor| executor.model.clone()),
            );
            insert_if_some(
                &mut properties,
                "version",
                learned_executor.and_then(|executor| executor.version.clone()),
            );
            insert_if_some(
                &mut properties,
                "state",
                assignment.map(|value| value.state.clone()),
            );
            nodes.push(lens_record(
                format!("agent:assignment:{id}"),
                "agent_assignment",
                format!("Agent for {id}"),
                "Recorded executor metadata and current assignment state.",
                properties,
            ));
        }
        let mut properties = serde_json::Map::new();
        properties.insert("task_id".to_owned(), Value::String(id.to_owned()));
        if let Some(capability) = node.get("capability") {
            properties.insert("capability".to_owned(), capability.clone());
        }
        if let Some(executor) = node.get("executor") {
            properties.insert("declared_executor".to_owned(), executor.clone());
        }
        if let Some(routes) = node.get("route_candidates") {
            properties.insert("route_candidates".to_owned(), routes.clone());
        }
        if let Some(profile) = node.pointer("/policy_contract/sandbox_profile") {
            properties.insert("sandbox_profile".to_owned(), profile.clone());
        }
        if let Some(provenance) = node.pointer("/policy_contract/provenance") {
            properties.insert("policy_provenance".to_owned(), provenance.clone());
        }
        if properties.len() > 1 {
            nodes.push(lens_record(
                format!("harness:binding:{id}"),
                "harness_binding",
                node.get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(id),
                "Declared capability, provider route, and sandbox metadata from the execution graph.",
                properties,
            ));
            if learned_executor.is_some() || assignment.is_some() {
                edges.push(json!({
                    "id": format!("agent-harness:{id}"),
                    "source": format!("agent:assignment:{id}"),
                    "target": format!("harness:binding:{id}"),
                    "type": "executes_with",
                }));
            }
        }
    }
    (nodes, edges)
}

fn derived_project_intelligence(project: &crate::project_file::FractalProject) -> Value {
    let graph_nodes = project
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let graph_edges = project
        .graph
        .get("edges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let economic_nodes = economic_lens_nodes(project, &graph_nodes);
    let (memory_nodes, memory_edges) = memory_lens(project);
    let (trace_nodes, trace_edges) = trace_lens(project);
    let (failure_nodes, failure_edges, failure_hash) = failure_learning_lens(project);
    let (agent_nodes, agent_edges) = agent_harness_lens(project, &graph_nodes);
    let mut source_hashes = BTreeSet::from([project.graph_hash.clone()]);
    if let Some(efficiency) = &project.efficiency {
        if !efficiency.config_hash.is_empty() {
            source_hashes.insert(efficiency.config_hash.clone());
        }
    }
    if let Some(hash) = failure_hash {
        source_hashes.insert(hash);
    }
    let source_hashes: Vec<String> = source_hashes.into_iter().collect();
    let mut lenses = serde_json::Map::new();
    lenses.insert(
        "overview".to_owned(),
        derived_lens(
            "overview",
            graph_nodes.clone(),
            graph_edges.clone(),
            &source_hashes,
            &project.updated_at,
        ),
    );
    lenses.insert(
        "execution".to_owned(),
        derived_lens(
            "execution",
            graph_nodes,
            graph_edges,
            &source_hashes,
            &project.updated_at,
        ),
    );
    for (lens_id, nodes, edges) in [
        ("resource_economic", economic_nodes, Vec::new()),
        ("memory_knowledge", memory_nodes, memory_edges),
        ("trace_evidence", trace_nodes, trace_edges),
        ("failure_learning", failure_nodes, failure_edges),
        ("agent_model_tool_harness", agent_nodes, agent_edges),
    ] {
        lenses.insert(
            lens_id.to_owned(),
            derived_lens(lens_id, nodes, edges, &source_hashes, &project.updated_at),
        );
    }
    json!({
        "schema": INTELLIGENCE_SNAPSHOT_SCHEMA,
        "source": {
            "kind": "canonical_project_projection",
            "schema": project.schema,
        },
        "generated_at": project.updated_at,
        "lenses": lenses,
    })
}

fn lens_label(lens_id: &str) -> &'static str {
    match lens_id {
        "overview" => "Overview",
        "execution" => "Execution",
        "resource_economic" => "Economics",
        "memory_knowledge" => "Memory",
        "trace_evidence" => "Traces & evidence",
        "failure_learning" => "Failures & lessons",
        "agent_model_tool_harness" => "Agents & tools",
        _ => "Unavailable",
    }
}

fn lens_summary(lens_id: &str) -> &'static str {
    match lens_id {
        "overview" => "The full bounded project picture across every available authority.",
        "execution" => "Tasks, dependencies, waves, status, and active ownership.",
        "resource_economic" => {
            "Estimates, observed usage, bills, receipts, rewards, and finality remain distinct."
        }
        "memory_knowledge" => "Verified outcomes and reusable knowledge recorded for future work.",
        "trace_evidence" => {
            "Assignments, verification evidence, and causal execution observations."
        }
        "failure_learning" => "Failures, repair observations, resolutions, and adopted lessons.",
        "agent_model_tool_harness" => "Assigned agents, models, tools, and harness boundaries.",
        _ => "No lens data is available.",
    }
}

fn unavailable_lens(lens_id: &str) -> Value {
    json!({
        "lens_id": lens_id,
        "label": lens_label(lens_id),
        "summary": lens_summary(lens_id),
        "availability": "unavailable",
        "nodes": [],
        "edges": [],
    })
}

fn fallback_graph_lens(snapshot: &Value, lens_id: &str) -> Value {
    let graph = snapshot.get("graph").cloned().unwrap_or_else(|| json!({}));
    let nodes = graph
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let edges = graph
        .get("edges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    json!({
        "lens_id": lens_id,
        "label": lens_label(lens_id),
        "summary": lens_summary(lens_id),
        "availability": if nodes.is_empty() { "unavailable" } else { "available" },
        "counts": {"nodes": nodes.len(), "edges": edges.len()},
        "nodes": nodes,
        "edges": edges,
        "provenance": {
            "source_hashes": graph.get("graph_hash").and_then(Value::as_str).map(|hash| vec![hash]).unwrap_or_default(),
        },
    })
}

fn canonical_intelligence(snapshot: &Value) -> Value {
    let supplied = snapshot
        .get("intelligence")
        .and_then(Value::as_object)
        .filter(|value| {
            value.get("schema").and_then(Value::as_str) == Some(INTELLIGENCE_SNAPSHOT_SCHEMA)
        });
    let supplied_lenses = supplied
        .and_then(|value| value.get("lenses"))
        .and_then(Value::as_object);
    let mut lenses = serde_json::Map::new();
    for lens_id in LENS_IDS {
        let lens = supplied_lenses
            .and_then(|items| items.get(lens_id))
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| match lens_id {
                "overview" | "execution" => fallback_graph_lens(snapshot, lens_id),
                _ => unavailable_lens(lens_id),
            });
        lenses.insert(lens_id.to_owned(), lens);
    }
    let mut result = supplied.cloned().unwrap_or_default();
    result.insert(
        "schema".to_owned(),
        Value::String(INTELLIGENCE_SNAPSHOT_SCHEMA.to_owned()),
    );
    result.insert("lenses".to_owned(), Value::Object(lenses));
    Value::Object(result)
}

fn node_id(value: &Value) -> Option<&str> {
    value
        .get("id")
        .or_else(|| value.get("node_id"))
        .and_then(Value::as_str)
}

fn edge_endpoints(value: &Value) -> Option<(&str, &str)> {
    let source = value
        .get("source")
        .or_else(|| value.get("from"))
        .or_else(|| value.get("source_id"))
        .and_then(Value::as_str)?;
    let target = value
        .get("target")
        .or_else(|| value.get("to"))
        .or_else(|| value.get("target_id"))
        .and_then(Value::as_str)?;
    Some((source, target))
}

fn query_terms(query: &str) -> Vec<String> {
    const STOP_WORDS: [&str; 20] = [
        "show",
        "find",
        "me",
        "the",
        "a",
        "an",
        "and",
        "graph",
        "overview",
        "execution",
        "economic",
        "economics",
        "memory",
        "traces",
        "evidence",
        "failures",
        "lessons",
        "agents",
        "tools",
        "tasks",
    ];
    query
        .split(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '-'
        })
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|term| !STOP_WORDS.contains(&term.as_str()))
        .collect()
}

fn inferred_lens_ids(query: &str) -> Vec<String> {
    let lower = query.to_ascii_lowercase();
    let mut lenses = Vec::new();
    let patterns = [
        (
            "failure_learning",
            ["failure", "lesson", "repair"].as_slice(),
        ),
        ("trace_evidence", ["trace", "evidence", "span"].as_slice()),
        (
            "memory_knowledge",
            ["memory", "knowledge", "remember"].as_slice(),
        ),
        (
            "resource_economic",
            ["economic", "cost", "budget", "bill", "receipt", "reward"].as_slice(),
        ),
        (
            "agent_model_tool_harness",
            ["agent", "model", "tool", "harness"].as_slice(),
        ),
        (
            "execution",
            ["execution", "task", "wave", "milestone"].as_slice(),
        ),
    ];
    for (lens_id, terms) in patterns {
        if terms.iter().any(|term| lower.contains(term)) {
            lenses.push(lens_id.to_owned());
        }
    }
    if lenses.is_empty() {
        lenses.push("overview".to_owned());
    }
    lenses
}

fn validate_query(query: &IntelligenceQueryRequest) -> std::result::Result<(), QueryApiError> {
    if query.schema != INTELLIGENCE_QUERY_SCHEMA {
        return Err(QueryApiError::bad_request(
            "invalid_schema",
            format!("schema must be {INTELLIGENCE_QUERY_SCHEMA}"),
        ));
    }
    let query_length = query.query.chars().count();
    if query.query.trim().is_empty() || query_length > MAX_QUERY_CHARS {
        return Err(QueryApiError::bad_request(
            "invalid_query",
            format!("query must contain 1..={MAX_QUERY_CHARS} characters"),
        ));
    }
    if query.lens_ids.len() > MAX_QUERY_LENSES {
        return Err(QueryApiError::bad_request(
            "too_many_lenses",
            format!("lens_ids is limited to {MAX_QUERY_LENSES} entries"),
        ));
    }
    if let Some(invalid) = query
        .lens_ids
        .iter()
        .find(|lens| !LENS_IDS.contains(&lens.as_str()))
    {
        return Err(QueryApiError::bad_request(
            "unknown_lens",
            format!("unknown lens_id {invalid}"),
        ));
    }
    if query.root_ids.len() > MAX_QUERY_ROOTS {
        return Err(QueryApiError::bad_request(
            "too_many_roots",
            format!("root_ids is limited to {MAX_QUERY_ROOTS} entries"),
        ));
    }
    if query.bounds.max_depth.unwrap_or(0) > MAX_QUERY_DEPTH
        || query.bounds.max_nodes.unwrap_or(100) > MAX_QUERY_NODES
        || query.bounds.max_edges.unwrap_or(200) > MAX_QUERY_EDGES
    {
        return Err(QueryApiError::bad_request(
            "bounds_exceeded",
            format!(
                "bounds may not exceed max_depth={MAX_QUERY_DEPTH}, max_nodes={MAX_QUERY_NODES}, max_edges={MAX_QUERY_EDGES}"
            ),
        ));
    }
    Ok(())
}

fn read_query_request(
    request: &mut tiny_http::Request,
) -> std::result::Result<IntelligenceQueryRequest, QueryApiError> {
    let declared_length = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Length"))
        .and_then(|header| header.value.as_str().parse::<u64>().ok());
    if declared_length.is_some_and(|length| length > MAX_QUERY_BODY_BYTES) {
        return Err(QueryApiError::payload_too_large());
    }
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_QUERY_BODY_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|_| QueryApiError::bad_request("invalid_body", "could not read query body"))?;
    if body.len() as u64 > MAX_QUERY_BODY_BYTES {
        return Err(QueryApiError::payload_too_large());
    }
    parse_query_request(&body)
}

fn parse_query_request(
    body: &[u8],
) -> std::result::Result<IntelligenceQueryRequest, QueryApiError> {
    let query = serde_json::from_slice::<IntelligenceQueryRequest>(body).map_err(|error| {
        QueryApiError::bad_request("invalid_body", format!("invalid query body: {error}"))
    })?;
    validate_query(&query)?;
    Ok(query)
}

fn bounded_lens(lens: &Value, query: &IntelligenceQueryRequest) -> Value {
    if lens.get("availability").and_then(Value::as_str) == Some("unavailable") {
        return lens.clone();
    }
    let source_nodes = lens
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let source_edges = lens
        .get("edges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let terms = query_terms(&query.query);
    let matching: BTreeSet<String> = source_nodes
        .iter()
        .filter_map(|node| {
            let id = node_id(node)?;
            let searchable = serde_json::to_string(node).ok()?.to_ascii_lowercase();
            (terms.is_empty() || terms.iter().any(|term| searchable.contains(term)))
                .then(|| id.to_owned())
        })
        .collect();
    let mut selected = if query.root_ids.is_empty() {
        matching.clone()
    } else {
        query.root_ids.iter().cloned().collect()
    };
    if !query.root_ids.is_empty() {
        let mut outgoing: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for edge in &source_edges {
            if let Some((source, target)) = edge_endpoints(edge) {
                outgoing
                    .entry(source.to_owned())
                    .or_default()
                    .push(target.to_owned());
            }
        }
        let max_depth = query.bounds.max_depth.unwrap_or(0);
        let mut queue: VecDeque<(String, u32)> =
            query.root_ids.iter().cloned().map(|id| (id, 0)).collect();
        while let Some((id, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for target in outgoing.get(&id).into_iter().flatten() {
                if selected.insert(target.clone()) {
                    queue.push_back((target.clone(), depth + 1));
                }
            }
        }
        if !terms.is_empty() {
            selected.retain(|id| matching.contains(id) || query.root_ids.contains(id));
        }
    }
    let node_limit = query.bounds.max_nodes.unwrap_or(100);
    let edge_limit = query.bounds.max_edges.unwrap_or(200);
    let nodes: Vec<Value> = source_nodes
        .iter()
        .filter(|node| node_id(node).is_some_and(|id| selected.contains(id)))
        .take(node_limit)
        .cloned()
        .collect();
    let node_ids: BTreeSet<&str> = nodes.iter().filter_map(node_id).collect();
    let edges: Vec<Value> = source_edges
        .iter()
        .filter(|edge| {
            edge_endpoints(edge).is_some_and(|(source, target)| {
                node_ids.contains(source) && node_ids.contains(target)
            })
        })
        .take(edge_limit)
        .cloned()
        .collect();
    let mut result = lens.clone();
    if !result.is_object() {
        result = unavailable_lens("unknown");
    }
    let object = result.as_object_mut().expect("lens object");
    object.insert("nodes".to_owned(), Value::Array(nodes.clone()));
    object.insert("edges".to_owned(), Value::Array(edges.clone()));
    object.insert(
        "counts".to_owned(),
        json!({"nodes": nodes.len(), "edges": edges.len()}),
    );
    object.insert(
        "truncated".to_owned(),
        Value::Bool(source_nodes.len() > nodes.len() || source_edges.len() > edges.len()),
    );
    result
}

fn intelligence_query(
    workspace: &Path,
    query: &IntelligenceQueryRequest,
) -> std::result::Result<Value, QueryApiError> {
    let snapshot = graph_snapshot(workspace).map_err(|error| QueryApiError {
        status: StatusCode(409),
        code: "snapshot_unavailable",
        message: format!("canonical graph snapshot unavailable: {error:#}"),
    })?;
    let project_key = snapshot
        .get("project_key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if query
        .project_key
        .as_deref()
        .is_some_and(|requested| requested != project_key)
    {
        return Err(QueryApiError {
            status: StatusCode(404),
            code: "project_not_found",
            message: "project_key does not match the board's bound project".to_owned(),
        });
    }
    let intelligence = canonical_intelligence(&snapshot);
    let lenses = intelligence
        .get("lenses")
        .and_then(Value::as_object)
        .expect("canonical intelligence lenses");
    let requested = if query.lens_ids.is_empty() {
        inferred_lens_ids(&query.query)
    } else {
        query.lens_ids.clone()
    };
    let mut selected = serde_json::Map::new();
    for lens_id in requested {
        if let Some(lens) = lenses.get(&lens_id) {
            selected.insert(lens_id, bounded_lens(lens, query));
        }
    }
    Ok(json!({
        "schema": INTELLIGENCE_QUERY_RESPONSE_SCHEMA,
        "project_key": project_key,
        "query": {
            "schema": INTELLIGENCE_QUERY_SCHEMA,
            "query": query.query,
            "modality": query.modality.as_str(),
            "lens_ids": selected.keys().cloned().collect::<Vec<_>>(),
            "root_ids": query.root_ids,
            "bounds": {
                "max_depth": query.bounds.max_depth.unwrap_or(0),
                "max_nodes": query.bounds.max_nodes.unwrap_or(100),
                "max_edges": query.bounds.max_edges.unwrap_or(200),
            },
        },
        "intelligence": {
            "schema": INTELLIGENCE_SNAPSHOT_SCHEMA,
            "source": {"kind": "canonical_project", "path": ".fractal/project.fractal"},
            "lenses": selected,
        },
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
    use sha2::{Digest, Sha256};
    use std::sync::Mutex;

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

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "fractal-board-{}-{}-{}",
            label,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_minimal_project(workspace: &Path, title: &str) -> String {
        fs::create_dir_all(workspace.join(".fractal")).unwrap();
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [{
                "id": "n1",
                "title": "Task one",
                "instruction": "do it",
                "capability": "code.generate"
            }],
            "edges": []
        });
        let graph_hash = fractal_contracts::canonical_sha256(&graph).expect("hash graph");
        graph
            .as_object_mut()
            .unwrap()
            .insert("graph_hash".to_owned(), json!(graph_hash));
        crate::project_file::persist(workspace, &graph, title).unwrap();
        let project = crate::project_file::load(workspace).unwrap();
        project.graph_hash
    }

    fn write_projection_project(workspace: &Path) -> String {
        fs::create_dir_all(workspace.join(".fractal")).unwrap();
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [{
                "id": "n1",
                "title": "Repair compiler",
                "instruction": "repair the compiler regression",
                "capability": "code.generate",
                "executor": {"provider": "cursor-cli"},
                "route_candidates": ["cursor-cli", "codex"],
                "budget": {"timeout_ms": 120000},
                "efficiency": {"estimated_remaining_tokens": 2400},
                "policy_contract": {
                    "budgets": {"max_cost_usd": 5, "max_input_tokens": 12000},
                    "sandbox_profile": "workspace-write",
                    "provenance": "prd:canonical"
                }
            }],
            "edges": []
        });
        let graph_hash = fractal_contracts::canonical_sha256(&graph).expect("hash graph");
        graph
            .as_object_mut()
            .unwrap()
            .insert("graph_hash".to_owned(), json!(graph_hash));
        crate::project_file::persist(workspace, &graph, "Projection fixture").unwrap();
        crate::project_file::mutate_document(workspace, |project| {
            assert!(!project.extra.contains_key("intelligence"));
            project.execution = Some(
                serde_json::from_value(json!({
                    "schema": "fractal.execution_state.v1",
                    "phase": "executing",
                    "assignments": {
                        "n1": {
                            "agent_id": "agent-cursor-1",
                            "agent_label": "cursor-cli",
                            "state": "completed",
                            "checked_out_at": "2026-08-24T10:00:00Z",
                            "completed_at": "2026-08-24T10:02:00Z"
                        }
                    },
                    "updated_at": "2026-08-24T10:02:00Z"
                }))
                .expect("typed execution fixture"),
            );
            project.learning.nodes.insert(
                "n1".to_owned(),
                serde_json::from_value(json!({
                    "node_id": "n1",
                    "node_type": "task",
                    "objective": "Repair compiler",
                    "depends_on": [],
                    "finished_at": "2026-08-24T10:02:00Z",
                    "executor": {
                        "agent": "cursor-cli",
                        "model": "cursor-agent",
                        "version": "2026.08"
                    },
                    "attempt_count": 2,
                    "outcome": "unverified_success",
                    "failure_code": "tool_failure",
                    "verification": {
                        "type": "test_suite",
                        "passed": true,
                        "evidence_refs": ["sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
                    },
                    "artifacts_produced": ["sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],
                    "estimated_cost": 1.75,
                    "actual_cost": 1.25,
                    "notes": "Compiler repair completed; promotion is still pending."
                }))
                .expect("typed learning fixture"),
            );
            Ok(())
        })
        .unwrap();

        let mut failures = crate::failure_graph::FailureGraph::empty();
        let failure_id = crate::failure_graph::failure_id("n1", "tool_failure");
        failures.failures.insert(
            failure_id.clone(),
            crate::failure_graph::FailureRecord {
                id: failure_id,
                node_id: "n1".to_owned(),
                attempt: 1,
                failure_code: "tool_failure".to_owned(),
                outcome: "failed_execution".to_owned(),
                summary: "Compiler command failed before the retry.".to_owned(),
                ..Default::default()
            },
        );
        failures.lessons.insert(
            "lesson:inspect-generated-output".to_owned(),
            crate::failure_graph::LessonRecord {
                id: "lesson:inspect-generated-output".to_owned(),
                summary: "Inspect generated output before retrying the compiler.".to_owned(),
                capability: Some("code.generate".to_owned()),
                component: Some("compiler".to_owned()),
                ..Default::default()
            },
        );
        crate::failure_graph::normalize(&mut failures).unwrap();
        crate::project_file::replace_failure_graph(workspace, failures).unwrap();
        graph_hash
    }

    fn sha256_prefixed(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let mut out = String::with_capacity(71);
        out.push_str("sha256:");
        for byte in digest {
            out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
            out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap());
        }
        out
    }

    fn write_inventory_fixture(
        path: &Path,
        records: Vec<crate::master_graph::InventoryRecord>,
    ) -> crate::master_graph::RepositoryInventory {
        let mut records = records;
        records.sort_by(|a, b| a.canonical_workspace.cmp(&b.canonical_workspace));
        let inventory = crate::master_graph::RepositoryInventory {
            schema: "fractal.repository_inventory.v1".to_owned(),
            inventory_hash: sha256_prefixed(b"board-test-inventory"),
            records,
            extra: Default::default(),
        };
        fs::write(path, serde_json::to_vec_pretty(&inventory).unwrap()).unwrap();
        inventory
    }

    fn record(
        workspace: &Path,
        label: &str,
        number: u64,
        exists: bool,
    ) -> crate::master_graph::InventoryRecord {
        let canonical = if exists {
            workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.to_path_buf())
                .to_string_lossy()
                .into_owned()
        } else {
            workspace.to_string_lossy().into_owned()
        };
        crate::master_graph::InventoryRecord {
            canonical_workspace: canonical,
            exists,
            labels: vec![label.to_owned()],
            registry_numbers: vec![number],
            unavailable_reason: if exists {
                None
            } else {
                Some("workspace_path_does_not_exist".to_owned())
            },
            git: None,
            project_fractal: Some(crate::master_graph::InventoryProjectFractal {
                available: exists,
                relative_path: Some(".fractal/project.fractal".to_owned()),
                size_bytes: None,
                unavailable_reason: (!exists).then(|| "missing".to_owned()),
            }),
            extra: Default::default(),
        }
    }

    fn master_state(
        root: &Path,
        inventory: crate::master_graph::RepositoryInventory,
    ) -> MasterBoardState {
        MasterBoardState {
            inventory_path: root.join("inventory.json"),
            inventory,
            viewer_dir: PathBuf::new(),
            token: "test-token".to_owned(),
            bound_workspace: root.to_path_buf(),
        }
    }

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
    fn embedded_master_graph_assets_are_available() {
        let js = String::from_utf8_lossy(embedded_asset("master-graph.js"));
        let css = String::from_utf8_lossy(embedded_asset("master-graph.css"));
        assert!(js.contains("master-graph"));
        assert!(!js.is_empty());
        assert!(!css.is_empty());
    }

    #[test]
    fn embedded_graph_ui_is_the_provenance_pinned_society_bundle() {
        let js = embedded_asset("fractal-graph-ui.js");
        let css = embedded_asset("fractal-graph-ui.css");
        let manifest: Value =
            serde_json::from_slice(embedded_asset("fractal-graph-ui.manifest.json")).unwrap();
        assert!(String::from_utf8_lossy(js).contains(GRAPH_UI_BUNDLE_ID));
        assert_eq!(manifest["schema"], "fractal.graph_ui_bundle.v1");
        assert_eq!(manifest["renderer"], GRAPH_UI_BUNDLE_ID);
        assert_eq!(
            manifest["source_repository"],
            "fractalsociety/fractalsociety-website"
        );
        assert_ne!(manifest["source_commit"], "pending");
        assert_eq!(
            manifest["asset_hashes"]["fractal-graph-ui.js"],
            sha256_prefixed(js)
        );
        assert_eq!(
            manifest["asset_hashes"]["fractal-graph-ui.css"],
            sha256_prefixed(css)
        );
    }

    #[test]
    fn master_board_launch_url_selects_master_mode() {
        assert_eq!(master_board_url(8093), "http://127.0.0.1:8093/?mode=master");
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
        let _guard = test_lock();
        let root = temp_root("identity");
        let workspace = root.join("project");
        write_minimal_project(&workspace, "Identity project");
        let identity = board_identity(&workspace).unwrap();
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
        assert!(identity_matches_value(&identity, &workspace, graph_hash));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn board_port_release_waits_for_socket_not_a_fixed_sleep() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!wait_for_port_release(port, Duration::from_millis(30)));
        drop(listener);
        assert!(wait_for_port_release(port, Duration::from_millis(100)));
    }

    #[test]
    fn master_projects_route_returns_board_projects_schema() {
        let _guard = test_lock();
        let root = temp_root("projects");
        let project_a = root.join("alpha-app");
        write_minimal_project(&project_a, "Alpha");
        let missing = root.join("missing-app");
        let inventory = write_inventory_fixture(
            &root.join("inventory.json"),
            vec![
                record(&project_a, "alpha-app", 1, true),
                record(&missing, "missing-app", 2, false),
            ],
        );
        let state = master_state(&root, inventory);
        let reply = master_projects_reply(&state);
        assert_eq!(reply.status.0, 200);
        assert_eq!(
            reply.body.get("schema").and_then(Value::as_str),
            Some(BOARD_PROJECTS_SCHEMA)
        );
        let projects = reply
            .body
            .get("projects")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(projects.len(), 1);
        assert!(!reply
            .body
            .get("unavailable")
            .and_then(Value::as_array)
            .unwrap()
            .is_empty());
        assert!(reply.etag.as_deref().unwrap().contains("sha256:"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn master_graph_route_returns_view_schema_and_etag() {
        let _guard = test_lock();
        let root = temp_root("master-graph");
        let project_a = root.join("alpha-app");
        write_minimal_project(&project_a, "Alpha");
        let inventory = write_inventory_fixture(
            &root.join("inventory.json"),
            vec![record(&project_a, "alpha-app", 1, true)],
        );
        let state = master_state(&root, inventory);
        let reply = master_graph_reply(&state);
        assert_eq!(reply.status.0, 200);
        assert_eq!(
            reply.body.get("schema").and_then(Value::as_str),
            Some("fractal.master_graph_view.v1")
        );
        let view_hash = reply
            .body
            .get("view_hash")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned();
        let expected_etag = format!("\"{view_hash}\"");
        assert_eq!(reply.etag.as_deref(), Some(expected_etag.as_str()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_graph_serves_inventory_member_and_rejects_traversal() {
        let _guard = test_lock();
        let root = temp_root("project-graph");
        let project_a = root.join("alpha-app");
        write_minimal_project(&project_a, "Alpha");
        let inventory = write_inventory_fixture(
            &root.join("inventory.json"),
            vec![record(&project_a, "alpha-app", 1, true)],
        );
        let state = master_state(&root, inventory);
        let key = crate::master_graph::derive_project_key(
            &project_a.canonicalize().unwrap().to_string_lossy(),
        );

        let ok = project_graph_reply(&state, Some(&key));
        assert_eq!(ok.status.0, 200);
        assert_eq!(
            ok.body.get("schema").and_then(Value::as_str),
            Some("fractal.execution_graph_view.v1")
        );
        assert!(ok.etag.as_deref().unwrap().starts_with("\"sha256:"));

        let traversal = project_graph_reply(&state, Some("../etc/passwd"));
        assert_eq!(traversal.status.0, 404);
        assert_eq!(
            traversal.body.get("code").and_then(Value::as_str),
            Some("not_in_inventory")
        );

        let encoded = project_graph_reply(&state, Some("%2e%2e%2fetc%2fpasswd"));
        assert_eq!(encoded.status.0, 404);

        let unknown = project_graph_reply(&state, Some("not-a-real-project-deadbeef0001"));
        assert_eq!(unknown.status.0, 404);
        assert_eq!(
            unknown.body.get("code").and_then(Value::as_str),
            Some("not_in_inventory")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unavailable_project_returns_bounded_diagnostics() {
        let _guard = test_lock();
        let root = temp_root("unavailable");
        let missing = root.join("gone-app");
        let inventory = write_inventory_fixture(
            &root.join("inventory.json"),
            vec![record(&missing, "gone-app", 9, false)],
        );
        let state = master_state(&root, inventory);
        let key = crate::master_graph::derive_project_key(&missing.to_string_lossy());
        let reply = project_graph_reply(&state, Some(&key));
        assert_eq!(reply.status.0, 409);
        assert_eq!(
            reply.body.get("code").and_then(Value::as_str),
            Some("unavailable_project")
        );
        assert!(reply
            .body
            .get("diagnostics")
            .and_then(Value::as_array)
            .map(|items| !items.is_empty())
            .unwrap_or(false));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mutation_methods_on_master_endpoints_are_read_only() {
        assert!(is_safe_project_key("alpha-app-bbbfd315b970"));
        assert!(!is_safe_project_key("../etc/passwd"));
        assert!(!is_safe_project_key("alpha/app"));
        assert!(!is_safe_project_key("alpha\\app"));
        assert_eq!(percent_decode("%2e%2e%2fetc%2fpasswd"), "../etc/passwd");
        // 405 path is enforced in respond_master before handlers run.
        let methods = [Method::Post, Method::Put, Method::Patch, Method::Delete];
        for method in methods {
            assert_ne!(method, Method::Get);
        }
        assert_eq!(
            query_param("/api/project-graph?project_key=abc-123", "project_key").as_deref(),
            Some("abc-123")
        );
    }

    #[test]
    fn etag_matching_is_cache_safe() {
        assert!(etag_matches("\"sha256:abc\"", "\"sha256:abc\""));
        assert!(etag_matches(
            "\"sha256:abc\"",
            "W/\"sha256:abc\", \"sha256:abc\""
        ));
        assert!(!etag_matches("\"sha256:abc\"", "\"sha256:def\""));
    }

    #[test]
    fn existing_individual_graph_view_schema_is_unchanged() {
        let _guard = test_lock();
        let root = temp_root("individual-regression");
        let workspace = root.join("solo");
        write_minimal_project(&workspace, "Solo");
        let view = project_view(&workspace, "token-regression").unwrap();
        assert_eq!(
            view.get("schema").and_then(Value::as_str),
            Some("fractal.execution_graph_view.v1")
        );
        assert_eq!(
            view.get("run_control")
                .and_then(|value| value.get("token"))
                .and_then(Value::as_str),
            Some("token-regression")
        );
        assert!(view.get("totals").is_some());
        assert!(view.get("groups").and_then(Value::as_array).is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn canonical_graph_snapshot_exposes_the_shared_typed_contract() {
        let _guard = test_lock();
        let root = temp_root("canonical-snapshot");
        let workspace = root.join("solo");
        write_minimal_project(&workspace, "Solo");
        let snapshot = graph_snapshot(&workspace).unwrap();
        assert_eq!(snapshot["schema"], GRAPH_SNAPSHOT_SCHEMA);
        assert_eq!(snapshot["bundle"], GRAPH_UI_BUNDLE_ID);
        assert!(snapshot["graph"]["nodes"].is_array());
        assert!(snapshot.get("execution").is_some());
        assert!(snapshot.get("learning").is_some());
        assert!(snapshot.get("efficiency").is_some());
        assert!(snapshot.get("intelligence").is_some());

        let intelligence = canonical_intelligence(&snapshot);
        assert_eq!(intelligence["schema"], INTELLIGENCE_SNAPSHOT_SCHEMA);
        let lenses = intelligence["lenses"].as_object().unwrap();
        assert_eq!(lenses.len(), LENS_IDS.len());
        assert_eq!(lenses["overview"]["availability"], "available");
        assert_eq!(lenses["execution"]["availability"], "available");
        assert_eq!(lenses["resource_economic"]["availability"], "unavailable");
        assert!(lenses["resource_economic"].get("counts").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn typed_intelligence_query_is_bounded_and_fail_closed() {
        let valid = br#"{
          "schema":"fractal.intelligence.query.v1",
          "query":"show execution tasks",
          "modality":"text",
          "lens_ids":["execution"],
          "bounds":{"max_depth":2,"max_nodes":20,"max_edges":40}
        }"#;
        let parsed = parse_query_request(valid).unwrap();
        assert_eq!(parsed.lens_ids, vec!["execution"]);

        let unknown_field = br#"{
          "schema":"fractal.intelligence.query.v1",
          "query":"show execution",
          "modality":"text",
          "ambient_authority":true
        }"#;
        assert_eq!(
            parse_query_request(unknown_field).unwrap_err().code,
            "invalid_body"
        );

        let unknown_lens = br#"{
          "schema":"fractal.intelligence.query.v1",
          "query":"show credentials",
          "modality":"text",
          "lens_ids":["credential_store"]
        }"#;
        assert_eq!(
            parse_query_request(unknown_lens).unwrap_err().code,
            "unknown_lens"
        );

        let excessive = format!(
            r#"{{"schema":"{INTELLIGENCE_QUERY_SCHEMA}","query":"show tasks","modality":"voice","bounds":{{"max_nodes":{}}}}}"#,
            MAX_QUERY_NODES + 1
        );
        assert_eq!(
            parse_query_request(excessive.as_bytes()).unwrap_err().code,
            "bounds_exceeded"
        );
    }

    #[test]
    fn intelligence_query_preserves_unavailable_and_filters_canonical_nodes() {
        let _guard = test_lock();
        let root = temp_root("canonical-query");
        let workspace = root.join("solo");
        write_minimal_project(&workspace, "Solo");

        let execution = parse_query_request(
            br#"{
              "schema":"fractal.intelligence.query.v1",
              "query":"Task one",
              "modality":"text",
              "lens_ids":["execution"],
              "bounds":{"max_nodes":1,"max_edges":1}
            }"#,
        )
        .unwrap();
        let response = intelligence_query(&workspace, &execution).unwrap();
        assert_eq!(response["schema"], INTELLIGENCE_QUERY_RESPONSE_SCHEMA);
        assert_eq!(
            response["intelligence"]["lenses"]["execution"]["counts"]["nodes"],
            1
        );

        let economics = parse_query_request(
            br#"{
              "schema":"fractal.intelligence.query.v1",
              "query":"show economics",
              "modality":"voice"
            }"#,
        )
        .unwrap();
        let response = intelligence_query(&workspace, &economics).unwrap();
        let lens = &response["intelligence"]["lenses"]["resource_economic"];
        assert_eq!(lens["availability"], "unavailable");
        assert_eq!(lens["nodes"].as_array().unwrap().len(), 0);
        assert!(lens.get("counts").is_none());
        let encoded = serde_json::to_string(lens).unwrap();
        assert!(!encoded.contains("realized_tokens_saved"));
        assert!(!encoded.contains("observed_cost"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn intelligence_query_derives_all_lenses_without_precomputed_intelligence() {
        let _guard = test_lock();
        let root = temp_root("derived-intelligence");
        let workspace = root.join("project");
        write_projection_project(&workspace);
        let project = crate::project_file::load(&workspace).unwrap();
        assert!(!project.extra.contains_key("intelligence"));

        let snapshot = graph_snapshot(&workspace).unwrap();
        let intelligence = &snapshot["intelligence"];
        assert_eq!(
            intelligence["source"]["kind"],
            "canonical_project_projection"
        );
        for lens_id in LENS_IDS {
            assert_eq!(
                intelligence["lenses"][lens_id]["availability"], "available",
                "{lens_id} should derive from canonical project fields"
            );
        }

        let economics =
            serde_json::to_string(&intelligence["lenses"]["resource_economic"]).unwrap();
        assert!(economics.contains("declared_estimate"));
        assert!(economics.contains("estimated_cost"));
        assert!(economics.contains("observed_cost"));
        assert!(!economics.contains("realized_tokens_saved"));

        let memory = serde_json::to_string(&intelligence["lenses"]["memory_knowledge"]).unwrap();
        assert!(memory.contains("unverified_success"));
        assert!(memory.contains("without upgrading its verification status"));

        let traces = serde_json::to_string(&intelligence["lenses"]["trace_evidence"]).unwrap();
        assert!(traces.contains("assignment_trace"));
        assert!(traces.contains("sha256:aaaaaaaa"));

        let failures = serde_json::to_string(&intelligence["lenses"]["failure_learning"]).unwrap();
        assert!(failures.contains("\"type\":\"failure\""));
        assert!(failures.contains("\"type\":\"lesson\""));

        let agents =
            serde_json::to_string(&intelligence["lenses"]["agent_model_tool_harness"]).unwrap();
        assert!(agents.contains("cursor-cli"));
        assert!(agents.contains("cursor-agent"));
        assert!(agents.contains("harness_binding"));

        for (lens_id, query_text) in [
            ("overview", "show overview"),
            ("execution", "show execution"),
            ("resource_economic", "show economics"),
            ("memory_knowledge", "show memory"),
            ("trace_evidence", "show traces"),
            ("failure_learning", "show failures and lessons"),
            ("agent_model_tool_harness", "show agents and tools"),
        ] {
            let request = parse_query_request(
                serde_json::to_string(&json!({
                    "schema": INTELLIGENCE_QUERY_SCHEMA,
                    "query": query_text,
                    "modality": "text",
                    "lens_ids": [lens_id]
                }))
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
            let response = intelligence_query(&workspace, &request).unwrap();
            let lens = &response["intelligence"]["lenses"][lens_id];
            assert_eq!(lens["availability"], "available");
            assert!(
                lens["counts"]["nodes"].as_u64().unwrap_or(0) > 0,
                "{lens_id} query should return derived records"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn asset_allowlist_includes_master_modules_and_rejects_traversal_paths() {
        assert!(!embedded_asset("master-graph.js").is_empty());
        assert!(!embedded_asset("master-graph.css").is_empty());
        assert!(!embedded_asset("fractal-graph-ui.js").is_empty());
        assert!(!embedded_asset("fractal-graph-ui.css").is_empty());
        assert!(!embedded_asset("fractal-graph-ui.manifest.json").is_empty());
        assert!(embedded_asset("../secrets.txt").is_empty());
        assert!(embedded_asset("/etc/passwd").is_empty());
    }

    #[test]
    fn failure_graph_view_is_redacted_bounded_and_etagged() {
        let _guard = test_lock();
        let root = temp_root("failure-view");
        let workspace = root.join("solo");
        write_minimal_project(&workspace, "Solo");
        let mut graph = crate::failure_graph::FailureGraph::empty();
        let failure_id = crate::failure_graph::failure_id("n1", "tool_failure");
        graph.failures.insert(
            failure_id.clone(),
            crate::failure_graph::FailureRecord {
                id: failure_id.clone(),
                node_id: "n1".to_owned(),
                attempt: 1,
                failure_code: "tool_failure".to_owned(),
                outcome: "failed_execution".to_owned(),
                summary: "compiler failed".to_owned(),
                source_ref: Some("src/main.rs#L1".to_owned()),
                evidence: vec![crate::failure_graph::EvidenceRef {
                    sha256: Some(
                        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .to_owned(),
                    ),
                    path: Some("logs/raw.log".to_owned()),
                    ..Default::default()
                }],
                observations: vec![crate::failure_graph::FailureObservation {
                    attempt: 1,
                    outcome: "failed_execution".to_owned(),
                    summary: "compiler failed".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        crate::failure_graph::normalize(&mut graph).unwrap();
        crate::project_file::replace_failure_graph(&workspace, graph).unwrap();
        let reply = failure_graph_reply(&workspace);
        assert_eq!(reply.status.0, 200);
        assert_eq!(
            reply.body.get("schema").and_then(Value::as_str),
            Some(FAILURE_GRAPH_VIEW_SCHEMA)
        );
        assert_eq!(reply.body["summary"]["unresolved"], 1);
        assert!(reply.etag.as_deref().unwrap().contains("sha256:"));
        let body = serde_json::to_string(&reply.body).unwrap();
        assert!(!body.contains("logs/raw.log"));
        assert!(body.contains("sha256:aaaaaaaa"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_or_unsupported_failure_envelopes_degrade_with_safe_diagnostics() {
        let _guard = test_lock();
        let root = temp_root("failure-invalid");
        let workspace = root.join("solo");
        write_minimal_project(&workspace, "Solo");
        let path = crate::project_file::path(&workspace);
        let mut raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        raw["failure_graph"] =
            json!({"schema": "fractal.failure_graph.v99", "raw_log": "/tmp/secret.log"});
        fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        let reply = failure_graph_reply(&workspace);
        assert_eq!(reply.status.0, 200);
        assert_eq!(reply.body["summary"]["total"], 0);
        assert_eq!(
            reply.body["diagnostics"][0]["code"],
            "unsupported_failure_graph_schema"
        );
        let body = serde_json::to_string(&reply.body).unwrap();
        assert!(!body.contains("/tmp/secret.log"));
        let _ = fs::remove_dir_all(root);
    }
}
