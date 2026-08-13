//! Join an existing project as a worker without choosing a provider.
//!
//! Establishes exactly one assignment receive loop per joined worker, with a
//! bounded lease that is renewed until completion chaining delivers the next
//! dependency-ready node through the same listener.

use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::cli::JoinArgs;
use crate::coordinator::{
    renewal_message, validate_lease_secs, CompletionReport, WorkerLease, COMPLETION_SCHEMA,
};

const JOIN_SCHEMA: &str = "fractal.join.v1";
const ASSIGNMENT_SCHEMA: &str = "fractal.worker_assignment.v1";
const COORDINATOR_FRESHNESS_SECS: u64 = 30;

#[derive(Debug, PartialEq, Eq, Clone)]
struct Assignment {
    worker_id: Option<String>,
    task_id: Option<String>,
    node_id: String,
    details: String,
    generation: Option<u64>,
    expires_at_ms: Option<u64>,
    project: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Received {
    Assignment(Assignment),
    NoWork {
        amendment_requested: bool,
        details: String,
    },
    LeaseRenewed {
        generation: u64,
    },
    CompletionRejected {
        details: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CoordinatorDiscovery {
    None,
    Unique { id: String },
    Ambiguous { ids: Vec<String> },
}

/// Active listener accounting used by tests to prove one receive loop per worker.
#[derive(Debug, Default)]
pub(crate) struct ListenerRegistry {
    active: Mutex<std::collections::BTreeMap<String, u32>>,
}

impl ListenerRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(std::collections::BTreeMap::new()),
        })
    }

    pub(crate) fn enter(self: &Arc<Self>, worker_id: &str) -> Result<ListenerGuard> {
        let mut guard = self.active.lock().expect("listener registry");
        let count = guard.entry(worker_id.to_owned()).or_insert(0);
        if *count > 0 {
            bail!("worker {worker_id} already has an active assignment listener");
        }
        *count = 1;
        Ok(ListenerGuard {
            registry: Arc::clone(self),
            worker_id: worker_id.to_owned(),
        })
    }

    #[cfg(test)]
    pub(crate) fn active_count(&self, worker_id: &str) -> u32 {
        self.active
            .lock()
            .expect("listener registry")
            .get(worker_id)
            .copied()
            .unwrap_or(0)
    }
}

pub(crate) struct ListenerGuard {
    registry: Arc<ListenerRegistry>,
    worker_id: String,
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.registry.active.lock() {
            guard.remove(&self.worker_id);
        }
    }
}

pub(crate) fn run(args: &JoinArgs) -> Result<()> {
    if args.role.trim().is_empty() {
        bail!("worker role cannot be empty");
    }
    if args.poll_secs == 0 {
        bail!("--poll-secs must be greater than zero");
    }
    let lease_secs = validate_lease_secs(args.lease_secs)?;

    let workspace = discover_workspace(&args.repo)?;
    crate::project_file::load(&workspace)
        .with_context(|| format!("load project graph in {}", workspace.display()))?;
    let requested_agent_id = args
        .agent_id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| generated_agent_id(&args.role));
    let agent_id = normalize_agent_id(&requested_agent_id);
    let agent_label = args
        .agent_label
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("Fractal · {} · {}", args.role, agent_id));
    let provider = detect_client_from_env();
    let squad = args
        .squad_bin
        .clone()
        .unwrap_or_else(|| PathBuf::from("squad"));

    run_squad(&squad, &workspace, &["init"])
        .context("initialize the local squad coordinator workspace")?;
    run_squad(
        &squad,
        &workspace,
        &[
            "join".to_owned(),
            agent_id.clone(),
            "--role".to_owned(),
            args.role.clone(),
            "--protocol-version".to_owned(),
            "2".to_owned(),
        ],
    )
    .with_context(|| format!("register worker {agent_id} with squad"))?;

    let agents = run_squad(&squad, &workspace, &["agents", "--json"])
        .context("inspect squad coordinator availability")?;
    let discovery = discover_active_coordinators(&agents.stdout);
    let coordinator_target = match &discovery {
        CoordinatorDiscovery::None => None,
        CoordinatorDiscovery::Unique { id } => Some(id.clone()),
        CoordinatorDiscovery::Ambiguous { ids } => {
            // Deterministic: address the lexicographically first fresh graph
            // supervisor and never spawn a competing one-shot.
            eprintln!(
                "  join note: ambiguous active coordinators {:?}; using {}",
                ids,
                ids.first().map(String::as_str).unwrap_or("unknown")
            );
            ids.first().cloned()
        }
    };
    if coordinator_target.is_none() {
        crate::coordinator::assign_once(
            &workspace,
            args.squad_bin.as_deref(),
            &agent_id,
            &agent_label,
        )
        .context("ask the local one-shot coordinator for an assignment")?;
    }

    let ready_message = format!(
        "WORKER_JOIN_READY {{\"schema\":\"fractal.worker_join.v1\",\"agent_id\":{agent_id:?},\"agent_label\":{agent_label:?},\"role\":{role:?},\"provider\":{provider:?},\"project\":{project:?},\"lease_secs\":{lease_secs}}}. Assign only a dependency-ready, claimable parallel graph node. If none exists, evaluate a governed graph split/amendment and report AMENDMENT_REQUESTED or NO_PARALLEL_WORK; never mutate project.fractal directly.",
        agent_id = agent_id, agent_label = agent_label, role = args.role,
        provider = provider, project = workspace.display().to_string(),
        lease_secs = lease_secs,
    );
    if let Some(coordinator_id) = coordinator_target.as_ref() {
        run_squad(
            &squad,
            &workspace,
            &[
                "send".to_owned(),
                agent_id.clone(),
                coordinator_id.clone(),
                ready_message,
            ],
        )
        .context("announce worker readiness to the coordinator")?;
    }
    emit_state(
        args,
        "ready",
        &workspace,
        &agent_id,
        &agent_label,
        &provider,
        None,
        None,
        None,
        "waiting for a coordinator assignment",
    )?;

    let listeners = ListenerRegistry::new();
    let _listener = listeners.enter(&agent_id)?;
    let deadline =
        (args.timeout_secs > 0).then(|| Instant::now() + Duration::from_secs(args.timeout_secs));
    let mut active_lease: Option<WorkerLease> = None;
    let mut renew_after = Instant::now() + renewal_period(lease_secs);

    loop {
        if let Some(lease) = active_lease.as_ref() {
            if Instant::now() >= renew_after {
                let _ = run_squad(
                    &squad,
                    &workspace,
                    &[
                        "send".to_owned(),
                        agent_id.clone(),
                        coordinator_target
                            .clone()
                            .unwrap_or_else(|| "@all".to_owned()),
                        renewal_message(lease),
                    ],
                );
                renew_after = Instant::now() + renewal_period(lease_secs);
            }
        }

        let output = run_squad(
            &squad,
            &workspace,
            &[
                "receive".to_owned(),
                agent_id.clone(),
                "--json".to_owned(),
                "--wait".to_owned(),
                "--timeout".to_owned(),
                args.poll_secs.to_string(),
            ],
        );
        let received = match output {
            Ok(output) => parse_received(&output.stdout),
            Err(error) => {
                if deadline.is_some_and(|value| Instant::now() >= value) || args.once {
                    emit_state(
                        args,
                        "no_work",
                        &workspace,
                        &agent_id,
                        &agent_label,
                        &provider,
                        None,
                        None,
                        None,
                        "no assignment arrived during the requested wait",
                    )?;
                    return Ok(());
                }
                if error.to_string().contains("not a squad workspace") {
                    bail!("coordinator receive failed: {error:#}");
                }
                continue;
            }
        };

        match received {
            Some(Received::Assignment(assignment)) => {
                if assignment
                    .worker_id
                    .as_deref()
                    .is_some_and(|worker_id| worker_id != agent_id)
                {
                    continue;
                }
                if let Some(task_id) = assignment.task_id.as_deref() {
                    run_squad(&squad, &workspace, &["task", "ack", &agent_id, task_id])
                        .with_context(|| format!("acknowledge coordinator task {task_id}"))?;
                }
                let already_owned =
                    crate::project_file::assignment(&workspace, &assignment.node_id)?.is_some_and(
                        |current| current.state == "checked_out" && current.agent_id == agent_id,
                    );
                let checkout = if already_owned {
                    Ok(())
                } else {
                    crate::project_file::transition(
                        &workspace,
                        &assignment.node_id,
                        "checkout",
                        &agent_id,
                        &agent_label,
                    )
                };
                if let Err(error) = checkout {
                    let failure = format!(
                        "JOIN_CHECKOUT_FAILED node_id={} agent_id={} reason={error:#}",
                        assignment.node_id, agent_id
                    );
                    let _ = run_squad(
                        &squad,
                        &workspace,
                        &[
                            "send".to_owned(),
                            agent_id.clone(),
                            coordinator_target
                                .clone()
                                .unwrap_or_else(|| "@all".to_owned()),
                            failure,
                        ],
                    );
                    emit_state(
                        args,
                        "error",
                        &workspace,
                        &agent_id,
                        &agent_label,
                        &provider,
                        Some(&assignment.node_id),
                        assignment.task_id.as_deref(),
                        assignment.generation,
                        &format!("canonical checkout rejected: {error:#}"),
                    )?;
                    bail!(
                        "canonical checkout rejected for {}: {error:#}",
                        assignment.node_id
                    );
                }
                if let Err(error) = crate::project_sync::sync_worker_transition_now(&workspace) {
                    eprintln!("  live graph sync note: {error:#}");
                }
                active_lease = Some(WorkerLease {
                    project: assignment
                        .project
                        .clone()
                        .unwrap_or_else(|| workspace.display().to_string()),
                    worker_id: agent_id.clone(),
                    worker_label: agent_label.clone(),
                    node_id: assignment.node_id.clone(),
                    task_id: assignment
                        .task_id
                        .clone()
                        .unwrap_or_else(|| format!("task-{}", assignment.node_id)),
                    generation: assignment.generation.unwrap_or(1),
                    expires_at_ms: assignment.expires_at_ms.unwrap_or_else(|| {
                        now_ms().saturating_add(lease_secs.saturating_mul(1_000))
                    }),
                });
                renew_after = Instant::now() + renewal_period(lease_secs);
                emit_state(
                    args,
                    "assigned",
                    &workspace,
                    &agent_id,
                    &agent_label,
                    &provider,
                    Some(&assignment.node_id),
                    assignment.task_id.as_deref(),
                    assignment.generation,
                    &assignment.details,
                )?;
                if args.once {
                    return Ok(());
                }
                // Stay in the same receive loop for completion chaining.
            }
            Some(Received::NoWork {
                amendment_requested,
                details,
            }) => {
                let state = if amendment_requested {
                    "amendment_requested"
                } else {
                    "no_work"
                };
                emit_state(
                    args,
                    state,
                    &workspace,
                    &agent_id,
                    &agent_label,
                    &provider,
                    None,
                    None,
                    None,
                    &details,
                )?;
                if args.once || deadline.is_some_and(|value| Instant::now() >= value) {
                    return Ok(());
                }
            }
            Some(Received::LeaseRenewed { generation }) => {
                if let Some(lease) = active_lease.as_mut() {
                    lease.generation = generation;
                    lease.expires_at_ms = now_ms().saturating_add(lease_secs.saturating_mul(1_000));
                }
            }
            Some(Received::CompletionRejected { details }) => {
                emit_state(
                    args,
                    "error",
                    &workspace,
                    &agent_id,
                    &agent_label,
                    &provider,
                    active_lease.as_ref().map(|lease| lease.node_id.as_str()),
                    active_lease.as_ref().map(|lease| lease.task_id.as_str()),
                    active_lease.as_ref().map(|lease| lease.generation),
                    &details,
                )?;
            }
            None if args.once || deadline.is_some_and(|value| Instant::now() >= value) => {
                emit_state(
                    args,
                    "no_work",
                    &workspace,
                    &agent_id,
                    &agent_label,
                    &provider,
                    None,
                    None,
                    None,
                    "no assignment arrived during the requested wait",
                )?;
                return Ok(());
            }
            None => {}
        }
    }
}

fn renewal_period(lease_secs: u64) -> Duration {
    Duration::from_secs((lease_secs / 3).max(1))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[allow(clippy::too_many_arguments)]
fn emit_state(
    args: &JoinArgs,
    state: &str,
    workspace: &Path,
    agent_id: &str,
    agent_label: &str,
    provider: &str,
    node_id: Option<&str>,
    task_id: Option<&str>,
    generation: Option<u64>,
    message: &str,
) -> Result<()> {
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "schema": JOIN_SCHEMA, "state": state, "role": args.role,
                "agent_id": agent_id, "agent_label": agent_label, "provider": provider,
                "project": workspace, "node_id": node_id, "task_id": task_id,
                "generation": generation, "lease_secs": args.lease_secs, "message": message,
            }))?
        );
    } else {
        match state {
            "ready" => println!("Joined as {agent_label} ({agent_id}); {message}..."),
            "assigned" => println!(
                "Assigned graph node {}: {message}",
                node_id.unwrap_or("unknown")
            ),
            "amendment_requested" => {
                println!("Coordinator requested governed graph expansion: {message}")
            }
            "no_work" => println!("No parallel work is currently available: {message}"),
            "error" => eprintln!("Worker join error: {message}"),
            _ => println!("{state}: {message}"),
        }
    }
    Ok(())
}

fn run_squad(bin: &Path, workspace: &Path, args: &[impl AsRef<str>]) -> Result<Output> {
    for attempt in 0..40 {
        let output = Command::new(bin)
            .current_dir(workspace)
            .args(args.iter().map(AsRef::as_ref))
            .output()
            .with_context(|| format!("run squad command {}", bin.display()))?;
        if output.status.success() {
            return Ok(output);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if attempt < 39
            && (stderr.contains("database is locked") || stderr.contains("Error code 5"))
        {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        bail!("squad command failed ({}): {}", output.status, stderr);
    }
    unreachable!("bounded squad retry loop always returns or fails")
}

fn discover_workspace(start: &Path) -> Result<PathBuf> {
    let start = fs::canonicalize(start).with_context(|| format!("resolve {}", start.display()))?;
    let mut current = if start.is_dir() {
        start.clone()
    } else {
        start
            .parent()
            .context("join path has no parent")?
            .to_path_buf()
    };
    loop {
        if current.join(".fractal/project.fractal").is_file() {
            return Ok(current);
        }
        if !current.pop() {
            break;
        }
    }
    bail!(
        "no Fractal project graph found at or above {}",
        start.display()
    )
}

fn generated_agent_id(role: &str) -> String {
    let host = env::var("HOSTNAME")
        .or_else(|_| env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "local".to_owned());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default();
    format!(
        "fractal-{}-{}-{}-{}",
        sanitize_component(role),
        sanitize_component(&host),
        std::process::id(),
        timestamp
    )
}

fn normalize_agent_id(value: &str) -> String {
    let normalized = sanitize_component(value);
    if normalized == "unknown" {
        generated_agent_id("worker")
    } else {
        normalized
    }
}

fn sanitize_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}

fn detect_client_from_env() -> String {
    let pairs = env::vars().collect::<Vec<_>>();
    let refs = pairs
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    detect_client_from_pairs(&refs)
}

fn detect_client_from_pairs(pairs: &[(&str, &str)]) -> String {
    let has = |keys: &[&str]| {
        keys.iter().any(|key| {
            pairs
                .iter()
                .any(|(candidate, value)| candidate == key && !value.trim().is_empty())
        })
    };
    if has(&[
        "FRACTAL_AGENT_CLIENT",
        "FRACTAL_CLIENT",
        "FRACTAL_WORKER_CLIENT",
    ]) {
        for key in [
            "FRACTAL_AGENT_CLIENT",
            "FRACTAL_CLIENT",
            "FRACTAL_WORKER_CLIENT",
        ] {
            if let Some((_, value)) = pairs
                .iter()
                .find(|(candidate, value)| *candidate == key && !value.trim().is_empty())
            {
                return value.trim().to_ascii_lowercase();
            }
        }
    }
    if has(&["CODEX_HOME", "CODEX_SESSION_ID", "CODEX_THREAD_ID"]) {
        return "codex".to_owned();
    }
    if has(&["CLAUDECODE", "CLAUDE_CODE", "CLAUDE_SESSION_ID"]) {
        return "claude".to_owned();
    }
    if has(&["CURSOR_AGENT", "CURSOR_SESSION_ID"]) {
        return "cursor".to_owned();
    }
    if has(&["GEMINI_CLI", "GEMINI_SESSION_ID"]) {
        return "gemini".to_owned();
    }
    "unknown".to_owned()
}

pub(crate) fn discover_active_coordinators(bytes: &[u8]) -> CoordinatorDiscovery {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    discover_active_coordinators_at(bytes, now_secs)
}

fn discover_active_coordinators_at(bytes: &[u8], now_secs: u64) -> CoordinatorDiscovery {
    let mut ids = json_values(bytes)
        .iter()
        .filter_map(|value| {
            let status = value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let role = value
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let id = value.get("id").and_then(Value::as_str).unwrap_or_default();
            let last_seen = value
                .get("last_seen")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let heartbeat_is_fresh = last_seen > 0
                && last_seen <= now_secs
                && now_secs.saturating_sub(last_seen) <= COORDINATOR_FRESHNESS_SECS;
            let is_coordinator = status == "active"
                && value.get("archived_at").is_none_or(Value::is_null)
                && heartbeat_is_fresh
                && id.starts_with("fractal-coordinator-")
                && !id.starts_with("fractal-coordinator-once-")
                && matches!(
                    role.as_str(),
                    "coordinator" | "graph-supervisor" | "supervisor"
                );
            is_coordinator.then(|| id.to_owned())
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    match ids.as_slice() {
        [] => CoordinatorDiscovery::None,
        [id] => CoordinatorDiscovery::Unique { id: id.clone() },
        _ => CoordinatorDiscovery::Ambiguous { ids },
    }
}

#[cfg(test)]
fn has_active_coordinator(bytes: &[u8]) -> bool {
    !matches!(
        discover_active_coordinators(bytes),
        CoordinatorDiscovery::None
    )
}

fn json_values(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .flat_map(|value| match value {
            Value::Array(values) => values,
            value => vec![value],
        })
        .collect()
}

fn parse_received(bytes: &[u8]) -> Option<Received> {
    for line in String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let value = serde_json::from_str::<Value>(line).ok();
        let text = value
            .as_ref()
            .and_then(|value| value.get("content"))
            .and_then(Value::as_str)
            .unwrap_or(line);
        if text.starts_with("WORKER_JOIN_READY ") {
            continue;
        }
        if text.starts_with("LEASE_RENEWED ") {
            let generation = text
                .split("generation=")
                .nth(1)
                .and_then(|part| part.split_whitespace().next())
                .and_then(|part| part.parse().ok())
                .unwrap_or(0);
            return Some(Received::LeaseRenewed { generation });
        }
        if text.starts_with("COMPLETION_REJECTED ") {
            return Some(Received::CompletionRejected {
                details: text.to_owned(),
            });
        }
        if let Some(assignment) = parse_structured_assignment(text)
            .or_else(|| value.as_ref().and_then(parse_assignment_value))
        {
            return Some(Received::Assignment(assignment));
        }
        let task = value.as_ref().and_then(|value| value.get("task"));
        let node_id = value
            .as_ref()
            .and_then(find_node_id)
            .or_else(|| task.and_then(find_node_id))
            .or_else(|| find_node_id(&Value::String(text.to_owned())));
        if let Some(node_id) = node_id {
            let task_id = value
                .as_ref()
                .and_then(|value| value.get("task_id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    task.and_then(|task| task.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
            let generation = value
                .as_ref()
                .and_then(|value| value.get("generation"))
                .and_then(Value::as_u64)
                .or_else(|| find_u64_in_text(text, "generation"));
            let details = task
                .and_then(|task| task.get("body"))
                .and_then(Value::as_str)
                .or_else(|| {
                    value
                        .as_ref()
                        .and_then(|value| value.get("content"))
                        .and_then(Value::as_str)
                })
                .unwrap_or(text)
                .to_owned();
            return Some(Received::Assignment(Assignment {
                worker_id: value
                    .as_ref()
                    .and_then(|value| value.get("worker_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                task_id,
                node_id,
                details,
                generation,
                expires_at_ms: find_u64_in_text(text, "expires_at_ms"),
                project: None,
            }));
        }
        let lower = text.to_ascii_lowercase();
        if lower.contains("no_parallel_work")
            || lower.contains("no parallel work")
            || lower.contains("no_work")
        {
            return Some(Received::NoWork {
                amendment_requested: lower.contains("amendment_requested")
                    || lower.contains("amendment requested"),
                details: text.to_owned(),
            });
        }
    }
    None
}

fn parse_structured_assignment(text: &str) -> Option<Assignment> {
    let json_start = text.find('{')?;
    let value = serde_json::Deserializer::from_str(&text[json_start..])
        .into_iter::<Value>()
        .next()?
        .ok()?;
    parse_assignment_value(&value)
}

fn parse_assignment_value(value: &Value) -> Option<Assignment> {
    if value.get("schema").and_then(Value::as_str) == Some(ASSIGNMENT_SCHEMA) {
        return Some(Assignment {
            worker_id: value
                .get("worker_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            task_id: value
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            node_id: value.get("node_id")?.as_str()?.to_owned(),
            details: value.to_string(),
            generation: value.get("generation").and_then(Value::as_u64),
            expires_at_ms: value.get("expires_at_ms").and_then(Value::as_u64),
            project: value
                .get("project")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    let nested = value
        .get("assignment")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| parse_assignment_value(&value));
    if nested.is_some() {
        return nested;
    }
    None
}

fn find_u64_in_text(text: &str, key: &str) -> Option<u64> {
    let start = text.find(key)?;
    let rest = text[start + key.len()..].trim_start_matches([' ', ':', '=', char::from(96), '"']);
    rest.split(|character: char| {
        character.is_whitespace()
            || character == char::from(96)
            || matches!(character, '"' | ',' | '}' | ']')
    })
    .next()
    .and_then(|value| value.parse().ok())
}

fn find_node_id(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in ["node_id", "graph_node_id", "nodeId", "graphNodeId"] {
                if let Some(node) = map
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                {
                    return Some(node.to_owned());
                }
            }
            map.values().find_map(find_node_id)
        }
        Value::Array(values) => values.iter().find_map(find_node_id),
        Value::String(text) => find_node_id_in_text(text),
        _ => None,
    }
}

fn find_node_id_in_text(text: &str) -> Option<String> {
    for key in ["node_id", "graph_node_id", "nodeId", "graphNodeId"] {
        let Some(start) = text.find(key) else {
            continue;
        };
        let rest =
            text[start + key.len()..].trim_start_matches([' ', ':', '=', char::from(96), '"']);
        let node = rest
            .split(|character: char| {
                character.is_whitespace()
                    || character == char::from(96)
                    || matches!(character, '"' | ',' | '}' | ']')
            })
            .next()
            .unwrap_or_default();
        if !node.is_empty() {
            return Some(node.to_owned());
        }
    }
    None
}

/// Build a versioned completion report for coordinator validation.
#[allow(dead_code)]
pub(crate) fn build_completion_report(
    project: &str,
    worker_id: &str,
    node_id: &str,
    task_id: &str,
    generation: u64,
    evidence: Vec<String>,
) -> CompletionReport {
    CompletionReport {
        schema: COMPLETION_SCHEMA.to_owned(),
        project: project.to_owned(),
        worker_id: worker_id.to_owned(),
        node_id: node_id.to_owned(),
        task_id: task_id.to_owned(),
        generation,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;

    #[test]
    fn detects_client_without_a_client_argument() {
        assert_eq!(
            detect_client_from_pairs(&[("CODEX_SESSION_ID", "abc")]),
            "codex"
        );
        assert_eq!(detect_client_from_pairs(&[("CLAUDECODE", "1")]), "claude");
        assert_eq!(
            detect_client_from_pairs(&[("FRACTAL_AGENT_CLIENT", "custom")]),
            "custom"
        );
        assert_eq!(detect_client_from_pairs(&[]), "unknown");
    }

    #[test]
    fn normalizes_ids_for_squad_session_paths() {
        assert_eq!(normalize_agent_id("codex/root"), "codex-root");
        assert!(!generated_agent_id("worker").contains('/'));
    }

    #[test]
    fn parses_structured_assignment_and_graph_node() {
        let received = parse_received(br#"{"kind":"task_assigned","task_id":"t1","task":{"id":"t1","body":"node_id=parallel_task"}}"#).unwrap();
        assert_eq!(
            received,
            Received::Assignment(Assignment {
                worker_id: None,
                task_id: Some("t1".to_owned()),
                node_id: "parallel_task".to_owned(),
                details: "node_id=parallel_task".to_owned(),
                generation: None,
                expires_at_ms: None,
                project: None,
            })
        );
    }

    #[test]
    fn parses_versioned_assignment_payload_with_generation() {
        let payload = r#"{"content":"{\"schema\":\"fractal.worker_assignment.v1\",\"project\":\"p\",\"worker_id\":\"w\",\"node_id\":\"n9\",\"task_id\":\"t9\",\"generation\":7,\"expires_at_ms\":100}"#;
        // content is a JSON string; feed as message text directly
        let text = r#"{"schema":"fractal.worker_assignment.v1","project":"p","worker_id":"w","node_id":"n9","task_id":"t9","generation":7,"expires_at_ms":100}"#;
        let received =
            parse_received(format!(r#"{{"content":{}}}"#, json!(text)).as_bytes()).unwrap();
        match received {
            Received::Assignment(assignment) => {
                assert_eq!(assignment.worker_id.as_deref(), Some("w"));
                assert_eq!(assignment.node_id, "n9");
                assert_eq!(assignment.generation, Some(7));
                assert_eq!(assignment.task_id.as_deref(), Some("t9"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let _ = payload;
    }

    #[test]
    fn recognizes_governed_no_work_escalation() {
        assert_eq!(
            parse_received(br#"{"content":"AMENDMENT_REQUESTED: no parallel work exists yet"}"#),
            Some(Received::NoWork {
                amendment_requested: true,
                details: "AMENDMENT_REQUESTED: no parallel work exists yet".to_owned()
            })
        );
    }

    #[test]
    fn arbitrary_managers_are_not_graph_coordinators() {
        assert!(!has_active_coordinator(
            br#"{"id":"m","role":"manager","status":"active","last_seen":1}"#
        ));
        assert!(!has_active_coordinator(
            br#"{"id":"w","role":"worker","status":"active","last_seen":1}"#
        ));
    }

    #[test]
    fn coordinate_graph_supervisor_counts_as_active_coordinator() {
        assert_eq!(
            discover_active_coordinators_at(
                br#"{"id":"fractal-coordinator-7","role":"graph-supervisor","status":"active","last_seen":995,"archived_at":null}"#,
                1_000,
            ),
            CoordinatorDiscovery::Unique {
                id: "fractal-coordinator-7".to_owned()
            }
        );
        assert_eq!(
            discover_active_coordinators_at(
                br#"{"id":"fractal-coordinator-once-1-2","role":"graph-supervisor","status":"active","last_seen":995,"archived_at":null}"#,
                1_000,
            ),
            CoordinatorDiscovery::None
        );
    }

    #[test]
    fn discovers_zero_unique_and_ambiguous_coordinators_deterministically() {
        assert_eq!(
            discover_active_coordinators_at(
                br#"{"id":"w","role":"worker","status":"active","last_seen":995}"#,
                1_000,
            ),
            CoordinatorDiscovery::None
        );
        assert_eq!(
            discover_active_coordinators_at(
                br#"{"id":"fractal-coordinator-b","role":"graph-supervisor","status":"active","last_seen":995}
{"id":"fractal-coordinator-a","role":"coordinator","status":"active","last_seen":994}"#,
                1_000,
            ),
            CoordinatorDiscovery::Ambiguous {
                ids: vec![
                    "fractal-coordinator-a".to_owned(),
                    "fractal-coordinator-b".to_owned()
                ]
            }
        );
        assert_eq!(
            discover_active_coordinators_at(
                br#"{"id":"fractal-coordinator-only","role":"coordinator","status":"active","last_seen":995}"#,
                1_000,
            ),
            CoordinatorDiscovery::Unique {
                id: "fractal-coordinator-only".to_owned()
            }
        );
    }

    #[test]
    fn coordinator_discovery_rejects_stale_missing_future_and_archived_heartbeats() {
        let agents = br#"{"id":"fractal-coordinator-stale","role":"graph-supervisor","status":"active","last_seen":969,"archived_at":null}
{"id":"fractal-coordinator-missing","role":"graph-supervisor","status":"active","archived_at":null}
{"id":"fractal-coordinator-future","role":"graph-supervisor","status":"active","last_seen":1001,"archived_at":null}
{"id":"fractal-coordinator-archived","role":"graph-supervisor","status":"active","last_seen":995,"archived_at":998}
{"id":"specialist-manager","role":"manager","status":"active","last_seen":995,"archived_at":null}"#;
        assert_eq!(
            discover_active_coordinators_at(agents, 1_000),
            CoordinatorDiscovery::None
        );
    }

    #[test]
    fn ignores_other_workers_readiness_broadcasts() {
        assert_eq!(
            parse_received(br#"{"content":"WORKER_JOIN_READY {\"agent_id\":\"other\"}. If none exists, report AMENDMENT_REQUESTED or NO_PARALLEL_WORK."}"#),
            None
        );
    }

    #[test]
    fn listener_registry_enforces_one_receive_loop_per_worker() {
        let registry = ListenerRegistry::new();
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                registry.enter("worker-a")
            }));
        }
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("listener thread"))
            .collect();
        let ok = results.iter().filter(|result| result.is_ok()).count();
        let err = results.iter().filter(|result| result.is_err()).count();
        assert_eq!(ok, 1);
        assert_eq!(err, 1);
        assert_eq!(registry.active_count("worker-a"), 1);
        drop(results);
        assert_eq!(registry.active_count("worker-a"), 0);
    }

    #[test]
    fn lease_bounds_match_coordinator_contract() {
        assert_eq!(crate::coordinator::DEFAULT_LEASE_SECS, 60);
        assert_eq!(crate::coordinator::MAX_LEASE_SECS, 300);
        assert!(validate_lease_secs(0).is_err());
        assert!(validate_lease_secs(301).is_err());
    }

    #[test]
    fn build_completion_report_is_versioned_and_includes_identity() {
        let report = build_completion_report(
            "proj",
            "w1",
            "n1",
            "t1",
            3,
            vec!["sha256:deadbeef".to_owned()],
        );
        assert_eq!(report.schema, COMPLETION_SCHEMA);
        assert_eq!(report.generation, 3);
        assert!(!report.evidence.is_empty());
        let encoded = crate::coordinator::completion_message(&report);
        assert!(encoded.starts_with("WORKER_COMPLETION "));
        assert!(encoded.contains("sha256:deadbeef"));
    }
}
