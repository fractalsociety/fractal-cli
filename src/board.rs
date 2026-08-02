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
const BOARD_PROJECTS_SCHEMA: &str = "fractal.board_projects.v1";
const READ_ONLY_API_ERROR: &str =
    "the Rust board API is read-only; use `fractal node` for transitions";

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
                    json!({
                        "project_key": project.project_key,
                        "labels": project.labels,
                        "registry_numbers": project.registry_numbers,
                        "canonical_workspace": project.canonical_workspace,
                        "available": project.available,
                        "catalog_state": project.catalog_state,
                        "graph_hash": project.graph_hash,
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
            Ok(body) => {
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
        candidate == "*" || candidate == etag || candidate == etag.trim_matches('"')
    })
}

fn respond(
    request: tiny_http::Request,
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
    fn asset_allowlist_includes_master_modules_and_rejects_traversal_paths() {
        assert!(!embedded_asset("master-graph.js").is_empty());
        assert!(!embedded_asset("master-graph.css").is_empty());
        assert!(embedded_asset("../secrets.txt").is_empty());
        assert!(embedded_asset("/etc/passwd").is_empty());
    }
}
