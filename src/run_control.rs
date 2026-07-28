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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProjectRunState {
    schema: String,
    run_id: String,
    status: String,
    pid: u32,
    graph_hash: Option<String>,
    updated_at_ms: u64,
}

pub(crate) struct RunGuard {
    run_id: Option<String>,
    workspace: Option<PathBuf>,
}

impl RunGuard {
    pub(crate) fn start_or_join(workspace: &Path, request: &str, _port: u16) -> Result<Self> {
        if std::env::var_os(RUN_ID_ENV).is_some() {
            return Ok(Self {
                run_id: None,
                workspace: None,
            });
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
            workspace: Some(workspace),
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
                // Dropping the coordinator is not proof that the graph finished.
                // The desktop app may quit, an output pipe may close, or the CLI
                // may unwind after an execution error. Only the persisted graph
                // can prove completion; every other exit remains resumable.
                run.status = terminal_status(
                    self.workspace
                        .as_deref()
                        .unwrap_or_else(|| Path::new(&run.workspace)),
                )
                .to_owned();
                run.updated_at_ms = now_ms();
                write_project_state(&run).ok();
                fs::remove_file(run_path(&run_id)).ok();
            }
        }
        std::env::remove_var(RUN_ID_ENV);
    }
}

fn terminal_status(workspace: &Path) -> &'static str {
    let path = workspace.join(".fractal").join("project.fractal");
    let phase = fs::read_to_string(path)
        .ok()
        .and_then(|document| serde_json::from_str::<serde_json::Value>(&document).ok())
        .and_then(|document| {
            document
                .get("execution")
                .and_then(|execution| execution.get("phase"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    if phase.as_deref() == Some("completed") {
        "completed"
    } else {
        "interrupted"
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
        if let Some(project) = args.project.as_deref() {
            if let Some((name, phase, workspace)) = persisted_project_status(project) {
                match phase.as_str() {
                    "halted" => {
                        println!("Already paused: {name} ({workspace})");
                        println!("No agents are running; completed graph waves remain resumable.");
                    }
                    "completed" => {
                        println!("Already finished: {name} ({workspace})");
                        println!("No agents are running.");
                    }
                    "planning" | "executing" => {
                        halt_persisted_workspace(Path::new(&workspace), true)?;
                        println!("Stopped {name} ({workspace})");
                        println!(
                            "The stale coordinator state was reconciled; completed graph waves remain resumable."
                        );
                    }
                    _ => {
                        println!("Not running: {name} ({workspace})");
                        println!(
                            "The saved graph phase is {phase}; no coordinator or workers need to be stopped."
                        );
                    }
                }
                return Ok(());
            }
            bail!("no running or registered Fractal project matches {project:?}");
        }
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
        let stalled = stalled_projects();
        if stalled.is_empty() {
            println!("No Fractal builds are running.");
        } else {
            println!("No live Fractal coordinators are running.");
            println!("Stalled Fractal projects awaiting pause reconciliation:");
            for (name, phase, workspace) in stalled {
                println!("  {name:<28} state {phase}");
                println!("    {workspace}");
            }
            println!("Run `fractal pause --project NAME` to halt and synchronize one.");
        }
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
        "add_branch",
        task_ref,
        None,
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
        let selected: Vec<_> = runs
            .into_iter()
            .filter(|run| run_matches_project(run, &needle))
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

fn persisted_project_status(project: &str) -> Option<(String, String, String)> {
    let needle = project.trim().to_ascii_lowercase();
    let matches: Vec<_> = crate::projects::list()
        .into_iter()
        .filter_map(|entry| {
            let workspace = PathBuf::from(&entry.workspace);
            workspace_matches_project(&workspace, &entry.label, &needle).then(|| {
                let (name, phase) = persisted_project_details(&workspace)?;
                Some((name, phase, workspace.to_string_lossy().into_owned()))
            })?
        })
        .collect();
    (matches.len() == 1).then(|| matches[0].clone())
}

fn stalled_projects() -> Vec<(String, String, String)> {
    let mut projects: Vec<_> = crate::projects::list()
        .into_iter()
        .filter_map(|entry| {
            let workspace = PathBuf::from(&entry.workspace);
            let (name, phase) = persisted_project_details(&workspace)?;
            if !matches!(phase.as_str(), "planning" | "executing") {
                return None;
            }
            let state = read_project_run_state(&workspace)?;
            if state.status != "running"
                || (process_alive(state.pid) && process_workspace_matches(state.pid, &workspace))
            {
                return None;
            }
            Some((name, phase, workspace.to_string_lossy().into_owned()))
        })
        .collect();
    projects.sort_by(|left, right| left.0.cmp(&right.0));
    projects
}

fn halt_persisted_workspace(workspace: &Path, sync_runtime: bool) -> Result<()> {
    if let Some(mut state) = read_project_run_state(workspace) {
        if state.status == "running"
            && process_alive(state.pid)
            && process_workspace_matches(state.pid, workspace)
        {
            signal_process(state.pid, libc::SIGTERM);
            thread::sleep(Duration::from_millis(250));
            if process_alive(state.pid) && is_fractal_process(state.pid) {
                signal_process(state.pid, libc::SIGKILL);
            }
        }
        state.status = "halted".to_owned();
        state.updated_at_ms = now_ms();
        atomic_write(
            &workspace.join(".fractal").join("run-state.json"),
            &serde_json::to_vec_pretty(&state)?,
        )?;
        fs::remove_file(run_path(&state.run_id)).ok();
    }
    if crate::project_file::path(workspace).exists()
        && !crate::project_file::release_stale_assignments(workspace)?
    {
        crate::project_file::set_execution_phase(workspace, "halted")?;
    }
    if sync_runtime {
        if let Err(error) = crate::project_sync::sync_runtime_halt_now(workspace) {
            eprintln!("  halted graph sync note: {error:#}");
        }
    }
    Ok(())
}

fn read_project_run_state(workspace: &Path) -> Option<ProjectRunState> {
    fs::read(workspace.join(".fractal").join("run-state.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn persisted_project_details(workspace: &Path) -> Option<(String, String)> {
    if let Ok(document) = crate::project_file::load(workspace) {
        let phase = document
            .execution
            .map(|execution| execution.phase)
            .unwrap_or_else(|| "pending".to_owned());
        return Some((document.project.title, phase));
    }
    let identity = read_managed_project_identity(workspace)?;
    let state = read_project_run_state(workspace)?;
    let phase = match state.status.as_str() {
        "running" => "planning",
        "halted" | "interrupted" => "halted",
        "completed" => "completed",
        _ => return None,
    };
    Some((
        identity
            .get("title")
            .and_then(serde_json::Value::as_str)?
            .to_owned(),
        phase.to_owned(),
    ))
}

fn workspace_matches_project(workspace: &Path, label: &str, needle: &str) -> bool {
    if workspace.to_string_lossy().to_ascii_lowercase() == needle {
        return true;
    }
    let needle_keys = project_keys(needle);
    let mut aliases = vec![label.to_owned()];
    if let Some(name) = workspace.file_name() {
        aliases.push(name.to_string_lossy().into_owned());
    }
    aliases.extend(project_identity_aliases(workspace));
    aliases.into_iter().any(|alias| {
        let alias_keys = project_keys(&alias);
        needle_keys.iter().any(|key| alias_keys.contains(key))
    })
}

fn run_matches_project(run: &ActiveRun, needle: &str) -> bool {
    if run.workspace.to_ascii_lowercase() == needle {
        return true;
    }
    let needle_keys = project_keys(needle);
    let workspace = Path::new(&run.workspace);
    let mut aliases = vec![run.project.clone()];
    if let Some(name) = workspace.file_name() {
        aliases.push(name.to_string_lossy().into_owned());
    }
    aliases.extend(project_identity_aliases(workspace));
    aliases.into_iter().any(|alias| {
        let alias_keys = project_keys(&alias);
        needle_keys.iter().any(|key| alias_keys.contains(key))
    })
}

fn project_identity_aliases(workspace: &Path) -> Vec<String> {
    let mut aliases: Vec<String> = fs::read(workspace.join(".fractal").join("project.fractal"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|document| document.get("project").cloned())
        .map(|project| {
            ["slug", "title"]
                .into_iter()
                .filter_map(|field| project.get(field)?.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if let Some(identity) = read_managed_project_identity(workspace) {
        aliases.extend(
            ["slug", "title"]
                .into_iter()
                .filter_map(|field| identity.get(field)?.as_str().map(str::to_owned)),
        );
    }
    aliases
}

fn read_managed_project_identity(workspace: &Path) -> Option<serde_json::Value> {
    fs::read(workspace.join(".fractal").join("managed-project.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn project_keys(value: &str) -> Vec<String> {
    let mut words: Vec<String> = value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_ascii_lowercase())
        .collect();
    let mut keys = vec![project_key(value)];
    if words.first().is_some_and(|word| word == "the") {
        words.remove(0);
    }
    if words
        .last()
        .is_some_and(|word| matches!(word.as_str(), "app" | "build" | "project"))
    {
        words.pop();
    }
    for word in &mut words {
        if let Some(number) = spoken_number(word) {
            *word = number.to_owned();
        }
    }
    let conversational = project_key(&words.join(" "));
    if !conversational.is_empty() && !keys.contains(&conversational) {
        keys.push(conversational);
    }
    keys
}

fn spoken_number(value: &str) -> Option<&'static str> {
    Some(match value {
        "zero" => "0",
        "one" => "1",
        "two" => "2",
        "three" => "3",
        "four" => "4",
        "five" => "5",
        "six" => "6",
        "seven" => "7",
        "eight" => "8",
        "nine" => "9",
        "ten" => "10",
        _ => return None,
    })
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
        schema: "fractal.run_state.v1".to_owned(),
        run_id: run.run_id.clone(),
        status: run.status.clone(),
        pid: run.pid,
        graph_hash: run.graph_hash.clone(),
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

fn process_workspace_matches(pid: u32, workspace: &Path) -> bool {
    let expected = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    #[cfg(target_os = "linux")]
    if fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .is_some_and(|path| path == expected)
    {
        return true;
    }
    std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.strip_prefix('n').map(PathBuf::from))
        })
        .and_then(|path| path.canonicalize().ok())
        .is_some_and(|path| path == expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_exit_is_completed_only_when_the_graph_proves_it() {
        let root = std::env::temp_dir().join(format!(
            "fractal-run-terminal-status-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let directory = root.join(".fractal");
        fs::create_dir_all(&directory).unwrap();

        fs::write(
            directory.join("project.fractal"),
            r#"{"execution":{"phase":"executing"}}"#,
        )
        .unwrap();
        assert_eq!(terminal_status(&root), "interrupted");

        fs::write(
            directory.join("project.fractal"),
            r#"{"execution":{"phase":"completed"}}"#,
        )
        .unwrap();
        assert_eq!(terminal_status(&root), "completed");

        fs::remove_dir_all(root).ok();
    }

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

    #[test]
    fn project_selection_accepts_graph_title_and_conversational_app_suffix() {
        let root = std::env::temp_dir().join(format!(
            "fractal-run-project-alias-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(root.join(".fractal")).unwrap();
        fs::write(
            root.join(".fractal/project.fractal"),
            r#"{"project":{"slug":"racket","title":"Racket"}}"#,
        )
        .unwrap();
        let run = ActiveRun {
            schema: "fractal.active_run.v1".to_owned(),
            run_id: "run".to_owned(),
            pid: std::process::id(),
            workspace: root.to_string_lossy().into_owned(),
            project: "racket-1785197928063".to_owned(),
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
            select_runs(vec![run], Some("the Racket app"))
                .unwrap()
                .first()
                .unwrap()
                .project,
            "racket-1785197928063"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn persisted_project_matching_accepts_graph_title_after_run_has_stopped() {
        let root = std::env::temp_dir().join(format!(
            "fractal-stopped-project-alias-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(root.join(".fractal")).unwrap();
        fs::write(
            root.join(".fractal/project.fractal"),
            r#"{"project":{"slug":"coffee5","title":"Coffee5"}}"#,
        )
        .unwrap();
        assert!(workspace_matches_project(
            &root,
            "coffee5-1785198755992",
            "coffee five app"
        ));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn stale_planning_run_is_halted_and_checkouts_are_released() {
        let root = std::env::temp_dir().join(format!(
            "fractal-stale-planning-run-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        crate::project_file::configure_managed_identity(
            &root,
            "Monkey",
            "Build an app about a monkey.",
        )
        .unwrap();
        let mut graph = serde_json::json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [{
                "id": "build",
                "capability": "code.generate",
                "instruction": "Build it."
            }],
            "edges": []
        });
        graph["graph_hash"] =
            serde_json::Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        crate::project_file::persist(&root, &graph, "Monkey").unwrap();
        crate::project_file::transition(&root, "build", "checkout", "codex", "Codex").unwrap();
        let state = ProjectRunState {
            schema: "fractal.run_state.v1".to_owned(),
            run_id: "stale-run".to_owned(),
            status: "running".to_owned(),
            pid: u32::MAX,
            graph_hash: None,
            updated_at_ms: 1,
        };
        atomic_write(
            &root.join(".fractal/run-state.json"),
            &serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        halt_persisted_workspace(&root, false).unwrap();

        let project = crate::project_file::load(&root).unwrap();
        let execution = project.execution.unwrap();
        assert_eq!(execution.phase, "halted");
        assert_eq!(execution.assignments["build"].state, "released");
        assert_eq!(read_project_run_state(&root).unwrap().status, "halted");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn named_pause_resolves_before_the_planning_graph_exists() {
        let root = std::env::temp_dir().join(format!(
            "fractal-early-planning-run-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        crate::project_file::configure_managed_identity(
            &root,
            "Monkey",
            "Build an app about a monkey.",
        )
        .unwrap();
        let state = ProjectRunState {
            schema: "fractal.run_state.v1".to_owned(),
            run_id: "early-run".to_owned(),
            status: "running".to_owned(),
            pid: u32::MAX,
            graph_hash: None,
            updated_at_ms: 1,
        };
        atomic_write(
            &root.join(".fractal/run-state.json"),
            &serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        assert!(workspace_matches_project(
            &root,
            "generated-folder",
            "monkey"
        ));
        assert_eq!(
            persisted_project_details(&root),
            Some(("Monkey".to_owned(), "planning".to_owned()))
        );
        halt_persisted_workspace(&root, false).unwrap();
        assert_eq!(read_project_run_state(&root).unwrap().status, "halted");
        fs::remove_dir_all(root).ok();
    }
}
