//! Local assignment coordinator used by `fractal join` and Coordinate's
//! local development path, including the bounded worker-session lease protocol.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cli::CoordinatorArgs;

// Squad preserves the last declared status after a client disappears. Never
// adopt an "active" label without a recent heartbeat: it is only an
// observation, not proof that a worker process still exists.
const WORKER_RECONCILE_MAX_IDLE_SECS: u64 = 15;
/// Default assignment lease lifetime for joined workers.
pub(crate) const DEFAULT_LEASE_SECS: u64 = 60;
/// Finite upper bound on configurable lease duration (matches FractalRuntime).
pub(crate) const MAX_LEASE_SECS: u64 = 300;
const LEASE_STORE_SCHEMA: &str = "fractal.worker_session_leases.v1";
const ASSIGNMENT_SCHEMA: &str = "fractal.worker_assignment.v1";
pub(crate) const COMPLETION_SCHEMA: &str = "fractal.worker_completion.v1";
const RENEWAL_SCHEMA: &str = "fractal.worker_lease_renew.v1";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NextAssignment {
    Assigned(String),
    GraphComplete,
    AmendmentRequested,
}

/// Injectable millisecond clock for deterministic lease tests.
#[derive(Clone, Debug)]
pub(crate) struct LeaseClock {
    now_ms: Arc<Mutex<u64>>,
}

impl LeaseClock {
    pub(crate) fn system() -> Self {
        Self {
            now_ms: Arc::new(Mutex::new(system_now_ms())),
        }
    }

    #[cfg(test)]
    pub(crate) fn fake(start_ms: u64) -> Self {
        Self {
            now_ms: Arc::new(Mutex::new(start_ms)),
        }
    }

    pub(crate) fn now_ms(&self) -> u64 {
        *self.now_ms.lock().expect("lease clock lock")
    }

    pub(crate) fn set_ms(&self, value: u64) {
        *self.now_ms.lock().expect("lease clock lock") = value;
    }

    #[cfg(test)]
    pub(crate) fn advance_ms(&self, delta: u64) {
        let mut guard = self.now_ms.lock().expect("lease clock lock");
        *guard = guard.saturating_add(delta);
    }

    fn refresh_system(&self) {
        self.set_ms(system_now_ms());
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkerLease {
    pub(crate) project: String,
    pub(crate) worker_id: String,
    pub(crate) worker_label: String,
    pub(crate) node_id: String,
    pub(crate) task_id: String,
    pub(crate) generation: u64,
    pub(crate) expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionReport {
    pub(crate) schema: String,
    pub(crate) project: String,
    pub(crate) worker_id: String,
    pub(crate) node_id: String,
    pub(crate) task_id: String,
    pub(crate) generation: u64,
    pub(crate) evidence: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CompletionReject {
    MissingEvidence,
    WrongOwner,
    ExpiredLease,
    StaleGeneration,
    Duplicate,
    UnknownLease,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CompletionResult {
    Accepted { next: Option<String> },
    Rejected(CompletionReject),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExpansionProposal {
    pub(crate) command_id: String,
    pub(crate) frontier_fingerprint: String,
    pub(crate) lease_window: u64,
    pub(crate) blocked_frontier: Vec<String>,
    pub(crate) instruction: String,
    pub(crate) wave: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DurableLeaseStore {
    schema: String,
    project: String,
    lease_secs: u64,
    next_generation: u64,
    leases: Vec<WorkerLease>,
    accepted_completions: Vec<(String, String, u64)>,
    expansion_emitted: Vec<(String, u64)>,
    reclaimed_expired: Vec<String>,
}

/// In-memory worker-session lease table with durable load/store.
#[derive(Debug)]
pub(crate) struct SessionLeaseTable {
    project: String,
    lease_secs: u64,
    clock: LeaseClock,
    next_generation: u64,
    leases: BTreeMap<(String, String), WorkerLease>,
    accepted_completions: BTreeSet<(String, String, u64)>,
    expansion_emitted: BTreeSet<(String, u64)>,
    reclaimed_expired: BTreeSet<String>,
}

impl SessionLeaseTable {
    pub(crate) fn new(
        project: impl Into<String>,
        lease_secs: u64,
        clock: LeaseClock,
    ) -> Result<Self> {
        let lease_secs = validate_lease_secs(lease_secs)?;
        Ok(Self {
            project: project.into(),
            lease_secs,
            clock,
            next_generation: 1,
            leases: BTreeMap::new(),
            accepted_completions: BTreeSet::new(),
            expansion_emitted: BTreeSet::new(),
            reclaimed_expired: BTreeSet::new(),
        })
    }

    fn store_path(workspace: &Path) -> PathBuf {
        workspace
            .join(".fractal")
            .join("worker-session-leases.json")
    }

    pub(crate) fn load_or_new(
        workspace: &Path,
        project: impl Into<String>,
        lease_secs: u64,
        clock: LeaseClock,
    ) -> Result<Self> {
        let mut table = Self::new(project, lease_secs, clock)?;
        let path = Self::store_path(workspace);
        if !path.is_file() {
            return Ok(table);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read lease store {}", path.display()))?;
        let store: DurableLeaseStore = serde_json::from_str(&raw)
            .with_context(|| format!("parse lease store {}", path.display()))?;
        if store.schema != LEASE_STORE_SCHEMA {
            bail!("unsupported worker session lease store schema");
        }
        if !store.project.is_empty() {
            table.project = store.project;
        }
        if store.lease_secs > 0 && store.lease_secs <= MAX_LEASE_SECS {
            table.lease_secs = store.lease_secs;
        }
        table.next_generation = store.next_generation.max(1);
        for lease in store.leases {
            table
                .leases
                .insert((lease.worker_id.clone(), lease.node_id.clone()), lease);
        }
        table.accepted_completions = store.accepted_completions.into_iter().collect();
        table.expansion_emitted = store.expansion_emitted.into_iter().collect();
        table.reclaimed_expired = store.reclaimed_expired.into_iter().collect();
        Ok(table)
    }

    pub(crate) fn persist(&self, workspace: &Path) -> Result<()> {
        let path = Self::store_path(workspace);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let store = DurableLeaseStore {
            schema: LEASE_STORE_SCHEMA.to_owned(),
            project: self.project.clone(),
            lease_secs: self.lease_secs,
            next_generation: self.next_generation,
            leases: self.leases.values().cloned().collect(),
            accepted_completions: self.accepted_completions.iter().cloned().collect(),
            expansion_emitted: self.expansion_emitted.iter().cloned().collect(),
            reclaimed_expired: self.reclaimed_expired.iter().cloned().collect(),
        };
        let encoded = serde_json::to_vec_pretty(&store)?;
        std::fs::write(&path, encoded)
            .with_context(|| format!("write lease store {}", path.display()))?;
        Ok(())
    }

    pub(crate) fn issue_lease(
        &mut self,
        worker_id: &str,
        worker_label: &str,
        node_id: &str,
        task_id: &str,
    ) -> WorkerLease {
        self.reclaimed_expired.remove(node_id);
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let now = self.clock.now_ms();
        let lease = WorkerLease {
            project: self.project.clone(),
            worker_id: worker_id.to_owned(),
            worker_label: worker_label.to_owned(),
            node_id: node_id.to_owned(),
            task_id: task_id.to_owned(),
            generation,
            expires_at_ms: now.saturating_add(self.lease_secs.saturating_mul(1_000)),
        };
        self.leases
            .insert((worker_id.to_owned(), node_id.to_owned()), lease.clone());
        lease
    }

    #[cfg(test)]
    pub(crate) fn get_lease(&self, worker_id: &str, node_id: &str) -> Option<&WorkerLease> {
        self.leases.get(&(worker_id.to_owned(), node_id.to_owned()))
    }

    pub(crate) fn renew(
        &mut self,
        project: &str,
        worker_id: &str,
        node_id: &str,
        task_id: &str,
        generation: u64,
    ) -> Result<WorkerLease, CompletionReject> {
        if project != self.project {
            return Err(CompletionReject::UnknownLease);
        }
        let now = self.clock.now_ms();
        let Some(lease) = self
            .leases
            .get_mut(&(worker_id.to_owned(), node_id.to_owned()))
        else {
            return Err(CompletionReject::UnknownLease);
        };
        if lease.generation != generation {
            return Err(CompletionReject::StaleGeneration);
        }
        if lease.task_id != task_id {
            return Err(CompletionReject::WrongOwner);
        }
        if now >= lease.expires_at_ms {
            return Err(CompletionReject::ExpiredLease);
        }
        lease.expires_at_ms = now.saturating_add(self.lease_secs.saturating_mul(1_000));
        Ok(lease.clone())
    }

    pub(crate) fn accept_completion(
        &mut self,
        report: &CompletionReport,
        next_node: Option<String>,
    ) -> CompletionResult {
        if report.schema != COMPLETION_SCHEMA {
            return CompletionResult::Rejected(CompletionReject::UnknownLease);
        }
        if report.project != self.project {
            return CompletionResult::Rejected(CompletionReject::UnknownLease);
        }
        if report.evidence.is_empty()
            || report
                .evidence
                .iter()
                .any(|item| item.trim().is_empty() || item.len() > 256)
        {
            return CompletionResult::Rejected(CompletionReject::MissingEvidence);
        }
        let key = (
            report.worker_id.clone(),
            report.node_id.clone(),
            report.generation,
        );
        if self.accepted_completions.contains(&key) {
            return CompletionResult::Rejected(CompletionReject::Duplicate);
        }
        let now = self.clock.now_ms();
        let Some(lease) = self
            .leases
            .get(&(report.worker_id.clone(), report.node_id.clone()))
            .cloned()
        else {
            return CompletionResult::Rejected(CompletionReject::UnknownLease);
        };
        if lease.worker_id != report.worker_id {
            return CompletionResult::Rejected(CompletionReject::WrongOwner);
        }
        if lease.generation != report.generation {
            return CompletionResult::Rejected(CompletionReject::StaleGeneration);
        }
        if lease.task_id != report.task_id {
            return CompletionResult::Rejected(CompletionReject::WrongOwner);
        }
        if now >= lease.expires_at_ms {
            return CompletionResult::Rejected(CompletionReject::ExpiredLease);
        }
        self.accepted_completions.insert(key);
        self.leases
            .remove(&(report.worker_id.clone(), report.node_id.clone()));
        CompletionResult::Accepted { next: next_node }
    }

    /// Re-adopt valid leases from durable checkouts; expire abandoned ones once.
    pub(crate) fn reconcile_restart(
        &mut self,
        checkouts: &[(String, String, String)],
        active_workers: &BTreeSet<String>,
    ) -> Vec<String> {
        let now = self.clock.now_ms();
        let mut reclaimed = Vec::new();
        let checkout_keys: BTreeSet<(String, String)> = checkouts
            .iter()
            .map(|(worker, node, _)| (worker.clone(), node.clone()))
            .collect();

        let stale_keys: Vec<(String, String)> = self
            .leases
            .iter()
            .filter_map(|((worker, node), lease)| {
                let abandoned = !active_workers.contains(worker)
                    || now >= lease.expires_at_ms
                    || !checkout_keys.contains(&(worker.clone(), node.clone()));
                abandoned.then_some((worker.clone(), node.clone()))
            })
            .collect();
        for key in stale_keys {
            if let Some(lease) = self.leases.remove(&key) {
                if self.reclaimed_expired.insert(lease.node_id.clone()) {
                    reclaimed.push(lease.node_id);
                }
            }
        }

        for (worker_id, node_id, worker_label) in checkouts {
            if self.reclaimed_expired.contains(node_id) {
                // Expired earlier in this or a prior pass; caller releases once.
                continue;
            }
            if !active_workers.contains(worker_id) {
                if self.reclaimed_expired.insert(node_id.clone()) {
                    reclaimed.push(node_id.clone());
                }
                continue;
            }
            match self.leases.get(&(worker_id.clone(), node_id.clone())) {
                Some(lease) if now < lease.expires_at_ms => {}
                Some(_) => {
                    self.leases.remove(&(worker_id.clone(), node_id.clone()));
                    if self.reclaimed_expired.insert(node_id.clone()) {
                        reclaimed.push(node_id.clone());
                    }
                }
                None => {
                    let task_id = format!("adopted-{node_id}");
                    let _ = self.issue_lease(worker_id, worker_label, node_id, &task_id);
                }
            }
        }
        reclaimed.sort();
        reclaimed.dedup();
        reclaimed
    }

    pub(crate) fn lease_window(&self) -> u64 {
        let window_ms = self.lease_secs.saturating_mul(1_000).max(1);
        self.clock.now_ms() / window_ms
    }

    pub(crate) fn maybe_expansion_proposal(
        &mut self,
        waiting_workers: usize,
        blocked_frontier: &[String],
        claimable_ready: usize,
        wave: u32,
    ) -> Option<ExpansionProposal> {
        if waiting_workers == 0 || blocked_frontier.is_empty() {
            return None;
        }
        if claimable_ready >= waiting_workers {
            return None;
        }
        let mut frontier = blocked_frontier.to_vec();
        frontier.sort();
        frontier.dedup();
        let fingerprint = frontier.join("|");
        let lease_window = self.lease_window();
        let key = (fingerprint.clone(), lease_window);
        if !self.expansion_emitted.insert(key) {
            return None;
        }
        let command_id = format!("frontier-expand-{fingerprint}-{lease_window}");
        let instruction = format!(
            "Blocked frontier [{}] lacks enough independent claimable nodes for {waiting_workers} waiting worker(s). Propose a bounded graph expansion that preserves all canonical dependencies and does not claim or duplicate existing nodes.",
            frontier.join(", ")
        );
        Some(ExpansionProposal {
            command_id,
            frontier_fingerprint: fingerprint,
            lease_window,
            blocked_frontier: frontier,
            instruction,
            wave: wave.max(1),
        })
    }
}

pub(crate) fn validate_lease_secs(secs: u64) -> Result<u64> {
    if secs == 0 || secs > MAX_LEASE_SECS {
        bail!("lease duration must be in 1..={MAX_LEASE_SECS} seconds (got {secs})");
    }
    Ok(secs)
}

fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

pub(crate) fn assignment_message(lease: &WorkerLease) -> String {
    json!({
        "schema": ASSIGNMENT_SCHEMA,
        "project": lease.project,
        "worker_id": lease.worker_id,
        "worker_label": lease.worker_label,
        "node_id": lease.node_id,
        "task_id": lease.task_id,
        "generation": lease.generation,
        "expires_at_ms": lease.expires_at_ms,
    })
    .to_string()
}

pub(crate) fn parse_completion_report(message: &str) -> Option<CompletionReport> {
    let payload = message
        .trim()
        .strip_prefix("WORKER_COMPLETION ")
        .unwrap_or(message.trim());
    let value = serde_json::Deserializer::from_str(payload)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    if value.get("schema").and_then(Value::as_str) != Some(COMPLETION_SCHEMA) {
        return None;
    }
    let evidence = value
        .get("evidence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    Some(CompletionReport {
        schema: COMPLETION_SCHEMA.to_owned(),
        project: value.get("project")?.as_str()?.to_owned(),
        worker_id: value.get("worker_id")?.as_str()?.to_owned(),
        node_id: value.get("node_id")?.as_str()?.to_owned(),
        task_id: value.get("task_id")?.as_str()?.to_owned(),
        generation: value.get("generation")?.as_u64()?,
        evidence,
    })
}

pub(crate) fn parse_renewal(message: &str) -> Option<(String, String, String, String, u64)> {
    let payload = message
        .trim()
        .strip_prefix("WORKER_LEASE_RENEW ")
        .unwrap_or(message.trim());
    let value = serde_json::Deserializer::from_str(payload)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    if value.get("schema").and_then(Value::as_str) != Some(RENEWAL_SCHEMA) {
        return None;
    }
    Some((
        value.get("project")?.as_str()?.to_owned(),
        value.get("worker_id")?.as_str()?.to_owned(),
        value.get("node_id")?.as_str()?.to_owned(),
        value.get("task_id")?.as_str()?.to_owned(),
        value.get("generation")?.as_u64()?,
    ))
}

pub(crate) fn renewal_message(lease: &WorkerLease) -> String {
    format!(
        "WORKER_LEASE_RENEW {}",
        json!({
            "schema": RENEWAL_SCHEMA,
            "project": lease.project,
            "worker_id": lease.worker_id,
            "node_id": lease.node_id,
            "task_id": lease.task_id,
            "generation": lease.generation,
        })
    )
}

#[allow(dead_code)]
pub(crate) fn completion_message(report: &CompletionReport) -> String {
    format!(
        "WORKER_COMPLETION {}",
        json!({
            "schema": report.schema,
            "project": report.project,
            "worker_id": report.worker_id,
            "node_id": report.node_id,
            "task_id": report.task_id,
            "generation": report.generation,
            "evidence": report.evidence,
        })
    )
}

pub(crate) fn run(args: &CoordinatorArgs) -> Result<()> {
    if args.poll_secs == 0 {
        bail!("--poll-secs must be greater than zero");
    }
    let lease_secs = validate_lease_secs(args.lease_secs)?;
    let workspace = discover_workspace(&args.repo)?;
    // A standalone `fractal coordinator` is itself the durable project run.
    // Register it just like a managed build so status/pause never mistake a
    // healthy coordinator for a dead historical supervisor.
    let _run_guard =
        crate::run_control::RunGuard::start_or_join(&workspace, "local graph coordinator", 0)?;
    crate::project_file::load(&workspace)
        .with_context(|| format!("load project graph in {}", workspace.display()))?;
    let squad = args
        .squad_bin
        .clone()
        .unwrap_or_else(|| PathBuf::from("squad"));
    run_squad(&squad, &workspace, &["init"])?;

    let coordinator_id = format!("fractal-coordinator-{}", std::process::id());
    run_squad(
        &squad,
        &workspace,
        &[
            "join".to_owned(),
            coordinator_id.clone(),
            "--role".to_owned(),
            "graph-supervisor".to_owned(),
            "--protocol-version".to_owned(),
            "2".to_owned(),
        ],
    )?;
    let active_message = format!(
        "Fractal coordinator active: {} ({})",
        coordinator_id,
        workspace.display()
    );
    if args.quiet {
        eprintln!("{active_message}");
    } else {
        println!("{active_message}");
    }

    apply_pending_amendments(&workspace)?;
    heartbeat_run(&workspace)?;

    let project = workspace.display().to_string();
    let clock = LeaseClock::system();
    let mut leases = SessionLeaseTable::load_or_new(&workspace, project, lease_secs, clock)?;
    reconcile_session(&mut leases, &squad, &workspace)?;
    let mut waiting_workers: BTreeMap<String, String> = BTreeMap::new();

    loop {
        leases.clock.refresh_system();
        heartbeat_run(&workspace)?;
        let output = run_squad(
            &squad,
            &workspace,
            &[
                "receive".to_owned(),
                coordinator_id.clone(),
                "--json".to_owned(),
                "--wait".to_owned(),
                "--timeout".to_owned(),
                args.poll_secs.to_string(),
            ],
        )?;
        let mut handled = false;
        for message in messages(&output.stdout) {
            if let Some(report) = parse_completion_report(&message) {
                handled = true;
                handle_completion(&mut leases, &squad, &workspace, &coordinator_id, &report)?;
                if !worker_owns_checkout(&workspace, &report.worker_id)? {
                    waiting_workers.insert(
                        report.worker_id.clone(),
                        format!("Fractal · worker · {}", report.worker_id),
                    );
                }
                continue;
            }
            if let Some((project, worker_id, node_id, task_id, generation)) =
                parse_renewal(&message)
            {
                handled = true;
                match leases.renew(&project, &worker_id, &node_id, &task_id, generation) {
                    Ok(lease) => {
                        send(
                            &squad,
                            &workspace,
                            &coordinator_id,
                            &worker_id,
                            &format!("LEASE_RENEWED generation={}", lease.generation),
                        )?;
                    }
                    Err(reason) => {
                        send(
                            &squad,
                            &workspace,
                            &coordinator_id,
                            &worker_id,
                            &format!("LEASE_RENEW_REJECTED {reason:?}"),
                        )?;
                    }
                }
                leases.persist(&workspace)?;
                continue;
            }
            if let Some((worker_id, worker_label)) = worker_join_request(&message) {
                handled = true;
                waiting_workers.insert(worker_id.clone(), worker_label.clone());
                assign_worker(
                    &mut leases,
                    &squad,
                    &workspace,
                    &coordinator_id,
                    &worker_id,
                    &worker_label,
                )?;
                if worker_owns_checkout(&workspace, &worker_id)? {
                    waiting_workers.remove(&worker_id);
                }
            }
        }
        if args.once {
            break;
        }
        // Maintenance must not depend on an empty inbox. A busy or replayed
        // Squad queue is normal after recovery; gating graph amendments and
        // reassignment on `!handled` lets old traffic starve useful work
        // indefinitely while the coordinator still appears healthy.
        apply_pending_amendments(&workspace)?;
        heartbeat_run(&workspace)?;
        assign_newly_ready_work(
            &mut leases,
            &squad,
            &workspace,
            &coordinator_id,
            &mut waiting_workers,
        )?;
        maybe_queue_expansion(&mut leases, &squad, &workspace, &coordinator_id)?;
        if handled && !crate::amendments::has_pending(&workspace) && graph_is_terminal(&workspace)?
        {
            break;
        }
        if !handled {
            std::thread::sleep(Duration::from_millis(50));
        }
        leases.persist(&workspace)?;
    }

    leases.persist(&workspace)?;
    let _ = run_squad(&squad, &workspace, &["leave".to_owned(), coordinator_id]);
    Ok(())
}

fn heartbeat_run(workspace: &Path) -> Result<()> {
    let document = crate::project_file::load(workspace)?;
    crate::run_control::set_graph(&document.graph_hash, "");
    Ok(())
}

fn reconcile_session(leases: &mut SessionLeaseTable, squad: &Path, workspace: &Path) -> Result<()> {
    let agents = run_squad(squad, workspace, &["agents", "--json"])?;
    let workers = active_workers(&agents.stdout);
    let active: BTreeSet<String> = workers.iter().map(|(id, _)| id.clone()).collect();
    let checkouts = durable_checkouts(workspace)?;
    let reclaimed = leases.reconcile_restart(&checkouts, &active);
    for node_id in reclaimed {
        if let Some((worker, _, label)) = checkouts.iter().find(|(_, node, _)| node == &node_id) {
            let _ = crate::project_file::transition(workspace, &node_id, "release", worker, label);
        } else {
            let _ = crate::project_file::release_stale_assignments(workspace);
        }
    }
    leases.persist(workspace)?;
    Ok(())
}

fn durable_checkouts(workspace: &Path) -> Result<Vec<(String, String, String)>> {
    let document = crate::project_file::load(workspace)?;
    let mut out = Vec::new();
    if let Some(execution) = document.execution.as_ref() {
        for (node_id, assignment) in &execution.assignments {
            if assignment.state == "checked_out" {
                out.push((
                    assignment.agent_id.clone(),
                    node_id.clone(),
                    assignment.agent_label.clone(),
                ));
            }
        }
    }
    out.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));
    Ok(out)
}

fn apply_pending_amendments(workspace: &Path) -> Result<()> {
    if !crate::amendments::has_pending(workspace) {
        return Ok(());
    }
    let lead_agent = crate::execute::detect_agents()
        .into_iter()
        .next()
        .context("accepted graph amendments are pending but no lead agent is available")?;
    let document = crate::project_file::load(workspace)?;
    let previous_hash = document.graph_hash.clone();
    let (_, graph_hash) = crate::amendments::apply_next_pending(
        document.graph,
        previous_hash.clone(),
        workspace,
        &lead_agent,
    );
    if graph_hash != previous_hash {
        let persisted = crate::project_file::load(workspace)?;
        if persisted.graph_hash != graph_hash {
            bail!("amended graph was not persisted as canonical project state");
        }
    }
    Ok(())
}

fn assign_newly_ready_work(
    leases: &mut SessionLeaseTable,
    squad: &Path,
    workspace: &Path,
    coordinator_id: &str,
    waiting_workers: &mut BTreeMap<String, String>,
) -> Result<()> {
    if crate::architect::enabled(workspace) {
        waiting_workers.clear();
        return Ok(());
    }
    let agents = run_squad(squad, workspace, &["agents", "--json"])?;
    let active = active_workers(&agents.stdout)
        .into_iter()
        .map(|(id, _)| id)
        .collect::<BTreeSet<_>>();
    waiting_workers.retain(|worker_id, _| active.contains(worker_id));
    let waiting = waiting_workers
        .iter()
        .map(|(id, label)| (id.clone(), label.clone()))
        .collect::<Vec<_>>();
    for (worker_id, worker_label) in waiting {
        if worker_owns_checkout(workspace, &worker_id)? {
            waiting_workers.remove(&worker_id);
            continue;
        }
        if next_ready_node(workspace)?.is_some() {
            assign_worker(
                leases,
                squad,
                workspace,
                coordinator_id,
                &worker_id,
                &worker_label,
            )?;
            if worker_owns_checkout(workspace, &worker_id)? {
                waiting_workers.remove(&worker_id);
            }
        }
    }
    Ok(())
}

fn maybe_queue_expansion(
    leases: &mut SessionLeaseTable,
    squad: &Path,
    workspace: &Path,
    coordinator_id: &str,
) -> Result<()> {
    // A governed amendment backlog already represents concrete future
    // capacity. Do not add generic frontier tasks merely because those plans
    // have not compiled yet; that creates duplicate work and planner debt.
    if crate::amendments::has_pending(workspace) {
        return Ok(());
    }
    let agents = run_squad(squad, workspace, &["agents", "--json"])?;
    let waiting = active_workers(&agents.stdout)
        .into_iter()
        .filter(|(worker_id, _)| !worker_owns_checkout(workspace, worker_id).unwrap_or(true))
        .count();
    let ready = count_ready_nodes(workspace)?;
    let blocked = blocked_frontier(workspace)?;
    let wave = earliest_open_wave(workspace).unwrap_or(1);
    if let Some(proposal) = leases.maybe_expansion_proposal(waiting, &blocked, ready, wave) {
        crate::amendments::queue(
            workspace,
            proposal.command_id.clone(),
            "add_wave_task",
            "",
            Some(proposal.wave),
            &proposal.instruction,
            "worker-session-coordinator",
        )?;
        send(
            squad,
            workspace,
            coordinator_id,
            "@all",
            &format!(
                "AMENDMENT_REQUESTED: frontier={} window={}",
                proposal.frontier_fingerprint, proposal.lease_window
            ),
        )?;
        leases.persist(workspace)?;
    }
    Ok(())
}

fn handle_completion(
    leases: &mut SessionLeaseTable,
    squad: &Path,
    workspace: &Path,
    coordinator_id: &str,
    report: &CompletionReport,
) -> Result<()> {
    let owner_ok =
        crate::project_file::assignment(workspace, &report.node_id)?.is_some_and(|assignment| {
            assignment.state == "checked_out" && assignment.agent_id == report.worker_id
        });
    if !owner_ok {
        send(
            squad,
            workspace,
            coordinator_id,
            &report.worker_id,
            "COMPLETION_REJECTED WrongOwner",
        )?;
        return Ok(());
    }
    let provisional = leases.accept_completion(report, None);
    if let CompletionResult::Rejected(reason) = provisional {
        send(
            squad,
            workspace,
            coordinator_id,
            &report.worker_id,
            &format!("COMPLETION_REJECTED {reason:?}"),
        )?;
        leases.persist(workspace)?;
        return Ok(());
    }
    // Roll back the provisional accept bookkeeping only after canonical transition.
    // accept_completion already recorded; if transition fails, leave duplicate guard.
    if let Err(error) = crate::project_file::transition(
        workspace,
        &report.node_id,
        "complete",
        &report.worker_id,
        &report.worker_id,
    ) {
        send(
            squad,
            workspace,
            coordinator_id,
            &report.worker_id,
            &format!("COMPLETION_REJECTED transition={error:#}"),
        )?;
        leases.persist(workspace)?;
        return Ok(());
    }
    let _ = crate::project_file::record_verification_result(
        workspace,
        &report.node_id,
        true,
        report.evidence.clone(),
    );
    let label = format!("Fractal · worker · {}", report.worker_id);
    assign_worker(
        leases,
        squad,
        workspace,
        coordinator_id,
        &report.worker_id,
        &label,
    )?;
    leases.persist(workspace)?;
    Ok(())
}

pub(crate) fn assign_once(
    workspace: &Path,
    squad_override: Option<&Path>,
    worker_id: &str,
    worker_label: &str,
) -> Result<()> {
    let squad = squad_override.unwrap_or_else(|| Path::new("squad"));
    run_squad(squad, workspace, &["init"])?;
    let coordinator_id = format!(
        "fractal-coordinator-once-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis())
    );
    run_squad(
        squad,
        workspace,
        &[
            "join".to_owned(),
            coordinator_id.clone(),
            "--role".to_owned(),
            "graph-supervisor".to_owned(),
            "--protocol-version".to_owned(),
            "2".to_owned(),
        ],
    )?;
    let mut leases = SessionLeaseTable::load_or_new(
        workspace,
        workspace.display().to_string(),
        DEFAULT_LEASE_SECS,
        LeaseClock::system(),
    )?;
    let result = assign_worker(
        &mut leases,
        squad,
        workspace,
        &coordinator_id,
        worker_id,
        worker_label,
    );
    leases.persist(workspace)?;
    let _ = run_squad(squad, workspace, &["leave".to_owned(), coordinator_id]);
    result?;
    Ok(())
}

fn assign_worker(
    leases: &mut SessionLeaseTable,
    squad: &Path,
    workspace: &Path,
    coordinator_id: &str,
    worker_id: &str,
    worker_label: &str,
) -> Result<()> {
    // Every entry point (join messages, periodic reconciliation, completion
    // chaining, and restart adoption) converges here. Enforce the one-slot
    // invariant at this boundary so two near-simultaneous triggers cannot
    // checkout a second node for the same worker.
    if worker_owns_checkout(workspace, worker_id)? {
        return Ok(());
    }
    if crate::architect::enabled(workspace) {
        send(
            squad,
            workspace,
            coordinator_id,
            worker_id,
            "ARCHITECT_MODE: flat assignment is disabled; specialist team leaders own delegation.",
        )?;
        return Ok(());
    }
    match checkout_next(workspace, worker_id, worker_label)? {
        NextAssignment::Assigned(node_id) => {
            let task_id = format!("task-{node_id}");
            let lease = leases.issue_lease(worker_id, worker_label, &node_id, &task_id);
            let title = format!("Parallel graph task: {node_id}");
            let body = format!(
                "node_id={node_id}\nagent_id={worker_id}\nagent_label={worker_label}\ngeneration={}\ntask_id={}\nassignment={}\nAssigned by the local Fractal coordinator.",
                lease.generation,
                lease.task_id,
                assignment_message(&lease)
            );
            if let Err(error) = run_squad(
                squad,
                workspace,
                &[
                    "task".to_owned(),
                    "create".to_owned(),
                    coordinator_id.to_owned(),
                    worker_id.to_owned(),
                    "--title".to_owned(),
                    title,
                    "--body".to_owned(),
                    body,
                ],
            ) {
                let _ = crate::project_file::transition(
                    workspace,
                    &node_id,
                    "release",
                    worker_id,
                    worker_label,
                );
                leases
                    .leases
                    .remove(&(worker_id.to_owned(), node_id.clone()));
                return Err(error);
            }
            leases.persist(workspace)?;
        }
        NextAssignment::GraphComplete => {
            send(
                squad,
                workspace,
                coordinator_id,
                worker_id,
                "NO_PARALLEL_WORK: the graph is complete.",
            )?;
        }
        NextAssignment::AmendmentRequested => {
            let message =
                "AMENDMENT_REQUESTED: no dependency-ready graph node is available; evaluate a governed graph split before assigning more work.";
            send(squad, workspace, coordinator_id, worker_id, message)?;
            maybe_queue_expansion(leases, squad, workspace, coordinator_id)?;
        }
    }
    Ok(())
}

pub(crate) fn checkout_next(
    workspace: &Path,
    worker_id: &str,
    worker_label: &str,
) -> Result<NextAssignment> {
    let max_attempts = crate::project_file::load(workspace)?
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .map_or(1, |nodes| nodes.len().saturating_add(1));
    for _ in 0..max_attempts {
        let Some(node_id) = next_ready_node(workspace)? else {
            break;
        };
        if crate::project_file::transition(workspace, &node_id, "checkout", worker_id, worker_label)
            .is_err()
        {
            // Another joining process won this node between discovery and the
            // canonical checkout. Reload and choose the next ready node.
            continue;
        }
        return Ok(NextAssignment::Assigned(node_id));
    }
    let document = crate::project_file::load(workspace)?;
    if document
        .execution
        .as_ref()
        .is_some_and(|execution| execution.phase == "completed")
    {
        Ok(NextAssignment::GraphComplete)
    } else {
        Ok(NextAssignment::AmendmentRequested)
    }
}

fn discover_workspace(start: &Path) -> Result<PathBuf> {
    let start = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()?.join(start)
    };
    let mut candidate = start.canonicalize().unwrap_or_else(|_| start.clone());
    loop {
        if candidate.join(".fractal/project.fractal").is_file() {
            return Ok(candidate);
        }
        if !candidate.pop() {
            bail!(
                "could not find .fractal/project.fractal from {}",
                start.display()
            );
        }
    }
}

fn run_squad<S: AsRef<str>>(squad: &Path, workspace: &Path, args: &[S]) -> Result<Output> {
    for attempt in 0..40 {
        let output = Command::new(squad)
            .args(args.iter().map(AsRef::as_ref))
            .current_dir(workspace)
            .output()
            .with_context(|| format!("run squad in {}", workspace.display()))?;
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
        bail!(
            "squad {} failed: {}",
            args.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(" "),
            stderr
        );
    }
    unreachable!("bounded squad retry loop always returns or fails")
}

fn send(squad: &Path, workspace: &Path, from: &str, to: &str, message: &str) -> Result<()> {
    run_squad(
        squad,
        workspace,
        &[
            "send".to_owned(),
            from.to_owned(),
            to.to_owned(),
            message.to_owned(),
        ],
    )?;
    Ok(())
}

fn messages(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter_map(|value| {
            value
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

fn worker_join_request(message: &str) -> Option<(String, String)> {
    let (_, payload) = message.split_once("WORKER_JOIN_READY ")?;
    let value = serde_json::Deserializer::from_str(payload.trim())
        .into_iter::<Value>()
        .next()?
        .ok()?;
    let id = value.get("agent_id")?.as_str()?.to_owned();
    let label = value
        .get("agent_label")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    Some((id, label))
}

fn active_workers(bytes: &[u8]) -> Vec<(String, String)> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    active_workers_at(bytes, now)
}

fn active_workers_at(bytes: &[u8], now: u64) -> Vec<(String, String)> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|value| {
            let status = value.get("status").and_then(Value::as_str);
            let recent = value
                .get("last_seen")
                .and_then(Value::as_u64)
                .is_some_and(|last_seen| {
                    now.saturating_sub(last_seen) <= WORKER_RECONCILE_MAX_IDLE_SECS
                });
            value.get("role").and_then(Value::as_str) == Some("worker")
                && matches!(status, Some("active" | "idle"))
                && recent
                && value.get("archived_at").is_none_or(Value::is_null)
        })
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.to_owned();
            let label = format!("Fractal · worker · {id}");
            Some((id, label))
        })
        .collect()
}

fn worker_owns_checkout(workspace: &Path, worker_id: &str) -> Result<bool> {
    let document = crate::project_file::load(workspace)?;
    Ok(document.execution.as_ref().is_some_and(|execution| {
        execution
            .assignments
            .values()
            .any(|assignment| assignment.state == "checked_out" && assignment.agent_id == worker_id)
    }))
}

fn next_ready_node(workspace: &Path) -> Result<Option<String>> {
    Ok(ready_nodes(workspace)?.into_iter().next())
}

fn ready_nodes(workspace: &Path) -> Result<Vec<String>> {
    let document = crate::project_file::load(workspace)?;
    let architect_reserved = crate::architect::reserved_node_ids(workspace);
    let assignments = document
        .execution
        .as_ref()
        .map(|execution| &execution.assignments);
    let edges = document
        .graph
        .get("edges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut ready = Vec::new();
    for node in document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        if architect_reserved.contains(id) {
            continue;
        }
        if assignments
            .and_then(|values| values.get(id))
            .is_some_and(|assignment| {
                assignment.state == "checked_out" || assignment.state == "completed"
            })
        {
            continue;
        }
        let is_ready = edges
            .iter()
            .filter(|edge| edge.get("to").and_then(Value::as_str) == Some(id))
            .filter_map(|edge| edge.get("from").and_then(Value::as_str))
            .all(|dependency| {
                assignments
                    .and_then(|values| values.get(dependency))
                    .is_some_and(|assignment| assignment.state == "completed")
            });
        if is_ready {
            ready.push(id.to_owned());
        }
    }
    Ok(ready)
}

fn count_ready_nodes(workspace: &Path) -> Result<usize> {
    Ok(ready_nodes(workspace)?.len())
}

fn blocked_frontier(workspace: &Path) -> Result<Vec<String>> {
    let document = crate::project_file::load(workspace)?;
    let assignments = document
        .execution
        .as_ref()
        .map(|execution| &execution.assignments);
    let edges = document
        .graph
        .get("edges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut blocked = Vec::new();
    for node in document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        if assignments
            .and_then(|values| values.get(id))
            .is_some_and(|assignment| {
                assignment.state == "checked_out" || assignment.state == "completed"
            })
        {
            continue;
        }
        let incomplete: Vec<&str> = edges
            .iter()
            .filter(|edge| edge.get("to").and_then(Value::as_str) == Some(id))
            .filter_map(|edge| edge.get("from").and_then(Value::as_str))
            .filter(|dependency| {
                assignments
                    .and_then(|values| values.get(*dependency))
                    .is_none_or(|assignment| assignment.state != "completed")
            })
            .collect();
        if !incomplete.is_empty() {
            blocked.push(id.to_owned());
        }
    }
    blocked.sort();
    Ok(blocked)
}

fn earliest_open_wave(workspace: &Path) -> Option<u32> {
    let document = crate::project_file::load(workspace).ok()?;
    let assignments = document
        .execution
        .as_ref()
        .map(|execution| &execution.assignments);
    let mut waves = BTreeSet::new();
    for node in document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        if assignments
            .and_then(|values| values.get(id))
            .is_some_and(|assignment| assignment.state == "completed")
        {
            continue;
        }
        if let Some(wave) = node
            .pointer("/execution/wave")
            .and_then(Value::as_u64)
            .map(|value| value as u32)
        {
            waves.insert(wave);
        }
    }
    waves.into_iter().next()
}

fn graph_is_terminal(workspace: &Path) -> Result<bool> {
    let document = crate::project_file::load(workspace)?;
    Ok(document
        .execution
        .as_ref()
        .is_some_and(|execution| execution.phase == "completed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Barrier;
    use std::thread;

    fn temp_workspace(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fractal-coordinator-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ))
    }

    fn write_graph(workspace: &Path, graph: Value) -> Result<()> {
        std::fs::create_dir_all(workspace)?;
        let mut graph = graph;
        crate::graph_store::rehash_graph(&mut graph)?;
        crate::project_file::persist(workspace, &graph, "Coordinator test")?;
        Ok(())
    }

    #[test]
    fn worker_join_parser_accepts_instructions_after_payload() {
        let message = concat!(
            "WORKER_JOIN_READY ",
            r#"{"schema":"fractal.worker_join.v1","agent_id":"worker-7","agent_label":"Worker Seven"}"#,
            ". Assign a dependency-ready graph node."
        );
        assert_eq!(
            worker_join_request(message),
            Some(("worker-7".to_owned(), "Worker Seven".to_owned()))
        );
    }

    #[test]
    fn active_worker_parser_adopts_recent_idle_and_rejects_stale_agents() {
        let agents = concat!(
            r#"{"id":"worker-1","role":"worker","status":"active","last_seen":995,"archived_at":null}"#,
            "\n",
            r#"{"id":"worker-2","role":"worker","status":"idle","last_seen":990,"archived_at":null}"#,
            "\n",
            r#"{"id":"worker-stale","role":"worker","status":"idle","last_seen":1,"archived_at":null}"#,
            "\n",
            r#"{"id":"worker-stale-active","role":"worker","status":"active","last_seen":1,"archived_at":null}"#,
            "\n",
            r#"{"id":"worker-3","role":"worker","status":"active","last_seen":1,"archived_at":123}"#,
            "\n",
            r#"{"id":"coordinator","role":"graph-supervisor","status":"active","last_seen":1,"archived_at":null}"#,
        );
        assert_eq!(
            active_workers_at(agents.as_bytes(), 1_000),
            vec![
                (
                    "worker-1".to_owned(),
                    "Fractal · worker · worker-1".to_owned()
                ),
                (
                    "worker-2".to_owned(),
                    "Fractal · worker · worker-2".to_owned()
                )
            ]
        );
    }

    #[test]
    fn completion_can_chain_the_same_worker_to_the_next_ready_node() -> Result<()> {
        let workspace = temp_workspace("chain");
        write_graph(
            &workspace,
            json!({
                "schema": "fractal.execution_graph.v1",
                "graph_id": "fg_chain_test",
                "nodes": [
                    {"id": "a", "capability": "code.generate", "instruction": "A"},
                    {"id": "b", "capability": "code.generate", "instruction": "B"},
                    {"id": "c", "capability": "code.generate", "instruction": "C"}
                ],
                "edges": [{"from": "a", "to": "c", "condition": "success"}]
            }),
        )?;

        assert_eq!(
            checkout_next(&workspace, "worker-1", "Worker 1")?,
            NextAssignment::Assigned("a".to_owned())
        );
        crate::project_file::transition(&workspace, "a", "complete", "worker-1", "Worker 1")?;
        assert_eq!(
            checkout_next(&workspace, "worker-1", "Worker 1")?,
            NextAssignment::Assigned("b".to_owned())
        );
        crate::project_file::transition(&workspace, "b", "complete", "worker-1", "Worker 1")?;
        assert_eq!(
            checkout_next(&workspace, "worker-1", "Worker 1")?,
            NextAssignment::Assigned("c".to_owned())
        );
        crate::project_file::transition(&workspace, "c", "complete", "worker-1", "Worker 1")?;
        assert_eq!(
            checkout_next(&workspace, "worker-1", "Worker 1")?,
            NextAssignment::GraphComplete
        );

        std::fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn lease_secs_must_be_positive_and_bounded() {
        assert!(validate_lease_secs(0).is_err());
        assert!(validate_lease_secs(MAX_LEASE_SECS + 1).is_err());
        assert_eq!(validate_lease_secs(30).unwrap(), 30);
    }

    #[test]
    fn concurrent_workers_receive_unique_ready_nodes() -> Result<()> {
        let workspace = temp_workspace("unique");
        write_graph(
            &workspace,
            json!({
                "schema": "fractal.execution_graph.v1",
                "graph_id": "fg_unique",
                "nodes": [
                    {"id": "p1", "capability": "code.generate", "instruction": "P1"},
                    {"id": "p2", "capability": "code.generate", "instruction": "P2"},
                    {"id": "p3", "capability": "code.generate", "instruction": "P3"}
                ],
                "edges": []
            }),
        )?;
        let barrier = Arc::new(Barrier::new(3));
        let workspace = Arc::new(workspace);
        let mut handles = Vec::new();
        for worker in ["w1", "w2"] {
            let barrier = Arc::clone(&barrier);
            let workspace = Arc::clone(&workspace);
            let worker = worker.to_owned();
            handles.push(thread::spawn(move || {
                barrier.wait();
                checkout_next(&workspace, &worker, &worker)
            }));
        }
        barrier.wait();
        let mut assigned = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker thread"))
            .collect::<Result<Vec<_>>>()?;
        assigned.sort_by_key(|item| match item {
            NextAssignment::Assigned(node) => node.clone(),
            _ => String::new(),
        });
        match (&assigned[0], &assigned[1]) {
            (NextAssignment::Assigned(a), NextAssignment::Assigned(b)) => {
                assert_ne!(a, b);
            }
            other => panic!("expected two unique assignments, got {other:?}"),
        }
        std::fs::remove_dir_all(workspace.as_ref())?;
        Ok(())
    }

    #[test]
    fn repeated_assignment_triggers_keep_one_checkout_per_worker() -> Result<()> {
        let workspace = temp_workspace("one-checkout-per-worker");
        write_graph(
            &workspace,
            json!({
                "schema": "fractal.execution_graph.v1",
                "graph_id": "fg_one_checkout",
                "nodes": [
                    {"id": "p1", "capability": "code.generate", "instruction": "P1"},
                    {"id": "p2", "capability": "code.generate", "instruction": "P2"}
                ],
                "edges": []
            }),
        )?;
        assert!(matches!(
            checkout_next(&workspace, "w1", "W1")?,
            NextAssignment::Assigned(_)
        ));
        let mut leases = SessionLeaseTable::new("proj", 60, LeaseClock::fake(1_000))?;

        // The missing squad executable proves the duplicate trigger returned
        // before attempting a second checkout or task delivery.
        assign_worker(
            &mut leases,
            Path::new("/definitely/missing/squad"),
            &workspace,
            "coord",
            "w1",
            "W1",
        )?;
        let project = crate::project_file::load(&workspace)?;
        let owned = project
            .execution
            .as_ref()
            .into_iter()
            .flat_map(|execution| execution.assignments.values())
            .filter(|assignment| assignment.state == "checked_out" && assignment.agent_id == "w1")
            .count();
        assert_eq!(owned, 1);
        std::fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn renewal_extends_only_matching_live_lease() -> Result<()> {
        let clock = LeaseClock::fake(1_000);
        let mut table = SessionLeaseTable::new("proj", 60, clock.clone())?;
        let lease = table.issue_lease("w1", "W1", "n1", "t1");
        clock.advance_ms(10_000);
        let renewed = table
            .renew("proj", "w1", "n1", "t1", lease.generation)
            .expect("renew");
        assert!(renewed.expires_at_ms > lease.expires_at_ms);
        assert!(matches!(
            table.renew("proj", "w1", "n1", "t1", lease.generation + 1),
            Err(CompletionReject::StaleGeneration)
        ));
        assert!(matches!(
            table.renew("proj", "w2", "n1", "t1", lease.generation),
            Err(CompletionReject::UnknownLease)
        ));
        Ok(())
    }

    #[test]
    fn stale_generation_and_wrong_owner_completion_are_rejected() -> Result<()> {
        let clock = LeaseClock::fake(5_000);
        let mut table = SessionLeaseTable::new("proj", 60, clock)?;
        let lease = table.issue_lease("w1", "W1", "n1", "t1");
        let mut report = CompletionReport {
            schema: COMPLETION_SCHEMA.to_owned(),
            project: "proj".to_owned(),
            worker_id: "w1".to_owned(),
            node_id: "n1".to_owned(),
            task_id: "t1".to_owned(),
            generation: lease.generation + 9,
            evidence: vec!["sha256:abc".to_owned()],
        };
        assert_eq!(
            table.accept_completion(&report, None),
            CompletionResult::Rejected(CompletionReject::StaleGeneration)
        );
        report.generation = lease.generation;
        report.worker_id = "other".to_owned();
        assert_eq!(
            table.accept_completion(&report, None),
            CompletionResult::Rejected(CompletionReject::UnknownLease)
        );
        report.worker_id = "w1".to_owned();
        report.task_id = "wrong-task".to_owned();
        assert_eq!(
            table.accept_completion(&report, None),
            CompletionResult::Rejected(CompletionReject::WrongOwner)
        );
        Ok(())
    }

    #[test]
    fn evidence_is_required_and_accepted_completion_chains_once() -> Result<()> {
        let clock = LeaseClock::fake(10_000);
        let mut table = SessionLeaseTable::new("proj", 60, clock)?;
        let lease = table.issue_lease("w1", "W1", "n1", "t1");
        let missing = CompletionReport {
            schema: COMPLETION_SCHEMA.to_owned(),
            project: "proj".to_owned(),
            worker_id: "w1".to_owned(),
            node_id: "n1".to_owned(),
            task_id: "t1".to_owned(),
            generation: lease.generation,
            evidence: vec![],
        };
        assert_eq!(
            table.accept_completion(&missing, Some("n2".to_owned())),
            CompletionResult::Rejected(CompletionReject::MissingEvidence)
        );
        let ok = CompletionReport {
            evidence: vec!["ref:artifact-1".to_owned()],
            ..missing
        };
        assert_eq!(
            table.accept_completion(&ok, Some("n2".to_owned())),
            CompletionResult::Accepted {
                next: Some("n2".to_owned())
            }
        );
        assert_eq!(
            table.accept_completion(&ok, Some("n3".to_owned())),
            CompletionResult::Rejected(CompletionReject::Duplicate)
        );
        Ok(())
    }

    #[test]
    fn coordinator_restart_readopts_valid_and_reclaims_expired_once() -> Result<()> {
        let clock = LeaseClock::fake(100_000);
        let mut table = SessionLeaseTable::new("proj", 10, clock.clone())?;
        let live = table.issue_lease("w1", "W1", "live", "t-live");
        let _expired = table.issue_lease("w2", "W2", "dead", "t-dead");
        clock.set_ms(150_000);
        table
            .leases
            .get_mut(&("w1".to_owned(), "live".to_owned()))
            .expect("live")
            .expires_at_ms = 200_000;
        table
            .leases
            .get_mut(&("w2".to_owned(), "dead".to_owned()))
            .expect("dead")
            .expires_at_ms = 140_000;
        let mut active = BTreeSet::new();
        active.insert("w1".to_owned());
        active.insert("w2".to_owned());
        let checkouts = vec![
            ("w1".to_owned(), "live".to_owned(), "W1".to_owned()),
            ("w2".to_owned(), "dead".to_owned(), "W2".to_owned()),
        ];
        let first = table.reconcile_restart(&checkouts, &active);
        assert_eq!(first, vec!["dead".to_owned()]);
        assert_eq!(
            table.get_lease("w1", "live").map(|lease| lease.generation),
            Some(live.generation)
        );
        let second = table.reconcile_restart(&checkouts, &active);
        assert!(second.is_empty(), "expired reclaim must happen once");
        Ok(())
    }

    #[test]
    fn dependency_blocked_frontier_never_assigns_early() -> Result<()> {
        let workspace = temp_workspace("blocked");
        write_graph(
            &workspace,
            json!({
                "schema": "fractal.execution_graph.v1",
                "graph_id": "fg_blocked",
                "nodes": [
                    {"id": "a", "capability": "code.generate", "instruction": "A"},
                    {"id": "b", "capability": "code.generate", "instruction": "B"}
                ],
                "edges": [{"from": "a", "to": "b", "condition": "success"}]
            }),
        )?;
        assert_eq!(
            checkout_next(&workspace, "w1", "W1")?,
            NextAssignment::Assigned("a".to_owned())
        );
        // b depends on a; with a checked out (not complete) b must not assign.
        let document = crate::project_file::load(&workspace)?;
        let ready = ready_nodes(&workspace)?;
        assert!(!ready.iter().any(|node| node == "b"));
        assert!(!document
            .execution
            .as_ref()
            .unwrap()
            .assignments
            .contains_key("b"));
        std::fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn expansion_proposals_are_bounded_and_deduplicated_across_polling_and_restart() -> Result<()> {
        let clock = LeaseClock::fake(1_000_000);
        let mut table = SessionLeaseTable::new("proj", 60, clock.clone())?;
        let blocked = vec!["x".to_owned(), "y".to_owned()];
        let first = table
            .maybe_expansion_proposal(2, &blocked, 0, 2)
            .expect("first proposal");
        assert!(first.instruction.contains("Blocked frontier"));
        assert!(first.blocked_frontier.contains(&"x".to_owned()));
        assert!(table.maybe_expansion_proposal(2, &blocked, 0, 2).is_none());
        // Persist and reload simulates coordinator restart within same lease window.
        let workspace = temp_workspace("expand");
        std::fs::create_dir_all(workspace.join(".fractal"))?;
        table.persist(&workspace)?;
        let mut reloaded = SessionLeaseTable::load_or_new(&workspace, "proj", 60, clock)?;
        assert!(reloaded
            .maybe_expansion_proposal(2, &blocked, 0, 2)
            .is_none());
        std::fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn one_listener_slot_per_worker_is_tracked_by_single_live_lease() -> Result<()> {
        let barrier = Arc::new(Barrier::new(2));
        let clock = LeaseClock::fake(50_000);
        let table = Arc::new(Mutex::new(SessionLeaseTable::new("proj", 60, clock)?));
        let barrier_thread = Arc::clone(&barrier);
        let table_thread = Arc::clone(&table);
        let handle = thread::spawn(move || {
            barrier_thread.wait();
            let mut guard = table_thread.lock().expect("table");
            guard.issue_lease("w1", "W1", "n1", "t1")
        });
        barrier.wait();
        let lease = handle.join().expect("thread");
        let guard = table.lock().expect("table");
        assert_eq!(guard.leases.len(), 1);
        assert_eq!(
            guard.get_lease("w1", "n1").map(|item| item.generation),
            Some(lease.generation)
        );
        Ok(())
    }

    #[test]
    fn expired_lease_completion_is_rejected() -> Result<()> {
        let clock = LeaseClock::fake(1_000);
        let mut table = SessionLeaseTable::new("proj", 1, clock.clone())?;
        let lease = table.issue_lease("w1", "W1", "n1", "t1");
        clock.advance_ms(2_000);
        let report = CompletionReport {
            schema: COMPLETION_SCHEMA.to_owned(),
            project: "proj".to_owned(),
            worker_id: "w1".to_owned(),
            node_id: "n1".to_owned(),
            task_id: "t1".to_owned(),
            generation: lease.generation,
            evidence: vec!["e1".to_owned()],
        };
        assert_eq!(
            table.accept_completion(&report, None),
            CompletionResult::Rejected(CompletionReject::ExpiredLease)
        );
        Ok(())
    }
}
