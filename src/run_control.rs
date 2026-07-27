//! Durable local run registry and safe cancellation for CLI and voice builds.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::{StatusArgs, StopArgs};

const RUN_ID_ENV: &str = "FRACTAL_ACTIVE_RUN_ID";
static REGISTRY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ActiveRun {
    schema: String,
    run_id: String,
    pid: u32,
    workspace: String,
    project: String,
    request: String,
    status: String,
    started_at_ms: u64,
    updated_at_ms: u64,
    graph_hash: Option<String>,
    board_url: Option<String>,
    worker_groups: Vec<u32>,
    active_nodes: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct ProjectRunState<'a> {
    schema: &'static str,
    run_id: &'a str,
    status: &'a str,
    pid: u32,
    graph_hash: &'a Option<String>,
    updated_at_ms: u64,
}

pub(crate) struct RunGuard {
    run_id: Option<String>,
}

impl RunGuard {
    pub(crate) fn start_or_join(workspace: &Path, request: &str, _port: u16) -> Result<Self> {
        if std::env::var_os(RUN_ID_ENV).is_some() {
            return Ok(Self { run_id: None });
        }
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let now = now_ms();
        let run_id = format!("{now}-{}", std::process::id());
        let run = ActiveRun {
            schema: "fractal.active_run.v1".to_owned(),
            run_id: run_id.clone(),
            pid: std::process::id(),
            project: workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_owned()),
            workspace: workspace.to_string_lossy().into_owned(),
            request: request.chars().take(240).collect(),
            status: "running".to_owned(),
            started_at_ms: now,
            updated_at_ms: now,
            graph_hash: None,
            board_url: None,
            worker_groups: Vec::new(),
            active_nodes: BTreeMap::new(),
        };
        write_run(&run)?;
        write_project_state(&run).ok();
        std::env::set_var(RUN_ID_ENV, &run_id);
        start_hosted_control_monitor(run_id.clone());
        Ok(Self {
            run_id: Some(run_id),
        })
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let Some(run_id) = self.run_id.take() else {
            return;
        };
        if let Ok(Some(mut run)) = read_run(&run_id) {
            if run.status == "running" {
                run.status = "completed".to_owned();
                run.updated_at_ms = now_ms();
                write_project_state(&run).ok();
                fs::remove_file(run_path(&run_id)).ok();
            }
        }
        std::env::remove_var(RUN_ID_ENV);
    }
}

pub(crate) struct WorkerGuard {
    pid: u32,
    registered: bool,
}

impl WorkerGuard {
    pub(crate) fn register(pid: u32) -> Self {
        let registered = mutate_current(|run| {
            if !run.worker_groups.contains(&pid) {
                run.worker_groups.push(pid);
            }
        })
        .is_ok();
        Self { pid, registered }
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if self.registered {
            let pid = self.pid;
            mutate_current(|run| run.worker_groups.retain(|group| *group != pid)).ok();
        }
    }
}

pub(crate) fn set_graph(graph_hash: &str, board_url: &str) {
    mutate_current(|run| {
        run.graph_hash = Some(graph_hash.to_owned());
        run.board_url = Some(board_url.to_owned());
    })
    .ok();
}

pub(crate) fn node_transition(board: Option<&str>, node: &str, action: &str, agent: &str) {
    mutate_current(|run| {
        if let Some(board) = board {
            run.board_url = Some(board.to_owned());
        }
        match action {
            "checkout" => {
                run.active_nodes.insert(node.to_owned(), agent.to_owned());
            }
            "complete" | "release" => {
                run.active_nodes.remove(node);
            }
            _ => {}
        }
    })
    .ok();
}

pub(crate) fn terminate_worker(pid: u32) {
    signal_group(pid, libc::SIGTERM);
    thread::sleep(Duration::from_millis(100));
    if process_alive(pid) {
        signal_group(pid, libc::SIGKILL);
    }
}

pub(crate) fn stop(args: &StopArgs) -> Result<()> {
    let mut runs = live_runs()?;
    if runs.is_empty() {
        println!("No Fractal builds are running.");
        return Ok(());
    }
    if !args.all {
        runs = select_runs(runs, args.project.as_deref())?;
    }
    for run in &runs {
        halt(run, None)?;
        println!("Stopped {} ({})", run.project, run.workspace);
    }
    println!(
        "{} build{} halted; completed graph waves remain resumable.",
        runs.len(),
        if runs.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

pub(crate) fn status(args: &StatusArgs) -> Result<()> {
    let runs = live_runs()?;
    if runs.is_empty() {
        println!("No Fractal builds are running.");
        return Ok(());
    }
    if args.running {
        println!("Running Fractal builds:");
    } else {
        println!("Fractal build status:");
    }
    for run in runs {
        let graph = run.graph_hash.as_deref().unwrap_or("planning");
        println!("  {:<28} pid {:<7} graph {}", run.project, run.pid, graph);
        println!("    {}", run.workspace);
    }
    Ok(())
}

pub(crate) fn workspace_is_running(workspace: &Path) -> bool {
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    live_runs().is_ok_and(|runs| {
        runs.iter()
            .any(|run| Path::new(&run.workspace) == workspace)
    })
}

/// Return the trusted workspace registered for the current coordinator. This is
/// used to provision narrow loopback graph controls without accepting a path
/// from the browser.
pub(crate) fn current_workspace() -> Option<PathBuf> {
    let run_id = std::env::var_os(RUN_ID_ENV)?;
    read_run(&run_id.to_string_lossy())
        .ok()
        .flatten()
        .filter(|run| run.status == "running")
        .map(|run| PathBuf::from(run.workspace))
}

fn start_hosted_control_monitor(run_id: String) {
    thread::spawn(move || {
        let mut consecutive_errors = 0_u32;
        loop {
            thread::sleep(Duration::from_secs(3));
            let Ok(Some(run)) = read_run(&run_id) else {
                break;
            };
            if run.status != "running" {
                break;
            }
            match crate::project_sync::poll_control_command(Path::new(&run.workspace)) {
                Ok(Some(crate::project_sync::HostedControl::Pause(command_id))) => {
                    if let Err(error) = halt(&run, Some(&command_id)) {
                        eprintln!("  hosted pause note: {error:#}");
                    }
                    break;
                }
                Ok(Some(crate::project_sync::HostedControl::AmendmentQueued)) => {
                    consecutive_errors = 0;
                }
                Ok(None) => consecutive_errors = 0,
                Err(error) => {
                    consecutive_errors = consecutive_errors.saturating_add(1);
                    if consecutive_errors == 1 || consecutive_errors.is_multiple_of(20) {
                        eprintln!("  hosted pause poll note: {error:#}");
                    }
                }
            }
        }
    });
}

/// Queue a spoken or typed amendment against the one active build. The lead
/// consumes it at the next dependency-safe boundary between execution waves.
pub(crate) fn queue_active_amendment(task_ref: &str, instruction: &str) -> Result<()> {
    let runs: Vec<_> = live_runs()?
        .into_iter()
        .filter(|run| run.status == "running" && process_alive(run.pid))
        .collect();
    if runs.is_empty() {
        bail!("no Fractal build is currently running");
    }
    if runs.len() > 1 {
        bail!("more than one Fractal build is running; add the branch from its project graph");
    }
    let command_id = format!("local-{}-{}", now_ms(), std::process::id());
    crate::amendments::queue(
        Path::new(&runs[0].workspace),
        command_id,
        task_ref,
        instruction,
        "voice",
    )?;
    println!(
        "Accepted: task {task_ref} will receive a new planner branch between execution waves."
    );
    Ok(())
}

fn halt(original: &ActiveRun, hosted_command_id: Option<&str>) -> Result<()> {
    let mut run = original.clone();
    run.status = "halted".to_owned();
    run.updated_at_ms = now_ms();
    write_run(&run)?;
    write_project_state(&run).ok();

    if let Some(board) = &run.board_url {
        let http = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(500))
            .build();
        for (node, agent) in &run.active_nodes {
            let url = format!("{}/api/tasks/{}/release", board.trim_end_matches('/'), node);
            let body = serde_json::json!({ "agent_id": agent, "agent_label": "Fractal · Halted" })
                .to_string();
            let _ = http
                .post(&url)
                .set("Content-Type", "application/json")
                .send_string(&body);
        }
    }
    let workspace = Path::new(&run.workspace);
    for (node, agent) in &run.active_nodes {
        crate::project_file::transition(workspace, node, "release", agent, agent).ok();
    }
    crate::project_file::set_execution_phase(workspace, "halted").ok();

    // Stop the coordinator first so a terminated planner cannot be mistaken for
    // an ordinary failure and trigger fallback planning while cancellation is
    // already in progress. Worker groups are independently terminated next.
    signal_process(run.pid, libc::SIGTERM);
    for group in &run.worker_groups {
        signal_group(*group, libc::SIGTERM);
    }
    thread::sleep(Duration::from_millis(250));
    for group in &run.worker_groups {
        signal_group(*group, libc::SIGKILL);
    }
    if process_alive(run.pid) && is_fractal_process(run.pid) {
        signal_process(run.pid, libc::SIGKILL);
    }
    if let Some(command_id) = hosted_command_id {
        if let Err(error) = crate::project_sync::mark_pause_agents_stopped(workspace, command_id) {
            eprintln!("  hosted pause progress note: {error:#}");
        }
    }
    if let Err(error) = crate::project_sync::sync_runtime_halt_now(workspace) {
        eprintln!("  halted graph sync note: {error:#}");
    }
    Ok(())
}

fn select_runs(runs: Vec<ActiveRun>, project: Option<&str>) -> Result<Vec<ActiveRun>> {
    if let Some(project) = project {
        let needle = project.trim().to_ascii_lowercase();
        let needle_key = project_key(&needle);
        let selected: Vec<_> = runs
            .into_iter()
            .filter(|run| {
                run.project.to_ascii_lowercase() == needle
                    || run.workspace.to_ascii_lowercase() == needle
                    || project_key(&run.project) == needle_key
                    || Path::new(&run.workspace)
                        .file_name()
                        .is_some_and(|name| project_key(&name.to_string_lossy()) == needle_key)
            })
            .collect();
        if selected.is_empty() {
            bail!("no running Fractal project matches {project:?}");
        }
        if selected.len() > 1 {
            bail!(
                "more than one running project matches {project:?}; use its absolute workspace path"
            );
        }
        return Ok(selected);
    }

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    if let Some(run) = runs.iter().find(|run| {
        cwd.as_ref()
            .is_some_and(|cwd| Path::new(&run.workspace) == cwd)
    }) {
        return Ok(vec![run.clone()]);
    }
    if let Some(active) = active_voice_project() {
        if let Some(run) = runs.iter().find(|run| Path::new(&run.workspace) == active) {
            return Ok(vec![run.clone()]);
        }
    }
    if runs.len() == 1 {
        return Ok(runs);
    }
    bail!("multiple builds are running; use `fractal stop --project NAME` or `fractal stop --all`")
}

fn project_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn active_voice_project() -> Option<PathBuf> {
    let home = fractal_home();
    fs::read_to_string(home.join("voice-active-project"))
        .ok()
        .map(|value| PathBuf::from(value.trim()))
}

fn live_runs() -> Result<Vec<ActiveRun>> {
    let mut runs = Vec::new();
    fs::create_dir_all(runs_dir())?;
    for entry in fs::read_dir(runs_dir())? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        let Ok(run) = serde_json::from_slice::<ActiveRun>(&bytes) else {
            continue;
        };
        if run.status == "running" && process_alive(run.pid) && is_fractal_process(run.pid) {
            runs.push(run);
        } else {
            fs::remove_file(entry.path()).ok();
        }
    }
    runs.sort_by_key(|run| run.started_at_ms);
    Ok(runs)
}

fn mutate_current(mutator: impl FnOnce(&mut ActiveRun)) -> Result<()> {
    let _guard = REGISTRY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("run registry lock");
    let Some(run_id) = std::env::var_os(RUN_ID_ENV) else {
        return Ok(());
    };
    let run_id = run_id.to_string_lossy();
    let Some(mut run) = read_run(&run_id)? else {
        return Ok(());
    };
    mutator(&mut run);
    run.updated_at_ms = now_ms();
    write_run(&run)?;
    write_project_state(&run).ok();
    Ok(())
}

fn read_run(run_id: &str) -> Result<Option<ActiveRun>> {
    match fs::read(run_path(run_id)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_run(run: &ActiveRun) -> Result<()> {
    fs::create_dir_all(runs_dir())?;
    atomic_write(&run_path(&run.run_id), &serde_json::to_vec_pretty(run)?)
}

fn write_project_state(run: &ActiveRun) -> Result<()> {
    let directory = Path::new(&run.workspace).join(".fractal");
    fs::create_dir_all(&directory)?;
    let state = ProjectRunState {
        schema: "fractal.run_state.v1",
        run_id: &run.run_id,
        status: &run.status,
        pid: run.pid,
        graph_hash: &run.graph_hash,
        updated_at_ms: run.updated_at_ms,
    };
    atomic_write(
        &directory.join("run-state.json"),
        &serde_json::to_vec_pretty(&state)?,
    )
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("tmp-{}-{}", std::process::id(), now_ms()));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}

fn runs_dir() -> PathBuf {
    fractal_home().join("active-runs")
}

fn run_path(run_id: &str) -> PathBuf {
    runs_dir().join(format!("{run_id}.json"))
}

fn fractal_home() -> PathBuf {
    std::env::var_os("FRACTAL_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".fractal")))
        .unwrap_or_else(|| PathBuf::from(".fractal"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn process_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn signal_process(pid: u32, signal: i32) {
    if pid != std::process::id() {
        unsafe {
            libc::kill(pid as libc::pid_t, signal);
        }
    }
}

fn signal_group(group: u32, signal: i32) {
    unsafe {
        libc::kill(-(group as libc::pid_t), signal);
    }
}

fn is_fractal_process(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).to_ascii_lowercase())
        .is_some_and(|command| {
            Path::new(command.trim())
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("fractal"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_selection_is_exact_and_case_insensitive() {
        let run = ActiveRun {
            schema: "fractal.active_run.v1".to_owned(),
            run_id: "run".to_owned(),
            pid: std::process::id(),
            workspace: "/tmp/My-App".to_owned(),
            project: "My-App".to_owned(),
            request: "build".to_owned(),
            status: "running".to_owned(),
            started_at_ms: 1,
            updated_at_ms: 1,
            graph_hash: None,
            board_url: None,
            worker_groups: Vec::new(),
            active_nodes: BTreeMap::new(),
        };
        assert_eq!(
            select_runs(vec![run], Some("my-app")).unwrap()[0].project,
            "My-App"
        );
        let run = select_runs(
            vec![ActiveRun {
                schema: "fractal.active_run.v1".to_owned(),
                run_id: "run".to_owned(),
                pid: std::process::id(),
                workspace: "/tmp/expense-tracker".to_owned(),
                project: "expense-tracker".to_owned(),
                request: "build".to_owned(),
                status: "running".to_owned(),
                started_at_ms: 1,
                updated_at_ms: 1,
                graph_hash: None,
                board_url: None,
                worker_groups: Vec::new(),
                active_nodes: BTreeMap::new(),
            }],
            Some("expense tracker"),
        )
        .unwrap();
        assert_eq!(run[0].project, "expense-tracker");
    }
}
