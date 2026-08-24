//! Drive a committed execution graph to a built, verified result by handing
//! each build node to a real headless worker (claude / codex / cursor) in the
//! trusted workspace, then running the graph's verification node against what it
//! produced. This is what turns a typed request into an actual artifact.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::efficiency::{RepairAction, WasteType};
use crate::efficiency_accounting::{self, EpisodeDraft, UpsertOutcome};
use crate::efficiency_config::EfficiencyConfig;
use crate::efficiency_detector::{self, NodeSnapshot, SnapshotState};
use crate::efficiency_policy::{
    self, ApprovalState, ImpactAssessment, PolicyDecision, PolicyRequest,
};

/// One node's execution record, for signed receipts + evidence roots.
#[derive(Clone)]
pub(crate) struct NodeRun {
    pub node: String,
    pub agent: String,
    pub is_verify: bool,
    pub ok: bool,
    /// The evidence-floor verdict when this was a verify node that actually ran a
    /// suite (`None` when the node was not a verifier or had nothing to verify).
    pub verified: Option<bool>,
    /// Content-addressed evidence digest (hex) of the workspace after the node.
    pub evidence_hex: String,
    /// Wall-clock time the node's agent took, in milliseconds — a progress
    /// signal the mid-run morphogenesis supervisor reads (a slow node triggers a
    /// proactive verification graft). Zero when unmeasured.
    pub latency_ms: u64,
}

/// Outcome of driving one graph.
pub(crate) struct RunOutcome {
    pub built: bool,
    pub verified: Option<bool>,
    pub detail: String,
    /// The node whose verification failed (drives governed evolution), if any.
    pub failed_node: Option<String>,
    /// Per-node execution log for chain receipts + the sanitized export.
    pub log: Vec<NodeRun>,
}
/// The headless worker to use, from `$FRACTAL_WORKER` (default `claude`).
fn worker_kind() -> String {
    std::env::var("FRACTAL_WORKER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "claude".to_owned())
}

/// Human label for the configured worker.
pub(crate) fn worker_label() -> String {
    worker_kind()
}

pub(crate) fn is_build(capability: &str) -> bool {
    capability.contains("code.generate")
        || capability.ends_with(".edit")
        || capability.contains("code.write")
        || capability == "content.analyze"
}

pub(crate) fn is_verify(capability: &str) -> bool {
    capability.contains("tests") || capability.contains("verify")
}

/// Order node ids so every edge `from → to` runs `from` first (Kahn).
fn topo_order(graph: &Value) -> Result<Vec<Value>> {
    let nodes = graph
        .get("nodes")
        .and_then(Value::as_array)
        .context("graph has no nodes")?;
    let mut by_id: BTreeMap<String, Value> = BTreeMap::new();
    let mut indegree: BTreeMap<String, usize> = BTreeMap::new();
    for node in nodes {
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .context("node missing id")?;
        by_id.insert(id.to_owned(), node.clone());
        indegree.entry(id.to_owned()).or_insert(0);
    }
    let mut adjacency: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (from, to) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        );
        if let (Some(from), Some(to)) = (from, to) {
            if by_id.contains_key(from) && by_id.contains_key(to) {
                adjacency
                    .entry(from.to_owned())
                    .or_default()
                    .push(to.to_owned());
                *indegree.get_mut(to).unwrap() += 1;
            }
        }
    }
    let mut ready: VecDeque<String> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut ordered = Vec::new();
    while let Some(id) = ready.pop_front() {
        if let Some(node) = by_id.get(&id) {
            ordered.push(node.clone());
        }
        for next in adjacency.get(&id).cloned().unwrap_or_default() {
            let degree = indegree.get_mut(&next).unwrap();
            *degree -= 1;
            if *degree == 0 {
                ready.push_back(next);
            }
        }
    }
    if ordered.len() != by_id.len() {
        bail!("execution graph has a cycle");
    }
    Ok(ordered)
}

/// A model for `kind`, overridden by `$FRACTAL_<KIND>_MODEL` when set.
///
/// Claude defaults to its rolling `opus` alias so unattended workers do not
/// fall back to the CLI's account-dependent default model.
fn model_for(kind: &str) -> Option<String> {
    let key = format!(
        "FRACTAL_{}_MODEL",
        kind.to_ascii_uppercase().replace('-', "_")
    );
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| match kind {
            "claude" => Some("opus".to_owned()),
            _ => None,
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentRole {
    Worker,
    LeadPlanner,
}

/// Build the headless worker command for `kind`, honoring a pinned model.
///
/// Lead planning is deliberately a separate invocation role.  In particular,
/// selecting Codex as the lead must not make its implementation-worker command
/// inherit the planner's high-effort Sol configuration.
pub(crate) fn worker_command(kind: &str, prompt: &str, role: AgentRole) -> Result<Command> {
    let identity = kind;
    let kind = command_kind_for_agent(kind);
    let mut command = match kind {
        "claude" => {
            let mut c = Command::new("claude");
            c.arg("-p");
            if let Some(model) = model_for("claude") {
                c.arg("--model").arg(model);
            }
            c.arg("--dangerously-skip-permissions").arg(prompt);
            c
        }
        "codex" | "codex-luna" => {
            let mut c = Command::new("codex");
            c.arg("exec");
            if role == AgentRole::LeadPlanner {
                // Keep the lead's planning model and reasoning effort explicit,
                // independent of ~/.codex/config.toml or worker routing pins.
                c.arg("--model").arg("gpt-5.6-sol");
                c.arg("--config").arg("model_reasoning_effort=\"high\"");
            } else {
                // `codex-luna` is a logical worker route backed by the Codex
                // binary. Keep the worker model explicit so a lead's
                // FRACTAL_CODEX_MODEL (or local Codex config) can never leak
                // Sol High into implementation work. Plain `codex` can still
                // reach this branch for a solo lead running a non-root node.
                c.arg("--model").arg("gpt-5.6-luna");
            }
            c.arg("--dangerously-bypass-approvals-and-sandbox")
                .arg(prompt);
            c
        }
        "cursor" | "cursor-agent" => {
            let mut c = Command::new("cursor-agent");
            c.arg("-p");
            if let Some(model) = model_for("cursor") {
                c.arg("--model").arg(model);
            }
            c.arg("--force").arg(prompt);
            c
        }
        "hermes" => {
            // Hermes one-shot (`-z`) with tools auto-approved (`--yolo`). A pinned
            // model routes through OpenRouter (where the free nemotron models live)
            // unless $FRACTAL_HERMES_PROVIDER overrides the provider.
            let mut c = Command::new("hermes");
            c.arg("--yolo");
            if let Some(model) = model_for("hermes") {
                let provider = std::env::var("FRACTAL_HERMES_PROVIDER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "openrouter".to_owned());
                c.arg("-m").arg(model).arg("--provider").arg(provider);
            }
            c.arg("-z").arg(prompt);
            c
        }
        other => bail!("unknown worker: {other} (use claude|codex|codex-luna|cursor|hermes)"),
    };
    command.env("FRACTAL_WORKER", identity);
    Ok(command)
}

/// Run a specific `kind` of worker on `instruction` in `workspace`, streaming.
/// The result of running an agent: whether it succeeded, and whether it was
/// killed for exceeding its time budget (a hang).
pub(crate) struct AgentRun {
    pub ok: bool,
    pub timed_out: bool,
    /// IDs of prior verified lessons injected into this worker's prompt.  The
    /// IDs are carried to the final lifecycle transition so reuse is recorded
    /// as an additive typed edge, never as a mutable weight.
    pub lesson_ids: Vec<String>,
}

#[allow(dead_code)]
fn run_worker_as(
    kind: &str,
    instruction: &str,
    workspace: &Path,
    timeout_ms: u64,
) -> Result<AgentRun> {
    run_worker_as_for_node(kind, instruction, workspace, timeout_ms, None)
}

fn run_worker_as_for_node(
    kind: &str,
    instruction: &str,
    workspace: &Path,
    timeout_ms: u64,
    node: Option<&Value>,
) -> Result<AgentRun> {
    let (lesson_section, lesson_ids) = node
        .map(|node| {
            crate::project_file::load(workspace)
                .map(|document| {
                    crate::lessons::render_for_node(
                        &crate::project_file::failure_graph(&document),
                        node,
                    )
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let lesson_section = if lesson_section.is_empty() {
        String::new()
    } else {
        format!("\n\n{lesson_section}")
    };
    let prompt = format!(
        "You are one agent on a coordinated team; a lead has planned the project (read INTERFACE.md \
         if it exists). Do exactly this assigned task and nothing else:\n\n{instruction}\n\nWork \
         entirely in the current directory. Create or edit only the files this task needs and make \
         any tests pass. Do not ask questions; make reasonable choices.{lesson_section}"
    );
    let mut run = run_agent_prompt(kind, &prompt, workspace, timeout_ms)?;
    run.lesson_ids = lesson_ids;
    Ok(run)
}

fn run_lead_agent_as(
    kind: &str,
    instruction: &str,
    workspace: &Path,
    timeout_ms: u64,
) -> Result<AgentRun> {
    let prompt = format!(
        "You are the lead planner/orchestrator for a coordinated team. Do exactly this assigned \
         lead task and nothing else:\n\n{instruction}\n\nWork entirely in the current directory. \
         Preserve the team's graph contract and make any required verification pass. Do not ask \
         questions; make reasonable choices."
    );
    run_lead_agent_prompt(kind, &prompt, workspace, timeout_ms)
}

/// Run an agent with a verbatim prompt (no team-task wrapper) under a hard time
/// budget: if it does not finish within `timeout_ms`, the process is killed and
/// reported as a (timed-out) failure — so a hung agent never stalls the whole
/// build; the node fails and the governed repair loop re-instructs it.
pub(crate) fn run_agent_prompt(
    kind: &str,
    prompt: &str,
    workspace: &Path,
    timeout_ms: u64,
) -> Result<AgentRun> {
    run_agent_prompt_with_role(
        kind,
        prompt,
        workspace,
        timeout_ms,
        AgentRole::Worker,
        "worker",
    )
}

/// Run a lead planner/orchestrator with its role-specific model configuration.
/// Generic implementation workers continue through [`run_agent_prompt`].
pub(crate) fn run_lead_agent_prompt(
    kind: &str,
    prompt: &str,
    workspace: &Path,
    timeout_ms: u64,
) -> Result<AgentRun> {
    run_agent_prompt_with_role(
        kind,
        prompt,
        workspace,
        timeout_ms,
        AgentRole::LeadPlanner,
        "lead planner",
    )
}

fn run_agent_prompt_with_role(
    kind: &str,
    prompt: &str,
    workspace: &Path,
    timeout_ms: u64,
    role: AgentRole,
    label: &str,
) -> Result<AgentRun> {
    // Detach the worker's stdin: headless agents (e.g. `claude -p`) otherwise
    // inherit the CLI's piped stdin and block reading it instead of exiting.
    let mut command = worker_command(kind, prompt, role)?;
    command
        .current_dir(workspace)
        .stdin(std::process::Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to launch {label} `{kind}` (is it on PATH?)"))?;
    let worker = crate::run_control::WorkerGuard::register(child.id());
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait()? {
            Some(status) => {
                // Some headless CLIs (notably Cursor) leave a worker-server
                // child in the invocation's process group after the CLI exits.
                // The task is complete, so close that exact group before
                // releasing its guard; otherwise every node leaks ~200 MiB.
                crate::run_control::terminate_worker(child.id());
                drop(worker);
                return Ok(AgentRun {
                    ok: status.success(),
                    timed_out: false,
                    lesson_ids: Vec::new(),
                });
            }
            None => {
                if Instant::now() >= deadline {
                    crate::run_control::terminate_worker(child.id());
                    let _ = child.wait();
                    drop(worker);
                    return Ok(AgentRun {
                        ok: false,
                        timed_out: true,
                        lesson_ids: Vec::new(),
                    });
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// The kill-timeout for an agent working `node`. `$FRACTAL_AGENT_TIMEOUT_MS`, when
/// set, is an explicit absolute override; otherwise it is the node's declared
/// budget but never less than a generous 15-minute floor — so a legitimately long
/// task is not killed, only a genuinely hung agent.
fn agent_timeout_ms(node: &Value) -> u64 {
    if let Some(override_ms) = std::env::var("FRACTAL_AGENT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return override_ms;
    }
    let budget = node
        .get("budget")
        .and_then(|budget| budget.get("timeout_ms"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    budget.max(900_000) // 15-minute floor
}

/// The binary that provides a given logical agent.
fn agent_binary(kind: &str) -> &str {
    match command_kind_for_agent(kind) {
        "cursor" | "cursor-agent" => "cursor-agent",
        "codex-luna" => "codex",
        other => other,
    }
}

/// Whether `binary` is resolvable on `PATH`.
fn binary_on_path(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(binary);
                candidate.is_file() || candidate.with_extension("exe").is_file()
            })
        })
        .unwrap_or(false)
}

/// Every supported agent whose binary is on `PATH` (ignores env config).
pub(crate) fn available_agents() -> Vec<String> {
    ["claude", "codex", "cursor", "hermes"]
        .into_iter()
        .filter(|kind| binary_on_path(agent_binary(kind)))
        .map(str::to_owned)
        .collect()
}

/// The agents to run, from `$FRACTAL_AGENTS` (comma-separated) or auto-detected
/// among claude / codex / cursor on `PATH`.
///
/// Codex has two logical routes in the scheduler: plain `codex` is reserved
/// for the first (lead planner/orchestrator) slot, while `codex-luna` is the
/// implementation-worker slot. When Codex is the lead, the physical binary is
/// represented by both slots; when it is not the lead, it contributes only the
/// worker slot. Both routes use the `codex` binary, but the worker route is
/// pinned to `gpt-5.6-luna` by [`worker_command`]. Keeping the logical route in
/// the roster makes leases and board assignments truthful.
pub(crate) fn detect_agents() -> Vec<String> {
    if let Ok(raw) = std::env::var("FRACTAL_AGENT_POOL") {
        return detect_pool_roster(&raw, binary_on_path)
            .unwrap_or_else(|error| panic!("invalid FRACTAL_AGENT_POOL: {error:#}"));
    }
    if let Ok(list) = std::env::var("FRACTAL_AGENTS") {
        let chosen: Vec<String> = list
            .split(',')
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect();
        if !chosen.is_empty() {
            return logical_agent_routes(chosen);
        }
    }
    let mut detected: Vec<String> = ["codex", "cursor", "claude", "hermes"]
        .into_iter()
        .filter(|kind| binary_on_path(agent_binary(kind)))
        .map(str::to_owned)
        .collect();
    if let Ok(lead) = std::env::var("FRACTAL_LEAD_AGENT") {
        let lead = lead.trim();
        if let Some(index) = detected.iter().position(|agent| agent == lead) {
            let selected = detected.remove(index);
            detected.insert(0, selected);
        }
    }
    logical_agent_routes(detected)
}

/// Convert physical Codex entries into the role-aware logical routes used by
/// scheduling and durable assignments. The first roster entry is always the
/// lead slot; subsequent Codex entries are Luna implementation workers.
fn logical_agent_routes(agents: Vec<String>) -> Vec<String> {
    let mut routes = Vec::with_capacity(agents.len() + 1);
    for (index, agent) in agents.into_iter().enumerate() {
        if agent == "codex" {
            if index == 0 {
                routes.push("codex".to_owned());
                routes.push("codex-luna".to_owned());
            } else {
                routes.push("codex-luna".to_owned());
            }
        } else {
            routes.push(agent);
        }
    }
    routes
}

/// Opt-in heterogeneous worker pool (`$FRACTAL_AGENT_POOL`). Worker slots are
/// counted separately from the Codex lead planner, which is never part of the
/// 20–42 worker capacity.
const POOL_PROVIDERS: [&str; 4] = ["codex", "cursor", "claude", "hermes"];
const POOL_MIN_WORKER_SLOTS: usize = 20;
const POOL_MAX_WORKER_SLOTS: usize = 42;
/// Matches the bounded repair budget in `orchestrate` (`MAX_REPAIRS`).
const POOL_NODE_RETRY_LIMIT: u32 = 3;

/// One expanded worker slot with a stable provider-qualified identity.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PoolSlot {
    id: String,
    provider: &'static str,
    kind: &'static str,
    index: usize,
}

/// Strip a `:N` pool suffix so command adapters keep seeing logical kinds.
fn command_kind_for_agent(agent: &str) -> &str {
    match agent.rsplit_once(':') {
        Some((kind, index))
            if !kind.is_empty()
                && !index.is_empty()
                && index.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            kind
        }
        _ => agent,
    }
}

/// A graph may hard-pin a node to one worker implementation. Presentation is
/// not scheduling authority: this value is enforced at the claim boundary and
/// again by the wave runner instead of being treated as advisory prose.
fn node_required_agent(node: &Value) -> Option<&str> {
    node.pointer("/executor/agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
}

pub(crate) fn agent_matches_requirement(agent: &str, required: &str) -> bool {
    let actual = command_kind_for_agent(agent);
    match required {
        "cursor" | "cursor-agent" => matches!(actual, "cursor" | "cursor-agent"),
        "codex" => matches!(actual, "codex" | "codex-luna"),
        "codex-luna" => actual == "codex-luna",
        "claude" => actual == "claude",
        "hermes" => actual == "hermes",
        other => actual == other,
    }
}

fn node_allows_agent(node: &Value, agent: &str) -> bool {
    node_required_agent(node)
        .map(|required| agent_matches_requirement(agent, required))
        .unwrap_or(true)
}

fn node_allows_agent_with_reroutes(
    node: &Value,
    agent: &str,
    reroutes: &BTreeMap<String, String>,
) -> bool {
    let rerouted = node
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| reroutes.get(id));
    rerouted
        .map(|required| agent_matches_requirement(agent, required))
        .unwrap_or_else(|| node_allows_agent(node, agent))
}

fn validate_node_agent_requirements(
    nodes: &[Value],
    agents: &[String],
    reroutes: &BTreeMap<String, String>,
) -> Result<()> {
    let node_ids: BTreeSet<&str> = nodes
        .iter()
        .filter_map(|node| node.get("id").and_then(Value::as_str))
        .collect();
    if let Some(unknown) = reroutes.keys().find(|id| !node_ids.contains(id.as_str())) {
        bail!("resume reroute names unknown graph node `{unknown}`");
    }
    for node in nodes {
        let id = node.get("id").and_then(Value::as_str).unwrap_or("node");
        let required = reroutes
            .get(id)
            .map(String::as_str)
            .or_else(|| node_required_agent(node));
        let Some(required) = required else {
            continue;
        };
        if !agents
            .iter()
            .any(|agent| agent_matches_requirement(agent, required))
        {
            bail!(
                "node `{id}` requires worker `{required}`, but the active roster is [{}]",
                agents.join(", ")
            );
        }
    }
    Ok(())
}

/// One hybrid run owns isolated worker checkouts and a single integration
/// boundary. Worker models never share a mutable source tree; only Fractal may
/// serialize their commits into the canonical workspace.
struct HybridSession {
    workspace: PathBuf,
    git_boundary: Mutex<()>,
    next_worktree: std::sync::atomic::AtomicU64,
}

impl HybridSession {
    fn initialize(workspace: &Path) -> Result<Self> {
        let root = hybrid_git_output(workspace, &["rev-parse", "--show-toplevel"])
            .context("hybrid mode requires a Git repository")?;
        let root = std::fs::canonicalize(root.trim()).context("resolve Git repository root")?;
        let requested = std::fs::canonicalize(workspace).context("resolve hybrid workspace")?;
        if root != requested {
            bail!(
                "hybrid mode must run from the Git repository root: {}",
                root.display()
            );
        }
        hybrid_git_output(workspace, &["rev-parse", "--verify", "HEAD"])
            .context("hybrid mode requires an existing HEAD commit")?;
        let tracked = hybrid_git_output(
            workspace,
            &["status", "--porcelain=v1", "--untracked-files=no"],
        )?;
        if !tracked.trim().is_empty() {
            bail!(
                "hybrid mode requires a clean tracked workspace before parallel integration:\n{}",
                tracked.trim()
            );
        }
        Ok(Self {
            workspace: requested,
            git_boundary: Mutex::new(()),
            next_worktree: std::sync::atomic::AtomicU64::new(0),
        })
    }

    fn run_worker(&self, node: &Value, agent: &str, timeout_ms: u64) -> Result<AgentRun> {
        let id = node.get("id").and_then(Value::as_str).unwrap_or("node");
        let instruction = node
            .get("instruction")
            .and_then(Value::as_str)
            .unwrap_or("");
        let worktree = self.create_worktree(id, agent)?;
        copy_hybrid_context(&self.workspace, &worktree)?;
        let run = run_worker_as_for_node(agent, instruction, &worktree, timeout_ms, Some(node))?;
        if !run.ok {
            eprintln!(
                "  [{agent}] hybrid worktree preserved after worker failure: {}",
                worktree.display()
            );
            return Ok(run);
        }

        self.integrate_worker_result(node, agent, &worktree)?;
        self.remove_worktree(&worktree)?;
        Ok(run)
    }

    fn create_worktree(&self, node: &str, agent: &str) -> Result<PathBuf> {
        let serial = self
            .next_worktree
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let leaf = format!(
            "fractal-hybrid-{}-{}-{}-{}",
            std::process::id(),
            hybrid_path_component(node),
            hybrid_path_component(agent),
            serial
        );
        let path = std::env::temp_dir().join(leaf);
        if path.exists() {
            bail!(
                "refuse to reuse existing hybrid worktree {}",
                path.display()
            );
        }
        let _git = self.git_boundary.lock().expect("hybrid git boundary");
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.workspace)
            .args(["worktree", "add", "--detach", "--quiet"])
            .arg(&path)
            .arg("HEAD")
            .status()
            .context("create isolated hybrid worktree")?;
        if !status.success() {
            bail!("git worktree add failed with {status}");
        }
        Ok(path)
    }

    fn integrate_worker_result(&self, node: &Value, agent: &str, worktree: &Path) -> Result<()> {
        let id = node.get("id").and_then(Value::as_str).unwrap_or("node");
        if node_required_agent(node).is_some() {
            if let Some(source) = declared_artifact_path(node, worktree) {
                if !source.exists() {
                    bail!(
                        "hybrid worker `{agent}` did not produce declared artifact {}",
                        source.display()
                    );
                }
            }
        }

        // Worker-controlled staging is never integration authority. Reset the
        // task index, then stage only the PRD-declared ownership below.
        if !hybrid_git_status(worktree, &["reset", "--quiet", "HEAD", "--"])? {
            bail!("git reset failed while normalizing hybrid node `{id}`");
        }
        let owned = hybrid_owned_paths(node, worktree)?;
        if !owned.is_empty() {
            let status = Command::new("git")
                .arg("-C")
                .arg(worktree)
                .args(["add", "-A", "--"])
                .args(&owned)
                .status()
                .context("stage declared hybrid ownership")?;
            if !status.success() {
                bail!("git add failed while staging hybrid node `{id}`");
            }
        }
        reject_hybrid_scope_escape(worktree, id)?;
        let staged = !hybrid_git_status(worktree, &["diff", "--cached", "--quiet"])?;
        let commit = if staged {
            let message = format!("fractal({id}): integrate {agent} worker result");
            let status = Command::new("git")
                .arg("-C")
                .arg(worktree)
                .args(["-c", "user.name=Fractal"])
                .args(["-c", "user.email=fractal@local"])
                .args(["commit", "--quiet", "-m"])
                .arg(&message)
                .status()
                .context("commit hybrid worker result")?;
            if !status.success() {
                bail!("hybrid worker commit failed with {status}");
            }
            Some(
                hybrid_git_output(worktree, &["rev-parse", "HEAD"])?
                    .trim()
                    .to_owned(),
            )
        } else {
            None
        };

        if commit.is_none()
            && is_build(node.get("capability").and_then(Value::as_str).unwrap_or(""))
        {
            bail!(
                "hybrid build node `{id}` exited successfully but made no tracked source changes"
            );
        }

        let _git = self.git_boundary.lock().expect("hybrid git boundary");
        if let Some(commit) = commit {
            let clean = hybrid_git_output(
                &self.workspace,
                &["status", "--porcelain=v1", "--untracked-files=no"],
            )?;
            if !clean.trim().is_empty() {
                bail!(
                    "canonical workspace changed before integrating `{id}`:\n{}",
                    clean.trim()
                );
            }
            let status = Command::new("git")
                .arg("-C")
                .arg(&self.workspace)
                .args(["cherry-pick", "--quiet"])
                .arg(&commit)
                .status()
                .context("integrate hybrid worker commit")?;
            if !status.success() {
                let _ = Command::new("git")
                    .arg("-C")
                    .arg(&self.workspace)
                    .args(["cherry-pick", "--abort"])
                    .status();
                bail!(
                    "hybrid integration conflict for node `{id}` from `{agent}`; worktree preserved at {}",
                    worktree.display()
                );
            }
        }
        copy_declared_artifact(node, worktree, &self.workspace)?;
        Ok(())
    }

    fn remove_worktree(&self, worktree: &Path) -> Result<()> {
        let _git = self.git_boundary.lock().expect("hybrid git boundary");
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.workspace)
            .args(["worktree", "remove", "--force"])
            .arg(worktree)
            .status()
            .context("remove completed hybrid worktree")?;
        if !status.success() {
            bail!("git worktree remove failed with {status}");
        }
        Ok(())
    }
}

/// Validate the hybrid integration boundary before a resume starts any durable
/// run/board side effects. `run_multi_agent_hybrid` repeats this check when it
/// creates the actual session so a workspace change cannot race the preflight.
pub(crate) fn validate_hybrid_workspace(workspace: &Path) -> Result<()> {
    HybridSession::initialize(workspace).map(|_| ())
}

fn hybrid_path_component(value: &str) -> String {
    let component: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    if component.is_empty() {
        "node".to_owned()
    } else {
        component
    }
}

fn hybrid_git_output(workspace: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.first().copied().unwrap_or("command")))?;
    if !output.status.success() {
        bail!(
            "git {} failed with {}: {}",
            args.first().copied().unwrap_or("command"),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("git output was not UTF-8")
}

/// Return the status success bit for Git commands whose nonzero result is data
/// (for example `git diff --quiet`) rather than a launch failure.
fn hybrid_git_status(workspace: &Path, args: &[&str]) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .status()
        .with_context(|| format!("run git {}", args.first().copied().unwrap_or("command")))?;
    Ok(status.success())
}

fn hybrid_owned_paths(node: &Value, worktree: &Path) -> Result<Vec<String>> {
    let mut declared = BTreeSet::new();
    if let Some(paths) = node
        .pointer("/efficiency/files_or_systems_affected")
        .and_then(Value::as_array)
    {
        for path in paths.iter().filter_map(Value::as_str) {
            declared.insert(path.trim().to_owned());
        }
    }
    if let Some(path) = node
        .pointer("/efficiency/expected_artifact")
        .and_then(Value::as_str)
    {
        declared.insert(path.trim().to_owned());
    }

    let mut owned = Vec::new();
    for path in declared {
        if path.is_empty()
            || matches!(path.as_str(), "." | "./")
            || path.starts_with(':')
            || path.chars().any(char::is_whitespace)
        {
            continue;
        }
        let candidate = Path::new(&path);
        let first = candidate.components().next();
        if candidate.is_absolute()
            || candidate.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
            || matches!(first, Some(std::path::Component::Normal(name)) if name == ".git" || name == ".fractal")
        {
            continue;
        }
        let tracked = !hybrid_git_output(worktree, &["ls-files", "--", &path])?
            .trim()
            .is_empty();
        if worktree.join(candidate).exists() || tracked {
            owned.push(path);
        }
    }
    owned.sort();
    owned.dedup();
    Ok(owned)
}

fn reject_hybrid_scope_escape(worktree: &Path, node: &str) -> Result<()> {
    let unstaged = hybrid_git_output(worktree, &["diff", "--name-only"])?;
    if let Some(path) = unstaged.lines().find(|path| !path.trim().is_empty()) {
        bail!(
            "hybrid node `{node}` modified tracked path `{path}` outside its declared file ownership"
        );
    }
    let untracked = hybrid_git_output(worktree, &["ls-files", "--others", "--exclude-standard"])?;
    if let Some(path) = untracked
        .lines()
        .map(str::trim)
        .find(|path| !path.is_empty() && !is_hybrid_generated_path(path))
    {
        bail!("hybrid node `{node}` created path `{path}` outside its declared file ownership");
    }
    Ok(())
}

fn is_hybrid_generated_path(path: &str) -> bool {
    path == "Cargo.lock"
        || path == ".coverage"
        || path.ends_with(".pyc")
        || [
            ".fractal/",
            "target/",
            "node_modules/",
            ".build/",
            "build/",
            "dist/",
            "__pycache__/",
            ".pytest_cache/",
            ".venv/",
            "venv/",
            "coverage/",
        ]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn copy_hybrid_context(workspace: &Path, worktree: &Path) -> Result<()> {
    let target = worktree.join(".fractal");
    for name in ["project.fractal", "lead-prd.json"] {
        let source = workspace.join(".fractal").join(name);
        if source.is_file() {
            std::fs::create_dir_all(&target)?;
            std::fs::copy(&source, target.join(name))?;
        }
    }
    Ok(())
}

fn copy_declared_artifact(node: &Value, worktree: &Path, workspace: &Path) -> Result<()> {
    let Some(source) = declared_artifact_path(node, worktree) else {
        return Ok(());
    };
    let Some(target) = declared_artifact_path(node, workspace) else {
        return Ok(());
    };
    if source == target || !source.is_file() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&source, &target).with_context(|| {
        format!(
            "copy hybrid artifact {} to {}",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn is_pool_slot_id(agent: &str) -> bool {
    matches!(
        command_kind_for_agent(agent),
        "codex-luna" | "cursor" | "cursor-agent" | "claude" | "hermes"
    ) && agent.contains(':')
}

fn slot_provider(agent: &str) -> Option<&'static str> {
    match command_kind_for_agent(agent) {
        "codex-luna" => Some("codex"),
        "cursor" | "cursor-agent" => Some("cursor"),
        "claude" => Some("claude"),
        "hermes" => Some("hermes"),
        _ => None,
    }
}

fn pool_worker_kind(provider: &str) -> Option<&'static str> {
    match provider {
        "codex" => Some("codex-luna"),
        "cursor" => Some("cursor"),
        "claude" => Some("claude"),
        "hermes" => Some("hermes"),
        _ => None,
    }
}

fn provider_caps_from_agents(agents: &[String]) -> BTreeMap<&'static str, usize> {
    let mut caps = BTreeMap::new();
    for agent in agents {
        if let Some(provider) = slot_provider(agent) {
            *caps.entry(provider).or_insert(0) += 1;
        }
    }
    caps
}

fn slot_may_claim(
    state: &Schedule,
    agent: &str,
    pool_mode: bool,
    caps: &BTreeMap<&'static str, usize>,
) -> bool {
    if !pool_mode {
        return true;
    }
    if state.slot_leases.contains_key(agent) {
        return false;
    }
    let Some(provider) = slot_provider(agent) else {
        return true;
    };
    let used = state
        .slot_leases
        .keys()
        .filter(|id| slot_provider(id) == Some(provider))
        .count();
    used < caps.get(provider).copied().unwrap_or(0)
}

/// Record a worker failure. Returns true when the failure is terminal.
///
/// In pool mode, non-lead failures requeue through [`reopen_for_retry`] up to
/// [`POOL_NODE_RETRY_LIMIT`] so other provider slots stay productive. Default
/// (non-pool) scheduling still fails the graph immediately.
fn pool_requeue_failure(
    state: &mut Schedule,
    workspace: &Path,
    id: &str,
    pool_mode: bool,
    is_lead: bool,
) -> bool {
    if !pool_mode || is_lead {
        state.failed = Some(id.to_owned());
        return true;
    }
    let retries = state.retry_counts.entry(id.to_owned()).or_insert(0);
    *retries = retries.saturating_add(1);
    let retries = *retries;
    if retries <= POOL_NODE_RETRY_LIMIT {
        let _ = reopen_for_retry(workspace, id);
        false
    } else {
        state.failed = Some(id.to_owned());
        true
    }
}

/// Parse `$FRACTAL_AGENT_POOL` (`codex=6,cursor=6,claude=6,hermes=6`).
///
/// Rejects duplicates, unknown providers, zero/overflow counts, totals outside
/// 20–42, and any config that omits one of the four required providers.
fn parse_agent_pool(raw: &str) -> Result<BTreeMap<String, usize>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("FRACTAL_AGENT_POOL is empty");
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            bail!("FRACTAL_AGENT_POOL has an empty entry");
        }
        let Some((key, value)) = part.split_once('=') else {
            bail!("FRACTAL_AGENT_POOL entry `{part}` must be provider=count");
        };
        let provider = key.trim();
        let count_str = value.trim();
        if !POOL_PROVIDERS.contains(&provider) {
            bail!("unknown provider `{provider}` in FRACTAL_AGENT_POOL");
        }
        if !seen.insert(provider.to_owned()) {
            bail!("duplicate provider `{provider}` in FRACTAL_AGENT_POOL");
        }
        if count_str.is_empty() || !count_str.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("invalid count for `{provider}` in FRACTAL_AGENT_POOL");
        }
        if count_str.len() > 1 && count_str.starts_with('0') {
            bail!("invalid count for `{provider}` in FRACTAL_AGENT_POOL");
        }
        let count: u64 = count_str.parse().map_err(|_| {
            anyhow::anyhow!("overflow count for `{provider}` in FRACTAL_AGENT_POOL")
        })?;
        if count == 0 {
            bail!("zero count for `{provider}` in FRACTAL_AGENT_POOL");
        }
        if count > POOL_MAX_WORKER_SLOTS as u64 {
            bail!("overflow count for `{provider}` in FRACTAL_AGENT_POOL");
        }
        counts.insert(provider.to_owned(), count as usize);
    }
    for provider in POOL_PROVIDERS {
        if !counts.contains_key(provider) {
            bail!("FRACTAL_AGENT_POOL is missing required provider `{provider}`");
        }
    }
    let total: usize = counts.values().copied().sum();
    if !(POOL_MIN_WORKER_SLOTS..=POOL_MAX_WORKER_SLOTS).contains(&total) {
        bail!(
            "FRACTAL_AGENT_POOL total worker slots {total} must be {POOL_MIN_WORKER_SLOTS}-{POOL_MAX_WORKER_SLOTS}"
        );
    }
    Ok(counts)
}

fn expand_pool_slots(counts: &BTreeMap<String, usize>) -> Vec<PoolSlot> {
    let mut slots = Vec::new();
    for provider in POOL_PROVIDERS {
        let n = counts.get(provider).copied().unwrap_or(0);
        let kind = pool_worker_kind(provider).expect("known pool provider");
        for index in 1..=n {
            slots.push(PoolSlot {
                id: format!("{kind}:{index}"),
                provider,
                kind,
                index,
            });
        }
    }
    slots
}

fn resolve_agent_pool(raw: &str, available: impl Fn(&str) -> bool) -> Result<Vec<PoolSlot>> {
    let counts = parse_agent_pool(raw)?;
    let missing: Vec<&str> = POOL_PROVIDERS
        .iter()
        .copied()
        .filter(|provider| !available(agent_binary(provider)))
        .collect();
    if !missing.is_empty() {
        bail!(
            "FRACTAL_AGENT_POOL binaries unavailable for: {}",
            missing.join(", ")
        );
    }
    Ok(expand_pool_slots(&counts))
}

fn detect_pool_roster(raw: &str, available: impl Fn(&str) -> bool) -> Result<Vec<String>> {
    let lead = std::env::var("FRACTAL_LEAD_AGENT")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    detect_pool_roster_with_lead(raw, available, lead.as_deref())
}

fn detect_pool_roster_with_lead(
    raw: &str,
    available: impl Fn(&str) -> bool,
    lead: Option<&str>,
) -> Result<Vec<String>> {
    let slots = resolve_agent_pool(raw, available)?;
    let lead = lead
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("codex");
    let mut roster = Vec::with_capacity(slots.len() + 1);
    roster.push(lead.to_owned());
    roster.extend(slots.into_iter().map(|slot| slot.id));
    Ok(roster)
}

/// Result of executing one node.
struct NodeOutcome {
    ok: bool,
    verified: Option<bool>,
    /// True when the agent was killed for exceeding its time budget.
    timed_out: bool,
    /// A human-readable note (e.g. the evidence-floor verdict) to surface.
    note: Option<String>,
    /// Prior verified lesson IDs that were injected into this node's worker
    /// prompt.  This is bookkeeping only; lesson confidence is never updated.
    lessons: Vec<String>,
}

impl NodeOutcome {
    fn success(verified: Option<bool>, note: Option<String>) -> Self {
        Self {
            ok: true,
            verified,
            timed_out: false,
            note,
            lessons: Vec::new(),
        }
    }

    fn failure(verified: Option<bool>, timed_out: bool, note: Option<String>) -> Self {
        Self {
            ok: false,
            verified,
            timed_out,
            note,
            lessons: Vec::new(),
        }
    }

    fn with_lessons(mut self, lessons: Vec<String>) -> Self {
        self.lessons = lessons;
        self
    }
}

fn declared_artifact_path(node: &Value, workspace: &Path) -> Option<PathBuf> {
    let raw = node
        .pointer("/efficiency/expected_artifact")
        .and_then(Value::as_str)?
        .trim();
    if raw.is_empty() || raw.chars().any(char::is_whitespace) {
        return None;
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else if raw.contains('/') || raw.starts_with('.') {
        Some(workspace.join(path))
    } else {
        None
    }
}

fn run_worker_node(
    node: &Value,
    agent: &str,
    workspace: &Path,
    hybrid: Option<&HybridSession>,
) -> Result<NodeOutcome> {
    let instruction = node
        .get("instruction")
        .and_then(Value::as_str)
        .unwrap_or("");
    let timeout_ms = agent_timeout_ms(node);
    let run = match hybrid {
        Some(session) => session.run_worker(node, agent, timeout_ms)?,
        None => run_worker_as_for_node(agent, instruction, workspace, timeout_ms, Some(node))?,
    };
    let timeout_note = run.timed_out.then(|| {
        format!(
            "agent hung — killed after {}s; failing the task so it is repaired",
            timeout_ms / 1000
        )
    });
    if !run.ok {
        return Ok(
            NodeOutcome::failure(None, run.timed_out, timeout_note).with_lessons(run.lesson_ids)
        );
    }
    if let Some(path) =
        node_required_agent(node).and_then(|_| declared_artifact_path(node, workspace))
    {
        if !path.exists() {
            return Ok(NodeOutcome::failure(
                None,
                false,
                Some(format!(
                    "worker exited successfully but did not produce declared artifact {}",
                    path.display()
                )),
            )
            .with_lessons(run.lesson_ids));
        }
    }
    Ok(NodeOutcome::success(None, timeout_note).with_lessons(run.lesson_ids))
}

/// Execute one node with a given agent. Build nodes run the worker. An explicitly
/// pinned verification node first runs that worker (so `cursor` really means the
/// Cursor CLI), then the trusted host independently evaluates the workspace.
/// Unpinned verification nodes remain host-only acceptance gates.
fn run_node(node: &Value, agent: &str, workspace: &Path) -> Result<NodeOutcome> {
    run_node_with_hybrid(node, agent, workspace, None)
}

fn run_node_with_hybrid(
    node: &Value,
    agent: &str,
    workspace: &Path,
    hybrid: Option<&HybridSession>,
) -> Result<NodeOutcome> {
    let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
    let id = node.get("id").and_then(Value::as_str).unwrap_or("node");
    // Defense in depth: callers must atomically checkout first, but this
    // worker seam is also private authority against direct execution paths.
    // A malformed declaration, missing ledger, stale evidence, or reviewer
    // self-checkout therefore fails before any worker or verifier runs.
    let required = crate::external_gates::required_gates(node)
        .context("malformed external gate declaration")?;
    if !required.is_empty() {
        let document = crate::project_file::load(workspace)
            .context("load canonical project before gated execution")?;
        crate::external_gates::enforce_checkout(workspace, &document, id, agent)
            .with_context(|| format!("external gate denied execution of node {}", id))?;
    }
    if capability == "control.closeout" {
        run_lead_closeout(node, agent, workspace)
    } else if is_build(capability) {
        run_worker_node(node, agent, workspace, hybrid)
    } else if is_verify(capability) && node_required_agent(node).is_some() {
        let worker = run_worker_node(node, agent, workspace, hybrid)?;
        if !worker.ok {
            return Ok(worker);
        }
        match crate::verify::evaluate_workspace(workspace, id, agent)? {
            Some(verdict) if verdict.complete => Ok(NodeOutcome::success(
                Some(true),
                Some(format!("worker task complete; {}", verdict.detail)),
            )
            .with_lessons(worker.lessons)),
            Some(verdict) => Ok(
                NodeOutcome::failure(Some(false), false, Some(verdict.detail))
                    .with_lessons(worker.lessons),
            ),
            None => Ok(NodeOutcome::failure(
                None,
                false,
                Some(
                    "worker task completed, but no native verification suite was found".to_owned(),
                ),
            )
            .with_lessons(worker.lessons)),
        }
    } else if is_verify(capability) {
        // Genuine governance: judge the suite with the real deny-by-default floor.
        match crate::verify::evaluate_workspace(workspace, id, agent)? {
            Some(verdict) => {
                if verdict.complete {
                    Ok(NodeOutcome::success(Some(true), Some(verdict.detail)))
                } else {
                    Ok(NodeOutcome::failure(
                        Some(false),
                        false,
                        Some(verdict.detail),
                    ))
                }
            }
            // Nothing to run: unverifiable, but not a failure.
            None => Ok(NodeOutcome::success(None, None)),
        }
    } else {
        Ok(NodeOutcome::success(None, None))
    }
}

fn run_lead_closeout(node: &Value, agent: &str, workspace: &Path) -> Result<NodeOutcome> {
    let closeout_path = workspace.join(".fractal").join("closeout.json");
    std::fs::remove_file(&closeout_path).ok();
    let instruction = node
        .get("instruction")
        .and_then(Value::as_str)
        .unwrap_or("Review and close out the project.");
    let timeout_ms = agent_timeout_ms(node);
    let run = run_lead_agent_as(agent, instruction, workspace, timeout_ms)?;
    if !run.ok {
        return Ok(NodeOutcome::failure(
            Some(false),
            run.timed_out,
            Some(if run.timed_out {
                "lead closeout timed out".to_owned()
            } else {
                "lead closeout agent failed".to_owned()
            }),
        ));
    }

    let prd: Value = serde_json::from_slice(
        &std::fs::read(workspace.join(".fractal").join("lead-prd.json"))
            .context("lead closeout requires .fractal/lead-prd.json")?,
    )
    .context("decode lead PRD for closeout")?;
    let closeout: Value = serde_json::from_slice(
        &std::fs::read(&closeout_path).context("lead did not write .fractal/closeout.json")?,
    )
    .context("decode lead closeout")?;
    let approved = validate_closeout(&prd, &closeout)?;
    Ok(NodeOutcome::success(
        Some(true),
        Some(format!("lead approved {approved} acceptance criteria")),
    ))
}

fn validate_closeout(prd: &Value, closeout: &Value) -> Result<usize> {
    let required: BTreeSet<String> = prd
        .get("acceptance_criteria")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|criterion| criterion.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    if required.is_empty() {
        bail!("lead PRD has no acceptance criteria to close out");
    }

    if closeout.get("schema").and_then(Value::as_str) != Some("fractal.closeout.v1")
        || closeout.get("status").and_then(Value::as_str) != Some("approved")
        || closeout
            .get("summary")
            .and_then(Value::as_str)
            .is_none_or(|summary| summary.trim().is_empty())
    {
        bail!("lead closeout must be an approved fractal.closeout.v1 with a summary");
    }
    let acceptance = closeout
        .get("acceptance")
        .and_then(Value::as_array)
        .context("lead closeout has no acceptance evidence")?;
    let passed: BTreeSet<String> = acceptance
        .iter()
        .filter(|entry| entry.get("passed").and_then(Value::as_bool) == Some(true))
        .filter(|entry| {
            entry
                .get("evidence")
                .and_then(Value::as_str)
                .is_some_and(|evidence| !evidence.trim().is_empty())
        })
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let missing: Vec<&String> = required.difference(&passed).collect();
    if !missing.is_empty() {
        bail!("lead closeout did not approve acceptance criteria: {missing:?}");
    }
    Ok(required.len())
}

/// Commit a node transition through the central learning mutation APIs.
/// The local and hosted boards are projections of that same file.
fn report_node(
    board: Option<&str>,
    node: &str,
    action: &str,
    agent: &str,
    workspace: &Path,
) -> Result<()> {
    let board_action = match action {
        "failed_execution" | "failed_verification" | "timeout" | "cancelled" => "release",
        other => other,
    };
    if action == "checkout" {
        // A direct checkout can implicitly clear a prior terminal learning
        // outcome.  Capture that typed failure before the overwrite; this is
        // diagnostic-only on legacy/corrupt projects so the checkout itself
        // remains governed by the canonical project-file seam.
        if let Err(error) = capture_existing_failure_before_overwrite(workspace, node) {
            eprintln!("  failure graph capture note: {error:#}");
        }
    }
    let result = match action {
        "checkout" => crate::project_file::checkout_start_node(workspace, node, agent, agent),
        "complete" => finish_learning_success(workspace, node, agent, None, None, false),
        "failed_verification" => release_learning_failure(
            workspace,
            node,
            agent,
            crate::learning_data::NodeOutcome::FailedVerification,
            crate::learning_data::FailureCode::WeakVerifier,
            None,
        ),
        "timeout" => release_learning_failure(
            workspace,
            node,
            agent,
            crate::learning_data::NodeOutcome::FailedExecution,
            crate::learning_data::FailureCode::Timeout,
            None,
        ),
        "failed_execution" => release_learning_failure(
            workspace,
            node,
            agent,
            crate::learning_data::NodeOutcome::FailedExecution,
            crate::learning_data::FailureCode::ToolFailure,
            None,
        ),
        "cancelled" => release_learning_failure(
            workspace,
            node,
            agent,
            crate::learning_data::NodeOutcome::Cancelled,
            crate::learning_data::FailureCode::PrematureCompletion,
            None,
        ),
        other => Err(anyhow::anyhow!("unsupported learning transition `{other}`")),
    };
    if let Err(error) = result {
        // Checkout is the execution authority. Do not swallow a denied
        // checkout: callers must skip run_node and never execute unowned work.
        if action == "checkout" {
            return Err(error);
        }
        eprintln!("  live graph state note: {error:#}");
    } else {
        crate::run_control::node_transition(board, node, board_action, agent);
        crate::project_sync::maybe_sync_runtime(workspace);
    }
    Ok(())
}

/// Record a full success/failure transition with measured evidence and costs.
#[allow(clippy::too_many_arguments)]
fn report_node_outcome(
    board: Option<&str>,
    node: &str,
    agent: &str,
    workspace: &Path,
    outcome: &NodeOutcome,
    evidence_hex: &str,
    latency_ms: u64,
    predecessors: &[String],
) {
    report_node_outcome_with_lessons(
        board,
        node,
        agent,
        workspace,
        outcome,
        evidence_hex,
        latency_ms,
        predecessors,
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
fn report_node_outcome_with_lessons(
    board: Option<&str>,
    node: &str,
    agent: &str,
    workspace: &Path,
    outcome: &NodeOutcome,
    evidence_hex: &str,
    latency_ms: u64,
    predecessors: &[String],
    provided_lessons: &[String],
) {
    if outcome.ok {
        let human = agent.eq_ignore_ascii_case("human");
        let board_action = "complete";
        crate::run_control::node_transition(board, node, board_action, agent);
        let result = finish_learning_success(
            workspace,
            node,
            agent,
            outcome.verified,
            Some((evidence_hex, latency_ms, predecessors)),
            human,
        );
        if let Err(error) = result {
            eprintln!("  live graph state note: {error:#}");
        } else {
            crate::project_sync::maybe_sync_runtime(workspace);
        }
        record_lesson_reuse(workspace, node, provided_lessons, outcome, evidence_hex);
        return;
    }

    let (learning_outcome, failure_code) = if outcome.verified == Some(false) {
        (
            crate::learning_data::NodeOutcome::FailedVerification,
            crate::learning_data::FailureCode::WeakVerifier,
        )
    } else if outcome.timed_out {
        (
            crate::learning_data::NodeOutcome::FailedExecution,
            crate::learning_data::FailureCode::Timeout,
        )
    } else {
        (
            crate::learning_data::NodeOutcome::FailedExecution,
            crate::learning_data::FailureCode::ToolFailure,
        )
    };
    crate::run_control::node_transition(board, node, "release", agent);
    let evidence = compact_evidence_ref(node, evidence_hex);
    let result = release_learning_failure(
        workspace,
        node,
        agent,
        learning_outcome,
        failure_code,
        Some((evidence.as_str(), latency_ms)),
    );
    if let Err(error) = result {
        eprintln!("  live graph state note: {error:#}");
    } else {
        crate::project_sync::maybe_sync_runtime(workspace);
    }
    record_lesson_reuse(workspace, node, provided_lessons, outcome, evidence_hex);
}

fn compact_evidence_ref(node: &str, evidence_hex: &str) -> String {
    let digest = evidence_hex.strip_prefix("sha256:").unwrap_or(evidence_hex);
    let short: String = digest.chars().take(24).collect();
    let reference = format!("evidence:{node}:{short}");
    if reference.len() <= 240 && !reference.chars().any(char::is_whitespace) {
        reference
    } else {
        format!("evidence:{node}")
    }
}

fn compact_artifact_ref(node: &str, evidence_hex: &str) -> String {
    let digest = evidence_hex.strip_prefix("sha256:").unwrap_or(evidence_hex);
    let short: String = digest.chars().take(24).collect();
    let reference = format!("artifact:{node}:{short}");
    if reference.len() <= 240 && !reference.chars().any(char::is_whitespace) {
        reference
    } else {
        format!("artifact:{node}")
    }
}

fn failure_code_name(code: crate::learning_data::FailureCode) -> &'static str {
    match code {
        crate::learning_data::FailureCode::MissingDependency => "missing_dependency",
        crate::learning_data::FailureCode::NodeTooBroad => "node_too_broad",
        crate::learning_data::FailureCode::NodeTooNarrow => "node_too_narrow",
        crate::learning_data::FailureCode::IncorrectAgent => "incorrect_agent",
        crate::learning_data::FailureCode::InsufficientContext => "insufficient_context",
        crate::learning_data::FailureCode::ToolFailure => "tool_failure",
        crate::learning_data::FailureCode::ConflictingParallelEdits => "conflicting_parallel_edits",
        crate::learning_data::FailureCode::InvalidOutputSchema => "invalid_output_schema",
        crate::learning_data::FailureCode::WeakVerifier => "weak_verifier",
        crate::learning_data::FailureCode::Timeout => "timeout",
        crate::learning_data::FailureCode::BudgetExceeded => "budget_exceeded",
        crate::learning_data::FailureCode::PrematureCompletion => "premature_completion",
    }
}

fn learning_outcome_name(outcome: crate::learning_data::NodeOutcome) -> &'static str {
    match outcome {
        crate::learning_data::NodeOutcome::VerifiedSuccess => "verified_success",
        crate::learning_data::NodeOutcome::UnverifiedSuccess => "unverified_success",
        crate::learning_data::NodeOutcome::FailedExecution => "failed_execution",
        crate::learning_data::NodeOutcome::FailedVerification => "failed_verification",
        crate::learning_data::NodeOutcome::Cancelled => "cancelled",
        crate::learning_data::NodeOutcome::Superseded => "superseded",
        crate::learning_data::NodeOutcome::HumanCompleted => "human_completed",
    }
}

fn node_metadata(
    document: &crate::project_file::FractalProject,
    node: &str,
) -> (String, String, String) {
    let graph_node = document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|value| value.get("id").and_then(Value::as_str) == Some(node));
    let capability = graph_node
        .and_then(|value| value.get("capability"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let objective = graph_node
        .and_then(|value| value.get("title").or_else(|| value.get("instruction")))
        .and_then(Value::as_str)
        .unwrap_or(node)
        .to_owned();
    let component = document
        .learning
        .nodes
        .get(node)
        .map(|record| record.node_type.clone())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "node".to_owned());
    (capability, objective, component)
}

fn evidence_ref_for_digest(
    node: &str,
    evidence_hex: Option<&str>,
) -> Option<crate::failure_graph::EvidenceRef> {
    let value = evidence_hex?.trim();
    let digest = value.strip_prefix("sha256:").unwrap_or(value);
    if digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Some(crate::failure_graph::EvidenceRef {
            sha256: Some(format!("sha256:{digest}")),
            kind: Some("workspace_state".to_owned()),
            ..crate::failure_graph::EvidenceRef::default()
        });
    }
    let reference = compact_evidence_ref(node, value);
    (!reference.trim().is_empty()).then(|| crate::failure_graph::EvidenceRef {
        legacy_ref: Some(reference),
        kind: Some("workspace_state".to_owned()),
        ..crate::failure_graph::EvidenceRef::default()
    })
}

fn safe_legacy_evidence(reference: &str) -> bool {
    let value = reference.trim();
    !value.is_empty()
        && value.chars().count() <= crate::failure_graph::MAX_STRING_CHARS
        && !value.chars().any(char::is_control)
        && !value.chars().any(char::is_whitespace)
        && !value.starts_with('/')
        && !value.starts_with('~')
        && !value.contains("../")
        && !value.contains('\\')
}

fn structured_failure_summary(
    node: &str,
    outcome: crate::learning_data::NodeOutcome,
    failure_code: crate::learning_data::FailureCode,
) -> String {
    // Keep this projection intentionally boring: it is derived only from typed
    // runtime outcome labels and the node ID, never from prompts, logs, or
    // verifier output excerpts.
    crate::failure_graph::redact_summary(&format!(
        "node {node} reported {} with failure code {}",
        learning_outcome_name(outcome),
        failure_code_name(failure_code)
    ))
}

/// Append a bounded, structured failure observation before the learning
/// lifecycle releases a failed checkout.  Existing observations for the same
/// attempt/evidence are not duplicated when an operator subsequently reopens
/// that checkout.
fn capture_failure_observation(
    workspace: &Path,
    node: &str,
    agent: &str,
    outcome: crate::learning_data::NodeOutcome,
    failure_code: crate::learning_data::FailureCode,
    evidence_hex: Option<&str>,
) -> Result<String> {
    let document = crate::project_file::load(workspace)?;
    let record = document
        .learning
        .nodes
        .get(node)
        .with_context(|| format!("learning node `{node}` missing before failure capture"))?;
    let (capability, objective, component) = node_metadata(&document, node);
    let code = failure_code_name(failure_code).to_owned();
    let id = crate::failure_graph::failure_id(node, &code);
    let mut evidence = evidence_ref_for_digest(node, evidence_hex)
        .into_iter()
        .collect::<Vec<_>>();
    for reference in record
        .verification
        .as_ref()
        .into_iter()
        .flat_map(|verification| verification.evidence_refs.iter())
        .filter(|reference| safe_legacy_evidence(reference))
    {
        let candidate = crate::failure_graph::EvidenceRef {
            legacy_ref: Some(reference.clone()),
            kind: Some("verification".to_owned()),
            ..crate::failure_graph::EvidenceRef::default()
        };
        let duplicate = evidence.iter().any(|existing| {
            existing.sha256 == candidate.sha256 && existing.legacy_ref == candidate.legacy_ref
        });
        if !duplicate {
            evidence.push(candidate);
        }
    }
    let attempt = record.attempt_count.max(1);
    let already_observed = crate::project_file::failure_graph(&document)
        .failures
        .get(&id)
        .is_some_and(|failure| {
            failure.observations.iter().any(|observation| {
                observation.attempt == attempt
                    && (evidence.is_empty()
                        || observation.evidence.iter().any(|existing| {
                            evidence.iter().any(|incoming| {
                                existing.sha256 == incoming.sha256
                                    && existing.legacy_ref == incoming.legacy_ref
                            })
                        }))
            })
        });
    if already_observed {
        return Ok(id);
    }
    let executor = record.executor.clone().unwrap_or_default();
    let extra = [(
        "objective_fingerprint".to_owned(),
        Value::String(crate::lessons::objective_fingerprint(&objective)),
    )]
    .into_iter()
    .collect();
    let failure = crate::failure_graph::FailureRecord {
        id: id.clone(),
        node_id: node.to_owned(),
        attempt,
        failure_code: code,
        outcome: learning_outcome_name(outcome).to_owned(),
        state: crate::failure_graph::FailureState::Unresolved,
        summary: structured_failure_summary(node, outcome, failure_code),
        capability: Some(capability),
        component: Some(component),
        evidence,
        agent: executor.agent.or_else(|| Some(agent.to_owned())),
        model: executor.model,
        version: executor.version,
        observed: crate::failure_graph::GraphGitProvenance {
            graph_hash: Some(document.graph_hash.clone()),
            ..crate::failure_graph::GraphGitProvenance::default()
        },
        extra,
        ..crate::failure_graph::FailureRecord::default()
    };
    crate::project_file::append_failure(workspace, failure)
}

fn capture_existing_failure_before_overwrite(
    workspace: &Path,
    node: &str,
) -> Result<Option<String>> {
    let document = crate::project_file::load(workspace)?;
    let Some(record) = document.learning.nodes.get(node) else {
        return Ok(None);
    };
    let Some(outcome) = record.outcome else {
        return Ok(None);
    };
    let Some(failure_code) = record.failure_code else {
        return Ok(None);
    };
    if !matches!(
        outcome,
        crate::learning_data::NodeOutcome::FailedExecution
            | crate::learning_data::NodeOutcome::FailedVerification
            | crate::learning_data::NodeOutcome::Cancelled
    ) {
        return Ok(None);
    }
    let evidence = record
        .verification
        .as_ref()
        .and_then(|verification| verification.evidence_refs.first())
        .map(String::as_str);
    capture_failure_observation(
        workspace,
        node,
        record
            .executor
            .as_ref()
            .and_then(|executor| executor.agent.as_deref())
            .unwrap_or("runtime"),
        outcome,
        failure_code,
        evidence,
    )
    .map(Some)
}

fn finish_learning_success(
    workspace: &Path,
    node: &str,
    agent: &str,
    verified: Option<bool>,
    measured: Option<(&str, u64, &[String])>,
    human: bool,
) -> Result<()> {
    if let Some((evidence_hex, _latency_ms, predecessors)) = measured {
        let artifact = compact_artifact_ref(node, evidence_hex);
        let _ = crate::project_file::record_artifact_produced(workspace, node, &artifact);
        record_predecessor_consumption(workspace, node, predecessors);
        match verified {
            Some(true) => {
                let evidence = vec![compact_evidence_ref(node, evidence_hex)];
                crate::project_file::record_verification_result(workspace, node, true, evidence)?;
            }
            Some(false) => {
                let evidence = vec![compact_evidence_ref(node, evidence_hex)];
                crate::project_file::record_verification_result(workspace, node, false, evidence)?;
            }
            None => {}
        }
    }
    if human {
        crate::project_file::record_human_intervention(
            workspace,
            node,
            Some("human completed node"),
        )?;
        crate::project_file::finish_node(
            workspace,
            node,
            agent,
            crate::learning_data::NodeOutcome::HumanCompleted,
        )?;
        return Ok(());
    }
    let outcome = match verified {
        Some(true) => crate::learning_data::NodeOutcome::VerifiedSuccess,
        _ => crate::learning_data::NodeOutcome::UnverifiedSuccess,
    };
    crate::project_file::finish_node(workspace, node, agent, outcome)?;
    if verified == Some(true) {
        if let Some((evidence_hex, _, _)) = measured {
            // Lesson persistence is additive diagnostics.  A verified task has
            // already completed through the guarded lifecycle seam; if a
            // lesson write fails, retain the successful task result and report
            // the issue without attempting a compensating graph transition.
            if let Err(error) =
                resolve_failures_and_create_lessons(workspace, node, agent, evidence_hex)
            {
                eprintln!("  failure graph resolution note: {error:#}");
            }
        }
    }
    Ok(())
}

fn release_learning_failure(
    workspace: &Path,
    node: &str,
    agent: &str,
    outcome: crate::learning_data::NodeOutcome,
    failure_code: crate::learning_data::FailureCode,
    measured: Option<(&str, u64)>,
) -> Result<()> {
    // Capture first.  The policy is diagnostic fail-closed for the task result:
    // a capture error is returned after release, so callers cannot interpret
    // it as success, while the checked-out assignment is still safely closed.
    let capture = capture_failure_observation(
        workspace,
        node,
        agent,
        outcome,
        failure_code,
        measured.map(|(evidence, _)| evidence),
    );
    let release =
        crate::project_file::release_node(workspace, node, agent, Some((outcome, failure_code)));
    if let Some((evidence, _latency_ms)) = measured {
        if outcome == crate::learning_data::NodeOutcome::FailedVerification {
            let verification = crate::project_file::record_verification_result(
                workspace,
                node,
                false,
                vec![evidence.to_owned()],
            );
            if let Err(error) = verification {
                eprintln!("  verification evidence note: {error:#}");
            }
        }
    }
    release?;
    capture.map(|_| ())
}

/// Resolve unresolved observations only after a later, evidence-backed
/// verified success for the same node/capability.  Each resolution adopts one
/// deterministic lesson and adds typed failure→lesson→applicability edges.
fn resolve_failures_and_create_lessons(
    workspace: &Path,
    node: &str,
    agent: &str,
    evidence_hex: &str,
) -> Result<()> {
    let document = crate::project_file::load(workspace)?;
    let learning = document
        .learning
        .nodes
        .get(node)
        .with_context(|| format!("learning node `{node}` missing after verified success"))?;
    let (capability, objective, component) = node_metadata(&document, node);
    let evidence = evidence_ref_for_digest(node, Some(evidence_hex))
        .context("verified success is missing compact evidence")?;
    let graph = crate::project_file::failure_graph(&document);
    let unresolved = graph
        .failures
        .values()
        .filter(|failure| {
            failure.state == crate::failure_graph::FailureState::Unresolved
                && failure.node_id == node
                && (failure.capability.is_none()
                    || failure.capability.as_deref() == Some(capability.as_str()))
        })
        .map(|failure| (failure.id.clone(), failure.failure_code.clone()))
        .collect::<Vec<_>>();
    if unresolved.is_empty() {
        return Ok(());
    }
    let executor = learning.executor.clone().unwrap_or_default();
    for (failure_id, failure_code) in unresolved {
        let resolution_summary = crate::failure_graph::redact_summary(&format!(
            "verified success for node {node} and capability {capability} after prior {failure_code} observation"
        ));
        let resolution = crate::failure_graph::FailureResolution {
            success: true,
            summary: resolution_summary,
            evidence: vec![evidence.clone()],
            resolved_by: Some(agent.to_owned()),
            observed: crate::failure_graph::GraphGitProvenance {
                graph_hash: Some(document.graph_hash.clone()),
                ..crate::failure_graph::GraphGitProvenance::default()
            },
            ..crate::failure_graph::FailureResolution::default()
        };
        crate::project_file::resolve_failure(workspace, &failure_id, resolution)?;

        let lesson_summary = crate::failure_graph::redact_summary(&format!(
            "Verified evidence shows node {node} can complete capability {capability} after a prior {failure_code} observation; validate current source before reuse"
        ));
        let lesson_id = crate::failure_graph::lesson_id(
            &lesson_summary,
            Some(capability.as_str()),
            Some(component.as_str()),
        );
        let mut lesson_extra =
            crate::lessons::applicability_fields(node, &capability, &failure_code, &objective);
        lesson_extra.insert("failure_id".to_owned(), Value::String(failure_id.clone()));
        lesson_extra.insert(
            "created_at".to_owned(),
            Value::String(crate::project_file::project_timestamp()),
        );
        let candidate = crate::failure_graph::LessonRecord {
            id: lesson_id.clone(),
            summary: lesson_summary,
            status: crate::failure_graph::LessonStatus::Adopted,
            capability: Some(capability.clone()),
            component: Some(component.clone()),
            evidence: vec![evidence.clone()],
            agent: executor.agent.clone().or_else(|| Some(agent.to_owned())),
            model: executor.model.clone(),
            version: executor.version.clone(),
            observed: crate::failure_graph::GraphGitProvenance {
                graph_hash: Some(document.graph_hash.clone()),
                ..crate::failure_graph::GraphGitProvenance::default()
            },
            extra: lesson_extra,
            ..crate::failure_graph::LessonRecord::default()
        };

        // Do not resurrect an operator-rejected or superseded lesson with the
        // same deterministic key.  Existing adopted lessons remain intact so
        // repeated verified retries retain their evidence history.
        let current_graph = crate::project_file::load_failure_graph(workspace)?;
        let lesson_exists = current_graph.lessons.get(&lesson_id);
        if !lesson_exists.is_some_and(|lesson| {
            matches!(
                lesson.status,
                crate::failure_graph::LessonStatus::Rejected
                    | crate::failure_graph::LessonStatus::Superseded
            )
        }) {
            crate::project_file::upsert_lesson(workspace, candidate)?;
        }

        let edge_evidence = Some(evidence.clone());
        for edge_type in [
            crate::failure_graph::FailureEdgeType::ResolvedBy,
            crate::failure_graph::FailureEdgeType::LessonFrom,
        ] {
            let (from, to) = if edge_type == crate::failure_graph::FailureEdgeType::ResolvedBy {
                (failure_id.clone(), lesson_id.clone())
            } else {
                (lesson_id.clone(), failure_id.clone())
            };
            let _ = crate::project_file::add_failure_edge(
                workspace,
                crate::failure_graph::EdgeRecord {
                    id: crate::failure_graph::edge_id(edge_type, &from, &to),
                    edge_type,
                    from,
                    to,
                    evidence: edge_evidence.clone(),
                    ..crate::failure_graph::EdgeRecord::default()
                },
            )?;
        }
        let _ = crate::project_file::add_failure_edge(
            workspace,
            crate::failure_graph::EdgeRecord {
                edge_type: crate::failure_graph::FailureEdgeType::AppliesTo,
                from: lesson_id,
                to: node.to_owned(),
                evidence: Some(evidence.clone()),
                ..crate::failure_graph::EdgeRecord::default()
            },
        )?;
    }
    Ok(())
}

/// Record that a lesson was supplied to a worker after the final result is
/// known.  The edge carries evidence and outcome metadata but never modifies a
/// confidence weight; later selector ranking is derived from these facts.
fn record_lesson_reuse(
    workspace: &Path,
    node: &str,
    lesson_ids: &[String],
    outcome: &NodeOutcome,
    evidence_hex: &str,
) {
    if lesson_ids.is_empty() {
        return;
    }
    let evidence = evidence_ref_for_digest(node, Some(evidence_hex));
    for lesson_id in lesson_ids {
        let mut edge = crate::failure_graph::EdgeRecord {
            edge_type: crate::failure_graph::FailureEdgeType::ReusedIn,
            from: lesson_id.clone(),
            to: node.to_owned(),
            evidence: evidence.clone(),
            ..crate::failure_graph::EdgeRecord::default()
        };
        edge.extra.insert(
            "outcome".to_owned(),
            Value::String(if outcome.ok {
                "success".to_owned()
            } else {
                "failure".to_owned()
            }),
        );
        edge.extra.insert(
            "verified".to_owned(),
            outcome.verified.map(Value::Bool).unwrap_or(Value::Null),
        );
        if let Err(error) = crate::project_file::add_failure_edge(workspace, edge) {
            eprintln!("  lesson reuse note: {error:#}");
        }
    }
}

fn record_predecessor_consumption(workspace: &Path, node: &str, predecessors: &[String]) {
    let Ok(document) = crate::project_file::load(workspace) else {
        return;
    };
    for predecessor in predecessors {
        let Some(record) = document.learning.nodes.get(predecessor) else {
            continue;
        };
        for artifact in &record.artifacts_produced {
            let _ = crate::project_file::record_artifact_consumed(workspace, node, artifact);
        }
    }
}

/// Mark dependency-ready incomplete nodes before they are claimed.
pub(crate) fn mark_ready_frontier(
    workspace: &Path,
    graph: &Value,
    completed: &BTreeSet<String>,
) -> Result<()> {
    let preds = predecessor_map(graph);
    for (id, dependencies) in &preds {
        if completed.contains(id) {
            continue;
        }
        if dependencies.iter().all(|dep| completed.contains(dep)) {
            let _ = crate::project_file::mark_node_ready(workspace, id);
        }
    }
    Ok(())
}

/// Reopen a previously failed/released node so a retry can start cleanly while
/// preserving attempt_count from earlier runs.
pub(crate) fn reopen_for_retry(workspace: &Path, node: &str) -> Result<()> {
    if let Err(error) = capture_existing_failure_before_overwrite(workspace, node) {
        eprintln!("  failure graph capture note: {error:#}");
    }
    crate::project_file::reopen_node(workspace, node)
}

/// Operator-driven completion path: record intervention and HumanCompleted.
#[allow(dead_code)]
pub(crate) fn complete_as_human(workspace: &Path, node: &str, agent: &str) -> Result<()> {
    crate::project_file::record_human_intervention(
        workspace,
        node,
        Some("operator marked node complete"),
    )?;
    crate::project_file::finish_node(
        workspace,
        node,
        agent,
        crate::learning_data::NodeOutcome::HumanCompleted,
    )?;
    crate::project_sync::maybe_sync_runtime(workspace);
    Ok(())
}

/// Cancel an in-flight checkout with a controlled Cancelled outcome.
#[allow(dead_code)]
pub(crate) fn cancel_checked_out_node(workspace: &Path, node: &str, agent: &str) -> Result<()> {
    // Cancellation is a controlled terminal failure and must be observed before
    // the assignment is released.
    let capture = capture_failure_observation(
        workspace,
        node,
        agent,
        crate::learning_data::NodeOutcome::Cancelled,
        crate::learning_data::FailureCode::PrematureCompletion,
        None,
    );
    let release = crate::project_file::release_node(
        workspace,
        node,
        agent,
        Some((
            crate::learning_data::NodeOutcome::Cancelled,
            crate::learning_data::FailureCode::PrematureCompletion,
        )),
    );
    release?;
    capture?;
    crate::project_sync::maybe_sync_runtime(workspace);
    Ok(())
}

/// A content-addressed digest of the workspace's top-level files (sorted names +
/// contents), used as a node's evidence after it runs.
fn workspace_digest(workspace: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut names: Vec<String> = std::fs::read_dir(workspace)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut hasher = Sha256::new();
    for name in &names {
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        if let Ok(bytes) = std::fs::read(workspace.join(name)) {
            hasher.update(&bytes);
        }
    }
    let mut hex = String::from("sha256:");
    for byte in hasher.finalize() {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Shared scheduler state for the multi-agent pull-queue.
#[derive(Default)]
struct Schedule {
    completed: BTreeSet<String>,
    in_progress: BTreeSet<String>,
    /// Pool slot identity → node currently leased to that slot.
    slot_leases: BTreeMap<String, String>,
    retry_counts: BTreeMap<String, u32>,
    failed: Option<String>,
    built: bool,
    verified: Option<bool>,
    log: Vec<NodeRun>,
}

/// Drive the whole graph with several agents: each agent repeatedly checks out a
/// dependency-ready node no one else holds, runs it, and marks it complete —
/// until the entire graph is done (or a node fails). Independent branches run in
/// parallel across agents.
pub(crate) fn run_multi_agent(
    graph: &Value,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
    completed_seed: &BTreeSet<String>,
) -> Result<RunOutcome> {
    run_multi_agent_inner(
        graph,
        workspace,
        agents,
        board,
        completed_seed,
        None,
        &BTreeMap::new(),
    )
}

pub(crate) fn run_multi_agent_hybrid(
    graph: &Value,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
    completed_seed: &BTreeSet<String>,
) -> Result<RunOutcome> {
    run_multi_agent_hybrid_with_reroutes(
        graph,
        workspace,
        agents,
        board,
        completed_seed,
        &BTreeMap::new(),
    )
}

pub(crate) fn run_multi_agent_hybrid_with_reroutes(
    graph: &Value,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
    completed_seed: &BTreeSet<String>,
    reroutes: &BTreeMap<String, String>,
) -> Result<RunOutcome> {
    let hybrid = HybridSession::initialize(workspace)?;
    run_multi_agent_inner(
        graph,
        workspace,
        agents,
        board,
        completed_seed,
        Some(&hybrid),
        reroutes,
    )
}

fn run_multi_agent_inner(
    graph: &Value,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
    completed_seed: &BTreeSet<String>,
    hybrid: Option<&HybridSession>,
    reroutes: &BTreeMap<String, String>,
) -> Result<RunOutcome> {
    let ordered = topo_order(graph)?; // validates acyclic
    validate_node_agent_requirements(&ordered, agents, reroutes)?;
    let ids: Vec<String> = ordered
        .iter()
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let node_by_id: BTreeMap<String, Value> = ordered
        .iter()
        .filter_map(|node| {
            node.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_owned(), node.clone()))
        })
        .collect();
    let mut predecessors: BTreeMap<String, Vec<String>> =
        ids.iter().map(|id| (id.clone(), Vec::new())).collect();
    for edge in graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(from), Some(to)) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        ) {
            if let Some(list) = predecessors.get_mut(to) {
                list.push(from.to_owned());
            }
        }
    }
    let total = ids.len();
    let graph_hash = graph
        .get("graph_hash")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    // Resume: pre-mark already-completed tasks so they are skipped (the ready
    // check treats them as done) but still counted toward `total`.
    let schedule = Mutex::new(Schedule {
        completed: completed_seed
            .iter()
            .filter(|id| ids.contains(id))
            .cloned()
            .collect(),
        ..Schedule::default()
    });
    let _ = mark_ready_frontier(workspace, graph, completed_seed);

    // The lead (first agent) is the ORCHESTRATOR: it plans the project (the root
    // node) and closes it out (control), then assigns + monitors — it does not do
    // the coding tasks. Every other agent is a WORKER that pulls ready coding
    // tasks in parallel and steals another when it finishes early. A solo agent
    // does everything itself.
    let lead: &str = agents.first().map(String::as_str).unwrap_or("");
    let has_workers = agents.len() > 1;
    let pool_mode = agents.iter().any(|agent| is_pool_slot_id(agent));
    let provider_caps = provider_caps_from_agents(agents);
    std::thread::scope(|scope| {
        for agent in agents {
            let agent = agent.clone();
            let is_lead = agent.as_str() == lead;
            let provider_caps = provider_caps.clone();
            let (schedule, ids, node_by_id, predecessors, graph, graph_hash) = (
                &schedule,
                &ids,
                &node_by_id,
                &predecessors,
                graph,
                &graph_hash,
            );
            scope.spawn(move || {
              let mut mine: u64 = 0;
              loop {
                // Fairness: an agent that has already done more work waits a
                // little longer before grabbing the next node, so work rotates
                // across the team instead of one agent winning every race.
                // Bounded so large graphs are not slowed materially.
                // The yield for an agent that has already worked must exceed the
                // idle poll below, so idle agents win the next (parallel) node
                // instead of one fast agent grabbing them all.
                if mine > 0 {
                    std::thread::sleep(Duration::from_millis(mine.min(3) * 120));
                }
                // Atomically check out a ready, unclaimed node.
                // The scheduler barrier serializes claims against efficiency
                // boundary inspection so repairs never race checkout.
                let claimed = {
                    let _barrier = lock_scheduler();
                    let mut state = schedule.lock().expect("schedule lock");
                    if state.failed.is_some() || state.completed.len() == total {
                        break;
                    }
                    let capability_of = |id: &String| {
                        node_by_id
                            .get(id)
                            .and_then(|node| node.get("capability"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned()
                    };
                    let gate_admitted = |id: &String| {
                        let Some(node) = node_by_id.get(id) else {
                            return false;
                        };
                        let Ok(required) = crate::external_gates::required_gates(node) else {
                            return false;
                        };
                        if required.is_empty() {
                            return true;
                        }
                        let Ok(document) = crate::project_file::load(workspace) else {
                            return false;
                        };
                        crate::external_gates::scheduler_admitted(
                            workspace,
                            graph_hash,
                            node,
                            document.external_gate_ledger.as_ref(),
                        )
                    };
                    let is_ready = |id: &String, state: &Schedule| {
                        !state.completed.contains(id)
                            && !state.in_progress.contains(id)
                            && predecessors[id]
                                .iter()
                                .all(|pred| state.completed.contains(pred))
                            && gate_admitted(id)
                    };
                    let is_root = |id: &String| predecessors[id].is_empty();
                    let is_control = |id: &String| capability_of(id).starts_with("control.");
                    // Role split: the lead plans (root) + closes out (control);
                    // workers do the middle coding/verify tasks in parallel.
                    let for_this_agent = |id: &String| {
                        let node = &node_by_id[id];
                        if node_required_agent(node).is_some() {
                            node_allows_agent_with_reroutes(node, &agent, reroutes)
                        } else if !has_workers {
                            true
                        } else if is_lead {
                            is_root(id) || is_control(id)
                        } else {
                            !is_root(id) && !is_control(id)
                        }
                    };
                    let next = if slot_may_claim(&state, &agent, pool_mode, &provider_caps) {
                        ids.iter()
                            .find(|id| is_ready(id, &state) && for_this_agent(id))
                    } else {
                        None
                    };
                    if next.is_none() && state.in_progress.is_empty() {
                        // No worker can make progress. If a dependency-ready
                        // node is denied by the external-gate predicate,
                        // terminate the run instead of polling forever.
                        if let Some(denied) = ids.iter().find(|id| {
                            !state.completed.contains(*id)
                                && !state.in_progress.contains(*id)
                                && predecessors[*id]
                                    .iter()
                                    .all(|pred| state.completed.contains(pred))
                                && !gate_admitted(id)
                        }) {
                            state.failed = Some(denied.clone());
                            break;
                        }
                    }
                    match next {
                        Some(id) => {
                            state.in_progress.insert(id.clone());
                            state.slot_leases.insert(agent.clone(), id.clone());
                            Some(id.clone())
                        }
                        None => None,
                    }
                };
                let Some(id) = claimed else {
                    std::thread::sleep(Duration::from_millis(30));
                    continue;
                };

                let node = node_by_id.get(&id).expect("claimed node exists");
                let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
                let is_planning =
                    has_workers && is_lead && predecessors[&id].is_empty() && is_build(capability);
                let clr = crate::ui::CLEAR_LINE;
                if is_planning {
                    println!("{clr}  🧠 [{agent}] planning the project & interface, then assigning tasks to the team…");
                } else if has_workers && is_lead {
                    println!("{clr}  [{agent}] (orchestrator) ▸ {id}");
                } else {
                    println!("{clr}  [{agent}] ▸ checked out {id} ({capability})");
                }
                if let Err(error) = report_node(board, &id, "checkout", &agent, workspace) {
                    let evidence_hex = workspace_digest(workspace);
                    let mut state = schedule.lock().expect("schedule lock");
                    state.in_progress.remove(&id);
                    state.slot_leases.remove(&agent);
                    state.failed = Some(id.clone());
                    state.log.push(NodeRun {
                        node: id.clone(),
                        agent: agent.clone(),
                        is_verify: is_verify(capability),
                        ok: false,
                        verified: None,
                        evidence_hex,
                        latency_ms: 0,
                    });
                    eprintln!("  [{agent}] ✗ {id}: checkout denied: {error:#}");
                    break;
                }
                let started = std::time::Instant::now();
                let result = run_node_with_hybrid(node, &agent, workspace, hybrid);
                let latency_ms = started.elapsed().as_millis() as u64;

                let evidence_hex = workspace_digest(workspace);
                let node_is_verify = is_verify(capability);
                let mut node_verified: Option<bool> = None;
                let preds = predecessors.get(&id).cloned().unwrap_or_default();
                let mut state = schedule.lock().expect("schedule lock");
                state.in_progress.remove(&id);
                state.slot_leases.remove(&agent);
                let node_ok = match result {
                    Ok(outcome) => {
                        let NodeOutcome {
                            ok,
                            verified,
                            timed_out: _,
                            note,
                            lessons,
                        } = &outcome;
                        if is_build(capability) && *ok {
                            state.built = true;
                        }
                        node_verified = *verified;
                        if let Some(value) = verified {
                            state.verified = Some(*value);
                        }
                        let suffix = note
                            .as_deref()
                            .map(|note| format!(" — {note}"))
                            .unwrap_or_default();
                        if *ok {
                            let first_completion = state.completed.insert(id.clone());
                            if first_completion {
                                mine += 1;
                                report_node_outcome_with_lessons(
                                    board,
                                    &id,
                                    &agent,
                                    workspace,
                                    &outcome,
                                    &evidence_hex,
                                    latency_ms,
                                    &preds,
                                    lessons,
                                );
                                let _ = mark_ready_frontier(workspace, graph, &state.completed);
                                if is_planning {
                                    println!("{clr}  [{agent}] ✓ plan ready — dispatching tasks to the workers.");
                                } else {
                                    println!("{clr}  [{agent}] ✓ {id}{suffix}");
                                }
                            }
                        } else {
                            report_node_outcome_with_lessons(
                                board,
                                &id,
                                &agent,
                                workspace,
                                &outcome,
                                &evidence_hex,
                                latency_ms,
                                &preds,
                                lessons,
                            );
                            if pool_requeue_failure(&mut state, workspace, &id, pool_mode, is_lead)
                            {
                                println!("{clr}  [{agent}] ✗ {id}{suffix}");
                            } else {
                                let retries = state.retry_counts.get(&id).copied().unwrap_or(0);
                                println!("{clr}  [{agent}] ✗ {id}{suffix} — requeued ({retries}/{POOL_NODE_RETRY_LIMIT})");
                            }
                        }
                        *ok
                    }
                    Err(error) => {
                        report_node_outcome(
                            board,
                            &id,
                            &agent,
                            workspace,
                            &NodeOutcome::failure(None, false, None),
                            &evidence_hex,
                            latency_ms,
                            &preds,
                        );
                        if pool_requeue_failure(&mut state, workspace, &id, pool_mode, is_lead) {
                            eprintln!("  [{agent}] ✗ {id}: {error:#}");
                        } else {
                            let retries = state.retry_counts.get(&id).copied().unwrap_or(0);
                            eprintln!(
                                "  [{agent}] ✗ {id}: {error:#} — requeued ({retries}/{POOL_NODE_RETRY_LIMIT})"
                            );
                        }
                        false
                    }
                };
                state.log.push(NodeRun {
                    node: id.clone(),
                    agent: agent.clone(),
                    is_verify: node_is_verify,
                    ok: node_ok,
                    verified: node_verified,
                    evidence_hex,
                    latency_ms,
                });
              }
            });
        }
    });

    let state = schedule.into_inner().expect("schedule");
    let detail = if let Some(failed) = &state.failed {
        format!("stopped at node `{failed}`")
    } else {
        match state.verified {
            Some(true) => "built and verified by the agent team".to_owned(),
            Some(false) => "built but verification failed".to_owned(),
            None if state.built => "built (no tests to verify)".to_owned(),
            None => "graph completed".to_owned(),
        }
    };
    Ok(RunOutcome {
        built: state.built,
        verified: state.verified,
        detail,
        failed_node: state.failed.clone(),
        log: state.log.clone(),
    })
}

/// Predecessor map: for each node id, the ids that must complete before it runs.
pub(crate) fn predecessor_map(graph: &Value) -> BTreeMap<String, Vec<String>> {
    let ids: Vec<String> = graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let mut preds: BTreeMap<String, Vec<String>> =
        ids.iter().map(|id| (id.clone(), Vec::new())).collect();
    for edge in graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(from), Some(to)) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        ) {
            if let Some(list) = preds.get_mut(to) {
                list.push(from.to_owned());
            }
        }
    }
    preds
}

/// The dependency-ready frontier: nodes not yet in `completed` whose predecessors
/// are all complete, in the graph's node order. This is the mid-run supervisor's
/// unit of work — one wave — after which morphogen triggers are evaluated.
#[allow(dead_code)]
pub(crate) fn ready_frontier(graph: &Value, completed: &BTreeSet<String>) -> Vec<Value> {
    ready_frontier_filtered(graph, completed, None)
}

/// Ready frontier that also honors efficiency suppressions and one-wave delays.
pub(crate) fn ready_frontier_filtered(
    graph: &Value,
    completed: &BTreeSet<String>,
    runtime: Option<&EfficiencyRuntime>,
) -> Vec<Value> {
    let preds = predecessor_map(graph);
    let effective =
        |id: &str| completed.contains(id) || runtime.is_some_and(|rt| rt.suppressed.contains(id));
    graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|node| {
            let id = node.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() || effective(id) {
                return false;
            }
            if runtime.is_some_and(|rt| rt.deferred.contains(id)) {
                return false;
            }
            preds
                .get(id)
                .map(|list| list.iter().all(|pred| effective(pred)))
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

/// Shared barrier between efficiency boundary inspection and node checkout.
fn scheduler_barrier() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Acquire the scheduler barrier (tests and checkout/boundary serialization).
pub(crate) fn lock_scheduler() -> MutexGuard<'static, ()> {
    scheduler_barrier()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Try to acquire the scheduler barrier without blocking (concurrency tests).
#[allow(dead_code)]
pub(crate) fn try_lock_scheduler() -> Option<MutexGuard<'static, ()>> {
    scheduler_barrier().try_lock().ok()
}

/// Hash-safe runtime outcomes from efficiency repairs. Never rewrites committed
/// graph bytes or `graph_hash`; cancelled work is suppressed from checkout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EfficiencyRuntime {
    pub(crate) suppressed: BTreeSet<String>,
    pub(crate) deferred: BTreeSet<String>,
    pub(crate) reassignments: BTreeMap<String, String>,
    /// Monotonic token captured with each immutable snapshot for stale detection.
    pub(crate) snapshot_epoch: u64,
}

/// Result of one between-wave efficiency boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoundaryReport {
    pub(crate) inspected: bool,
    pub(crate) detections: usize,
    pub(crate) episode_recorded: bool,
    pub(crate) upsert: Option<UpsertOutcome>,
    pub(crate) decision: Option<PolicyDecision>,
    pub(crate) applied_action: Option<RepairAction>,
    pub(crate) applied_nodes: Vec<String>,
    pub(crate) stale_snapshot: bool,
    pub(crate) graph_hash_unchanged: bool,
}

/// Run the deterministic efficiency hook under the scheduler barrier: snapshot
/// active/queued nodes, detect waste, decide policy, record one idempotent
/// episode, and apply only accepted/authorized hash-safe repairs. Revalidates
/// readiness before mutation and refuses to proceed on a stale snapshot.
pub(crate) fn run_efficiency_boundary(
    graph: &Value,
    graph_hash: &str,
    completed: &BTreeSet<String>,
    workspace: &Path,
    config: &EfficiencyConfig,
    runtime: &mut EfficiencyRuntime,
) -> Result<BoundaryReport> {
    run_efficiency_boundary_inner(
        graph, graph_hash, completed, workspace, config, runtime, false,
    )
}

/// When `simulate_stale` is true, the detected node is treated as completed after
/// the snapshot (test injector) so apply revalidation refuses the mutation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_efficiency_boundary_inner(
    graph: &Value,
    graph_hash: &str,
    completed: &BTreeSet<String>,
    workspace: &Path,
    config: &EfficiencyConfig,
    runtime: &mut EfficiencyRuntime,
    simulate_stale: bool,
) -> Result<BoundaryReport> {
    let _barrier = lock_scheduler();
    let hash_before = graph
        .get("graph_hash")
        .and_then(Value::as_str)
        .unwrap_or(graph_hash)
        .to_owned();

    // Clear one-wave delays from the previous boundary before snapshotting.
    runtime.deferred.clear();
    runtime.snapshot_epoch = runtime.snapshot_epoch.saturating_add(1);

    let snapshots = snapshot_active_and_queued(graph, completed, runtime);
    let detections = efficiency_detector::detect_waste(&snapshots);
    if detections.is_empty() {
        return Ok(BoundaryReport {
            inspected: true,
            detections: 0,
            episode_recorded: false,
            upsert: None,
            decision: None,
            applied_action: None,
            applied_nodes: Vec::new(),
            stale_snapshot: false,
            graph_hash_unchanged: hash_before
                == graph
                    .get("graph_hash")
                    .and_then(Value::as_str)
                    .unwrap_or(graph_hash),
        });
    }

    // One episode per boundary: highest-priority detection (detector order).
    let detection = detections[0].clone();
    let impact = assess_impact(graph, completed, runtime, &snapshots, &detection);
    let approval = approval_state(config, detection.proposed_action);
    let outcome = efficiency_policy::decide(&PolicyRequest {
        mode: config.mode,
        waste: detection.waste_type,
        action: detection.proposed_action,
        impact,
        approval,
        scoped_autonomy: config.high_impact_autonomy.clone(),
    });

    let accepted = matches!(
        outcome.decision,
        PolicyDecision::ApplyApproved | PolicyDecision::AutoApply
    );
    let human_override = approval == ApprovalState::Overridden;
    if human_override {
        let _ = crate::project_file::record_human_intervention(
            workspace,
            &detection.detected_node,
            Some("efficiency human override"),
        );
    }
    let draft = EpisodeDraft {
        waste_type: detection.waste_type,
        detected_node: detection.detected_node.clone(),
        affected_node_ids: detection.affected_node_ids.clone(),
        proposed_action: detection.proposed_action,
        accepted,
        mode: config.mode,
        estimated_tokens_avoided: detection.gross_avoidable_tokens,
        estimation_basis: detection.estimation_basis.clone(),
        confidence: detection.confidence,
        realized_tokens_saved: None,
        realization_basis: None,
        actual_followup_result: Some(outcome.reason.to_owned()),
        human_override,
        actor: "fractal-efficiency".to_owned(),
        evidence_refs: detection
            .similarity_evidence
            .iter()
            .map(|evidence| {
                format!(
                    "sim:{}:{}:{:.2}",
                    detection.detected_node, evidence.peer_node_id, evidence.score
                )
            })
            .take(8)
            .collect(),
        config_hash: config.config_hash(),
        detected_at: None,
        resolved_at: accepted.then(crate::project_file::project_timestamp),
    };

    let upsert = match efficiency_accounting::record_episode(workspace, draft) {
        Ok(outcome) => Some(outcome),
        Err(error) => {
            eprintln!("  efficiency episode note: {error:#}");
            None
        }
    };

    let mut applied_action = None;
    let mut applied_nodes = Vec::new();
    let mut stale_snapshot = false;

    if accepted {
        let mut live_completed = completed.clone();
        if simulate_stale {
            live_completed.insert(detection.detected_node.clone());
        }
        let live = snapshot_active_and_queued(graph, &live_completed, runtime);
        let still_present = live.iter().any(|node| node.id == detection.detected_node)
            && detection
                .affected_node_ids
                .iter()
                .all(|id| live.iter().any(|node| node.id == *id) || live_completed.contains(id));
        if !still_present {
            stale_snapshot = true;
        } else {
            applied_nodes = apply_hash_safe_repair(
                graph,
                &live_completed,
                runtime,
                detection.proposed_action,
                &detection,
            );
            if !applied_nodes.is_empty() || detection.proposed_action == RepairAction::SplitDrift {
                applied_action = Some(detection.proposed_action);
            }
        }
    }

    let hash_after = graph
        .get("graph_hash")
        .and_then(Value::as_str)
        .unwrap_or(graph_hash);
    Ok(BoundaryReport {
        inspected: true,
        detections: detections.len(),
        episode_recorded: upsert.is_some(),
        upsert,
        decision: Some(outcome.decision),
        applied_action,
        applied_nodes,
        stale_snapshot,
        graph_hash_unchanged: hash_before == hash_after,
    })
}

/// Immutable active (next frontier) + queued (remaining) snapshot.
pub(crate) fn snapshot_active_and_queued(
    graph: &Value,
    completed: &BTreeSet<String>,
    runtime: &EfficiencyRuntime,
) -> Vec<NodeSnapshot> {
    let frontier_ids: BTreeSet<String> = ready_frontier_filtered(graph, completed, Some(runtime))
        .iter()
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| {
            let id = node.get("id").and_then(Value::as_str)?;
            if id.is_empty() || completed.contains(id) || runtime.suppressed.contains(id) {
                return None;
            }
            let state = if frontier_ids.contains(id) {
                SnapshotState::Active
            } else {
                SnapshotState::Queued
            };
            Some(node_to_snapshot(node, state))
        })
        .collect()
}

fn node_to_snapshot(node: &Value, state: SnapshotState) -> NodeSnapshot {
    let id = node
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let title = node
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| node.get("id").and_then(Value::as_str))
        .unwrap_or("")
        .to_owned();
    let instruction = node
        .get("instruction")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let efficiency = node.get("efficiency");
    let meta =
        efficiency.and_then(|raw| crate::compile::node_efficiency_from_graph_value(raw).ok());
    let (
        estimated_remaining_tokens,
        dependencies,
        expected_artifact,
        files_or_systems_affected,
        verification_plan,
        current_assumptions,
        similarity_to_other_active_nodes,
        confidence_still_useful,
    ) = match meta {
        Some(meta) => (
            meta.estimated_remaining_tokens,
            meta.dependencies,
            meta.expected_artifact,
            meta.files_or_systems_affected,
            meta.verification_plan,
            meta.current_assumptions,
            meta.similarity_to_other_active_nodes,
            meta.confidence_still_useful,
        ),
        None => (
            1_000,
            Vec::new(),
            title.clone(),
            Vec::new(),
            String::new(),
            Vec::new(),
            BTreeMap::new(),
            1.0,
        ),
    };
    let verifies_node_ids = node
        .get("verifies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let verifies_node_ids = if verifies_node_ids.is_empty() {
        efficiency
            .and_then(|raw| raw.get("verifies_node_ids"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect()
    } else {
        verifies_node_ids
    };
    NodeSnapshot {
        id,
        state,
        title,
        instruction,
        dependencies,
        estimated_remaining_tokens,
        expected_artifact,
        files_or_systems_affected,
        verification_plan,
        current_assumptions,
        similarity_to_other_active_nodes,
        confidence_still_useful,
        attempt_count: node
            .get("attempt_count")
            .and_then(Value::as_u64)
            .unwrap_or(1) as u32,
        produced_artifacts: node
            .get("produced_artifacts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect(),
        referenced_artifacts: node
            .get("referenced_artifacts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect(),
        verifies_node_ids,
    }
}

fn approval_state(config: &EfficiencyConfig, action: RepairAction) -> ApprovalState {
    if config.overridden.contains(&action) {
        ApprovalState::Overridden
    } else if config.approved.contains(&action) {
        ApprovalState::Granted
    } else {
        ApprovalState::NotRequested
    }
}

fn assess_impact(
    graph: &Value,
    completed: &BTreeSet<String>,
    runtime: &EfficiencyRuntime,
    snapshots: &[NodeSnapshot],
    detection: &efficiency_detector::EfficiencyDetection,
) -> ImpactAssessment {
    let exact_duplicate = match detection.waste_type {
        WasteType::DuplicateTask | WasteType::DuplicateTest | WasteType::DuplicateResearch => {
            let Some(detected) = snapshots
                .iter()
                .find(|node| node.id == detection.detected_node)
            else {
                return ImpactAssessment::default();
            };
            detection
                .affected_node_ids
                .iter()
                .filter(|id| *id != &detection.detected_node)
                .any(|peer| {
                    snapshots.iter().any(|node| {
                        node.id == *peer
                            && node.title == detected.title
                            && node.expected_artifact == detected.expected_artifact
                    })
                })
        }
        _ => false,
    };
    let critical = critical_path_nodes(graph, completed, runtime);
    let on_critical_path = detection
        .affected_node_ids
        .iter()
        .any(|id| critical.contains(id));
    let active: BTreeSet<String> = snapshots
        .iter()
        .filter(|node| node.state == SnapshotState::Active)
        .map(|node| node.id.clone())
        .collect();
    let succ = successor_map(graph);
    let blocks_active_dependents = detection.affected_node_ids.iter().any(|id| {
        succ.get(id)
            .into_iter()
            .flatten()
            .any(|child| active.contains(child) && !runtime.suppressed.contains(child))
    });
    ImpactAssessment {
        exact_duplicate,
        on_critical_path,
        blocks_active_dependents,
    }
}

fn successor_map(graph: &Value) -> BTreeMap<String, Vec<String>> {
    let mut succ: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(from), Some(to)) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        ) {
            succ.entry(from.to_owned()).or_default().push(to.to_owned());
        }
    }
    succ
}

fn critical_path_nodes(
    graph: &Value,
    completed: &BTreeSet<String>,
    runtime: &EfficiencyRuntime,
) -> BTreeSet<String> {
    let succ = successor_map(graph);
    let mut depth: BTreeMap<String, usize> = BTreeMap::new();
    let ids: Vec<String> = graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .filter(|id| !completed.contains(id) && !runtime.suppressed.contains(id))
        .collect();
    fn depth_of(
        id: &str,
        succ: &BTreeMap<String, Vec<String>>,
        completed: &BTreeSet<String>,
        suppressed: &BTreeSet<String>,
        memo: &mut BTreeMap<String, usize>,
    ) -> usize {
        if let Some(value) = memo.get(id) {
            return *value;
        }
        let child_depth = succ
            .get(id)
            .into_iter()
            .flatten()
            .filter(|child| !completed.contains(*child) && !suppressed.contains(*child))
            .map(|child| 1 + depth_of(child, succ, completed, suppressed, memo))
            .max()
            .unwrap_or(0);
        memo.insert(id.to_owned(), child_depth);
        child_depth
    }
    for id in &ids {
        depth_of(id, &succ, completed, &runtime.suppressed, &mut depth);
    }
    let max = depth.values().copied().max().unwrap_or(0);
    depth
        .into_iter()
        .filter(|(_, value)| *value == max && max > 0)
        .map(|(id, _)| id)
        .collect()
}

/// Apply a repair without rewriting committed graph JSON / `graph_hash`.
fn apply_hash_safe_repair(
    graph: &Value,
    completed: &BTreeSet<String>,
    runtime: &mut EfficiencyRuntime,
    action: RepairAction,
    detection: &efficiency_detector::EfficiencyDetection,
) -> Vec<String> {
    let mut touched = Vec::new();
    let succ = successor_map(graph);
    match action {
        RepairAction::Cancel | RepairAction::Merge | RepairAction::ConsolidateVerifiers => {
            // Keep the earliest affected node; suppress the detected duplicate /
            // later consolidatable peers so checkout never claims them.
            let keep = detection
                .affected_node_ids
                .iter()
                .filter(|id| !completed.contains(*id))
                .min()
                .cloned();
            for id in &detection.affected_node_ids {
                if completed.contains(id) {
                    continue;
                }
                if keep.as_ref() == Some(id) {
                    continue;
                }
                if runtime.suppressed.insert(id.clone()) {
                    touched.push(id.clone());
                }
            }
            if touched.is_empty() && runtime.suppressed.insert(detection.detected_node.clone()) {
                touched.push(detection.detected_node.clone());
            }
        }
        RepairAction::DelayVerification => {
            if runtime.deferred.insert(detection.detected_node.clone()) {
                touched.push(detection.detected_node.clone());
            }
        }
        RepairAction::StopDownstream => {
            let mut stack = vec![detection.detected_node.clone()];
            let mut seen = BTreeSet::new();
            while let Some(current) = stack.pop() {
                for child in succ.get(&current).into_iter().flatten() {
                    if seen.insert(child.clone()) {
                        if !completed.contains(child) && runtime.suppressed.insert(child.clone()) {
                            touched.push(child.clone());
                        }
                        stack.push(child.clone());
                    }
                }
            }
        }
        RepairAction::Reassign => {
            let agent = "efficiency-reassigned".to_owned();
            runtime
                .reassignments
                .insert(detection.detected_node.clone(), agent);
            touched.push(detection.detected_node.clone());
        }
        RepairAction::SplitDrift => {
            // Structure-changing split requires governed graph evolution (child
            // hash). Hash-safe path: do not mutate the committed graph in place.
            // Episode already records the authorized proposal.
        }
    }
    touched.sort();
    touched.dedup();
    touched
}

/// Execute one node with one agent, timing it and reporting board transitions.
/// Mirrors the per-node body of `run_multi_agent` and is the shared unit both the
/// whole-graph executor and the wave executor build on.
fn run_and_record(node: &Value, agent: &str, workspace: &Path, board: Option<&str>) -> NodeRun {
    let id = node
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
    let clr = crate::ui::CLEAR_LINE;
    println!("{clr}  [{agent}] ▸ {id} ({capability})");
    // Serialize checkout with the efficiency boundary: never claim a node while
    // inspection/repair holds the scheduler barrier.
    {
        let _barrier = lock_scheduler();
        if let Err(error) = report_node(board, &id, "checkout", agent, workspace) {
            eprintln!("  [{agent}] ✗ {id}: checkout denied: {error:#}");
            return NodeRun {
                node: id,
                agent: agent.to_owned(),
                is_verify: is_verify(capability),
                ok: false,
                verified: None,
                evidence_hex: workspace_digest(workspace),
                latency_ms: 0,
            };
        }
    }
    let started = std::time::Instant::now();
    let result = run_node(node, agent, workspace);
    let latency_ms = started.elapsed().as_millis() as u64;
    let evidence_hex = workspace_digest(workspace);
    let is_verify_node = is_verify(capability);
    let predecessors = {
        // Wave callers pass single nodes; consume any artifacts already recorded
        // for declared dependencies when the project file is present.
        node.get("depends_on")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    let mut verified = None;
    let ok = match result {
        Ok(outcome) => {
            verified = outcome.verified;
            let suffix = outcome
                .note
                .as_deref()
                .map(|note| format!(" — {note}"))
                .unwrap_or_default();
            report_node_outcome_with_lessons(
                board,
                &id,
                agent,
                workspace,
                &outcome,
                &evidence_hex,
                latency_ms,
                &predecessors,
                &outcome.lessons,
            );
            if outcome.ok {
                println!("{clr}  [{agent}] ✓ {id}{suffix}");
            } else {
                println!("{clr}  [{agent}] ✗ {id}{suffix}");
            }
            outcome.ok
        }
        Err(error) => {
            report_node_outcome(
                board,
                &id,
                agent,
                workspace,
                &NodeOutcome::failure(None, false, None),
                &evidence_hex,
                latency_ms,
                &predecessors,
            );
            eprintln!("  [{agent}] ✗ {id}: {error:#}");
            false
        }
    };
    NodeRun {
        node: id,
        agent: agent.to_owned(),
        is_verify: is_verify_node,
        ok,
        verified,
        evidence_hex,
        latency_ms,
    }
}

/// Run one wave — a set of already-ready, mutually independent nodes — in parallel
/// across the agent team, role-aware (the lead runs root/control nodes; workers
/// run the coding/verify tasks; a solo agent runs everything). Returns one
/// `NodeRun` per node. The mid-run supervisor calls this once per frontier.
#[allow(dead_code)]
pub(crate) fn run_wave(
    nodes: &[Value],
    graph: &Value,
    agents: &[String],
    workspace: &Path,
    board: Option<&str>,
) -> Vec<NodeRun> {
    run_wave_with_runtime(nodes, graph, agents, workspace, board, None)
}

/// Wave runner that honors efficiency reassignments without stealing in-flight
/// checkouts (callers must already exclude suppressed/deferred nodes).
pub(crate) fn run_wave_with_runtime(
    nodes: &[Value],
    graph: &Value,
    agents: &[String],
    workspace: &Path,
    board: Option<&str>,
    runtime: Option<&EfficiencyRuntime>,
) -> Vec<NodeRun> {
    let preds = predecessor_map(graph);
    let lead: &str = agents.first().map(String::as_str).unwrap_or("");
    let has_workers = agents.len() > 1;
    let workers: Vec<&str> = if has_workers {
        agents[1..].iter().map(String::as_str).collect()
    } else {
        vec![lead]
    };
    let runs = Mutex::new(Vec::new());
    let next_worker = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for node in nodes {
            let id = node.get("id").and_then(Value::as_str).unwrap_or("");
            let capability = node.get("capability").and_then(Value::as_str).unwrap_or("");
            let is_root = preds.get(id).map(|p| p.is_empty()).unwrap_or(true);
            let is_control = capability.starts_with("control.");
            let agent: String = if let Some(required) = node_required_agent(node) {
                // Hard affinity wins over learned/runtime reassignment. If the
                // roster is malformed, invoke the declared route rather than
                // silently substituting a different worker.
                agents
                    .iter()
                    .find(|agent| agent_matches_requirement(agent, required))
                    .cloned()
                    .unwrap_or_else(|| required.to_owned())
            } else if let Some(reassigned) =
                runtime.and_then(|rt| rt.reassignments.get(id)).cloned()
            {
                reassigned
            } else if !has_workers || is_root || is_control {
                lead.to_owned()
            } else {
                let idx = next_worker.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                workers[idx % workers.len()].to_owned()
            };
            let (node, runs) = (node, &runs);
            scope.spawn(move || {
                let run = run_and_record(node, &agent, workspace, board);
                runs.lock().expect("wave runs lock").push(run);
            });
        }
    });
    runs.into_inner().expect("wave runs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::efficiency::EfficiencyMode;
    use crate::efficiency_accounting::UpsertOutcome;
    use crate::efficiency_policy::PolicyDecision;
    use serde_json::json;
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn hybrid_test_repository(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "fractal-hybrid-test-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        for args in [
            &["init", "--quiet"][..],
            &["config", "user.name", "Fractal Test"][..],
            &["config", "user.email", "fractal-test@local"][..],
        ] {
            assert!(hybrid_git_status(&root, args).unwrap());
        }
        fs::write(root.join("README.md"), "hybrid base\n").unwrap();
        assert!(hybrid_git_status(&root, &["add", "README.md"]).unwrap());
        assert!(hybrid_git_status(&root, &["commit", "--quiet", "-m", "base"]).unwrap());
        root
    }

    #[test]
    fn hybrid_resume_preflight_rejects_dirty_tracked_workspace() {
        let root = hybrid_test_repository("dirty-resume");
        fs::write(root.join("README.md"), "uncommitted tracked edit\n").unwrap();

        let error = validate_hybrid_workspace(&root).unwrap_err();

        assert!(error
            .to_string()
            .contains("requires a clean tracked workspace"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hybrid_resume_skips_every_completed_seed_assignment() {
        let root = hybrid_test_repository("completed-seed");
        let mut graph = json!({
            "schema":"fractal.execution_graph.v1",
            "graph_id":"fg_hybrid_resume",
            "goal":"Resume only unfinished work",
            "nodes":[
                {"id":"already_done","capability":"code.generate","instruction":"must not run"},
                {"id":"also_done","capability":"project.tests.execute","instruction":"must not run"}
            ],
            "edges":[{"from":"already_done","to":"also_done"}]
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        crate::project_file::persist(&root, &graph, "Hybrid resume").unwrap();
        let completed = BTreeSet::from(["already_done".to_owned(), "also_done".to_owned()]);

        let outcome =
            run_multi_agent_hybrid(&graph, &root, &["codex".to_owned()], None, &completed).unwrap();

        assert!(outcome.log.is_empty(), "completed nodes must not run again");
        assert!(outcome.failed_node.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hybrid_worktrees_integrate_parallel_nonoverlapping_results() {
        let root = hybrid_test_repository("parallel");
        let session = HybridSession::initialize(&root).unwrap();
        let alpha = json!({
            "id":"alpha",
            "capability":"code.generate",
            "efficiency":{"files_or_systems_affected":["alpha.txt"],"expected_artifact":"alpha.txt"}
        });
        let beta = json!({
            "id":"beta",
            "capability":"code.generate",
            "efficiency":{"files_or_systems_affected":["beta.txt"],"expected_artifact":"beta.txt"}
        });
        let alpha_tree = session.create_worktree("alpha", "cursor").unwrap();
        let beta_tree = session.create_worktree("beta", "codex-luna").unwrap();
        fs::write(alpha_tree.join("alpha.txt"), "from cursor\n").unwrap();
        fs::create_dir_all(alpha_tree.join("target/debug")).unwrap();
        fs::write(alpha_tree.join("target/debug/build-output"), "generated\n").unwrap();
        fs::write(alpha_tree.join("Cargo.lock"), "generated\n").unwrap();
        fs::write(beta_tree.join("beta.txt"), "from codex\n").unwrap();

        session
            .integrate_worker_result(&alpha, "cursor", &alpha_tree)
            .unwrap();
        session.remove_worktree(&alpha_tree).unwrap();
        session
            .integrate_worker_result(&beta, "codex-luna", &beta_tree)
            .unwrap();
        session.remove_worktree(&beta_tree).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("alpha.txt")).unwrap(),
            "from cursor\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("beta.txt")).unwrap(),
            "from codex\n"
        );
        assert!(!root.join("target").exists());
        assert!(!root.join("Cargo.lock").exists());
        let log = hybrid_git_output(&root, &["log", "--format=%s", "-3"]).unwrap();
        assert!(log.contains("fractal(alpha): integrate cursor worker result"));
        assert!(log.contains("fractal(beta): integrate codex-luna worker result"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hybrid_verifier_copies_proof_without_source_commit() {
        let root = hybrid_test_repository("artifact");
        let session = HybridSession::initialize(&root).unwrap();
        let node = json!({
            "id":"cursor_verify",
            "capability":"project.tests.execute",
            "executor":{"agent":"cursor"},
            "efficiency":{"expected_artifact":".fractal/cursor-proof.json"}
        });
        let worktree = session.create_worktree("cursor_verify", "cursor").unwrap();
        fs::create_dir_all(worktree.join(".fractal")).unwrap();
        fs::write(
            worktree.join(".fractal/cursor-proof.json"),
            "{\"passed\":true}\n",
        )
        .unwrap();
        session
            .integrate_worker_result(&node, "cursor", &worktree)
            .unwrap();
        session.remove_worktree(&worktree).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".fractal/cursor-proof.json")).unwrap(),
            "{\"passed\":true}\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hybrid_integration_conflict_aborts_without_corrupting_canonical_tree() {
        let root = hybrid_test_repository("conflict");
        let session = HybridSession::initialize(&root).unwrap();
        let first = json!({
            "id":"first",
            "capability":"code.generate",
            "efficiency":{"files_or_systems_affected":["README.md"],"expected_artifact":"README.md"}
        });
        let second = json!({
            "id":"second",
            "capability":"code.generate",
            "efficiency":{"files_or_systems_affected":["README.md"],"expected_artifact":"README.md"}
        });
        let first_tree = session.create_worktree("first", "cursor").unwrap();
        let second_tree = session.create_worktree("second", "codex-luna").unwrap();
        fs::write(first_tree.join("README.md"), "cursor version\n").unwrap();
        fs::write(second_tree.join("README.md"), "codex version\n").unwrap();

        session
            .integrate_worker_result(&first, "cursor", &first_tree)
            .unwrap();
        session.remove_worktree(&first_tree).unwrap();
        let error = session
            .integrate_worker_result(&second, "codex-luna", &second_tree)
            .unwrap_err();
        assert!(error.to_string().contains("integration conflict"));
        assert_eq!(
            fs::read_to_string(root.join("README.md")).unwrap(),
            "cursor version\n"
        );
        assert!(
            hybrid_git_output(&root, &["status", "--porcelain=v1", "--untracked-files=no"])
                .unwrap()
                .trim()
                .is_empty()
        );
        session.remove_worktree(&second_tree).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hybrid_rejects_source_changes_outside_declared_ownership() {
        let root = hybrid_test_repository("scope");
        let session = HybridSession::initialize(&root).unwrap();
        let node = json!({
            "id":"owned",
            "capability":"code.generate",
            "efficiency":{"files_or_systems_affected":["owned.txt"],"expected_artifact":"owned.txt"}
        });
        let worktree = session.create_worktree("owned", "cursor").unwrap();
        fs::write(worktree.join("owned.txt"), "owned\n").unwrap();
        fs::write(worktree.join("escape.txt"), "not owned\n").unwrap();
        assert!(hybrid_git_status(&worktree, &["add", "escape.txt"]).unwrap());
        let error = session
            .integrate_worker_result(&node, "cursor", &worktree)
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("outside its declared file ownership"));
        assert!(!root.join("owned.txt").exists());
        session.remove_worktree(&worktree).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn executor_affinity_matches_pool_slots_and_rejects_fallback_workers() {
        let node = json!({"id":"cursor_check","executor":{"agent":"cursor"}});
        assert!(node_allows_agent(&node, "cursor"));
        assert!(node_allows_agent(&node, "cursor:7"));
        assert!(!node_allows_agent(&node, "codex-luna"));
        assert!(!node_allows_agent(&node, "claude"));

        let roster = vec!["codex".to_owned(), "codex-luna".to_owned()];
        let error =
            validate_node_agent_requirements(&[node], &roster, &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("requires worker `cursor`"));
    }

    #[test]
    fn explicit_resume_reroute_changes_only_the_named_node_affinity() {
        let failed = json!({"id":"claude_failed","executor":{"agent":"claude"}});
        let untouched = json!({"id":"claude_untouched","executor":{"agent":"claude"}});
        let reroutes = BTreeMap::from([("claude_failed".to_owned(), "codex-luna".to_owned())]);

        assert!(node_allows_agent_with_reroutes(
            &failed,
            "codex-luna:2",
            &reroutes
        ));
        assert!(!node_allows_agent_with_reroutes(
            &failed, "claude:1", &reroutes
        ));
        assert!(node_allows_agent_with_reroutes(
            &untouched, "claude:1", &reroutes
        ));
        assert!(!node_allows_agent_with_reroutes(
            &untouched,
            "codex-luna:2",
            &reroutes
        ));
    }

    #[test]
    fn declared_artifact_is_a_worker_completion_postcondition() {
        let root =
            std::env::temp_dir().join(format!("fractal-declared-artifact-{}", std::process::id()));
        let node = json!({
            "efficiency": {"expected_artifact": ".fractal/cursor-result.json"}
        });
        assert_eq!(
            declared_artifact_path(&node, &root),
            Some(root.join(".fractal/cursor-result.json"))
        );
        assert_eq!(
            declared_artifact_path(
                &json!({"efficiency":{"expected_artifact":"passing native tests"}}),
                &root
            ),
            None
        );
    }

    #[test]
    fn codex_lead_planner_is_pinned_to_sol_high_without_changing_workers() {
        let lead = worker_command("codex", "plan", AgentRole::LeadPlanner).unwrap();
        let lead_args: Vec<String> = lead
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(lead_args
            .windows(2)
            .any(|pair| { pair == ["--model".to_owned(), "gpt-5.6-sol".to_owned()] }));
        assert!(lead_args.windows(2).any(|pair| {
            pair == [
                "--config".to_owned(),
                "model_reasoning_effort=\"high\"".to_owned(),
            ]
        }));

        let worker = worker_command("codex", "build", AgentRole::Worker).unwrap();
        let worker_args: Vec<String> = worker
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(worker_args
            .windows(2)
            .any(|pair| { pair == ["--model".to_owned(), "gpt-5.6-luna".to_owned()] }));
        assert!(!worker_args
            .iter()
            .any(|arg| arg == "model_reasoning_effort=\"high\""));
    }

    #[test]
    fn codex_luna_worker_route_uses_the_codex_binary_and_luna_model() {
        assert_eq!(agent_binary("codex-luna"), "codex");
        let worker = worker_command("codex-luna", "build", AgentRole::Worker).unwrap();
        assert_eq!(worker.get_program().to_string_lossy(), "codex");
        let args: Vec<String> = worker
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args
            .windows(2)
            .any(|pair| { pair == ["--model".to_owned(), "gpt-5.6-luna".to_owned()] }));
        assert!(!args
            .iter()
            .any(|arg| arg == "gpt-5.6-sol" || arg == "model_reasoning_effort=\"high\""));
    }

    #[test]
    fn codex_roster_exposes_a_lead_and_luna_worker_route() {
        assert_eq!(
            logical_agent_routes(vec!["codex".to_owned(), "cursor".to_owned()]),
            vec![
                "codex".to_owned(),
                "codex-luna".to_owned(),
                "cursor".to_owned()
            ]
        );
        assert_eq!(
            logical_agent_routes(vec!["cursor".to_owned(), "codex".to_owned()]),
            vec!["cursor".to_owned(), "codex-luna".to_owned()]
        );
    }

    #[test]
    fn pool_slot_identity_uses_existing_command_adapters() {
        let worker = worker_command("codex-luna:3", "build", AgentRole::Worker).unwrap();
        assert_eq!(worker.get_program().to_string_lossy(), "codex");
        assert_eq!(
            worker.get_envs().find_map(|(key, value)| {
                (key.to_string_lossy() == "FRACTAL_WORKER")
                    .then(|| value.unwrap().to_string_lossy().into_owned())
            }),
            Some("codex-luna:3".to_owned())
        );
        let cursor = worker_command("cursor:2", "build", AgentRole::Worker).unwrap();
        assert_eq!(cursor.get_program().to_string_lossy(), "cursor-agent");
        let claude = worker_command("claude:1", "build", AgentRole::Worker).unwrap();
        assert_eq!(claude.get_program().to_string_lossy(), "claude");
        let hermes = worker_command("hermes:4", "build", AgentRole::Worker).unwrap();
        assert_eq!(hermes.get_program().to_string_lossy(), "hermes");
    }

    #[test]
    fn closeout_requires_evidence_for_every_acceptance_criterion() {
        let prd = json!({
            "acceptance_criteria": [
                {"id": "AC-1"},
                {"id": "AC-2"}
            ]
        });
        let complete = json!({
            "schema": "fractal.closeout.v1",
            "status": "approved",
            "summary": "All acceptance checks passed.",
            "acceptance": [
                {"id": "AC-1", "passed": true, "evidence": "test expense creation"},
                {"id": "AC-2", "passed": true, "evidence": "test persistence"}
            ],
            "risks": []
        });
        assert_eq!(validate_closeout(&prd, &complete).unwrap(), 2);

        let incomplete = json!({
            "schema": "fractal.closeout.v1",
            "status": "approved",
            "summary": "One check is missing.",
            "acceptance": [
                {"id": "AC-1", "passed": true, "evidence": "test expense creation"}
            ]
        });
        assert!(validate_closeout(&prd, &incomplete).is_err());
    }

    fn temp_workspace() -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "fractal-efficiency-boundary-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn efficiency_node(
        id: &str,
        title: &str,
        tokens: u64,
        artifact: &str,
        similarity: BTreeMap<String, f64>,
    ) -> Value {
        let mut efficiency = crate::compile::baseline_efficiency_value(
            tokens,
            Vec::new(),
            artifact,
            vec![artifact.to_owned()],
            "",
        );
        if let Some(object) = efficiency.as_object_mut() {
            object.insert(
                "similarity_to_other_active_nodes".to_owned(),
                Value::Object(
                    similarity
                        .into_iter()
                        .map(|(peer, score)| (peer, Value::String(score.to_string())))
                        .collect(),
                ),
            );
            object.insert(
                "confidence_still_useful".to_owned(),
                Value::String("0.9".to_owned()),
            );
        }
        json!({
            "id": id,
            "title": title,
            "capability": "code.generate",
            "instruction": format!("build {title}"),
            "efficiency": efficiency
        })
    }

    fn duplicate_task_graph() -> (Value, String) {
        let mut sim_b = BTreeMap::new();
        sim_b.insert("task_a".to_owned(), 0.94);
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "eff-boundary",
            "nodes": [
                efficiency_node("task_a", "implement payments api", 8_000, "src/payments.rs", BTreeMap::new()),
                efficiency_node("task_b", "implement payments api", 8_000, "src/payments.rs", sim_b),
                {
                    "id": "done",
                    "title": "complete",
                    "capability": "control.complete",
                    "instruction": "close out",
                    "efficiency": crate::compile::baseline_efficiency_value(
                        100,
                        vec!["task_a".to_owned(), "task_b".to_owned()],
                        "closeout",
                        Vec::new(),
                        "manual"
                    )
                }
            ],
            "edges": [
                {"from": "task_a", "to": "done"},
                {"from": "task_b", "to": "done"}
            ]
        });
        let hash = fractal_contracts::canonical_sha256(&graph).expect("hash");
        graph["graph_hash"] = Value::String(hash.clone());
        (graph, hash)
    }

    fn seed_workspace(workspace: &Path, graph: &Value) {
        fs::create_dir_all(workspace).unwrap();
        crate::project_file::persist(workspace, graph, "Efficiency Boundary").unwrap();
    }

    fn suggest_config() -> EfficiencyConfig {
        EfficiencyConfig {
            mode: EfficiencyMode::Suggest,
            approved: Vec::new(),
            overridden: Vec::new(),
            high_impact_autonomy: Vec::new(),
        }
    }

    fn auto_config() -> EfficiencyConfig {
        EfficiencyConfig {
            mode: EfficiencyMode::AutoOptimize,
            approved: Vec::new(),
            overridden: Vec::new(),
            high_impact_autonomy: Vec::new(),
        }
    }

    fn approved_cancel_config() -> EfficiencyConfig {
        EfficiencyConfig {
            mode: EfficiencyMode::Suggest,
            approved: vec![RepairAction::Cancel],
            overridden: Vec::new(),
            high_impact_autonomy: Vec::new(),
        }
    }

    #[test]
    fn efficiency_boundary_records_one_idempotent_episode_before_checkout() {
        let workspace = temp_workspace();
        let (graph, hash) = duplicate_task_graph();
        seed_workspace(&workspace, &graph);
        let completed = BTreeSet::new();
        let mut runtime = EfficiencyRuntime::default();
        let config = suggest_config();

        let first =
            run_efficiency_boundary(&graph, &hash, &completed, &workspace, &config, &mut runtime)
                .unwrap();
        assert!(first.inspected);
        assert!(first.detections >= 1);
        assert_eq!(first.decision, Some(PolicyDecision::Propose));
        assert!(first.applied_action.is_none());
        assert_eq!(first.upsert, Some(UpsertOutcome::Inserted));
        assert!(first.graph_hash_unchanged);

        let second =
            run_efficiency_boundary(&graph, &hash, &completed, &workspace, &config, &mut runtime)
                .unwrap();
        assert!(matches!(
            second.upsert,
            Some(UpsertOutcome::IdempotentReplay) | Some(UpsertOutcome::Updated)
        ));
        let document = crate::project_file::load(&workspace).unwrap();
        assert_eq!(document.graph_hash, hash);
        assert_eq!(document.efficiency.unwrap().episodes.len(), 1);
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn approval_applies_cancel_without_changing_graph_hash() {
        let workspace = temp_workspace();
        let (graph, hash) = duplicate_task_graph();
        let graph_bytes = serde_json::to_vec(&graph).unwrap();
        seed_workspace(&workspace, &graph);
        let completed = BTreeSet::new();
        let mut runtime = EfficiencyRuntime::default();
        let report = run_efficiency_boundary(
            &graph,
            &hash,
            &completed,
            &workspace,
            &approved_cancel_config(),
            &mut runtime,
        )
        .unwrap();
        assert_eq!(report.decision, Some(PolicyDecision::ApplyApproved));
        assert_eq!(report.applied_action, Some(RepairAction::Cancel));
        assert!(!report.applied_nodes.is_empty());
        assert!(report.graph_hash_unchanged);
        assert_eq!(serde_json::to_vec(&graph).unwrap(), graph_bytes);
        assert_eq!(graph["graph_hash"], hash);
        let frontier = ready_frontier_filtered(&graph, &completed, Some(&runtime));
        let ids: BTreeSet<_> = frontier
            .iter()
            .filter_map(|node| node.get("id").and_then(Value::as_str))
            .collect();
        assert!(!ids.contains("task_b") || !ids.contains("task_a"));
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn auto_optimize_cancels_exact_duplicate_hash_invariantly() {
        let workspace = temp_workspace();
        let (graph, hash) = duplicate_task_graph();
        seed_workspace(&workspace, &graph);
        let completed = BTreeSet::new();
        let mut runtime = EfficiencyRuntime::default();
        let report = run_efficiency_boundary(
            &graph,
            &hash,
            &completed,
            &workspace,
            &auto_config(),
            &mut runtime,
        )
        .unwrap();
        assert!(matches!(
            report.decision,
            Some(PolicyDecision::AutoApply) | Some(PolicyDecision::Propose)
        ));
        assert!(report.graph_hash_unchanged);
        assert_eq!(
            crate::project_file::load(&workspace).unwrap().graph_hash,
            hash
        );
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn stale_snapshot_refuses_repair_mutation() {
        let workspace = temp_workspace();
        let (graph, hash) = duplicate_task_graph();
        seed_workspace(&workspace, &graph);
        let completed = BTreeSet::new();
        let mut runtime = EfficiencyRuntime::default();
        let report = run_efficiency_boundary_inner(
            &graph,
            &hash,
            &completed,
            &workspace,
            &approved_cancel_config(),
            &mut runtime,
            true,
        )
        .unwrap();
        assert!(report.stale_snapshot);
        assert!(report.applied_nodes.is_empty());
        assert!(runtime.suppressed.is_empty());
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn concurrency_barrier_blocks_checkout_during_boundary() {
        let ready = Arc::new(Barrier::new(2));
        let released = Arc::new(Barrier::new(2));
        let ready2 = Arc::clone(&ready);
        let released2 = Arc::clone(&released);
        let holder = std::thread::spawn(move || {
            let _guard = lock_scheduler();
            ready2.wait();
            released2.wait();
        });
        ready.wait();
        assert!(
            try_lock_scheduler().is_none(),
            "checkout must not acquire the barrier while efficiency holds it"
        );
        released.wait();
        holder.join().unwrap();
        assert!(try_lock_scheduler().is_some());
    }

    #[test]
    fn resume_boundary_is_idempotent_over_completed_seed() {
        let workspace = temp_workspace();
        let (graph, hash) = duplicate_task_graph();
        seed_workspace(&workspace, &graph);
        let mut completed = BTreeSet::new();
        completed.insert("task_a".to_owned());
        let mut runtime = EfficiencyRuntime::default();
        let config = suggest_config();
        let first =
            run_efficiency_boundary(&graph, &hash, &completed, &workspace, &config, &mut runtime)
                .unwrap();
        assert!(first.inspected);
        let episodes = crate::project_file::load(&workspace)
            .unwrap()
            .efficiency
            .map(|data| data.episodes.len())
            .unwrap_or(0);
        let second =
            run_efficiency_boundary(&graph, &hash, &completed, &workspace, &config, &mut runtime)
                .unwrap();
        assert!(second.inspected);
        let episodes_after = crate::project_file::load(&workspace)
            .unwrap()
            .efficiency
            .map(|data| data.episodes.len())
            .unwrap_or(0);
        assert_eq!(episodes, episodes_after);
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn suppressed_nodes_do_not_enter_ready_frontier() {
        let (graph, _) = duplicate_task_graph();
        let completed = BTreeSet::new();
        let mut runtime = EfficiencyRuntime::default();
        runtime.suppressed.insert("task_b".to_owned());
        let frontier = ready_frontier_filtered(&graph, &completed, Some(&runtime));
        let ids: Vec<_> = frontier
            .iter()
            .filter_map(|node| node.get("id").and_then(Value::as_str))
            .collect();
        assert!(ids.contains(&"task_a"));
        assert!(!ids.contains(&"task_b"));
    }

    fn lifecycle_workspace(name: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // Keep lifecycle fixtures offline so background graph sync cannot race
        // checkouts in temporary workspaces.
        std::env::set_var("FRACTAL_OFFLINE", "1");
        std::env::temp_dir().join(format!(
            "fractal-lifecycle-{name}-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn checkout(workspace: &std::path::Path, node: &str, agent: &str) {
        crate::project_file::checkout_start_node(workspace, node, agent, agent)
            .unwrap_or_else(|error| panic!("checkout {node}: {error:#}"));
    }

    fn persist_two_node_graph(workspace: &std::path::Path) -> Value {
        fs::create_dir_all(workspace).unwrap();
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_lifecycle",
            "nodes": [
                {
                    "id": "build",
                    "capability": "code.generate",
                    "instruction": "Build",
                    "title": "Build"
                },
                {
                    "id": "verify",
                    "capability": "project.tests",
                    "instruction": "Verify",
                    "title": "Verify",
                    "depends_on": ["build"]
                }
            ],
            "edges": [{"from": "build", "to": "verify"}]
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        crate::project_file::persist(workspace, &graph, "Lifecycle").unwrap();
        graph
    }

    fn persist_gated_graph(workspace: &std::path::Path) -> (Value, String) {
        fs::create_dir_all(workspace).unwrap();
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_external_gate_execute",
            "nodes": [{
                "id": "secure",
                "capability": "code.generate",
                "instruction": "Secure build",
                "external_gates": ["security_review"]
            }],
            "edges": []
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        crate::project_file::persist(workspace, &graph, "External gate").unwrap();
        fs::write(workspace.join("review.txt"), b"review evidence").unwrap();
        (graph, "review.txt".to_owned())
    }

    #[test]
    fn gated_execution_paths_fail_closed_before_worker_invocation() {
        let workspace = lifecycle_workspace("gated-execution");
        let (graph, _) = persist_gated_graph(&workspace);
        let node = graph["nodes"][0].clone();

        // Missing ledger is denied by the direct worker seam and therefore
        // cannot invoke a worker command.
        assert!(run_node(&node, "worker", &workspace).is_err());
        assert!(report_node(None, "secure", "checkout", "worker", &workspace).is_err());
        assert!(!crate::project_file::load(&workspace)
            .unwrap()
            .execution
            .unwrap()
            .assignments
            .contains_key("secure"));
        let run = run_and_record(&node, "worker", &workspace, None);
        assert!(!run.ok);

        // Record one approval, then the reviewer remains forbidden from
        // executing the same node (separation of duties).
        let document = crate::project_file::load(&workspace).unwrap();
        let input = crate::external_gates::RecordApprovalInput {
            node_id: "secure".to_owned(),
            gate: "security_review".to_owned(),
            evidence_path: std::path::PathBuf::from("review.txt"),
            reviewer_id: "reviewer".to_owned(),
            reviewer_label: "Reviewer".to_owned(),
            role: "security_reviewer".to_owned(),
            attestation: format!("approve:{}:secure:security_review", document.graph_hash),
        };
        let approval = crate::external_gates::record_approval(&workspace, input).unwrap();
        assert!(!run_and_record(&node, "reviewer", &workspace, None).ok);

        // Tampering after approval is caught before execution.
        fs::write(workspace.join("review.txt"), b"drift").unwrap();
        assert!(run_node(&node, "worker", &workspace).is_err());
        assert_eq!(
            crate::project_file::load(&workspace)
                .unwrap()
                .external_gate_ledger
                .unwrap()
                .records
                .iter()
                .filter(|record| record.kind == "approval")
                .count(),
            1
        );
        assert!(!approval.content_hash.is_empty());
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn multi_agent_scheduler_terminates_denied_gated_frontier() {
        let workspace = lifecycle_workspace("gated-scheduler");
        let (graph, _) = persist_gated_graph(&workspace);
        let result = run_multi_agent(
            &graph,
            &workspace,
            &["codex".to_owned()],
            None,
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(result.failed_node.as_deref(), Some("secure"));
        assert!(result.log.iter().all(|run| !run.ok));
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn lifecycle_records_ready_checkout_success_artifacts_and_monotonic_timestamps() {
        let workspace = lifecycle_workspace("success");
        let graph = persist_two_node_graph(&workspace);
        mark_ready_frontier(&workspace, &graph, &BTreeSet::new()).unwrap();
        let ready = crate::project_file::load(&workspace).unwrap();
        assert!(ready.learning.nodes["build"].ready_at.is_some());
        assert!(ready.learning.nodes["verify"].ready_at.is_none());

        checkout(&workspace, "build", "codex");
        let after_start = crate::project_file::load(&workspace).unwrap();
        let build = &after_start.learning.nodes["build"];
        assert_eq!(build.attempt_count, 1);
        assert!(build.started_at.is_some());
        assert_eq!(
            build.executor.as_ref().and_then(|e| e.agent.as_deref()),
            Some("codex")
        );
        let started = build.started_at.clone().unwrap();

        report_node_outcome(
            None,
            "build",
            "codex",
            &workspace,
            &NodeOutcome::success(None, None),
            "sha256:abcdef0123456789abcdef0123456789",
            1_500,
            &[],
        );
        let after_success = crate::project_file::load(&workspace).unwrap();
        let build = &after_success.learning.nodes["build"];
        assert_eq!(
            build.outcome,
            Some(crate::learning_data::NodeOutcome::UnverifiedSuccess)
        );
        assert!(build.finished_at.as_ref().unwrap() >= &started);
        assert!(!build.artifacts_produced.is_empty());
        assert_eq!(build.attempt_count, 1);
        assert!(build.actual_cost.is_none());

        mark_ready_frontier(&workspace, &graph, &BTreeSet::from(["build".to_owned()])).unwrap();
        let ready_verify = crate::project_file::load(&workspace).unwrap();
        assert!(ready_verify.learning.nodes["verify"].ready_at.is_some());
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn lifecycle_maps_tool_failure_timeout_and_failed_verification() {
        let workspace = lifecycle_workspace("failures");
        let _graph = persist_two_node_graph(&workspace);

        checkout(&workspace, "build", "cursor");
        report_node_outcome(
            None,
            "build",
            "cursor",
            &workspace,
            &NodeOutcome::failure(None, false, Some("tool blew up".into())),
            "sha256:11111111111111111111111111111111",
            10,
            &[],
        );
        let tool = crate::project_file::load(&workspace).unwrap();
        assert_eq!(
            tool.learning.nodes["build"].outcome,
            Some(crate::learning_data::NodeOutcome::FailedExecution)
        );
        assert_eq!(
            tool.learning.nodes["build"].failure_code,
            Some(crate::learning_data::FailureCode::ToolFailure)
        );

        reopen_for_retry(&workspace, "build").unwrap();
        let reopened = crate::project_file::load(&workspace).unwrap();
        assert!(reopened.learning.nodes["build"].outcome.is_none());
        assert_eq!(reopened.learning.nodes["build"].attempt_count, 1);
        assert!(reopened.learning.nodes["build"].reopen_count >= 1);

        checkout(&workspace, "build", "cursor");
        report_node_outcome(
            None,
            "build",
            "cursor",
            &workspace,
            &NodeOutcome::failure(None, true, Some("hung".into())),
            "sha256:22222222222222222222222222222222",
            20,
            &[],
        );
        let timeout = crate::project_file::load(&workspace).unwrap();
        assert_eq!(
            timeout.learning.nodes["build"].failure_code,
            Some(crate::learning_data::FailureCode::Timeout)
        );
        assert_eq!(timeout.learning.nodes["build"].attempt_count, 2);

        reopen_for_retry(&workspace, "build").unwrap();
        checkout(&workspace, "build", "cursor");
        report_node_outcome(
            None,
            "build",
            "cursor",
            &workspace,
            &NodeOutcome::failure(Some(false), false, Some("tests failed".into())),
            "sha256:33333333333333333333333333333333",
            30,
            &[],
        );
        let verify = crate::project_file::load(&workspace).unwrap();
        assert_eq!(
            verify.learning.nodes["build"].outcome,
            Some(crate::learning_data::NodeOutcome::FailedVerification)
        );
        assert_eq!(
            verify.learning.nodes["build"].failure_code,
            Some(crate::learning_data::FailureCode::WeakVerifier)
        );
        assert_eq!(
            verify.learning.nodes["build"]
                .verification
                .as_ref()
                .and_then(|v| v.passed),
            Some(false)
        );
        assert!(!verify.learning.nodes["build"]
            .verification
            .as_ref()
            .unwrap()
            .evidence_refs
            .is_empty());
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn failure_graph_captures_retry_observations_and_verified_resolution() {
        let workspace = lifecycle_workspace("failure-graph-resolution");
        let graph = persist_two_node_graph(&workspace);
        let graph_hash = crate::project_file::load(&workspace).unwrap().graph_hash;

        checkout(&workspace, "build", "codex");
        report_node_outcome(
            None,
            "build",
            "codex",
            &workspace,
            &NodeOutcome::failure(None, false, Some("raw output must not persist".into())),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            12,
            &[],
        );
        let failed = crate::project_file::load(&workspace).unwrap();
        let failure_id = crate::failure_graph::failure_id("build", "tool_failure");
        let first_graph = failed
            .failure_graph
            .as_ref()
            .expect("captured failure graph");
        let first = first_graph.failures.get(&failure_id).expect("failure");
        assert_eq!(first.state, crate::failure_graph::FailureState::Unresolved);
        assert_eq!(first.observations.len(), 1);
        assert!(!first.summary.contains("raw output"));
        assert_eq!(first.observations[0].attempt, 1);
        assert_eq!(failed.graph_hash, graph_hash);
        assert_eq!(failed.graph, graph);

        reopen_for_retry(&workspace, "build").unwrap();
        checkout(&workspace, "build", "codex");
        report_node_outcome(
            None,
            "build",
            "codex",
            &workspace,
            &NodeOutcome::failure(None, true, Some("timeout log must not persist".into())),
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            15,
            &[],
        );
        let retried = crate::project_file::load(&workspace).unwrap();
        let retried_graph = retried.failure_graph.as_ref().unwrap();
        let retried_failure = retried_graph.failures.get(&failure_id).unwrap();
        assert_eq!(
            retried_failure.state,
            crate::failure_graph::FailureState::Unresolved
        );
        assert_eq!(retried_failure.observations.len(), 1);
        assert_eq!(retried_failure.attempt, 1);
        let timeout_id = crate::failure_graph::failure_id("build", "timeout");
        let timeout_failure = retried_graph.failures.get(&timeout_id).unwrap();
        assert_eq!(timeout_failure.observations.len(), 1);
        assert_eq!(timeout_failure.attempt, 2);

        reopen_for_retry(&workspace, "build").unwrap();
        checkout(&workspace, "build", "codex");
        report_node_outcome(
            None,
            "build",
            "codex",
            &workspace,
            &NodeOutcome::success(Some(true), None),
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            20,
            &[],
        );
        let resolved = crate::project_file::load(&workspace).unwrap();
        let failure_graph = resolved.failure_graph.as_ref().expect("failure graph");
        let resolved_failure = failure_graph.failures.get(&failure_id).unwrap();
        assert_eq!(
            resolved_failure.state,
            crate::failure_graph::FailureState::Resolved
        );
        assert_eq!(resolved_failure.observations.len(), 1);
        assert!(resolved_failure
            .resolution
            .as_ref()
            .is_some_and(|resolution| resolution.success && !resolution.evidence.is_empty()));
        assert_eq!(
            failure_graph.failures.get(&timeout_id).unwrap().state,
            crate::failure_graph::FailureState::Resolved
        );
        assert_eq!(resolved.graph_hash, graph_hash);
        assert_eq!(resolved.graph, graph);
        assert!(failure_graph.lessons.values().any(|lesson| {
            lesson.status == crate::failure_graph::LessonStatus::Adopted
                && !lesson.evidence.is_empty()
                && lesson.summary.contains("validate current source")
        }));
        assert!(failure_graph.edges.values().any(|edge| {
            edge.edge_type == crate::failure_graph::FailureEdgeType::ResolvedBy
                && edge.from == failure_id
        }));
        assert!(failure_graph
            .edges
            .values()
            .any(|edge| { edge.edge_type == crate::failure_graph::FailureEdgeType::LessonFrom }));
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn lifecycle_preserves_attempts_on_retry_and_records_artifact_lineage() {
        let workspace = lifecycle_workspace("lineage");
        let graph = persist_two_node_graph(&workspace);
        checkout(&workspace, "build", "claude");
        report_node_outcome(
            None,
            "build",
            "claude",
            &workspace,
            &NodeOutcome::success(None, None),
            "sha256:aaaabbbbccccddddeeeeffffaaaabbbb",
            5,
            &[],
        );
        let produced = crate::project_file::load(&workspace)
            .unwrap()
            .learning
            .nodes["build"]
            .artifacts_produced
            .clone();
        assert_eq!(produced.len(), 1);

        mark_ready_frontier(&workspace, &graph, &BTreeSet::from(["build".to_owned()])).unwrap();
        checkout(&workspace, "verify", "claude");
        report_node_outcome(
            None,
            "verify",
            "claude",
            &workspace,
            &NodeOutcome::success(Some(true), None),
            "sha256:ffffeeeeddddccccbbbbaaaaffffeeee",
            8,
            &["build".to_owned()],
        );
        let document = crate::project_file::load(&workspace).unwrap();
        assert_eq!(document.learning.nodes["verify"].consumed_by, produced);
        assert_eq!(
            document.learning.nodes["verify"].outcome,
            Some(crate::learning_data::NodeOutcome::VerifiedSuccess)
        );
        assert_eq!(
            document.learning.nodes["verify"]
                .verification
                .as_ref()
                .and_then(|v| v.passed),
            Some(true)
        );
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn lifecycle_human_completion_and_cancellation() {
        let workspace = lifecycle_workspace("human");
        let _graph = persist_two_node_graph(&workspace);
        crate::project_file::checkout_start_node(&workspace, "build", "human", "human").unwrap();
        complete_as_human(&workspace, "build", "human").unwrap();
        let human = crate::project_file::load(&workspace).unwrap();
        assert!(human.learning.nodes["build"].human_intervention);
        assert_eq!(
            human.learning.nodes["build"].outcome,
            Some(crate::learning_data::NodeOutcome::HumanCompleted)
        );

        crate::project_file::checkout_start_node(&workspace, "verify", "codex", "codex").unwrap();
        cancel_checked_out_node(&workspace, "verify", "codex").unwrap();
        let cancelled = crate::project_file::load(&workspace).unwrap();
        assert_eq!(
            cancelled.learning.nodes["verify"].outcome,
            Some(crate::learning_data::NodeOutcome::Cancelled)
        );
        assert_eq!(
            cancelled.learning.nodes["verify"].failure_code,
            Some(crate::learning_data::FailureCode::PrematureCompletion)
        );
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn cross_boundary_lifecycle_covers_controlled_paths_and_compact_evidence() {
        let workspace = lifecycle_workspace("cross-boundary");
        let graph = persist_two_node_graph(&workspace);

        // Success path with artifact production.
        mark_ready_frontier(&workspace, &graph, &BTreeSet::new()).unwrap();
        checkout(&workspace, "build", "codex");
        report_node_outcome(
            None,
            "build",
            "codex",
            &workspace,
            &NodeOutcome::failure(None, false, Some("tool blew up".into())),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            11,
            &[],
        );
        let failed = crate::project_file::load(&workspace).unwrap();
        assert_eq!(
            failed.learning.nodes["build"].outcome,
            Some(crate::learning_data::NodeOutcome::FailedExecution)
        );
        assert_eq!(
            failed.learning.nodes["build"].failure_code,
            Some(crate::learning_data::FailureCode::ToolFailure)
        );

        // Retry preserves attempt history, then verified success + consumption.
        reopen_for_retry(&workspace, "build").unwrap();
        checkout(&workspace, "build", "codex");
        report_node_outcome(
            None,
            "build",
            "codex",
            &workspace,
            &NodeOutcome::success(None, None),
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            22,
            &[],
        );
        let after_retry = crate::project_file::load(&workspace).unwrap();
        assert_eq!(after_retry.learning.nodes["build"].attempt_count, 2);
        let produced = after_retry.learning.nodes["build"]
            .artifacts_produced
            .clone();
        assert_eq!(produced.len(), 1);
        assert!(produced[0].chars().all(|c| !c.is_whitespace()));
        assert!(produced[0].len() <= 240);

        mark_ready_frontier(&workspace, &graph, &BTreeSet::from(["build".to_owned()])).unwrap();
        checkout(&workspace, "verify", "codex");
        report_node_outcome(
            None,
            "verify",
            "codex",
            &workspace,
            &NodeOutcome::success(Some(true), None),
            "sha256:cccccccccccccccccccccccccccccccc",
            33,
            &["build".to_owned()],
        );
        let verified = crate::project_file::load(&workspace).unwrap();
        assert_eq!(
            verified.learning.nodes["verify"].outcome,
            Some(crate::learning_data::NodeOutcome::VerifiedSuccess)
        );
        assert_eq!(verified.learning.nodes["verify"].consumed_by, produced);
        assert_eq!(
            verified.learning.nodes["verify"]
                .verification
                .as_ref()
                .and_then(|v| v.passed),
            Some(true)
        );
        assert!(!verified.learning.nodes["verify"]
            .verification
            .as_ref()
            .unwrap()
            .evidence_refs
            .is_empty());
        for evidence in &verified.learning.nodes["verify"]
            .verification
            .as_ref()
            .unwrap()
            .evidence_refs
        {
            assert!(evidence.chars().all(|c| !c.is_whitespace()));
            assert!(evidence.len() <= 240);
        }

        // Human intervention on a fresh node.
        crate::project_file::checkout_start_node(&workspace, "build", "human", "human").ok();
        // build already finished; exercise human completion API on a synthetic reopen.
        reopen_for_retry(&workspace, "build").unwrap();
        crate::project_file::checkout_start_node(&workspace, "build", "human", "human").unwrap();
        complete_as_human(&workspace, "build", "human").unwrap();
        let human = crate::project_file::load(&workspace).unwrap();
        assert!(human.learning.nodes["build"].human_intervention);
        assert_eq!(
            human.learning.nodes["build"].outcome,
            Some(crate::learning_data::NodeOutcome::HumanCompleted)
        );

        let round_trip: Value =
            serde_json::from_slice(&fs::read(crate::project_file::path(&workspace)).unwrap())
                .unwrap();
        assert_eq!(round_trip["graph"], graph);
        assert_eq!(
            round_trip["learning"]["nodes"]["verify"]["outcome"],
            json!("verified_success")
        );
        let _ = fs::remove_dir_all(workspace);
    }

    const POOL_24: &str = "codex=6,cursor=6,claude=6,hermes=6";
    const POOL_42: &str = "codex=12,cursor=10,claude=10,hermes=10";

    fn all_binaries_available(_binary: &str) -> bool {
        true
    }

    fn slots_from_counts(
        codex: usize,
        cursor: usize,
        claude: usize,
        hermes: usize,
    ) -> Vec<PoolSlot> {
        let mut counts = BTreeMap::new();
        counts.insert("codex".to_owned(), codex);
        counts.insert("cursor".to_owned(), cursor);
        counts.insert("claude".to_owned(), claude);
        counts.insert("hermes".to_owned(), hermes);
        expand_pool_slots(&counts)
    }

    #[derive(Clone, Copy)]
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            self.0
        }

        fn in_range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.next() % (hi - lo + 1)
        }
    }

    #[derive(Clone)]
    struct SimNode {
        id: String,
        preds: Vec<String>,
        duration: u64,
    }

    #[derive(Clone)]
    struct InjectedRunner {
        durations: BTreeMap<String, u64>,
        fail_first: BTreeSet<String>,
        stall_providers: BTreeSet<String>,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum InjectedOutcome {
        Success { duration: u64 },
        Fail { duration: u64 },
        Stall,
    }

    impl InjectedRunner {
        fn outcome(&self, slot: &PoolSlot, node: &str, attempt: u32) -> InjectedOutcome {
            if self.stall_providers.contains(slot.provider) {
                return InjectedOutcome::Stall;
            }
            let duration = *self.durations.get(node).unwrap_or(&1);
            if attempt == 1 && self.fail_first.contains(node) {
                InjectedOutcome::Fail { duration }
            } else {
                InjectedOutcome::Success { duration }
            }
        }
    }

    struct PoolMetrics {
        makespan: u64,
        queue_work_units: u64,
        completed: BTreeSet<String>,
        completions_by_provider: BTreeMap<String, usize>,
        max_leases_by_provider: BTreeMap<String, usize>,
        duplicate_leases: usize,
        duplicate_completions: usize,
        dependency_violations: usize,
        drops: usize,
        starvation: bool,
        completion_log: Vec<(String, String)>,
    }

    fn seeded_48_nodes(seed: u64) -> (Vec<SimNode>, InjectedRunner) {
        let mut rng = Lcg(seed);
        let mut nodes = Vec::with_capacity(48);
        let mut durations = BTreeMap::new();
        for index in 0..48 {
            let id = format!("n{index:02}");
            let duration = rng.in_range(2, 9);
            durations.insert(id.clone(), duration);
            nodes.push(SimNode {
                id,
                preds: Vec::new(),
                duration,
            });
        }
        (
            nodes,
            InjectedRunner {
                durations,
                fail_first: BTreeSet::new(),
                stall_providers: BTreeSet::new(),
            },
        )
    }

    fn shuffle_ids(ids: &mut [String], seed: u64) {
        let mut rng = Lcg(seed);
        for i in (1..ids.len()).rev() {
            let j = (rng.next() as usize) % (i + 1);
            ids.swap(i, j);
        }
    }

    fn simulate_heterogeneous_pool(
        nodes: &[SimNode],
        slots: &[PoolSlot],
        runner: &InjectedRunner,
        completed_seed: &BTreeSet<String>,
        ready_shuffle_seed: Option<u64>,
    ) -> PoolMetrics {
        let preds: BTreeMap<String, Vec<String>> = nodes
            .iter()
            .map(|node| (node.id.clone(), node.preds.clone()))
            .collect();
        let mut completed = completed_seed.clone();
        let mut in_progress: BTreeMap<String, String> = BTreeMap::new();
        let mut slot_leases: BTreeMap<String, String> = BTreeMap::new();
        let mut retry_counts: BTreeMap<String, u32> = BTreeMap::new();
        let mut jobs_done: BTreeMap<String, u64> = BTreeMap::new();
        let mut events: BTreeMap<(u64, String, String), bool> = BTreeMap::new();
        let mut completions_by_provider: BTreeMap<String, usize> = BTreeMap::new();
        let mut max_leases_by_provider: BTreeMap<String, usize> = BTreeMap::new();
        let mut completion_log = Vec::new();
        let mut duplicate_leases = 0;
        let mut duplicate_completions = 0;
        let mut dependency_violations = 0;
        let mut queue_work_units: u64 = 0;
        let mut time = 0_u64;
        let caps = {
            let identities: Vec<String> = slots.iter().map(|slot| slot.id.clone()).collect();
            provider_caps_from_agents(&identities)
        };

        let ready_now = |completed: &BTreeSet<String>,
                         in_progress: &BTreeMap<String, String>,
                         now: u64|
         -> Vec<String> {
            let mut ready: Vec<String> = nodes
                .iter()
                .filter(|node| {
                    !completed.contains(&node.id)
                        && !in_progress.contains_key(&node.id)
                        && node.preds.iter().all(|pred| completed.contains(pred))
                })
                .map(|node| node.id.clone())
                .collect();
            if let Some(seed) = ready_shuffle_seed {
                shuffle_ids(&mut ready, seed.wrapping_add(now));
            } else {
                ready.sort();
            }
            ready
        };

        loop {
            let mut ready = ready_now(&completed, &in_progress, time);
            let mut idle: Vec<&PoolSlot> = slots
                .iter()
                .filter(|slot| !slot_leases.contains_key(&slot.id))
                .collect();
            idle.sort_by_key(|slot| {
                (
                    jobs_done.get(&slot.id).copied().unwrap_or(0),
                    slot.provider,
                    slot.index,
                )
            });
            for slot in idle {
                if ready.is_empty() {
                    break;
                }
                if slot_leases.contains_key(&slot.id) {
                    duplicate_leases += 1;
                    continue;
                }
                let used = slot_leases
                    .keys()
                    .filter(|id| slot_provider(id) == Some(slot.provider))
                    .count();
                if used >= caps.get(slot.provider).copied().unwrap_or(0) {
                    continue;
                }
                let node_id = ready.remove(0);
                if in_progress.contains_key(&node_id) {
                    duplicate_leases += 1;
                    continue;
                }
                if let Some(node) = nodes.iter().find(|node| node.id == node_id) {
                    if !node.preds.iter().all(|pred| completed.contains(pred)) {
                        dependency_violations += 1;
                        continue;
                    }
                }
                in_progress.insert(node_id.clone(), slot.id.clone());
                slot_leases.insert(slot.id.clone(), node_id.clone());
                let attempt = retry_counts.get(&node_id).copied().unwrap_or(0) + 1;
                match runner.outcome(slot, &node_id, attempt) {
                    InjectedOutcome::Stall => {}
                    InjectedOutcome::Success { duration } => {
                        events.insert((time + duration, slot.id.clone(), node_id), true);
                    }
                    InjectedOutcome::Fail { duration } => {
                        events.insert((time + duration, slot.id.clone(), node_id), false);
                    }
                }
            }
            for (provider, count) in current_leases_by_provider(&slot_leases) {
                let entry = max_leases_by_provider.entry(provider).or_insert(0);
                *entry = (*entry).max(count);
            }
            let Some(((next_time, slot_id, node_id), success)) = events
                .keys()
                .next()
                .cloned()
                .and_then(|key| events.remove(&key).map(|success| (key, success)))
            else {
                break;
            };
            if next_time < time {
                break;
            }
            let queued = ready_now(&completed, &in_progress, time).len() as u64;
            queue_work_units =
                queue_work_units.saturating_add(queued.saturating_mul(next_time - time));
            time = next_time;
            slot_leases.remove(&slot_id);
            in_progress.remove(&node_id);
            if success {
                if !completed.insert(node_id.clone()) {
                    duplicate_completions += 1;
                } else {
                    if let Some(pred) = preds.get(&node_id) {
                        if !pred
                            .iter()
                            .all(|item| completed.contains(item) || item == &node_id)
                        {
                            dependency_violations += 1;
                        }
                    }
                    *jobs_done.entry(slot_id.clone()).or_insert(0) += 1;
                    if let Some(provider) = slot_provider(&slot_id) {
                        *completions_by_provider
                            .entry(provider.to_owned())
                            .or_insert(0) += 1;
                    }
                    completion_log.push((node_id, slot_id));
                }
            } else {
                let retries = retry_counts.entry(node_id.clone()).or_insert(0);
                *retries = retries.saturating_add(1);
                if *retries > POOL_NODE_RETRY_LIMIT {
                    completed.insert(format!("failed:{node_id}"));
                }
            }
        }

        let remaining_ready = nodes
            .iter()
            .filter(|node| {
                !completed.contains(&node.id)
                    && !in_progress.contains_key(&node.id)
                    && node.preds.iter().all(|pred| completed.contains(pred))
            })
            .count();
        let idle_slots = slots
            .iter()
            .filter(|slot| !slot_leases.contains_key(&slot.id))
            .count();
        let drops = if idle_slots > 0 { remaining_ready } else { 0 };
        let starvation = slots.iter().any(|slot| {
            !runner.stall_providers.contains(slot.provider)
                && completions_by_provider
                    .get(slot.provider)
                    .copied()
                    .unwrap_or(0)
                    == 0
                && nodes.iter().any(|node| !completed_seed.contains(&node.id))
        });

        PoolMetrics {
            makespan: time,
            queue_work_units,
            completed: completed
                .into_iter()
                .filter(|id| !id.starts_with("failed:"))
                .collect(),
            completions_by_provider,
            max_leases_by_provider,
            duplicate_leases,
            duplicate_completions,
            dependency_violations,
            drops,
            starvation,
            completion_log,
        }
    }

    fn current_leases_by_provider(leases: &BTreeMap<String, String>) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for slot in leases.keys() {
            if let Some(provider) = slot_provider(slot) {
                *counts.entry(provider.to_owned()).or_insert(0) += 1;
            }
        }
        counts
    }

    fn assert_safety(metrics: &PoolMetrics, expected_completed: usize) {
        assert_eq!(metrics.duplicate_leases, 0, "duplicate leases");
        assert_eq!(metrics.duplicate_completions, 0, "duplicate completions");
        assert_eq!(metrics.dependency_violations, 0, "dependency violations");
        assert_eq!(metrics.drops, 0, "dropped ready nodes: {}", metrics.drops);
        assert!(!metrics.starvation, "provider starvation");
        assert_eq!(metrics.completed.len(), expected_completed);
    }

    #[test]
    fn agent_pool_parses_exact_24_and_42_slot_rosters() {
        let slots_24 = resolve_agent_pool(POOL_24, all_binaries_available).unwrap();
        assert_eq!(slots_24.len(), 24);
        let ids_24: Vec<_> = slots_24.iter().map(|slot| slot.id.as_str()).collect();
        assert_eq!(ids_24[0], "codex-luna:1");
        assert_eq!(ids_24[5], "codex-luna:6");
        assert_eq!(ids_24[6], "cursor:1");
        assert_eq!(ids_24[12], "claude:1");
        assert_eq!(ids_24[18], "hermes:1");
        assert_eq!(ids_24[23], "hermes:6");
        assert!(slots_24.iter().all(|slot| slot.id != "codex"));
        assert_eq!(
            slots_24
                .iter()
                .filter(|slot| slot.kind == "codex-luna")
                .count(),
            6
        );

        let roster =
            detect_pool_roster_with_lead(POOL_24, all_binaries_available, Some("codex")).unwrap();
        assert_eq!(roster[0], "codex");
        assert_eq!(roster.len(), 25);
        assert!(!roster[1..].contains(&"codex".to_owned()));

        let slots_42 = resolve_agent_pool(POOL_42, all_binaries_available).unwrap();
        assert_eq!(slots_42.len(), 42);
        assert_eq!(
            slots_42
                .iter()
                .filter(|slot| slot.provider == "codex")
                .count(),
            12
        );
        assert_eq!(
            slots_42
                .iter()
                .filter(|slot| slot.provider == "cursor")
                .count(),
            10
        );
        let roster_42 =
            detect_pool_roster_with_lead(POOL_42, all_binaries_available, Some("codex")).unwrap();
        assert_eq!(roster_42[0], "codex");
        assert_eq!(roster_42.len(), 43);
    }

    #[test]
    fn agent_pool_rejects_malformed_configs_and_mixed_availability() {
        let cases = [
            ("", "empty"),
            ("codex=6,cursor=6,claude=6,hermes=6,codex=6", "duplicate"),
            ("codex=6,cursor=6,claude=6,gpt=6", "unknown"),
            ("codex=0,cursor=8,claude=8,hermes=8", "zero"),
            ("codex=100,cursor=1,claude=1,hermes=1", "overflow"),
            (
                "codex=18446744073709551616,cursor=1,claude=1,hermes=1",
                "overflow-parse",
            ),
            ("codex=6,cursor=6,claude=6", "missing"),
            ("codex=4,cursor=4,claude=4,hermes=4", "below-min"),
            ("codex=12,cursor=12,claude=12,hermes=12", "above-max"),
            ("codex=6,cursor=6,claude=6,hermes=", "invalid"),
        ];
        for (raw, label) in cases {
            assert!(
                parse_agent_pool(raw).is_err(),
                "expected {label} to be rejected: {raw}"
            );
        }
        let mixed = resolve_agent_pool(POOL_24, |binary| binary != "hermes");
        assert!(mixed.is_err(), "mixed availability must not fall back");
        let none = resolve_agent_pool(POOL_24, |_| false);
        assert!(none.is_err());
    }

    #[test]
    fn agent_pool_absent_keeps_one_slot_logical_routes() {
        assert_eq!(
            logical_agent_routes(vec![
                "codex".to_owned(),
                "cursor".to_owned(),
                "claude".to_owned(),
                "hermes".to_owned()
            ]),
            vec![
                "codex".to_owned(),
                "codex-luna".to_owned(),
                "cursor".to_owned(),
                "claude".to_owned(),
                "hermes".to_owned()
            ]
        );
    }

    #[test]
    fn agent_pool_injected_24_vs_baseline_meets_makespan_and_safety() {
        let (nodes, runner) = seeded_48_nodes(0xC0FFEE48);
        let baseline_slots = slots_from_counts(1, 1, 1, 1);
        let pool_slots = resolve_agent_pool(POOL_24, all_binaries_available).unwrap();
        let baseline =
            simulate_heterogeneous_pool(&nodes, &baseline_slots, &runner, &BTreeSet::new(), None);
        let pooled =
            simulate_heterogeneous_pool(&nodes, &pool_slots, &runner, &BTreeSet::new(), None);
        assert_safety(&baseline, 48);
        assert_safety(&pooled, 48);
        for provider in POOL_PROVIDERS {
            assert!(
                pooled
                    .completions_by_provider
                    .get(provider)
                    .copied()
                    .unwrap_or(0)
                    >= 1,
                "{provider} completed no work"
            );
            assert!(
                baseline
                    .completions_by_provider
                    .get(provider)
                    .copied()
                    .unwrap_or(0)
                    >= 1,
                "baseline {provider} completed no work"
            );
        }
        let reduction = (baseline.makespan - pooled.makespan) as f64 / baseline.makespan as f64;
        assert!(
            reduction >= 0.40,
            "makespan reduction {reduction:.3} < 0.40 (baseline={}, pool={})",
            baseline.makespan,
            pooled.makespan
        );
        let baseline_tp = 48.0 / baseline.makespan as f64;
        let pool_tp = 48.0 / pooled.makespan as f64;
        assert!(
            pool_tp + f64::EPSILON >= baseline_tp,
            "throughput regression: pool {pool_tp} < baseline {baseline_tp}"
        );
        assert!(
            pooled.queue_work_units <= baseline.queue_work_units,
            "queue work units rose: pool {} baseline {}",
            pooled.queue_work_units,
            baseline.queue_work_units
        );
        for provider in POOL_PROVIDERS {
            let cap = pool_slots
                .iter()
                .filter(|slot| slot.provider == provider)
                .count();
            let max = pooled
                .max_leases_by_provider
                .get(provider)
                .copied()
                .unwrap_or(0);
            assert!(max <= cap, "{provider} cap {cap} exceeded by {max}");
        }
    }

    #[test]
    fn agent_pool_provider_saturation_respects_caps() {
        let (nodes, runner) = seeded_48_nodes(11);
        let slots = resolve_agent_pool(POOL_24, all_binaries_available).unwrap();
        let metrics = simulate_heterogeneous_pool(&nodes, &slots, &runner, &BTreeSet::new(), None);
        assert_safety(&metrics, 48);
        assert_eq!(
            metrics
                .max_leases_by_provider
                .get("cursor")
                .copied()
                .unwrap_or(0),
            6
        );
        assert_eq!(
            metrics
                .max_leases_by_provider
                .get("codex")
                .copied()
                .unwrap_or(0),
            6
        );
    }

    #[test]
    fn agent_pool_stalled_provider_keeps_others_productive() {
        let (nodes, mut runner) = seeded_48_nodes(22);
        runner.stall_providers.insert("hermes".to_owned());
        let slots = resolve_agent_pool(POOL_24, all_binaries_available).unwrap();
        let metrics = simulate_heterogeneous_pool(&nodes, &slots, &runner, &BTreeSet::new(), None);
        assert_eq!(metrics.duplicate_leases, 0);
        assert_eq!(metrics.duplicate_completions, 0);
        assert_eq!(metrics.dependency_violations, 0);
        assert_eq!(metrics.completed.len(), 42);
        assert_eq!(metrics.drops, 0);
        assert!(
            metrics
                .completions_by_provider
                .get("codex")
                .copied()
                .unwrap_or(0)
                >= 1
        );
        assert!(
            metrics
                .completions_by_provider
                .get("cursor")
                .copied()
                .unwrap_or(0)
                >= 1
        );
        assert!(
            metrics
                .completions_by_provider
                .get("claude")
                .copied()
                .unwrap_or(0)
                >= 1
        );
        assert_eq!(
            metrics
                .completions_by_provider
                .get("hermes")
                .copied()
                .unwrap_or(0),
            0
        );
        assert!(metrics.makespan > 0);
    }

    #[test]
    fn agent_pool_worker_failure_requeues_without_duplicate_completion() {
        let (nodes, mut runner) = seeded_48_nodes(33);
        runner.fail_first.insert("n00".to_owned());
        runner.fail_first.insert("n07".to_owned());
        let slots = resolve_agent_pool(POOL_24, all_binaries_available).unwrap();
        let metrics = simulate_heterogeneous_pool(&nodes, &slots, &runner, &BTreeSet::new(), None);
        assert_safety(&metrics, 48);
        let n00 = metrics
            .completion_log
            .iter()
            .filter(|(node, _)| node == "n00")
            .count();
        assert_eq!(n00, 1);
    }

    #[test]
    fn agent_pool_restart_replay_skips_completed_seed() {
        let (nodes, runner) = seeded_48_nodes(44);
        let slots = resolve_agent_pool(POOL_24, all_binaries_available).unwrap();
        let first = simulate_heterogeneous_pool(&nodes, &slots, &runner, &BTreeSet::new(), None);
        let seed: BTreeSet<String> = first
            .completion_log
            .iter()
            .take(16)
            .map(|(node, _)| node.clone())
            .collect();
        assert_eq!(seed.len(), 16);
        let replay = simulate_heterogeneous_pool(&nodes, &slots, &runner, &seed, None);
        assert_safety(&replay, 48);
        for node in &seed {
            assert!(replay.completed.contains(node));
            assert_eq!(
                replay
                    .completion_log
                    .iter()
                    .filter(|(id, _)| id == node)
                    .count(),
                0,
                "replay must not re-complete {node}"
            );
        }
        assert_eq!(replay.completion_log.len(), 32);
    }

    #[test]
    fn agent_pool_shuffled_ready_input_preserves_safety() {
        let (nodes, runner) = seeded_48_nodes(55);
        let slots = resolve_agent_pool(POOL_24, all_binaries_available).unwrap();
        let a = simulate_heterogeneous_pool(&nodes, &slots, &runner, &BTreeSet::new(), Some(1));
        let b = simulate_heterogeneous_pool(&nodes, &slots, &runner, &BTreeSet::new(), Some(99));
        assert_safety(&a, 48);
        assert_safety(&b, 48);
        assert_eq!(a.completed, b.completed);
        for provider in POOL_PROVIDERS {
            assert!(
                a.completions_by_provider
                    .get(provider)
                    .copied()
                    .unwrap_or(0)
                    >= 1
            );
            assert!(
                b.completions_by_provider
                    .get(provider)
                    .copied()
                    .unwrap_or(0)
                    >= 1
            );
        }
    }

    #[test]
    fn agent_pool_42_slot_roster_schedules_seeded_workload() {
        let (nodes, runner) = seeded_48_nodes(0xC0FFEE48);
        let slots = resolve_agent_pool(POOL_42, all_binaries_available).unwrap();
        assert_eq!(slots.len(), 42);
        let metrics = simulate_heterogeneous_pool(&nodes, &slots, &runner, &BTreeSet::new(), None);
        assert_safety(&metrics, 48);
        for provider in POOL_PROVIDERS {
            assert!(
                metrics
                    .completions_by_provider
                    .get(provider)
                    .copied()
                    .unwrap_or(0)
                    >= 1
            );
        }
    }

    #[test]
    fn agent_pool_dependency_ready_only() {
        let mut nodes = Vec::new();
        for chain in 0..12 {
            for layer in 0..4 {
                let id = format!("c{chain:02}l{layer}");
                let preds = if layer == 0 {
                    Vec::new()
                } else {
                    vec![format!("c{chain:02}l{}", layer - 1)]
                };
                nodes.push(SimNode {
                    id,
                    preds,
                    duration: 3,
                });
            }
        }
        let durations: BTreeMap<_, _> = nodes
            .iter()
            .map(|node| (node.id.clone(), node.duration))
            .collect();
        let runner = InjectedRunner {
            durations,
            fail_first: BTreeSet::new(),
            stall_providers: BTreeSet::new(),
        };
        let slots = resolve_agent_pool(POOL_24, all_binaries_available).unwrap();
        let metrics =
            simulate_heterogeneous_pool(&nodes, &slots, &runner, &BTreeSet::new(), Some(7));
        assert_safety(&metrics, 48);
        assert_eq!(metrics.makespan, 12);
    }
}
