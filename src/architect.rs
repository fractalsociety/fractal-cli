//! Hierarchical master-architect loop for bounded specialist teams.
//!
//! Each admitted team is exactly one leader plus five workers. The architect
//! chooses a coherent five-node mission; the leader owns member assignment and
//! review through Squad. Host pressure and measured product health are hard
//! admission gates, so "continuous" never means unbounded process creation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::cli::ArchitectArgs;

const STATE_SCHEMA: &str = "fractal.architect_state.v1";
const STATUS_SCHEMA: &str = "fractal.architect_status.v1";
const TEAM_SIZE: usize = 6;
const WORKERS_PER_TEAM: usize = 5;
// The scale-42 queue benchmark is the largest live envelope verified by this
// project. Backpressure begins above it; within it, CPU/memory/frontier gates
// decide whether a six-agent team is actually safe to admit.
const MAX_PLANNER_BACKLOG: usize = 42;
const TEAM_COOLDOWN_MS: u64 = 60_000;
const WORKER_HEARTBEAT_STALE_SECS: u64 = 300;
const WORKER_RECOVERY_COOLDOWN_MS: u64 = 120_000;
const MASTER_ARCHITECT_MODEL: &str = "gpt-5.6-sol";
const LEADER_MODEL: &str = "gpt-5.6-sol";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ResourceSnapshot {
    logical_cores: usize,
    load_1m: f64,
    available_memory_bytes: u64,
    ready_nodes: usize,
    planner_backlog: usize,
    ci_green: bool,
    improvement_bps: i64,
    team_cooldown_ready: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct AdmissionPolicy {
    max_teams: usize,
    max_load_per_core: f64,
    min_free_memory_bytes: u64,
    min_improvement_bps: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct MissionTask {
    node_id: String,
    title: String,
    capability: String,
    instruction: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct TeamRecord {
    team_id: String,
    specialization: String,
    mission: String,
    leader_id: String,
    member_ids: Vec<String>,
    #[serde(default)]
    member_clients: Vec<String>,
    tasks: Vec<MissionTask>,
    status: String,
    process_ids: Vec<u32>,
    #[serde(default)]
    recovery_started_ms: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ArchitectState {
    schema: String,
    stop_requested: bool,
    teams: Vec<TeamRecord>,
    #[serde(default)]
    last_team_started_ms: u64,
}

impl Default for ArchitectState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA.to_owned(),
            stop_requested: false,
            teams: Vec::new(),
            last_team_started_ms: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Admission {
    Admit,
    Refuse(Vec<&'static str>),
}

pub(crate) fn run(args: &ArchitectArgs) -> Result<()> {
    let workspace = canonical_workspace(&args.repo)?;
    let state_path = state_path(&workspace);
    if args.stop {
        let mut state = load_state(&state_path)?;
        state.stop_requested = true;
        persist_state(&state_path, &state)?;
        println!("Architect stop requested for {}", workspace.display());
        return Ok(());
    }
    validate_args(args)?;
    // A stop request terminates the currently running loop. A later explicit
    // invocation is a resume request, so clear the durable latch before the
    // new loop starts.
    let mut state = load_state(&state_path)?;
    if state.stop_requested {
        state.stop_requested = false;
        persist_state(&state_path, &state)?;
    }
    let policy = AdmissionPolicy {
        max_teams: args.max_teams,
        max_load_per_core: args.max_load_per_core,
        min_free_memory_bytes: gib_to_bytes(args.min_free_memory_gib),
        min_improvement_bps: args.min_improvement_bps,
    };
    loop {
        let mut state = load_state(&state_path)?;
        reconcile_teams(&workspace, &mut state)?;
        if args.launch {
            relaunch_stranded_teams(&workspace, &mut state, args.max_teams)?;
        }
        persist_state(&state_path, &state)?;
        if state.stop_requested {
            emit_status(
                args,
                &workspace,
                &state,
                None,
                Admission::Refuse(vec!["stop_requested"]),
            )?;
            break;
        }
        let (tasks, dependency_closed) = frontier_tasks(&workspace)?;
        let governed_prd_incomplete = governed_numbered_prd_incomplete(&workspace)?;
        let snapshot = resource_snapshot(&workspace, tasks.len(), state.last_team_started_ms)?;
        let active_teams = state
            .teams
            .iter()
            .filter(|team| team.status == "launched")
            .count();
        let mut decision = admission_decision(&policy, &snapshot, active_teams);
        decision = allow_bounded_regression_remediation(decision, &tasks);
        decision = allow_dependency_closed_frontier(decision, &dependency_closed);
        let mut formed = None;
        if decision == Admission::Admit {
            let team_tasks = if dependency_closed.len() >= WORKERS_PER_TEAM {
                &dependency_closed
            } else {
                &tasks
            };
            if let Some(mut team) = form_team(team_tasks, &state)? {
                if args.launch {
                    team.status = "launched".to_owned();
                    state.teams.push(team.clone());
                    state.last_team_started_ms = now_ms();
                    persist_state(&state_path, &state)?;
                    match launch_team(&workspace, &team) {
                        Ok(process_ids) => {
                            team.process_ids = process_ids.clone();
                            if let Some(saved) = state
                                .teams
                                .iter_mut()
                                .find(|saved| saved.team_id == team.team_id)
                            {
                                saved.process_ids = process_ids;
                            }
                            persist_state(&state_path, &state)?;
                        }
                        Err(error) => {
                            if let Some(saved) = state
                                .teams
                                .iter_mut()
                                .find(|saved| saved.team_id == team.team_id)
                            {
                                saved.status = "failed_released".to_owned();
                            }
                            persist_state(&state_path, &state)?;
                            return Err(error);
                        }
                    }
                }
                formed = Some(team);
            } else {
                decision = Admission::Refuse(vec!["fragmented_specialist_frontier"]);
                if !governed_prd_incomplete
                    && snapshot.planner_backlog == 0
                    && !crate::amendments::has_pending(&workspace)
                {
                    queue_team_mission(&workspace, &state)?;
                }
            }
        } else if decision == Admission::Refuse(vec!["insufficient_specialist_frontier"])
            && !governed_prd_incomplete
            && snapshot.planner_backlog == 0
            && !crate::amendments::has_pending(&workspace)
        {
            queue_team_mission(&workspace, &state)?;
        }
        emit_status(args, &workspace, &state, formed.as_ref(), decision)?;
        if args.once {
            break;
        }
        thread::sleep(Duration::from_secs(args.poll_secs.max(1)));
    }
    Ok(())
}

fn validate_args(args: &ArchitectArgs) -> Result<()> {
    if !args.max_load_per_core.is_finite() || args.max_load_per_core <= 0.0 {
        bail!("--max-load-per-core must be finite and positive");
    }
    if !args.min_free_memory_gib.is_finite() || args.min_free_memory_gib < 0.0 {
        bail!("--min-free-memory-gib must be finite and non-negative");
    }
    Ok(())
}

fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    let workspace = path.canonicalize().context("resolve architect workspace")?;
    if !workspace.join(".fractal/project.fractal").is_file() {
        bail!("architect workspace has no .fractal/project.fractal");
    }
    Ok(workspace)
}

fn state_path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal/architect-state.json")
}

fn load_state(path: &Path) -> Result<ArchitectState> {
    if !path.is_file() {
        return Ok(ArchitectState::default());
    }
    let state: ArchitectState = serde_json::from_slice(&fs::read(path)?)?;
    if state.schema != STATE_SCHEMA {
        bail!("unsupported architect state schema");
    }
    Ok(state)
}

fn persist_state(path: &Path, state: &ArchitectState) -> Result<()> {
    let parent = path.parent().context("architect state has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(state)?)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

pub(crate) fn reserved_node_ids(workspace: &Path) -> BTreeSet<String> {
    load_state(&state_path(workspace))
        .ok()
        .into_iter()
        .flat_map(|state| state.teams)
        .filter(|team| team_reserves_nodes(team))
        .flat_map(|team| team.tasks.into_iter().map(|task| task.node_id))
        .collect()
}

fn team_reserves_nodes(team: &TeamRecord) -> bool {
    matches!(team.status.as_str(), "planned" | "launched")
}

pub(crate) fn enabled(workspace: &Path) -> bool {
    load_state(&state_path(workspace))
        .ok()
        .is_some_and(|state| !state.stop_requested)
}

pub(crate) fn checkout_authorized(workspace: &Path, agent_id: &str, node_id: &str) -> bool {
    load_state(&state_path(workspace))
        .ok()
        .is_some_and(|state| checkout_authorized_in(&state, agent_id, node_id))
}

fn checkout_authorized_in(state: &ArchitectState, agent_id: &str, node_id: &str) -> bool {
    state.teams.iter().any(|team| {
        team.status == "launched"
            && team
                .member_ids
                .iter()
                .position(|member| member == agent_id)
                .and_then(|index| team.tasks.get(index))
                .is_some_and(|task| task.node_id == node_id)
    })
}

fn reconcile_teams(workspace: &Path, state: &mut ArchitectState) -> Result<()> {
    let document: Value =
        serde_json::from_slice(&fs::read(workspace.join(".fractal/project.fractal"))?)?;
    let assignments = document
        .pointer("/execution/assignments")
        .and_then(Value::as_object);
    let now_ms = now_ms();
    let now_secs = now_ms / 1_000;
    // Heartbeat discovery is an additional health signal, not a prerequisite
    // for preserving work. If Squad itself is unavailable, process-based
    // reconciliation remains fail-safe and does not kill workers blindly.
    let heartbeats = squad_worker_heartbeats(workspace).ok();
    for team in &mut state.teams {
        let owned_checked_out = team_has_checked_out_work(team, assignments);
        if team.status == "completed" && owned_checked_out {
            team.status = if team.process_ids.iter().any(|pid| process_alive(*pid)) {
                "launched"
            } else {
                "failed_released"
            }
            .to_owned();
        }
        if team.status != "launched" {
            continue;
        }
        let complete = !owned_checked_out
            && team.tasks.iter().all(|task| {
                assignments
                    .and_then(|values| values.get(&task.node_id))
                    .and_then(|assignment| assignment.get("state"))
                    .and_then(Value::as_str)
                    == Some("completed")
            });
        if let Some(status) = team_terminal_status(team, owned_checked_out, complete) {
            team.status = status.to_owned();
            continue;
        }
        if !team.process_ids.iter().any(|pid| process_alive(*pid)) {
            team.status = "failed_released".to_owned();
            continue;
        }
        if let Some(heartbeats) = heartbeats.as_ref() {
            recover_stale_checked_out_workers(
                workspace,
                team,
                assignments,
                heartbeats,
                now_secs,
                now_ms,
            )?;
        }
        recover_dead_unstarted_workers(workspace, team, assignments, now_ms)?;
        recover_dead_leader(workspace, team, now_ms)?;
    }
    Ok(())
}

fn recover_dead_unstarted_workers(
    workspace: &Path,
    team: &mut TeamRecord,
    assignments: Option<&serde_json::Map<String, Value>>,
    current_ms: u64,
) -> Result<()> {
    // Fresh teams store five worker PIDs in member order plus one leader PID.
    // Older partial-recovery records did not preserve that shape; leave them
    // to the checked-out recovery path instead of guessing PID ownership.
    if team.process_ids.len() != TEAM_SIZE {
        return Ok(());
    }
    let fallback = mixed_worker_roster(WORKERS_PER_TEAM);
    for index in 0..WORKERS_PER_TEAM {
        let Some(task) = team.tasks.get(index) else {
            continue;
        };
        if assignments.is_some_and(|values| values.contains_key(&task.node_id))
            || process_alive(team.process_ids[index])
        {
            continue;
        }
        let member = &team.member_ids[index];
        if team.recovery_started_ms.get(member).is_some_and(|started| {
            current_ms.saturating_sub(*started) < WORKER_RECOVERY_COOLDOWN_MS
        }) {
            continue;
        }
        let client = fallback.get(index).map(String::as_str).unwrap_or("codex");
        let prompt = worker_launch_prompt(workspace, team, index)?;
        team.process_ids[index] = spawn_agent(workspace, client, &prompt)?;
        if index < team.member_clients.len() {
            team.member_clients[index] = client.to_owned();
        }
        team.recovery_started_ms.insert(member.clone(), current_ms);
    }
    Ok(())
}

fn recover_dead_leader(workspace: &Path, team: &mut TeamRecord, current_ms: u64) -> Result<()> {
    recover_dead_leader_with(
        workspace,
        team,
        current_ms,
        process_alive,
        |workspace, model, effort, prompt| spawn_codex(workspace, model, effort, prompt),
    )
}

fn recover_dead_leader_with<Alive, Spawn>(
    workspace: &Path,
    team: &mut TeamRecord,
    current_ms: u64,
    process_is_alive: Alive,
    mut spawn_leader: Spawn,
) -> Result<()>
where
    Alive: Fn(u32) -> bool,
    Spawn: FnMut(&Path, &str, &str, &str) -> Result<u32>,
{
    // Fresh teams store five worker PIDs followed by one leader PID. Older
    // partial records cannot safely identify the leader, so leave them alone.
    if team.process_ids.len() != TEAM_SIZE {
        return Ok(());
    }
    let leader_index = WORKERS_PER_TEAM;
    if process_is_alive(team.process_ids[leader_index]) {
        return Ok(());
    }
    if team
        .recovery_started_ms
        .get(&team.leader_id)
        .is_some_and(|started| current_ms.saturating_sub(*started) < WORKER_RECOVERY_COOLDOWN_MS)
    {
        return Ok(());
    }

    let prompt = leader_launch_prompt(team)?;
    let replacement = spawn_leader(workspace, LEADER_MODEL, "high", &prompt)?;
    team.process_ids[leader_index] = replacement;
    team.recovery_started_ms
        .insert(team.leader_id.clone(), current_ms);
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
struct WorkerHeartbeat {
    status: String,
    last_seen: u64,
    archived: bool,
}

fn squad_worker_heartbeats(workspace: &Path) -> Result<BTreeMap<String, WorkerHeartbeat>> {
    let output = Command::new("squad")
        .args(["agents", "--all", "--json"])
        .current_dir(workspace)
        .output()
        .context("inspect specialist worker heartbeats")?;
    if !output.status.success() {
        bail!(
            "squad heartbeat inspection failed: {}",
            bounded(&String::from_utf8_lossy(&output.stderr), 500)
        );
    }
    Ok(parse_worker_heartbeats(&output.stdout))
}

fn parse_worker_heartbeats(bytes: &[u8]) -> BTreeMap<String, WorkerHeartbeat> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|value| value.get("role").and_then(Value::as_str) == Some("worker"))
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.to_owned();
            Some((
                id,
                WorkerHeartbeat {
                    status: value
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                    last_seen: value.get("last_seen").and_then(Value::as_u64).unwrap_or(0),
                    archived: value
                        .get("archived_at")
                        .is_some_and(|value| !value.is_null()),
                },
            ))
        })
        .collect()
}

fn heartbeat_is_fresh(heartbeat: Option<&WorkerHeartbeat>, now_secs: u64) -> bool {
    heartbeat.is_some_and(|heartbeat| {
        !heartbeat.archived
            && matches!(heartbeat.status.as_str(), "active" | "idle")
            && heartbeat.last_seen > 0
            && heartbeat.last_seen <= now_secs
            && now_secs.saturating_sub(heartbeat.last_seen) <= WORKER_HEARTBEAT_STALE_SECS
    })
}

/// Return the freshest Squad heartbeat belonging to one canonical Fractal
/// worker identity. Squad appends a numeric collision suffix when another
/// live session already owns the canonical ID (for example,
/// `team-worker-1-2`). Only the exact ID and an ASCII-numeric `-N` suffix are
/// aliases; arbitrary prefixes and textual suffixes must never keep a stale
/// checkout alive.
fn freshest_worker_heartbeat<'a>(
    heartbeats: &'a BTreeMap<String, WorkerHeartbeat>,
    member: &str,
    now_secs: u64,
) -> Option<&'a WorkerHeartbeat> {
    let alias_prefix = format!("{member}-");
    heartbeats
        .iter()
        .filter(|(id, _)| {
            id.as_str() == member
                || id.strip_prefix(&alias_prefix).is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
        .max_by(|(left_id, left), (right_id, right)| {
            heartbeat_is_fresh(Some(left), now_secs)
                .cmp(&heartbeat_is_fresh(Some(right), now_secs))
                .then_with(|| {
                    left.last_seen
                        .cmp(&right.last_seen)
                        // Prefer the canonical ID for an equal timestamp so the
                        // selection is deterministic without changing freshness.
                        .then_with(|| {
                            (left_id.as_str() == member).cmp(&(right_id.as_str() == member))
                        })
                        .then_with(|| left_id.cmp(right_id))
                })
        })
        .map(|(_, heartbeat)| heartbeat)
}

fn worker_requires_recovery(
    owns_checkout: bool,
    heartbeat: Option<&WorkerHeartbeat>,
    now_secs: u64,
    recovery_started_ms: Option<u64>,
    current_ms: u64,
) -> bool {
    owns_checkout
        && !heartbeat_is_fresh(heartbeat, now_secs)
        && recovery_started_ms
            .is_none_or(|started| current_ms.saturating_sub(started) >= WORKER_RECOVERY_COOLDOWN_MS)
}

fn recover_stale_checked_out_workers(
    workspace: &Path,
    team: &mut TeamRecord,
    assignments: Option<&serde_json::Map<String, Value>>,
    heartbeats: &BTreeMap<String, WorkerHeartbeat>,
    now_secs: u64,
    current_ms: u64,
) -> Result<()> {
    let Some(assignments) = assignments else {
        return Ok(());
    };
    for index in 0..team.member_ids.len() {
        let member = team.member_ids[index].clone();
        let Some(task) = team.tasks.get(index).cloned() else {
            continue;
        };
        let client = team
            .member_clients
            .get(index)
            .map(String::as_str)
            .unwrap_or("codex");
        let owns_checkout = assignments.get(&task.node_id).is_some_and(|assignment| {
            assignment.get("state").and_then(Value::as_str) == Some("checked_out")
                && assignment.get("agent_id").and_then(Value::as_str) == Some(member.as_str())
        });
        let heartbeat = freshest_worker_heartbeat(heartbeats, &member, now_secs);
        if !owns_checkout || heartbeat_is_fresh(heartbeat, now_secs) {
            team.recovery_started_ms.remove(&member);
            continue;
        }
        if !worker_requires_recovery(
            owns_checkout,
            heartbeat,
            now_secs,
            team.recovery_started_ms.get(&member).copied(),
            current_ms,
        ) {
            continue;
        }

        if let Some(pid) = team.process_ids.get(index).copied() {
            terminate_codex_process(pid);
        }
        let prompt = format!(
            "Recover stale Fractal specialist {member} in team {team_id}. The canonical graph node {node_id} ({title:?}) remains checked out to this exact identity in {repo}. Join Squad with this exact ID as role worker, send {leader} a recovery-ready message, inspect and preserve all partial work, then finish only this instruction: {instruction:?}. While working, send a direct WORKER_HEARTBEAT for {node_id} to {leader} at least once every 60 seconds and after every long-running verification command. Complete or explicitly release the node with Fractal using identity {member}, report evidence to {leader}, and never claim another node.",
            member = member,
            team_id = team.team_id,
            node_id = task.node_id,
            title = task.title,
            repo = workspace.display(),
            leader = team.leader_id,
            instruction = task.instruction,
        );
        let replacement = spawn_agent(workspace, client, &prompt)?;
        if index < team.process_ids.len() {
            team.process_ids[index] = replacement;
        } else {
            team.process_ids.resize(index, 0);
            team.process_ids.push(replacement);
        }
        team.recovery_started_ms.insert(member, current_ms);
    }
    Ok(())
}

fn terminate_codex_process(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    {
        // Codex uses a small wrapper/worker process tree. Terminate children
        // first, then the tracked wrapper; never target a process group or an
        // unvalidated broad pattern.
        let _ = Command::new("pkill")
            .args(["-TERM", "-P", &pid.to_string()])
            .status();
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
}

fn team_has_checked_out_work(
    team: &TeamRecord,
    assignments: Option<&serde_json::Map<String, Value>>,
) -> bool {
    assignments.is_some_and(|values| {
        values.values().any(|assignment| {
            assignment.get("state").and_then(Value::as_str) == Some("checked_out")
                && assignment
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .is_some_and(|agent| team.member_ids.iter().any(|member| member == agent))
        })
    })
}

fn relaunch_stranded_teams(
    workspace: &Path,
    state: &mut ArchitectState,
    max_teams: usize,
) -> Result<()> {
    let document: Value =
        serde_json::from_slice(&fs::read(workspace.join(".fractal/project.fractal"))?)?;
    let assignments = document
        .pointer("/execution/assignments")
        .and_then(Value::as_object);
    let mut active = state
        .teams
        .iter()
        .filter(|team| team.status == "launched")
        .count();
    for team in &mut state.teams {
        if team.status != "failed_released" || (max_teams != 0 && active >= max_teams) {
            continue;
        }
        let stranded = stranded_team_assignments(&document, team, assignments);
        if stranded.is_empty() {
            continue;
        }
        let mut pids = Vec::with_capacity(stranded.len() + 1);
        for (member, task) in &stranded {
            let prompt = format!(
                "Recover existing Fractal team {team_id} as exact worker {member}. Node {node_id} ({title:?}) is already atomically checked out to this identity in {repo}; do not checkout or claim another node. Inspect and preserve existing partial work, finish only this instruction: {instruction:?}. Run focused verification, then complete or release {node_id} with fractal node using agent ID and label {member}. Send concise evidence to {leader}. Do not enter an indefinite receive loop and do not exit before completing or explicitly releasing the node.",
                team_id = team.team_id,
                member = member,
                node_id = task.node_id,
                title = task.title,
                repo = workspace.display(),
                instruction = task.instruction,
                leader = team.leader_id,
            );
            let member_index = team
                .member_ids
                .iter()
                .position(|candidate| candidate == member);
            let client = member_index
                .and_then(|index| team.member_clients.get(index))
                .map(String::as_str)
                .unwrap_or("codex");
            pids.push(spawn_agent(workspace, client, &prompt)?);
        }
        let recovery_members: Vec<&str> =
            stranded.iter().map(|(member, _)| member.as_str()).collect();
        let leader_prompt = format!(
            "Recover specialist team {team_id} as manager {leader}. These workers already own unfinished graph nodes: {members}. Join Squad with the exact leader ID, monitor their evidence, request bounded rework when needed, and ensure every owned node is completed or explicitly released. Do not implement worker tasks and do not create new assignments.",
            team_id = team.team_id,
            leader = team.leader_id,
            members = serde_json::to_string(&recovery_members)?,
        );
        pids.push(spawn_codex(
            workspace,
            LEADER_MODEL,
            "high",
            &leader_prompt,
        )?);
        team.process_ids = pids;
        team.status = "launched".to_owned();
        active += 1;
    }
    Ok(())
}

fn stranded_team_assignments(
    document: &Value,
    team: &TeamRecord,
    assignments: Option<&serde_json::Map<String, Value>>,
) -> Vec<(String, MissionTask)> {
    let Some(assignments) = assignments else {
        return Vec::new();
    };
    let nodes = document.pointer("/graph/nodes").and_then(Value::as_array);
    let mut stranded = Vec::new();
    for (node_id, assignment) in assignments {
        if assignment.get("state").and_then(Value::as_str) != Some("checked_out") {
            continue;
        }
        let Some(member) = assignment.get("agent_id").and_then(Value::as_str) else {
            continue;
        };
        if !team.member_ids.iter().any(|candidate| candidate == member) {
            continue;
        }
        let node = nodes
            .into_iter()
            .flatten()
            .find(|node| node.get("id").and_then(Value::as_str) == Some(node_id.as_str()));
        let task = node.map_or_else(
            || MissionTask {
                node_id: node_id.clone(),
                title: node_id.clone(),
                capability: "code.generate".to_owned(),
                instruction: "Finish and verify the already checked-out graph node.".to_owned(),
            },
            |node| MissionTask {
                node_id: node_id.clone(),
                title: node
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(node_id)
                    .to_owned(),
                capability: node
                    .get("capability")
                    .and_then(Value::as_str)
                    .unwrap_or("code.generate")
                    .to_owned(),
                instruction: bounded(
                    node.get("instruction")
                        .or_else(|| node.get("objective"))
                        .and_then(Value::as_str)
                        .unwrap_or("Finish and verify the already checked-out graph node."),
                    12_000,
                ),
            },
        );
        stranded.push((member.to_owned(), task));
    }
    stranded.sort_by(|left, right| left.1.node_id.cmp(&right.1.node_id));
    stranded
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        if waited == pid as i32 {
            return false;
        }
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat=,command="])
            .output();
        output.ok().is_some_and(|output| {
            output.status.success()
                && tracked_agent_process_record_alive(&String::from_utf8_lossy(&output.stdout))
        })
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn tracked_agent_process_record_alive(record: &str) -> bool {
    let record = record.trim();
    let Some((status, command)) = record.split_once(char::is_whitespace) else {
        return false;
    };
    if status.starts_with('Z') {
        return false;
    }
    let command = command.trim_start();
    command.contains("codex exec")
        || (command.contains("cursor-agent") && command.contains(" -p"))
        || (command.contains("hermes") && command.contains("--yolo"))
        || (command.contains("claude") && command.contains(" -p"))
}

#[allow(dead_code)]
fn ready_tasks(workspace: &Path) -> Result<Vec<MissionTask>> {
    Ok(frontier_tasks(workspace)?.0)
}

fn frontier_tasks(workspace: &Path) -> Result<(Vec<MissionTask>, Vec<MissionTask>)> {
    let document: Value =
        serde_json::from_slice(&fs::read(workspace.join(".fractal/project.fractal"))?)?;
    let reserved = reserved_node_ids(workspace);
    let graph_hash = document
        .get("graph_hash")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let ledger = document
        .get("external_gate_ledger")
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    let immediate = ready_tasks_from_document_with_gates(&document, &reserved, true)
        .into_iter()
        .filter(|task| {
            let node = document
                .pointer("/graph/nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|node| node.get("id").and_then(Value::as_str) == Some(task.node_id.as_str()));
            node.is_some_and(|node| {
                crate::external_gates::scheduler_admitted(
                    workspace,
                    graph_hash,
                    node,
                    ledger.as_ref(),
                )
            })
        })
        .collect::<Vec<_>>();
    let dependency_closed = if immediate.len() < WORKERS_PER_TEAM {
        dependency_closed_tasks_from_document_with_gates(&document, &reserved, &immediate, true)
            .into_iter()
            .filter(|task| {
                let node = document
                    .pointer("/graph/nodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|node| {
                        node.get("id").and_then(Value::as_str) == Some(task.node_id.as_str())
                    });
                node.is_some_and(|node| {
                    crate::external_gates::scheduler_admitted(
                        workspace,
                        graph_hash,
                        node,
                        ledger.as_ref(),
                    )
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok((immediate, dependency_closed))
}

#[derive(Clone, Debug, PartialEq)]
struct GraphTaskCandidate {
    task: MissionTask,
    dependencies: Vec<String>,
}

#[allow(dead_code)]
fn ready_tasks_from_document(document: &Value, reserved: &BTreeSet<String>) -> Vec<MissionTask> {
    ready_tasks_from_document_with_gates(document, reserved, false)
}

fn ready_tasks_from_document_with_gates(
    document: &Value,
    reserved: &BTreeSet<String>,
    allow_gated: bool,
) -> Vec<MissionTask> {
    let (candidates, completed) = graph_task_candidates_with_gates(document, reserved, allow_gated);
    let mut immediate = candidates
        .values()
        .filter(|candidate| {
            candidate
                .dependencies
                .iter()
                .all(|dependency| completed.contains(dependency))
        })
        .cloned()
        .collect::<Vec<_>>();
    immediate.sort_by(|left, right| left.task.node_id.cmp(&right.task.node_id));
    immediate
        .into_iter()
        .map(|candidate| candidate.task)
        .collect()
}

#[allow(dead_code)]
fn dependency_closed_tasks_from_document(
    document: &Value,
    reserved: &BTreeSet<String>,
    immediate: &[MissionTask],
) -> Vec<MissionTask> {
    dependency_closed_tasks_from_document_with_gates(document, reserved, immediate, false)
}

fn dependency_closed_tasks_from_document_with_gates(
    document: &Value,
    reserved: &BTreeSet<String>,
    immediate: &[MissionTask],
    allow_gated: bool,
) -> Vec<MissionTask> {
    if immediate.len() >= WORKERS_PER_TEAM {
        return immediate.to_vec();
    }
    let (candidates, completed) = graph_task_candidates_with_gates(document, reserved, allow_gated);
    let mut selected = immediate
        .iter()
        .map(|task| task.node_id.clone())
        .collect::<BTreeSet<_>>();
    let mut tasks = immediate.to_vec();

    // When the frontier has a short tail, keep an exact Team-6 pod alive by
    // selecting only a dependency-closed chain. A follower remains assigned
    // to its exact node and waits for the selected predecessor to complete;
    // it must never claim a different node to work around that gate.
    while tasks.len() < WORKERS_PER_TEAM {
        let next = candidates
            .values()
            .filter(|candidate| !selected.contains(&candidate.task.node_id))
            .filter(|candidate| {
                candidate.dependencies.iter().all(|dependency| {
                    completed.contains(dependency) || selected.contains(dependency)
                })
            })
            .filter(|candidate| {
                candidate
                    .dependencies
                    .iter()
                    .any(|dependency| selected.contains(dependency))
            })
            .min_by(|left, right| left.task.node_id.cmp(&right.task.node_id))
            .cloned();
        let Some(candidate) = next else {
            break;
        };
        let in_team_dependencies = candidate
            .dependencies
            .iter()
            .filter(|dependency| selected.contains(*dependency))
            .cloned()
            .collect::<Vec<_>>();
        let mut task = candidate.task;
        task.instruction = dependency_closed_instruction(&task.instruction, &in_team_dependencies);
        selected.insert(task.node_id.clone());
        tasks.push(task);
    }
    tasks
}

#[allow(dead_code)]
fn graph_task_candidates(
    document: &Value,
    reserved: &BTreeSet<String>,
) -> (BTreeMap<String, GraphTaskCandidate>, BTreeSet<String>) {
    graph_task_candidates_with_gates(document, reserved, false)
}

fn graph_task_candidates_with_gates(
    document: &Value,
    reserved: &BTreeSet<String>,
    allow_gated: bool,
) -> (BTreeMap<String, GraphTaskCandidate>, BTreeSet<String>) {
    let assignments = document
        .pointer("/execution/assignments")
        .and_then(Value::as_object);
    let graph = document.get("graph").unwrap_or(&document);
    let edges = graph
        .get("edges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut dependencies = BTreeMap::<String, Vec<String>>::new();
    for edge in &edges {
        // A failure edge is an alternative branch rather than a prerequisite;
        // checkout uses the same success-edge semantics below.
        if edge.get("condition").and_then(Value::as_str) == Some("failure") {
            continue;
        }
        let (Some(from), Some(to)) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        ) else {
            continue;
        };
        dependencies
            .entry(to.to_owned())
            .or_default()
            .push(from.to_owned());
    }
    for values in dependencies.values_mut() {
        values.sort();
        values.dedup();
    }

    let completed = assignments
        .into_iter()
        .flat_map(|values| values.iter())
        .filter_map(|(id, assignment)| {
            (assignment.get("state").and_then(Value::as_str) == Some("completed"))
                .then_some(id.clone())
        })
        .collect::<BTreeSet<_>>();
    let mut candidates = BTreeMap::<String, GraphTaskCandidate>::new();
    for node in graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        let state = assignments
            .and_then(|values| values.get(id))
            .and_then(|value| value.get("state"))
            .and_then(Value::as_str);
        // Only unassigned and explicitly released nodes are claimable. Any
        // other durable state represents an in-flight reservation that must
        // not be silently displaced by the architect.
        if !state.is_none_or(|state| state == "released") {
            continue;
        }
        if reserved.contains(id) || (!allow_gated && has_external_gates(node)) {
            continue;
        }
        candidates.insert(
            id.to_owned(),
            GraphTaskCandidate {
                task: MissionTask {
                    node_id: id.to_owned(),
                    title: node
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    capability: node
                        .get("capability")
                        .and_then(Value::as_str)
                        .unwrap_or("code.generate")
                        .to_owned(),
                    instruction: bounded(
                        node.get("instruction")
                            .and_then(Value::as_str)
                            .unwrap_or("Execute the graph node."),
                        12_000,
                    ),
                },
                dependencies: dependencies.get(id).cloned().unwrap_or_default(),
            },
        );
    }
    (candidates, completed)
}

fn has_external_gates(node: &Value) -> bool {
    match node.get("external_gates") {
        None | Some(Value::Null) => false,
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(values)) => !values.is_empty(),
        Some(Value::String(value)) => !value.trim().is_empty(),
        Some(_) => true,
    }
}

fn dependency_closed_instruction(instruction: &str, dependencies: &[String]) -> String {
    let dependencies = dependencies.join(", ");
    format!(
        "{instruction}\n\nIn-team dependency gate: {dependencies}. Do not modify source or run task work before atomically checking out this exact node. If any in-team dependency is incomplete, wait and retry checkout of this exact node until it becomes ready; never release this assignment or claim another node solely because the dependency is incomplete. If checkout reports an ownership conflict, stop and report the conflict to the leader rather than retrying with another node."
    )
}

fn governed_numbered_prd_incomplete(workspace: &Path) -> Result<bool> {
    let document: Value =
        serde_json::from_slice(&fs::read(workspace.join(".fractal/project.fractal"))?)?;
    Ok(governed_numbered_prd_document_incomplete(&document))
}

fn governed_numbered_prd_document_incomplete(document: &Value) -> bool {
    if document
        .pointer("/graph/source/kind")
        .and_then(Value::as_str)
        != Some("numbered_markdown_prd")
    {
        return false;
    }
    let assignments = document
        .pointer("/execution/assignments")
        .and_then(Value::as_object);
    document
        .pointer("/graph/nodes")
        .and_then(Value::as_array)
        .is_some_and(|nodes| {
            nodes
                .iter()
                .filter_map(|node| node.get("id").and_then(Value::as_str))
                .any(|node_id| {
                    assignments
                        .and_then(|values| values.get(node_id))
                        .and_then(|assignment| assignment.get("state"))
                        .and_then(Value::as_str)
                        != Some("completed")
                })
        })
}

fn specialization(capability: &str) -> String {
    if capability.contains("test") || capability.contains("verify") {
        "verification"
    } else if capability.contains("security") {
        "security"
    } else if capability.contains("analy") || capability.contains("plan") {
        "architecture"
    } else if capability.contains("data") || capability.contains("market") {
        "economics-data"
    } else {
        "implementation"
    }
    .to_owned()
}

fn form_team(tasks: &[MissionTask], state: &ArchitectState) -> Result<Option<TeamRecord>> {
    let occupied: BTreeSet<&str> = state
        .teams
        .iter()
        .filter(|team| team_reserves_nodes(team))
        .flat_map(|team| team.tasks.iter().map(|task| task.node_id.as_str()))
        .collect();
    let mut buckets: BTreeMap<String, Vec<MissionTask>> = BTreeMap::new();
    for task in tasks
        .iter()
        .filter(|task| !occupied.contains(task.node_id.as_str()))
    {
        let bucket = architect_team_cohort(&task.node_id)
            .map(|cohort| format!("cross-functional-{cohort}"))
            .unwrap_or_else(|| specialization(&task.capability));
        buckets.entry(bucket).or_default().push(task.clone());
    }
    // Keep the existing specialist-lane preference whenever one complete lane
    // is available. Only a genuinely fragmented frontier may use the mixed
    // fallback; this prevents a mixed team from displacing a coherent one.
    let homogeneous_skill = buckets
        .iter()
        .find(|(_, candidates)| candidates.len() >= WORKERS_PER_TEAM)
        .map(|(skill, _)| skill.clone());
    let (skill, mut candidates) = if let Some(skill) = homogeneous_skill {
        let candidates = buckets
            .remove(&skill)
            .expect("homogeneous bucket was found in the bucket map");
        (skill, candidates)
    } else {
        let candidates = buckets.into_values().flatten().collect::<Vec<_>>();
        if candidates.len() < WORKERS_PER_TEAM {
            return Ok(None);
        }
        ("cross-functional-frontier".to_owned(), candidates)
    };
    candidates.sort_by(|left, right| {
        (!is_explicit_regression_repair(left))
            .cmp(&(!is_explicit_regression_repair(right)))
            .then(left.node_id.cmp(&right.node_id))
    });
    let selected: Vec<MissionTask> = candidates.into_iter().take(WORKERS_PER_TEAM).collect();
    let mission = if selected
        .iter()
        .any(|task| task.instruction.contains("In-team dependency gate:"))
    {
        format!(
            "Complete and verify five dependency-closed {skill} graph nodes in prerequisite order with preserved ownership and evidence."
        )
    } else {
        format!(
            "Complete and verify five independent {skill} graph nodes with preserved ownership and evidence."
        )
    };
    let mut team_identity = selected
        .iter()
        .flat_map(|task| task.node_id.as_bytes())
        .copied()
        .collect::<Vec<_>>();
    team_identity.extend_from_slice(&state.teams.len().to_le_bytes());
    let digest = Sha256::digest(team_identity);
    let short_hash = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let team_id = format!("{skill}-{short_hash}");
    let leader_id = format!("{team_id}-leader");
    let member_ids = (1..=WORKERS_PER_TEAM)
        .map(|index| format!("{team_id}-worker-{index}"))
        .collect();
    let member_clients = mixed_worker_roster(WORKERS_PER_TEAM);
    Ok(Some(TeamRecord {
        team_id,
        specialization: skill.clone(),
        mission,
        leader_id,
        member_ids,
        member_clients,
        tasks: selected,
        status: "planned".to_owned(),
        process_ids: Vec::new(),
        recovery_started_ms: BTreeMap::new(),
    }))
}

fn team_terminal_status(
    team: &TeamRecord,
    owned_checked_out: bool,
    complete: bool,
) -> Option<&'static str> {
    if complete {
        return Some("completed");
    }
    if team.status == "launched"
        && !owned_checked_out
        && !team.process_ids.is_empty()
        && team.process_ids.len() != TEAM_SIZE
    {
        return Some("failed_released");
    }
    None
}

fn architect_team_cohort(node_id: &str) -> Option<&str> {
    let mut segments = node_id.split('.');
    (segments.next() == Some("branch"))
        .then(|| segments.next())
        .flatten()
        .filter(|segment| segment.starts_with("architect-team-"))
}

fn admission_decision(
    policy: &AdmissionPolicy,
    snapshot: &ResourceSnapshot,
    active_teams: usize,
) -> Admission {
    let mut reasons = Vec::new();
    if policy.max_teams != 0 && active_teams >= policy.max_teams {
        reasons.push("team_cap_reached");
    }
    if snapshot.logical_cores == 0
        || snapshot.load_1m / snapshot.logical_cores.max(1) as f64 > policy.max_load_per_core
    {
        reasons.push("cpu_load_limit");
    }
    if snapshot.available_memory_bytes < policy.min_free_memory_bytes {
        reasons.push("memory_limit");
    }
    if snapshot.ready_nodes < WORKERS_PER_TEAM {
        reasons.push("insufficient_specialist_frontier");
    }
    if snapshot.planner_backlog > MAX_PLANNER_BACKLOG {
        reasons.push("planner_backlog_limit");
    }
    if !snapshot.ci_green {
        reasons.push("regression_gate_failed");
    }
    if snapshot.improvement_bps < policy.min_improvement_bps {
        reasons.push("improvement_gate_failed");
    }
    if !snapshot.team_cooldown_ready {
        reasons.push("team_cooldown");
    }
    if reasons.is_empty() {
        Admission::Admit
    } else {
        Admission::Refuse(reasons)
    }
}

fn allow_bounded_regression_remediation(decision: Admission, tasks: &[MissionTask]) -> Admission {
    let Admission::Refuse(mut reasons) = decision else {
        return decision;
    };
    let has_explicit_repair = tasks.iter().any(is_explicit_regression_repair);
    if has_explicit_repair {
        reasons.retain(|reason| *reason != "regression_gate_failed");
    }
    if reasons.is_empty() {
        Admission::Admit
    } else {
        Admission::Refuse(reasons)
    }
}

fn allow_dependency_closed_frontier(
    decision: Admission,
    dependency_closed: &[MissionTask],
) -> Admission {
    let Admission::Refuse(mut reasons) = decision else {
        return decision;
    };
    if dependency_closed.len() == WORKERS_PER_TEAM {
        // A closed in-graph chain supplies the fifth worker, but it does not
        // waive any resource, quality, cap, or cooldown gate.
        reasons.retain(|reason| *reason != "insufficient_specialist_frontier");
    }
    if reasons.is_empty() {
        Admission::Admit
    } else {
        Admission::Refuse(reasons)
    }
}

fn is_explicit_regression_repair(task: &MissionTask) -> bool {
    let contract = format!("{}\n{}", task.title, task.instruction).to_ascii_lowercase();
    contract.contains("repair")
        && contract.contains("ci")
        && contract.contains("all must pass")
        && contract.contains("weaken")
}

fn resource_snapshot(
    workspace: &Path,
    ready_nodes: usize,
    last_team_started_ms: u64,
) -> Result<ResourceSnapshot> {
    let current_ms = now_ms();
    Ok(ResourceSnapshot {
        logical_cores: thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1),
        load_1m: load_average(),
        available_memory_bytes: available_memory_bytes(),
        ready_nodes,
        planner_backlog: planner_backlog(workspace)?,
        ci_green: ci_green(workspace),
        improvement_bps: measured_improvement_bps(workspace),
        team_cooldown_ready: last_team_started_ms == 0
            || current_ms.saturating_sub(last_team_started_ms) >= TEAM_COOLDOWN_MS,
    })
}

fn load_average() -> f64 {
    let mut values = [0.0_f64; 3];
    let read = unsafe { libc::getloadavg(values.as_mut_ptr(), values.len() as i32) };
    if read > 0 {
        values[0]
    } else {
        f64::INFINITY
    }
}

fn available_memory_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(raw) = fs::read_to_string("/proc/meminfo") {
            if let Some(kib) = raw.lines().find_map(|line| {
                line.strip_prefix("MemAvailable:")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse::<u64>().ok())
            }) {
                return kib.saturating_mul(1024);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = Command::new("vm_stat").output() {
            if output.status.success() {
                if let Some(bytes) = parse_vm_stat(&String::from_utf8_lossy(&output.stdout)) {
                    return bytes;
                }
            }
        }
    }
    0
}

fn parse_vm_stat(raw: &str) -> Option<u64> {
    let page_size = raw
        .lines()
        .next()?
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    let pages = raw
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            matches!(name, "Pages free" | "Pages inactive" | "Pages speculative")
                .then(|| value.trim().trim_end_matches('.').parse::<u64>().ok())
                .flatten()
        })
        .sum::<u64>();
    Some(pages.saturating_mul(page_size))
}

fn planner_backlog(workspace: &Path) -> Result<usize> {
    let directory = workspace.join(".fractal");
    let mut count = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "pending-amendments.jsonl" || name.starts_with("pending-amendments.processing-")
        {
            count += fs::read_to_string(entry.path())?
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
        }
    }
    Ok(count)
}

fn ci_green(workspace: &Path) -> bool {
    fs::read(workspace.join("artifacts/ci/report.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|value| value.get("passed").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn measured_improvement_bps(workspace: &Path) -> i64 {
    fs::read(workspace.join(".fractal/architect-metrics.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|value| value.get("improvement_bps").and_then(Value::as_i64))
        .unwrap_or(0)
}

fn launch_team(workspace: &Path, team: &TeamRecord) -> Result<Vec<u32>> {
    let mut pids = Vec::with_capacity(TEAM_SIZE);
    for (index, _assignment) in team.member_ids.iter().zip(&team.tasks).enumerate() {
        let client = team
            .member_clients
            .get(index)
            .map(String::as_str)
            .unwrap_or("codex");
        let prompt = worker_launch_prompt(workspace, team, index)?;
        pids.push(spawn_agent(workspace, client, &prompt)?);
    }
    let prompt = leader_launch_prompt(team)?;
    pids.push(spawn_codex(workspace, LEADER_MODEL, "high", &prompt)?);
    Ok(pids)
}

fn leader_launch_prompt(team: &TeamRecord) -> Result<String> {
    let assignments: Vec<Value> = team
        .tasks
        .iter()
        .zip(&team.member_ids)
        .map(|(task, member)| {
            json!({"member":member,"node_id":task.node_id,"title":task.title,"instruction":task.instruction})
        })
        .collect();
    Ok(format!(
        "You are specialist squad leader {leader} for mission {mission:?}. Join Squad with this exact ID as role manager. Immediately assign exactly one of these five graph tasks to each named member using direct squad send messages; include the exact node ID and checkout/verification acceptance criteria. Do not create a redundant structured-task acknowledgement gate: atomic Fractal checkout is authoritative ownership. Some assignments may include an in-team dependency gate. For those, instruct the member to wait and retry checkout of that exact assigned node until its listed dependencies complete; never release or substitute another node solely for an incomplete dependency. An ownership conflict is different: stop that assignment and report the conflict. Track readiness and results, inspect every result, request bounded rework on failure, and report the team outcome to master-architect. Keep receiving until all five graph nodes are complete or explicitly released. Do not implement member tasks yourself or allow work before successful checkout. Assignments: {assignments}",
        leader = team.leader_id,
        mission = team.mission,
        assignments = serde_json::to_string(&assignments)?
    ))
}

fn worker_launch_prompt(workspace: &Path, team: &TeamRecord, index: usize) -> Result<String> {
    let member = team
        .member_ids
        .get(index)
        .context("team member is missing")?;
    let task = team.tasks.get(index).context("team task is missing")?;
    Ok(format!(
        "You are specialist team member {member} in team {team_id}. Your master-authorized leader assignment is node {node_id} ({title:?}) from {leader}. Join Squad with this exact ID as role worker, send {leader} a ready message, and receive its assignment message. Do not add a second acknowledgement gate: the leader message plus atomic Fractal checkout is the ownership record. If receive times out, retry instead of exiting. Do not modify source or run task work before successful checkout. Atomically checkout exactly {node_id} in {repo} with agent ID and label {member}, implementing only this instruction: {instruction:?}. If the instruction names an in-team dependency and checkout reports it incomplete, wait and retry checkout of this exact node; never release or claim another node solely because that dependency is incomplete. If checkout reports an ownership conflict, stop and report the conflict to {leader}. While working, send a direct WORKER_HEARTBEAT for {node_id} to {leader} at least once every 60 seconds and after every long-running verification command. Verify it, complete or release it with evidence using the same identity, report the result to {leader}, then wait for rework or closure. Never claim another graph node.",
        team_id = team.team_id,
        leader = team.leader_id,
        node_id = task.node_id,
        title = task.title,
        repo = workspace.display(),
        instruction = task.instruction
    ))
}

fn queue_team_mission(workspace: &Path, state: &ArchitectState) -> Result<()> {
    let document = crate::project_file::load(workspace)?;
    // Team amendments graft peers onto the latest existing wave; they do not
    // create a structurally new wave number. Downstream dependencies are
    // rewired by the amendment compiler. Asking for max+1 permanently retries
    // a wave that cannot exist yet and stalls autonomous graph growth.
    let wave = latest_expandable_wave(&document.graph)?;
    let generation = state.teams.len() + 1;
    let command_id = format!("architect-team-{generation:04}");
    let focus = prd_mission_focus(generation);
    let instruction = format!(
        "Continuously improve the product and network with specialist team {generation}. Reconcile the graph project prompt with authoritative PRD, PRD_INDEX, MASTER_PRD, and status documents available in project-related repositories. Prioritize explicitly unfinished original acceptance criteria over more synthetic verification. This team's coherent focus is: {focus}. Use current graph, benchmark, regression, failure, and resource evidence. Produce exactly five implementation-heavy, artifact-disjoint, independently measurable tasks that can be delegated one-per-worker. Every task must name its owned paths, preserve existing authority boundaries, include a deterministic baseline, performance or feature acceptance evidence, rollback/fail-closed behavior, and full regression verification. Do not duplicate completed graph work and do not weaken a gate."
    );
    crate::amendments::queue(
        workspace,
        command_id,
        "add_team_wave",
        "",
        Some(wave),
        &instruction,
        "master_architect",
    )?;
    Ok(())
}

fn prd_mission_focus(generation: usize) -> &'static str {
    const FOCUSES: [&str; 4] = [
        "native runtime and protected execution: FractalRuntime/fractald, capability-limited tool mediation, unified-memory scheduling, and multi-node heartbeats",
        "self-evolving harness and training: real MLX replay, scaffold sensitivity/evolution, harness-following datasets and SFT, and binding deployment attestations",
        "capability economics and capital markets: demand gaps, automatic bounties, demand bonds, tournaments, settlement conservation, and manipulation resistance",
        "agent life economy and product surface: life control plane, rent/compute metering, daemon policies, SII/ladder commitments, player flows, and operator UX",
    ];
    FOCUSES[generation.saturating_sub(1) % FOCUSES.len()]
}

fn latest_expandable_wave(graph: &Value) -> Result<u32> {
    let wave = graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.pointer("/execution/wave").and_then(Value::as_u64))
        .max()
        .unwrap_or(0) as u32;
    if wave == 0 {
        bail!("architect cannot expand a graph without an implementation wave");
    }
    Ok(wave)
}

fn spawn_codex(workspace: &Path, model: &str, effort: &str, prompt: &str) -> Result<u32> {
    let child = Command::new("codex")
        .arg("exec")
        .arg("--model")
        .arg(model)
        .arg("--config")
        .arg(format!("model_reasoning_effort=\"{effort}\""))
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg(prompt)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("launch Codex specialist-team agent")?;
    Ok(child.id())
}

fn mixed_worker_roster(count: usize) -> Vec<String> {
    let disabled = std::env::var("FRACTAL_DISABLED_AGENTS").unwrap_or_default();
    let available = enabled_clients(
        &crate::execute::available_agents(),
        disabled.split(',').map(str::trim),
    );
    mixed_worker_roster_from(&available, count)
}

fn enabled_clients<'a>(
    available: &[String],
    disabled: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let disabled: BTreeSet<&str> = disabled
        .into_iter()
        .filter(|client| !client.is_empty())
        .collect();
    available
        .iter()
        .filter(|client| !disabled.contains(client.as_str()))
        .cloned()
        .collect()
}

fn mixed_worker_roster_from(available: &[String], count: usize) -> Vec<String> {
    let preferred = ["codex", "cursor", "hermes", "claude"];
    let clients: Vec<&str> = preferred
        .into_iter()
        .filter(|client| available.iter().any(|candidate| candidate == client))
        .collect();
    let clients = if clients.is_empty() {
        vec!["codex"]
    } else {
        clients
    };
    (0..count)
        .map(|index| clients[index % clients.len()].to_owned())
        .collect()
}

fn spawn_agent(workspace: &Path, client: &str, prompt: &str) -> Result<u32> {
    let mut command =
        crate::execute::worker_command(client, prompt, crate::execute::AgentRole::Worker)?;
    let child = command
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch {client} specialist-team agent"))?;
    Ok(child.id())
}

fn emit_status(
    args: &ArchitectArgs,
    workspace: &Path,
    state: &ArchitectState,
    formed: Option<&TeamRecord>,
    decision: Admission,
) -> Result<()> {
    let active_teams = state
        .teams
        .iter()
        .filter(|team| team.status == "launched")
        .count();
    let launched_agents = state
        .teams
        .iter()
        .filter(|team| team.status == "launched")
        .map(|team| team.process_ids.len())
        .sum::<usize>();
    let reasons = match &decision {
        Admission::Admit => Vec::new(),
        Admission::Refuse(reasons) => reasons.clone(),
    };
    let report = json!({
        "schema": STATUS_SCHEMA,
        "workspace": workspace,
        "master_architect_model": MASTER_ARCHITECT_MODEL,
        "team_contract": {"team_size":TEAM_SIZE,"leaders":1,"workers":WORKERS_PER_TEAM,"worker_to_leader_ratio":"5:1"},
        "decision": if decision == Admission::Admit { "admit" } else { "refuse" },
        "reasons": reasons,
        "formed_team": formed,
        "active_teams": active_teams,
        "launched_agents": launched_agents,
    });
    if args.json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!(
            "Architect {} · {} active team(s) · decision {}{}",
            MASTER_ARCHITECT_MODEL,
            active_teams,
            report["decision"].as_str().unwrap_or("refuse"),
            if reasons.is_empty() {
                String::new()
            } else {
                format!(" ({})", reasons.join(", "))
            }
        );
        if let Some(team) = formed {
            println!(
                "  planned {}: 1 leader + 5 {} workers",
                team.team_id, team.specialization
            );
        }
    }
    Ok(())
}

fn gib_to_bytes(gib: f64) -> u64 {
    (gib * 1024.0 * 1024.0 * 1024.0).min(u64::MAX as f64) as u64
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn bounded(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> ResourceSnapshot {
        ResourceSnapshot {
            logical_cores: 10,
            load_1m: 8.0,
            available_memory_bytes: 16 << 30,
            ready_nodes: 5,
            planner_backlog: 0,
            ci_green: true,
            improvement_bps: 25,
            team_cooldown_ready: true,
        }
    }

    fn policy() -> AdmissionPolicy {
        AdmissionPolicy {
            max_teams: 0,
            max_load_per_core: 1.25,
            min_free_memory_bytes: 8 << 30,
            min_improvement_bps: 0,
        }
    }

    fn recovery_team() -> TeamRecord {
        let tasks: Vec<MissionTask> = (0..WORKERS_PER_TEAM)
            .map(|index| MissionTask {
                node_id: format!("recovery-{index}"),
                title: format!("Recovery {index}"),
                capability: "code.generate".to_owned(),
                instruction: format!("Finish recovery task {index}."),
            })
            .collect();
        let mut team = form_team(&tasks, &ArchitectState::default())
            .unwrap()
            .unwrap();
        team.status = "launched".to_owned();
        team.process_ids = (100..106).collect();
        team
    }

    fn graph_document(nodes: Value, edges: Value, assignments: Value) -> Value {
        json!({
            "graph": {"nodes": nodes, "edges": edges},
            "execution": {"assignments": assignments}
        })
    }

    fn node(id: &str, capability: &str) -> Value {
        json!({
            "id": id,
            "title": id,
            "capability": capability,
            "instruction": format!("Work on {id}.")
        })
    }

    fn completed(ids: &[&str]) -> Value {
        let mut assignments = serde_json::Map::new();
        for id in ids {
            assignments.insert((*id).to_owned(), json!({"state": "completed"}));
        }
        Value::Object(assignments)
    }

    #[test]
    fn admits_only_a_healthy_complete_team_frontier() {
        assert_eq!(
            admission_decision(&policy(), &healthy(), 0),
            Admission::Admit
        );
        let mut snapshot = healthy();
        snapshot.ready_nodes = 4;
        assert_eq!(
            admission_decision(&policy(), &snapshot, 0),
            Admission::Refuse(vec!["insufficient_specialist_frontier"])
        );
    }

    #[test]
    fn dependency_closed_frontier_only_relaxes_specialist_frontier_gate() {
        let mut snapshot = healthy();
        snapshot.ready_nodes = 4;
        let fallback = vec![
            MissionTask {
                node_id: "follower".to_owned(),
                title: "follower".to_owned(),
                capability: "code.generate".to_owned(),
                instruction: "wait".to_owned(),
            };
            WORKERS_PER_TEAM
        ];
        assert_eq!(
            allow_dependency_closed_frontier(
                admission_decision(&policy(), &snapshot, 0),
                &fallback,
            ),
            Admission::Admit
        );

        snapshot.ci_green = false;
        assert_eq!(
            allow_dependency_closed_frontier(
                admission_decision(&policy(), &snapshot, 0),
                &fallback,
            ),
            Admission::Refuse(vec!["regression_gate_failed"])
        );

        snapshot.load_1m = 20.0;
        let mut capped = policy();
        capped.max_teams = 1;
        assert_eq!(
            allow_dependency_closed_frontier(admission_decision(&capped, &snapshot, 1), &fallback,),
            Admission::Refuse(vec![
                "team_cap_reached",
                "cpu_load_limit",
                "regression_gate_failed"
            ])
        );
    }

    #[test]
    fn incomplete_numbered_prd_suppresses_synthetic_missions() {
        let document = json!({
            "graph": {
                "source": {"kind": "numbered_markdown_prd"},
                "nodes": [{"id": "INT-008"}, {"id": "verify.INT-008"}]
            },
            "execution": {
                "assignments": {
                    "INT-008": {"state": "completed"}
                }
            }
        });
        assert!(governed_numbered_prd_document_incomplete(&document));
    }

    #[test]
    fn completed_numbered_prd_allows_post_graph_evolution() {
        let document = json!({
            "graph": {
                "source": {"kind": "numbered_markdown_prd"},
                "nodes": [{"id": "INT-008"}, {"id": "verify.INT-008"}]
            },
            "execution": {
                "assignments": {
                    "INT-008": {"state": "completed"},
                    "verify.INT-008": {"state": "completed"}
                }
            }
        });
        assert!(!governed_numbered_prd_document_incomplete(&document));
    }

    #[test]
    fn non_prd_graph_keeps_existing_synthetic_mission_behavior() {
        let document = json!({
            "graph": {
                "source": {"kind": "interactive_request"},
                "nodes": [{"id": "feature"}]
            },
            "execution": {"assignments": {}}
        });
        assert!(!governed_numbered_prd_document_incomplete(&document));
    }

    #[test]
    fn dependency_closed_frontier_fills_live_shaped_four_plus_one_pod() {
        let nodes = json!([
            node("INT-009", "code.generate"),
            node("verify.INT-013", "test.verify"),
            node("verify.INT-014", "test.verify"),
            node("verify.INT-049", "test.verify"),
            node("verify.INT-009", "test.verify"),
            node("INT-010", "code.generate")
        ]);
        let edges = json!([
            {"from": "verify.INT-008", "to": "INT-009", "condition": "success"},
            {"from": "INT-009", "to": "verify.INT-009", "condition": "success"},
            {"from": "verify.INT-009", "to": "INT-010", "condition": "success"}
        ]);
        let document = graph_document(
            nodes,
            edges,
            completed(&["verify.INT-008", "INT-013", "INT-014", "INT-049"]),
        );
        let immediate = ready_tasks_from_document(&document, &BTreeSet::new());
        assert_eq!(immediate.len(), 4);
        let tasks = dependency_closed_tasks_from_document(&document, &BTreeSet::new(), &immediate);
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.node_id.as_str())
                .collect::<Vec<_>>(),
            [
                "INT-009",
                "verify.INT-013",
                "verify.INT-014",
                "verify.INT-049",
                "verify.INT-009"
            ]
        );
        let follower = tasks
            .iter()
            .find(|task| task.node_id == "verify.INT-009")
            .expect("dependency follower is selected");
        assert!(follower
            .instruction
            .contains("In-team dependency gate: INT-009"));
        assert!(follower
            .instruction
            .contains("wait and retry checkout of this exact node"));
        assert!(follower
            .instruction
            .contains("never release this assignment or claim another node"));
        assert!(!tasks.iter().any(|task| task.node_id == "INT-010"));
    }

    #[test]
    fn dependency_closed_filler_excludes_unresolved_unselected_dependencies() {
        let nodes = json!([
            node("ready-1", "code.generate"),
            node("ready-2", "code.generate"),
            node("ready-3", "code.generate"),
            node("ready-4", "code.generate"),
            node("blocked", "code.generate")
        ]);
        let edges = json!([
            {"from": "missing", "to": "blocked", "condition": "success"}
        ]);
        let document = graph_document(nodes, edges, json!({}));
        let immediate = ready_tasks_from_document(&document, &BTreeSet::new());
        let tasks = dependency_closed_tasks_from_document(&document, &BTreeSet::new(), &immediate);
        assert_eq!(tasks.len(), 4);
        assert!(!tasks.iter().any(|task| task.node_id == "blocked"));
    }

    #[test]
    fn dependency_closed_frontier_excludes_external_gate_nodes() {
        let mut gated = node("gated", "code.generate");
        gated["external_gates"] = json!(["security_review"]);
        let nodes = json!([
            node("ready-1", "code.generate"),
            node("ready-2", "code.generate"),
            node("ready-3", "code.generate"),
            node("ready-4", "code.generate"),
            gated
        ]);
        let edges = json!([
            {"from": "ready-1", "to": "gated", "condition": "success"}
        ]);
        let document = graph_document(nodes, edges, json!({}));
        let immediate = ready_tasks_from_document(&document, &BTreeSet::new());
        let tasks = dependency_closed_tasks_from_document(&document, &BTreeSet::new(), &immediate);
        assert_eq!(tasks.len(), 4);
        assert!(!tasks.iter().any(|task| task.node_id == "gated"));
    }

    #[test]
    fn immediate_ready_frontier_of_five_is_returned_unchanged_and_preferred() {
        let nodes = json!([
            node("ready-1", "code.generate"),
            node("ready-2", "code.generate"),
            node("ready-3", "code.generate"),
            node("ready-4", "code.generate"),
            node("ready-5", "code.generate"),
            node("follower", "code.generate")
        ]);
        let edges = json!([
            {"from": "ready-1", "to": "follower", "condition": "success"}
        ]);
        let document = graph_document(nodes, edges, json!({}));
        let tasks = ready_tasks_from_document(&document, &BTreeSet::new());
        assert_eq!(tasks.len(), 5);
        assert_eq!(tasks[0].node_id, "ready-1");
        assert!(tasks
            .iter()
            .all(|task| !task.instruction.contains("In-team dependency gate")));
        assert!(!tasks.iter().any(|task| task.node_id == "follower"));
    }

    #[test]
    fn dependency_closed_frontier_excludes_reserved_nodes() {
        let nodes = json!([
            node("ready-1", "code.generate"),
            node("ready-2", "code.generate"),
            node("ready-3", "code.generate"),
            node("ready-4", "code.generate"),
            node("reserved", "code.generate"),
            node("follower", "code.generate")
        ]);
        let edges = json!([
            {"from": "reserved", "to": "follower", "condition": "success"}
        ]);
        let document = graph_document(nodes, edges, json!({}));
        let mut reserved = BTreeSet::new();
        reserved.insert("reserved".to_owned());
        let immediate = ready_tasks_from_document(&document, &reserved);
        let tasks = dependency_closed_tasks_from_document(&document, &reserved, &immediate);
        assert_eq!(tasks.len(), 4);
        assert!(!tasks.iter().any(|task| task.node_id == "reserved"));
        assert!(!tasks.iter().any(|task| task.node_id == "follower"));
    }

    #[test]
    fn leader_and_worker_prompts_preserve_dependency_retry_contract() {
        let tasks = vec![MissionTask {
            node_id: "follower".to_owned(),
            title: "Follower".to_owned(),
            capability: "code.generate".to_owned(),
            instruction: dependency_closed_instruction(
                "Work on follower.",
                &["leader-node".to_owned()],
            ),
        }];
        let mut team = form_team(
            &[
                tasks[0].clone(),
                MissionTask {
                    node_id: "n2".to_owned(),
                    title: "n2".to_owned(),
                    capability: "code.generate".to_owned(),
                    instruction: "work".to_owned(),
                },
                MissionTask {
                    node_id: "n3".to_owned(),
                    title: "n3".to_owned(),
                    capability: "code.generate".to_owned(),
                    instruction: "work".to_owned(),
                },
                MissionTask {
                    node_id: "n4".to_owned(),
                    title: "n4".to_owned(),
                    capability: "code.generate".to_owned(),
                    instruction: "work".to_owned(),
                },
                MissionTask {
                    node_id: "n5".to_owned(),
                    title: "n5".to_owned(),
                    capability: "code.generate".to_owned(),
                    instruction: "work".to_owned(),
                },
            ],
            &ArchitectState::default(),
        )
        .unwrap()
        .unwrap();
        assert!(team.mission.contains("dependency-closed"));
        assert!(!team.mission.contains("independent"));
        team.status = "launched".to_owned();
        let leader = leader_launch_prompt(&team).unwrap();
        let worker = worker_launch_prompt(Path::new("/repo"), &team, 0).unwrap();
        for prompt in [leader, worker] {
            assert!(
                prompt.contains("wait and retry checkout of that exact assigned node")
                    || prompt.contains("wait and retry checkout of this exact node")
            );
            assert!(prompt.contains("ownership conflict"));
            assert!(
                prompt.contains("Do not modify source")
                    || prompt.contains("Do not implement member tasks")
            );
        }
    }

    #[test]
    fn resource_and_regression_limits_accumulate_stable_reasons() {
        let mut snapshot = healthy();
        snapshot.load_1m = 20.0;
        snapshot.available_memory_bytes = 1;
        snapshot.planner_backlog = 43;
        snapshot.ci_green = false;
        snapshot.improvement_bps = -1;
        let mut configured = policy();
        configured.max_teams = 1;
        configured.min_improvement_bps = 1;
        assert_eq!(
            admission_decision(&configured, &snapshot, 1),
            Admission::Refuse(vec![
                "team_cap_reached",
                "cpu_load_limit",
                "memory_limit",
                "planner_backlog_limit",
                "regression_gate_failed",
                "improvement_gate_failed"
            ])
        );
    }

    #[test]
    fn explicit_ci_repair_can_cross_only_the_regression_gate() {
        let repair = MissionTask {
            node_id: "repair-ci".to_owned(),
            title: "Repair CI regression".to_owned(),
            capability: "code.generate".to_owned(),
            instruction: "Repair CI; all must pass and do not weaken tests.".to_owned(),
        };
        assert_eq!(
            allow_bounded_regression_remediation(
                Admission::Refuse(vec!["regression_gate_failed"]),
                std::slice::from_ref(&repair),
            ),
            Admission::Admit
        );
        assert_eq!(
            allow_bounded_regression_remediation(
                Admission::Refuse(vec!["cpu_load_limit", "regression_gate_failed"]),
                &[repair],
            ),
            Admission::Refuse(vec!["cpu_load_limit"])
        );
        let ordinary = MissionTask {
            node_id: "feature".to_owned(),
            title: "Add feature".to_owned(),
            capability: "code.generate".to_owned(),
            instruction: "Implement it.".to_owned(),
        };
        assert_eq!(
            allow_bounded_regression_remediation(
                Admission::Refuse(vec!["regression_gate_failed"]),
                &[ordinary],
            ),
            Admission::Refuse(vec!["regression_gate_failed"])
        );
    }

    #[test]
    fn team_contract_is_exactly_one_leader_and_five_workers() {
        let tasks: Vec<MissionTask> = (0..5)
            .map(|index| MissionTask {
                node_id: format!("n{index}"),
                title: format!("N{index}"),
                capability: "code.generate".to_owned(),
                instruction: "work".to_owned(),
            })
            .collect();
        let team = form_team(&tasks, &ArchitectState::default())
            .unwrap()
            .unwrap();
        assert_eq!(team.member_ids.len(), WORKERS_PER_TEAM);
        assert_eq!(1 + team.member_ids.len(), TEAM_SIZE);
        assert_eq!(team.tasks.len(), WORKERS_PER_TEAM);
        assert!(team
            .member_ids
            .iter()
            .all(|member| member.starts_with(&team.team_id)));
    }

    #[test]
    fn recovers_a_dead_leader_without_replacing_workers() {
        let mut team = recovery_team();
        let expected_prompt = leader_launch_prompt(&team).unwrap();
        let mut calls = Vec::new();
        recover_dead_leader_with(
            Path::new("/repo"),
            &mut team,
            500_000,
            |_| false,
            |workspace, model, effort, prompt| {
                calls.push((
                    workspace.to_owned(),
                    model.to_owned(),
                    effort.to_owned(),
                    prompt.to_owned(),
                ));
                Ok(900)
            },
        )
        .unwrap();

        assert_eq!(team.process_ids, vec![100, 101, 102, 103, 104, 900]);
        assert_eq!(
            team.recovery_started_ms.get(&team.leader_id),
            Some(&500_000)
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, Path::new("/repo"));
        assert_eq!(calls[0].1, LEADER_MODEL);
        assert_eq!(calls[0].2, "high");
        assert_eq!(calls[0].3, expected_prompt);
    }

    #[test]
    fn dead_leader_recovery_obeys_cooldown() {
        let mut team = recovery_team();
        team.recovery_started_ms.insert(
            team.leader_id.clone(),
            500_000 - (WORKER_RECOVERY_COOLDOWN_MS - 1),
        );
        let mut spawned = false;
        recover_dead_leader_with(
            Path::new("/repo"),
            &mut team,
            500_000,
            |_| false,
            |_, _, _, _| {
                spawned = true;
                Ok(900)
            },
        )
        .unwrap();

        assert!(!spawned);
        assert_eq!(team.process_ids, vec![100, 101, 102, 103, 104, 105]);
    }

    #[test]
    fn live_leader_is_left_alongside_workers() {
        let mut team = recovery_team();
        let mut spawned = false;
        recover_dead_leader_with(
            Path::new("/repo"),
            &mut team,
            500_000,
            |pid| pid == 105,
            |_, _, _, _| {
                spawned = true;
                Ok(900)
            },
        )
        .unwrap();

        assert!(!spawned);
        assert_eq!(team.process_ids, vec![100, 101, 102, 103, 104, 105]);
        assert!(team.recovery_started_ms.is_empty());
    }

    #[test]
    fn regression_repair_is_prioritized_into_a_bounded_team() {
        let mut tasks: Vec<MissionTask> = (0..5)
            .map(|index| MissionTask {
                node_id: format!("feature-{index}"),
                title: format!("Feature {index}"),
                capability: "code.generate".to_owned(),
                instruction: "Implement it.".to_owned(),
            })
            .collect();
        tasks.push(MissionTask {
            node_id: "z-repair-ci".to_owned(),
            title: "Repair CI regression".to_owned(),
            capability: "code.generate".to_owned(),
            instruction: "Repair CI; all must pass and do not weaken tests.".to_owned(),
        });
        let team = form_team(&tasks, &ArchitectState::default())
            .unwrap()
            .unwrap();
        assert_eq!(team.tasks.len(), WORKERS_PER_TEAM);
        assert!(team.tasks.iter().any(|task| task.node_id == "z-repair-ci"));
    }

    #[test]
    fn fragmented_frontier_forms_deterministic_cross_functional_team() {
        let tasks: Vec<MissionTask> = (0..5)
            .map(|index| MissionTask {
                node_id: format!("n{index}"),
                title: "n".to_owned(),
                capability: if index < 3 {
                    "code.generate"
                } else {
                    "project.tests.execute"
                }
                .to_owned(),
                instruction: "work".to_owned(),
            })
            .collect();
        let team = form_team(&tasks, &ArchitectState::default())
            .unwrap()
            .expect("five fragmented ready nodes should form a fallback team");
        assert_eq!(team.specialization, "cross-functional-frontier");
        assert_eq!(
            team.tasks
                .iter()
                .map(|task| task.node_id.as_str())
                .collect::<Vec<_>>(),
            ["n0", "n1", "n2", "n3", "n4"]
        );
        assert_eq!(team.tasks.len(), WORKERS_PER_TEAM);
    }

    #[test]
    fn homogeneous_specialist_lane_is_preferred_over_fragmented_frontier() {
        let mut tasks: Vec<MissionTask> = (0..WORKERS_PER_TEAM)
            .map(|index| MissionTask {
                node_id: format!("implementation-{index}"),
                title: "implementation".to_owned(),
                capability: "code.generate".to_owned(),
                instruction: "work".to_owned(),
            })
            .collect();
        tasks.extend((0..4).map(|index| MissionTask {
            node_id: format!("verification-{index}"),
            title: "verification".to_owned(),
            capability: "project.tests.execute".to_owned(),
            instruction: "work".to_owned(),
        }));
        let team = form_team(&tasks, &ArchitectState::default())
            .unwrap()
            .expect("complete homogeneous lane should be preferred");
        assert_eq!(team.specialization, "implementation");
        assert!(team
            .tasks
            .iter()
            .all(|task| task.capability == "code.generate"));
    }

    #[test]
    fn fragmented_frontier_below_five_does_not_form_a_team() {
        let tasks: Vec<MissionTask> = (0..4)
            .map(|index| MissionTask {
                node_id: format!("n{index}"),
                title: "n".to_owned(),
                capability: if index < 2 {
                    "code.generate"
                } else {
                    "project.tests.execute"
                }
                .to_owned(),
                instruction: "work".to_owned(),
            })
            .collect();
        assert!(form_team(&tasks, &ArchitectState::default())
            .unwrap()
            .is_none());
    }

    #[test]
    fn occupied_nodes_are_excluded_before_frontier_fallback() {
        let tasks: Vec<MissionTask> = (0..6)
            .map(|index| MissionTask {
                node_id: format!("n{index}"),
                title: "n".to_owned(),
                capability: "code.generate".to_owned(),
                instruction: "work".to_owned(),
            })
            .collect();
        let mut state = ArchitectState::default();
        state.teams.push(TeamRecord {
            team_id: "existing-team".to_owned(),
            specialization: "implementation".to_owned(),
            mission: "existing".to_owned(),
            leader_id: "existing-team-leader".to_owned(),
            member_ids: Vec::new(),
            member_clients: Vec::new(),
            tasks: vec![tasks[0].clone()],
            status: "launched".to_owned(),
            process_ids: Vec::new(),
            recovery_started_ms: BTreeMap::new(),
        });
        let team = form_team(&tasks, &state)
            .unwrap()
            .expect("five unoccupied ready nodes should form a team");
        assert_eq!(team.specialization, "implementation");
        assert_eq!(team.tasks.len(), WORKERS_PER_TEAM);
        assert!(team.tasks.iter().all(|task| task.node_id != "n0"));
    }

    #[test]
    fn parses_macos_available_pages() {
        let raw = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 10.\nPages active: 99.\nPages inactive: 20.\nPages speculative: 5.\n";
        assert_eq!(parse_vm_stat(raw), Some(35 * 16384));
    }

    #[test]
    fn team_workers_can_checkout_only_their_leader_assigned_node() {
        let tasks: Vec<MissionTask> = (0..5)
            .map(|index| MissionTask {
                node_id: format!("n{index}"),
                title: format!("N{index}"),
                capability: "code.generate".to_owned(),
                instruction: "work".to_owned(),
            })
            .collect();
        let mut state = ArchitectState::default();
        let mut team = form_team(&tasks, &state).unwrap().unwrap();
        team.status = "launched".to_owned();
        let worker = team.member_ids[0].clone();
        state.teams.push(team);
        assert!(checkout_authorized_in(&state, &worker, "n0"));
        assert!(!checkout_authorized_in(&state, &worker, "n1"));
        state.teams[0].status = "completed".to_owned();
        assert!(!checkout_authorized_in(&state, &worker, "n0"));
    }

    #[test]
    fn failed_team_releases_its_frontier_for_a_unique_retry() {
        let tasks: Vec<MissionTask> = (0..5)
            .map(|index| MissionTask {
                node_id: format!("n{index}"),
                title: format!("N{index}"),
                capability: "code.generate".to_owned(),
                instruction: "work".to_owned(),
            })
            .collect();
        let mut state = ArchitectState::default();
        let mut failed = form_team(&tasks, &state).unwrap().unwrap();
        let failed_id = failed.team_id.clone();
        failed.status = "failed_released".to_owned();
        state.teams.push(failed);
        let retry = form_team(&tasks, &state).unwrap().unwrap();
        assert_eq!(retry.tasks, tasks);
        assert_ne!(retry.team_id, failed_id);
    }

    #[test]
    fn active_planned_and_launched_teams_reserve_nodes_but_terminal_history_does_not() {
        let tasks: Vec<MissionTask> = (0..9)
            .map(|index| MissionTask {
                node_id: format!("n{index}"),
                title: format!("N{index}"),
                capability: "code.generate".to_owned(),
                instruction: "work".to_owned(),
            })
            .collect();
        let mut state = ArchitectState::default();
        for (index, status) in ["planned", "launched", "completed", "failed_released"]
            .into_iter()
            .enumerate()
        {
            state.teams.push(TeamRecord {
                team_id: format!("team-{status}"),
                specialization: "implementation".to_owned(),
                mission: "existing".to_owned(),
                leader_id: format!("team-{status}-leader"),
                member_ids: Vec::new(),
                member_clients: Vec::new(),
                tasks: vec![tasks[index].clone()],
                status: status.to_owned(),
                process_ids: Vec::new(),
                recovery_started_ms: BTreeMap::new(),
            });
        }

        let formed = form_team(&tasks, &state)
            .unwrap()
            .expect("five nodes remain after active reservations");
        let selected: BTreeSet<&str> = formed
            .tasks
            .iter()
            .map(|task| task.node_id.as_str())
            .collect();
        assert!(!selected.contains("n0"));
        assert!(!selected.contains("n1"));
        assert!(selected.contains("n2"));
        assert!(selected.contains("n3"));
    }

    #[test]
    fn partial_recovery_without_owned_checkout_becomes_terminal_and_releases_frontier() {
        let tasks: Vec<MissionTask> = (0..5)
            .map(|index| MissionTask {
                node_id: format!("requeue-{index}"),
                title: format!("Requeue {index}"),
                capability: "code.generate".to_owned(),
                instruction: "finish".to_owned(),
            })
            .collect();
        let mut state = ArchitectState::default();
        let mut recovered = form_team(&tasks, &state).unwrap().unwrap();
        recovered.status = "launched".to_owned();
        recovered.process_ids = vec![100, 900];
        state.teams.push(recovered);

        // While the partial recovery remains active, its remaining
        // null/released assignments stay reserved.
        assert!(form_team(&tasks, &state).unwrap().is_none());
        let terminal = team_terminal_status(&state.teams[0], false, false)
            .expect("partial recovery with no owned checkout must terminate");
        assert_eq!(terminal, "failed_released");
        state.teams[0].status = terminal.to_owned();

        let retry = form_team(&tasks, &state)
            .unwrap()
            .expect("released and unassigned tasks re-enter the frontier");
        assert_eq!(retry.tasks, tasks);
    }

    #[test]
    fn fresh_team_six_record_is_not_mistaken_for_partial_recovery() {
        let team = recovery_team();
        assert_eq!(team.process_ids.len(), TEAM_SIZE);
        assert_eq!(team_terminal_status(&team, false, false), None);
    }

    #[test]
    fn zombie_or_reused_pid_is_not_a_live_codex_agent() {
        assert!(!tracked_agent_process_record_alive("Z+   <defunct>"));
        assert!(!tracked_agent_process_record_alive(
            "S    /usr/bin/some-unrelated-service"
        ));
        assert!(tracked_agent_process_record_alive(
            "S+   node /opt/homebrew/bin/codex exec --model gpt-5.6-luna task"
        ));
        assert!(tracked_agent_process_record_alive(
            "S+   node /Users/me/.local/bin/cursor-agent --launcher path -p --force task"
        ));
        assert!(tracked_agent_process_record_alive(
            "S+   python /Users/me/.local/bin/hermes --yolo -z task"
        ));
        assert!(tracked_agent_process_record_alive(
            "S+   node /Users/me/.local/bin/claude -p task"
        ));
    }

    #[test]
    fn mixed_roster_uses_every_available_client_before_repeating() {
        let available = vec![
            "claude".to_owned(),
            "codex".to_owned(),
            "cursor".to_owned(),
            "hermes".to_owned(),
        ];
        assert_eq!(
            mixed_worker_roster_from(&available, 5),
            ["codex", "cursor", "hermes", "claude", "codex"]
        );
        assert_eq!(
            mixed_worker_roster_from(&["hermes".to_owned()], 3),
            ["hermes", "hermes", "hermes"]
        );
        assert_eq!(
            enabled_clients(&available, ["claude"]),
            ["codex", "cursor", "hermes"]
        );
    }

    #[test]
    fn autonomous_growth_targets_the_latest_existing_wave() {
        let graph = json!({"nodes": [
            {"id":"plan","execution":{"wave":0}},
            {"id":"old","execution":{"wave":4}},
            {"id":"latest","execution":{"wave":5}}
        ]});
        assert_eq!(latest_expandable_wave(&graph).unwrap(), 5);
        assert!(latest_expandable_wave(&json!({"nodes":[]})).is_err());
    }

    #[test]
    fn autonomous_growth_rotates_across_original_prd_feature_domains() {
        assert!(prd_mission_focus(1).contains("native runtime"));
        assert!(prd_mission_focus(2).contains("self-evolving harness"));
        assert!(prd_mission_focus(3).contains("capability economics"));
        assert!(prd_mission_focus(4).contains("agent life economy"));
        assert_eq!(prd_mission_focus(1), prd_mission_focus(5));
    }

    #[test]
    fn stale_specialist_heartbeat_overrides_a_live_wrapper_pid() {
        let stale = WorkerHeartbeat {
            status: "active".to_owned(),
            last_seen: 699,
            archived: false,
        };
        let fresh = WorkerHeartbeat {
            status: "idle".to_owned(),
            last_seen: 999,
            archived: false,
        };

        assert!(worker_requires_recovery(
            true,
            Some(&stale),
            1_000,
            None,
            1_000_000
        ));
        assert!(!worker_requires_recovery(
            true,
            Some(&fresh),
            1_000,
            None,
            1_000_000
        ));
        assert!(!worker_requires_recovery(
            false,
            Some(&stale),
            1_000,
            None,
            1_000_000
        ));
        assert!(!worker_requires_recovery(
            true,
            Some(&stale),
            1_000,
            Some(950_000),
            1_000_000
        ));
        assert!(worker_requires_recovery(
            true,
            Some(&stale),
            1_000,
            Some(800_000),
            1_000_000
        ));
    }

    #[test]
    fn fresh_numeric_collision_alias_prevents_recovery() {
        let member = "team-worker-1";
        let mut heartbeats = BTreeMap::new();
        heartbeats.insert(
            member.to_owned(),
            WorkerHeartbeat {
                status: "active".to_owned(),
                last_seen: 699,
                archived: false,
            },
        );
        heartbeats.insert(
            format!("{member}-2"),
            WorkerHeartbeat {
                status: "idle".to_owned(),
                last_seen: 999,
                archived: false,
            },
        );

        let heartbeat = freshest_worker_heartbeat(&heartbeats, member, 1_000);
        assert_eq!(heartbeat.map(|value| value.last_seen), Some(999));
        assert!(!worker_requires_recovery(
            true, heartbeat, 1_000, None, 1_000_000
        ));
    }

    #[test]
    fn archived_or_stale_collision_alias_does_not_prevent_recovery() {
        for heartbeat in [
            WorkerHeartbeat {
                status: "active".to_owned(),
                last_seen: 999,
                archived: true,
            },
            WorkerHeartbeat {
                status: "active".to_owned(),
                last_seen: 699,
                archived: false,
            },
        ] {
            let mut heartbeats = BTreeMap::new();
            heartbeats.insert("team-worker-1-2".to_owned(), heartbeat);
            let selected = freshest_worker_heartbeat(&heartbeats, "team-worker-1", 1_000);
            assert!(worker_requires_recovery(
                true, selected, 1_000, None, 1_000_000
            ));
        }
    }

    #[test]
    fn nonnumeric_and_lookalike_ids_do_not_count_as_collision_aliases() {
        let member = "team-worker-1";
        let fresh = WorkerHeartbeat {
            status: "active".to_owned(),
            last_seen: 999,
            archived: false,
        };
        let mut heartbeats = BTreeMap::new();
        heartbeats.insert(format!("{member}-recovery"), fresh.clone());
        heartbeats.insert("other-team-worker-1-2".to_owned(), fresh);

        let selected = freshest_worker_heartbeat(&heartbeats, member, 1_000);
        assert!(selected.is_none());
        assert!(worker_requires_recovery(
            true, selected, 1_000, None, 1_000_000
        ));
    }

    #[test]
    fn freshest_valid_collision_alias_is_selected() {
        let member = "team-worker-1";
        let mut heartbeats = BTreeMap::new();
        for (id, last_seen) in [
            (member.to_owned(), 900),
            (format!("{member}-2"), 950),
            (format!("{member}-3"), 940),
            (format!("{member}-not-numeric"), 9_999),
        ] {
            heartbeats.insert(
                id,
                WorkerHeartbeat {
                    status: "active".to_owned(),
                    last_seen,
                    archived: false,
                },
            );
        }

        assert_eq!(
            freshest_worker_heartbeat(&heartbeats, member, 1_000).map(|value| value.last_seen),
            Some(950)
        );
    }

    #[test]
    fn fresh_collision_alias_wins_over_newer_invalid_alias() {
        let member = "team-worker-1";
        let mut heartbeats = BTreeMap::new();
        heartbeats.insert(
            format!("{member}-2"),
            WorkerHeartbeat {
                status: "active".to_owned(),
                last_seen: 999,
                archived: true,
            },
        );
        heartbeats.insert(
            format!("{member}-3"),
            WorkerHeartbeat {
                status: "active".to_owned(),
                last_seen: 950,
                archived: false,
            },
        );

        let selected = freshest_worker_heartbeat(&heartbeats, member, 1_000);
        assert_eq!(selected.map(|value| value.last_seen), Some(950));
        assert!(!worker_requires_recovery(
            true, selected, 1_000, None, 1_000_000
        ));
    }

    #[test]
    fn worker_heartbeat_parser_rejects_future_archived_and_missing_evidence() {
        let agents =
            br#"{"id":"fresh","role":"worker","status":"active","last_seen":995,"archived_at":null}
{"id":"future","role":"worker","status":"active","last_seen":1001,"archived_at":null}
{"id":"archived","role":"worker","status":"active","last_seen":995,"archived_at":996}
{"id":"manager","role":"manager","status":"active","last_seen":995,"archived_at":null}"#;
        let parsed = parse_worker_heartbeats(agents);

        assert_eq!(parsed.len(), 3);
        assert!(heartbeat_is_fresh(parsed.get("fresh"), 1_000));
        assert!(!heartbeat_is_fresh(parsed.get("future"), 1_000));
        assert!(!heartbeat_is_fresh(parsed.get("archived"), 1_000));
        assert!(!heartbeat_is_fresh(parsed.get("missing"), 1_000));
    }

    #[test]
    fn recovery_finds_legacy_checkout_outside_original_team_tasks() {
        let original: Vec<MissionTask> = (0..5)
            .map(|index| MissionTask {
                node_id: format!("done-{index}"),
                title: format!("Done {index}"),
                capability: "code.generate".to_owned(),
                instruction: "done".to_owned(),
            })
            .collect();
        let mut team = form_team(&original, &ArchitectState::default())
            .unwrap()
            .unwrap();
        team.status = "failed_released".to_owned();
        let member = team.member_ids[2].clone();
        let document = json!({
            "graph": {"nodes": [{
                "id": "legacy-auto-chain",
                "title": "Recover legacy checkout",
                "capability": "code.generate",
                "instruction": "Preserve the partial work and finish it."
            }]},
            "execution": {"assignments": {"legacy-auto-chain": {
                "state": "checked_out",
                "agent_id": member
            }}}
        });
        let assignments = document
            .pointer("/execution/assignments")
            .and_then(Value::as_object);
        assert!(team_has_checked_out_work(&team, assignments));
        let stranded = stranded_team_assignments(&document, &team, assignments);
        assert_eq!(stranded.len(), 1);
        assert_eq!(stranded[0].0, team.member_ids[2]);
        assert_eq!(stranded[0].1.node_id, "legacy-auto-chain");
        assert_eq!(stranded[0].1.title, "Recover legacy checkout");
    }
}
