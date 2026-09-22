//! Project-scoped, opt-in bridge from the admitted Rust node attempt to the
//! local typed Python node runtime. This module never schedules work: Rust
//! supplies the current checkout, graph, route, and approval facts, then
//! rechecks the hard gate immediately before the worker command is spawned.

use std::collections::BTreeSet;
use std::error::Error as StdError;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

const CONFIG_PATH: &str = ".fractal/node-intelligence.json";
const CONFIG_SCHEMA: &str = "fractal.node_intelligence.config.v1";
const REQUEST_SCHEMA: &str = "fractal.node_intelligence.request.v1";
const RESPONSE_SCHEMA: &str = "fractal.node_intelligence.response.v1";
const CONTEXT_SCHEMA: &str = "fractal.node_context.v1";
const RECEIPT_SCHEMA: &str = "fractal.node_runtime_receipt.v1";
const APPROVAL_MANIFEST_SCHEMA: &str = "fractal.node_intelligence.approval_manifest.v1";
const HOST_POLICY_SCHEMA: &str = "fractal.node_intelligence.host_policy.v1";
const HOST_POLICY_ENV: &str = "FRACTAL_NODE_INTELLIGENCE_POLICY";
pub(crate) const ANALYSIS_CAPABILITY: &str = "intelligence.measurement.analyze";
const MAX_CONFIG_BYTES: u64 = 1_048_576;
const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_PRIVATE_RECEIPT_BYTES: usize = 256 * 1024;
const MAX_APPROVAL_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_HOST_POLICY_BYTES: u64 = 1_048_576;
const ECHO_FIELDS: [&str; 16] = [
    "request_id",
    "request_hash",
    "project_id",
    "graph_id",
    "graph_hash",
    "attempt",
    "network_ref",
    "capability_id",
    "node_ref",
    "model_ref",
    "policy_ref",
    "input_refs",
    "memory_refs",
    "evidence_refs",
    "handoff_refs",
    "review_packet_refs",
];
const TASK_FIELDS: [&str; 19] = [
    "operation",
    "timeout_ms",
    "network_ref",
    "capability_id",
    "node_ref",
    "model_ref",
    "policy_ref",
    "input_refs",
    "memory_refs",
    "evidence_refs",
    "handoff_refs",
    "review_packet_refs",
    "payload_classification",
    "public_features",
    "explicit_pin",
    "plan_ref",
    "authorized_approvers",
    "qualified_reviewers",
    "runtime",
];

/// A Python workflow review denial returned before Rust launches any worker
/// or analysis effect. The scheduler may block just this pinned attempt while
/// allowing unrelated ready nodes to continue.
#[derive(Debug)]
pub(crate) struct ReviewBlockedBeforeEffect {
    code: String,
}

impl fmt::Display for ReviewBlockedBeforeEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "workflow review denied the pinned attempt ({})",
            self.code
        )
    }
}

impl StdError for ReviewBlockedBeforeEffect {}

pub(crate) fn is_review_blocked_before_effect(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<ReviewBlockedBeforeEffect>())
}

#[derive(Clone, Debug)]
pub(crate) struct BridgeEvidence {
    pub(crate) request_hash: String,
    pub(crate) response_hash: String,
    pub(crate) attempt_ref: String,
    pub(crate) receipt_ref: String,
    pub(crate) context_manifest_ref: String,
    pub(crate) context_manifest_path: String,
    pub(crate) policy_ref: String,
    pub(crate) model_ref: String,
    pub(crate) network_ref: String,
    pub(crate) host_policy_digest: String,
    pub(crate) input_refs: Vec<String>,
    pub(crate) handoff_refs: Vec<String>,
    pub(crate) review_packet_refs: Vec<String>,
    pub(crate) decision: Value,
    pub(crate) analysis: Option<Value>,
    pub(crate) remaining_elapsed_ms: u64,
    pub(crate) remaining_calls: u64,
    pub(crate) approved_gate_refs: Vec<String>,
    pub(crate) pending_review: bool,
    config_digest: String,
    host_policy_path: PathBuf,
    gate_context: GateContext,
    node_id: String,
    worker_id: String,
    attempt_number: u32,
    graph_hash: String,
    pre_admission: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReviewAdmission {
    Ready(ReviewBinding),
    Pending(ReviewBinding),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewBinding {
    pub(crate) attempt_number: u32,
    pub(crate) attempt_ref: String,
    graph_hash: String,
    config_digest: String,
    host_policy_path: PathBuf,
    host_policy_digest: String,
    input_refs: Vec<String>,
    handoff_refs: Vec<String>,
    review_packet_refs: Vec<String>,
}

#[derive(Clone, Debug)]
struct GateContext {
    project_id: String,
    graph_hash: String,
    node_id: String,
    attempt_ref: String,
    network_ref: String,
    plan_ref: String,
    input_refs: Vec<String>,
    handoff_refs: Vec<String>,
    review_packet_refs: Vec<String>,
}

struct TaskConfiguration {
    values: Map<String, Value>,
    timeout: Duration,
}

#[derive(Clone, Debug)]
struct HostPolicy {
    path: PathBuf,
    digest: String,
    task: Value,
}

#[derive(Clone, Debug)]
struct BridgeRequest {
    value: Value,
    gate_context: GateContext,
    attempt_number: u32,
    graph_hash: String,
    node_id: String,
    worker_id: String,
    receipt_path: String,
    timeout: Duration,
    config_digest: String,
    host_policy_path: PathBuf,
    host_policy_digest: String,
    pre_admission: bool,
    review_operation: bool,
    previous_attempt_count: u32,
}

#[derive(Clone, Debug)]
struct Launcher {
    program: PathBuf,
    args: Vec<String>,
}

/// No config or no matching task means the prior worker command path is left
/// untouched. Once a task is explicitly enabled, every failure is fail-closed.
pub(crate) fn prepare(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    invocation: &crate::chain::jev_receipt::RouteInvocation,
) -> Result<Option<BridgeEvidence>> {
    prepare_for_operation(workspace, node_id, worker_id, invocation, "prepare")
}

pub(crate) fn prepare_with_timeout(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    invocation: &crate::chain::jev_receipt::RouteInvocation,
    timeout_cap: Duration,
) -> Result<Option<BridgeEvidence>> {
    prepare_for_operation_with_timeout(
        workspace,
        node_id,
        worker_id,
        invocation,
        "prepare",
        Some(timeout_cap),
    )
}

/// Anchor the managed runtime's absolute wall-clock budget at its first Rust
/// stage. Unconfigured tasks return None and retain their ordinary timeouts.
pub(crate) fn attempt_deadline(workspace: &Path, node_id: &str) -> Result<Option<Instant>> {
    let started = Instant::now();
    if task_configuration(workspace, node_id)?.is_none() {
        return Ok(None);
    }
    let document = crate::project_file::load(workspace)?;
    let node = graph_node(&document.graph, node_id)?;
    let max_elapsed_ms = node
        .pointer("/hard_limits/max_elapsed_ms")
        .and_then(Value::as_u64)
        .context("configured node-intelligence task lacks hard elapsed-time limit")?;
    Ok(Some(started + Duration::from_millis(max_elapsed_ms)))
}

fn remaining_deadline(deadline: Option<Instant>) -> Result<Option<Duration>> {
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("node-intelligence hard elapsed-time budget is exhausted");
    }
    Ok(Some(remaining))
}

pub(crate) fn prepare_analysis(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    invocation: &crate::chain::jev_receipt::RouteInvocation,
    expected_review: Option<&ReviewBinding>,
    deadline: Option<Instant>,
) -> Result<BridgeEvidence> {
    let deadline = match deadline {
        Some(deadline) => Some(deadline),
        None => attempt_deadline(workspace, node_id)?,
    };
    let document = crate::project_file::load(workspace)?;
    let capability = document
        .graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(node_id))
        .and_then(|node| node.get("capability"))
        .and_then(Value::as_str);
    if capability != Some(ANALYSIS_CAPABILITY) {
        bail!("analysis adapter requires canonical intelligence.measurement.analyze capability");
    }
    let preflight_timeout = remaining_deadline(deadline)?;
    let preflight = prepare_for_operation_with_timeout(
        workspace,
        node_id,
        worker_id,
        invocation,
        "prepare",
        preflight_timeout,
    )?
    .context("analysis adapter requires an explicitly enabled node-intelligence task")?;
    if let Some(expected) = expected_review {
        require_review_binding(&preflight, expected)?;
    }
    let preflight_budget_started = Instant::now();
    let remaining_before_recheck = preflight
        .remaining_elapsed_ms
        .saturating_sub(preflight_budget_started.elapsed().as_millis() as u64)
        .min(
            remaining_deadline(deadline)?.map_or(u64::MAX, |duration| duration.as_millis() as u64),
        );
    recheck_before_effect_with_budget(workspace, &preflight, worker_id, remaining_before_recheck)
        .context("host gate or workflow approval changed after analysis preflight")?;
    let analysis_remaining = preflight
        .remaining_elapsed_ms
        .saturating_sub(preflight_budget_started.elapsed().as_millis() as u64)
        .min(
            remaining_deadline(deadline)?.map_or(u64::MAX, |duration| duration.as_millis() as u64),
        );
    if analysis_remaining == 0 {
        bail!("node-intelligence elapsed budget is exhausted before analysis effect");
    }
    prepare_for_operation_with_timeout(
        workspace,
        node_id,
        worker_id,
        invocation,
        "analysis",
        Some(Duration::from_millis(analysis_remaining)),
    )?
    .context("analysis adapter requires an explicitly enabled node-intelligence task")
}

/// True when an enabled task pins at least one packet whose human review must
/// be admitted before Rust checks out the node. Malformed config is an error,
/// so it cannot silently bypass this pre-admission boundary.
pub(crate) fn requires_review_admission(workspace: &Path, node_id: &str) -> Result<bool> {
    let Some(configuration) = task_configuration(workspace, node_id)? else {
        return Ok(false);
    };
    match configuration.values.get("review_packet_refs") {
        None => Ok(false),
        Some(Value::Array(refs)) => {
            if refs
                .iter()
                .any(|value| value.as_str().is_none_or(|reference| !is_digest(reference)))
            {
                bail!("node-intelligence review_packet_refs contains an invalid digest");
            }
            Ok(!refs.is_empty())
        }
        Some(_) => bail!("node-intelligence review_packet_refs must be an array"),
    }
}

/// Whether a node has an explicit enabled bridge task and therefore must stay
/// on the managed pull-queue path. The supervised wave runner cannot preserve
/// admission, per-attempt deadlines, and host policy bindings, so it must not
/// check out any such task as an ordinary worker node.
pub(crate) fn has_enabled_task(workspace: &Path, node_id: &str) -> Result<bool> {
    Ok(task_configuration(workspace, node_id)?.is_some())
}

/// Ask the typed workflow store whether the predicted next attempt is eligible
/// before any checkout consumes an attempt or worker slot. This operation is
/// read-only with respect to graph/attempt state and never performs inference.
pub(crate) fn review_admission(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
) -> Result<ReviewAdmission> {
    let invocation = crate::chain::jev_receipt::RouteInvocation::unknown(worker_id);
    let evidence = prepare_for_operation(workspace, node_id, worker_id, &invocation, "review")?
        .context("review admission requires an explicitly enabled node-intelligence task")?;
    if !evidence.pre_admission {
        bail!("node-intelligence review did not use the pre-admission request path");
    }
    let binding = review_binding(&evidence);
    if evidence.pending_review {
        Ok(ReviewAdmission::Pending(binding))
    } else {
        Ok(ReviewAdmission::Ready(binding))
    }
}

pub(crate) fn require_review_binding(
    evidence: &BridgeEvidence,
    expected: &ReviewBinding,
) -> Result<()> {
    let actual = review_binding(evidence);
    if &actual != expected || evidence.pending_review {
        bail!("managed worker request does not match its pre-admitted review binding");
    }
    Ok(())
}

pub(crate) fn verify_reviewed_checkout(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    expected: &ReviewBinding,
    deadline: Instant,
) -> Result<()> {
    let configuration = task_configuration(workspace, node_id)?
        .context("review-pinned task was disabled before checkout")?;
    let invocation = crate::chain::jev_receipt::RouteInvocation::unknown(worker_id);
    let request = build_request(
        workspace,
        node_id,
        worker_id,
        &invocation,
        configuration,
        "prepare",
    )?;
    if request_binding(&request) != *expected {
        bail!("review-pinned graph, attempt, task config, or host policy changed before checkout");
    }
    recheck_request_binding(workspace, &request)?;
    let review = prepare_for_operation_with_timeout(
        workspace,
        node_id,
        worker_id,
        &invocation,
        "review",
        remaining_deadline(Some(deadline))?,
    )?
    .context("review-pinned task was disabled after checkout")?;
    if review.pending_review || review_binding(&review) != *expected {
        bail!("workflow review changed between pre-admission and managed checkout");
    }
    recheck_request_binding(
        workspace,
        &build_request(
            workspace,
            node_id,
            worker_id,
            &invocation,
            task_configuration(workspace, node_id)?
                .context("review-pinned task was disabled during admission")?,
            "prepare",
        )?,
    )
}

fn review_binding(evidence: &BridgeEvidence) -> ReviewBinding {
    ReviewBinding {
        attempt_number: evidence.attempt_number,
        attempt_ref: evidence.attempt_ref.clone(),
        graph_hash: evidence.graph_hash.clone(),
        config_digest: evidence.config_digest.clone(),
        host_policy_path: evidence.host_policy_path.clone(),
        host_policy_digest: evidence.host_policy_digest.clone(),
        input_refs: evidence.input_refs.clone(),
        handoff_refs: evidence.handoff_refs.clone(),
        review_packet_refs: evidence.review_packet_refs.clone(),
    }
}

fn request_binding(request: &BridgeRequest) -> ReviewBinding {
    ReviewBinding {
        attempt_number: request.attempt_number,
        attempt_ref: request.gate_context.attempt_ref.clone(),
        graph_hash: request.graph_hash.clone(),
        config_digest: request.config_digest.clone(),
        host_policy_path: request.host_policy_path.clone(),
        host_policy_digest: request.host_policy_digest.clone(),
        input_refs: request.gate_context.input_refs.clone(),
        handoff_refs: request.gate_context.handoff_refs.clone(),
        review_packet_refs: request.gate_context.review_packet_refs.clone(),
    }
}

fn recheck_current_workflow_review(
    workspace: &Path,
    evidence: &BridgeEvidence,
    worker_id: &str,
    timeout_cap: Duration,
) -> Result<()> {
    if evidence.review_packet_refs.is_empty() {
        return Ok(());
    }
    let invocation = crate::chain::jev_receipt::RouteInvocation::unknown(worker_id);
    let review = prepare_for_operation_with_timeout(
        workspace,
        &evidence.node_id,
        worker_id,
        &invocation,
        "review",
        Some(timeout_cap),
    )?
    .context("final workflow review check requires an enabled node-intelligence task")?;
    if review.pending_review {
        return Err(ReviewBlockedBeforeEffect {
            code: "review_pending_or_invalid".to_owned(),
        }
        .into());
    }
    if review.pre_admission
        || review.attempt_ref != evidence.attempt_ref
        || review.attempt_number != evidence.attempt_number
        || review.graph_hash != evidence.graph_hash
        || review.config_digest != evidence.config_digest
        || review.host_policy_path != evidence.host_policy_path
        || review.host_policy_digest != evidence.host_policy_digest
        || review.input_refs != evidence.input_refs
        || review.handoff_refs != evidence.handoff_refs
        || review.review_packet_refs != evidence.review_packet_refs
    {
        bail!("final workflow review result does not bind to the worker's pinned attempt");
    }
    Ok(())
}

fn prepare_for_operation(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    invocation: &crate::chain::jev_receipt::RouteInvocation,
    operation: &str,
) -> Result<Option<BridgeEvidence>> {
    prepare_for_operation_with_timeout(workspace, node_id, worker_id, invocation, operation, None)
}

fn prepare_for_operation_with_timeout(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    invocation: &crate::chain::jev_receipt::RouteInvocation,
    operation: &str,
    timeout_cap: Option<Duration>,
) -> Result<Option<BridgeEvidence>> {
    let Some(configuration) = task_configuration(workspace, node_id)? else {
        return Ok(None);
    };
    let mut request = build_request(
        workspace,
        node_id,
        worker_id,
        invocation,
        configuration,
        operation,
    )?;
    if let Some(timeout_cap) = timeout_cap {
        request.timeout = request.timeout.min(timeout_cap);
        if request.timeout.is_zero() {
            bail!("node-intelligence elapsed budget is exhausted before bridge launch");
        }
    }
    let raw_request =
        serde_json::to_vec(&request.value).context("encode node-intelligence request")?;
    if raw_request.len() > MAX_REQUEST_BYTES {
        bail!("node-intelligence request exceeds the bounded JSON contract");
    }
    recheck_request_binding(workspace, &request)?;
    let launcher = Launcher {
        program: PathBuf::from("python3"),
        args: vec![
            "-m".to_owned(),
            "intelligence_graph.node_runtime".to_owned(),
        ],
    };
    let response = run_bridge_child(workspace, &raw_request, request.timeout, &launcher)?;
    let evidence = validate_response(workspace, &request, &response)?;
    if request.pre_admission || request.review_operation {
        // A second check closes the interval in which another coordinator could
        // claim or alter the predicted attempt while the review process ran.
        recheck_request_binding(workspace, &request)?;
    }
    Ok(Some(evidence))
}

/// Revalidate exact packet/input approval bindings and hard external gates
/// after the Python child returns and immediately before effectful launch.
pub(crate) fn recheck_before_effect(
    workspace: &Path,
    evidence: &BridgeEvidence,
    worker_id: &str,
) -> Result<()> {
    recheck_before_effect_with_budget(
        workspace,
        evidence,
        worker_id,
        evidence.remaining_elapsed_ms,
    )
}

pub(crate) fn recheck_before_effect_with_budget(
    workspace: &Path,
    evidence: &BridgeEvidence,
    worker_id: &str,
    remaining_elapsed_ms: u64,
) -> Result<()> {
    if evidence.pre_admission {
        bail!("review-only admission evidence cannot authorize a worker effect");
    }
    if worker_id != evidence.worker_id {
        bail!("managed worker identity changed after node-intelligence decision");
    }
    // Re-read packet status at the last managed boundary. This helper never
    // makes another recommendation or effect; the host gate/policy check below
    // runs afterward so both authorities are current immediately before spawn.
    if remaining_elapsed_ms == 0 {
        bail!("node-intelligence elapsed budget is exhausted before effect");
    }
    recheck_current_workflow_review(
        workspace,
        evidence,
        worker_id,
        Duration::from_millis(remaining_elapsed_ms.min(evidence.remaining_elapsed_ms)),
    )?;
    recheck_current_binding(
        workspace,
        &evidence.node_id,
        worker_id,
        evidence.attempt_number,
        &evidence.graph_hash,
        &evidence.gate_context,
        &evidence.approved_gate_refs,
        &evidence.config_digest,
        &evidence.host_policy_path,
        &evidence.host_policy_digest,
        false,
        false,
        evidence.attempt_number.saturating_sub(1),
    )
}

fn recheck_request_binding(workspace: &Path, request: &BridgeRequest) -> Result<()> {
    recheck_current_binding(
        workspace,
        &request.node_id,
        &request.worker_id,
        request.attempt_number,
        &request.graph_hash,
        &request.gate_context,
        request
            .value
            .pointer("/runtime/approved_gate_refs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .as_slice(),
        &request.config_digest,
        &request.host_policy_path,
        &request.host_policy_digest,
        request.pre_admission,
        request.review_operation,
        request.previous_attempt_count,
    )
}

#[allow(clippy::too_many_arguments)]
fn recheck_current_binding(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    expected_attempt: u32,
    expected_graph_hash: &str,
    gate_context: &GateContext,
    expected_gate_refs: &[String],
    expected_config_digest: &str,
    expected_host_policy_path: &Path,
    expected_host_policy_digest: &str,
    pre_admission: bool,
    review_operation: bool,
    previous_attempt_count: u32,
) -> Result<()> {
    let document = crate::project_file::load(workspace)
        .context("reload current project before managed worker effect")?;
    if document.graph_hash != expected_graph_hash {
        bail!("graph changed during node-intelligence validation");
    }
    let attempt_number = document
        .learning
        .nodes
        .get(node_id)
        .map(|record| record.attempt_count)
        .unwrap_or_default();
    let assignment = document
        .execution
        .as_ref()
        .and_then(|state| state.assignments.get(node_id));
    if pre_admission {
        if expected_attempt != previous_attempt_count.saturating_add(1)
            || attempt_number != previous_attempt_count
            || assignment.is_some_and(|assignment| assignment.state == "checked_out")
        {
            bail!("predicted node attempt changed during review admission");
        }
    } else {
        if attempt_number != expected_attempt {
            bail!("managed attempt changed during node-intelligence validation");
        }
        let assignment = assignment.context("managed node is no longer checked out")?;
        if assignment.state != "checked_out" || assignment.agent_id != worker_id {
            bail!("managed node checkout changed during node-intelligence validation");
        }
    }
    let current_configuration = task_configuration(workspace, node_id)?
        .context("node-intelligence task was disabled during inference")?;
    if task_config_digest(&current_configuration)? != expected_config_digest {
        bail!("node-intelligence task config changed during inference");
    }
    let current_host_policy = load_host_policy(
        workspace,
        node_id,
        &format!("fractal:project:{}", document.project.slug),
        &document.graph_hash,
    )?;
    if current_host_policy.path != expected_host_policy_path
        || current_host_policy.digest != expected_host_policy_digest
    {
        bail!("node-intelligence host policy changed during inference");
    }
    let node = graph_node(&document.graph, node_id)?;
    let current_refs = if pre_admission || review_operation {
        Vec::new()
    } else {
        validated_gate_bindings(workspace, &document, node, gate_context)?
    };
    if current_refs != expected_gate_refs {
        bail!("active external gate evidence changed during node-intelligence validation");
    }
    Ok(())
}

fn task_configuration(workspace: &Path, node_id: &str) -> Result<Option<TaskConfiguration>> {
    let path = workspace.join(CONFIG_PATH);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES
    {
        bail!("node-intelligence project config must be a bounded regular file");
    }
    let file = open_nofollow(&path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        bail!("node-intelligence project config exceeds size limit");
    }
    let config = parse_unique_json(&bytes).context("decode node-intelligence project config")?;
    let object = config
        .as_object()
        .context("node-intelligence project config must be an object")?;
    if object.get("schema").and_then(Value::as_str) != Some(CONFIG_SCHEMA) {
        bail!("unsupported node-intelligence project config schema");
    }
    let enabled = object
        .get("enabled")
        .and_then(Value::as_bool)
        .context("node-intelligence config enabled must be boolean")?;
    if !enabled {
        return Ok(None);
    }
    let allowed = ["schema", "enabled", "timeout_ms", "tasks"]
        .into_iter()
        .chain(TASK_FIELDS)
        .collect::<BTreeSet<_>>();
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        bail!("node-intelligence config contains an unsupported field");
    }
    let tasks = object
        .get("tasks")
        .and_then(Value::as_object)
        .context("enabled node-intelligence config requires a tasks map")?;
    let Some(node_task) = tasks.get(node_id) else {
        return Ok(None);
    };
    let node_task = node_task
        .as_object()
        .context("node-intelligence task config must be an object")?;
    if node_task
        .keys()
        .any(|key| !TASK_FIELDS.contains(&key.as_str()))
    {
        bail!("node-intelligence task config contains an unsupported field");
    }
    let mut merged = Map::new();
    for field in TASK_FIELDS {
        if let Some(value) = object.get(field) {
            merged.insert(field.to_owned(), value.clone());
        }
    }
    for (field, value) in node_task {
        if field == "runtime" {
            let global = merged
                .get("runtime")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let local = value
                .as_object()
                .context("node-intelligence task runtime must be an object")?;
            let mut combined = global;
            combined.extend(local.clone());
            merged.insert(field.clone(), Value::Object(combined));
        } else {
            merged.insert(field.clone(), value.clone());
        }
    }
    let timeout_ms = node_task
        .get("timeout_ms")
        .or_else(|| object.get("timeout_ms"))
        .and_then(Value::as_u64)
        .unwrap_or(5_000);
    if !(100..=180_000).contains(&timeout_ms) {
        bail!("node-intelligence timeout_ms must be between 100 and 180000");
    }
    Ok(Some(TaskConfiguration {
        values: merged,
        timeout: Duration::from_millis(timeout_ms),
    }))
}

fn build_request(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    invocation: &crate::chain::jev_receipt::RouteInvocation,
    configuration: TaskConfiguration,
    operation: &str,
) -> Result<BridgeRequest> {
    let document = crate::project_file::load(workspace)
        .context("load canonical project for node-intelligence attempt")?;
    let node = graph_node(&document.graph, node_id)?;
    let review_operation = operation == "review";
    let assignment = document
        .execution
        .as_ref()
        .and_then(|state| state.assignments.get(node_id));
    let previous_attempt_count = document
        .learning
        .nodes
        .get(node_id)
        .map(|record| record.attempt_count)
        .unwrap_or_default();
    let checked_out_to_worker = assignment.is_some_and(|assignment| {
        assignment.state == "checked_out" && assignment.agent_id == worker_id
    });
    if review_operation
        && assignment.is_some_and(|assignment| assignment.state == "checked_out")
        && !checked_out_to_worker
    {
        bail!("review operation cannot inspect another worker's checkout");
    }
    let pre_admission = review_operation && !checked_out_to_worker;
    let attempt_number = if pre_admission {
        if assignment.is_some_and(|assignment| assignment.state == "checked_out") {
            bail!("review admission cannot inspect an already checked-out node");
        }
        previous_attempt_count
            .checked_add(1)
            .context("node-intelligence attempt counter is exhausted")?
    } else {
        let assignment =
            assignment.context("node-intelligence requires a checked-out managed node")?;
        if assignment.state != "checked_out" || assignment.agent_id != worker_id {
            bail!("node-intelligence worker does not own the current node checkout");
        }
        if previous_attempt_count == 0 {
            bail!("node-intelligence requires a persisted current attempt");
        }
        previous_attempt_count
    };
    let graph_id = document
        .graph
        .get("graph_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("canonical graph is missing graph_id")?;
    let project_id = format!("fractal:project:{}", document.project.slug);
    let runtime = configuration
        .values
        .get("runtime")
        .and_then(Value::as_object)
        .context("enabled node-intelligence task requires runtime configuration")?;
    let receipt_path = runtime
        .get("receipt_path")
        .and_then(Value::as_str)
        .context("node-intelligence runtime requires receipt_path")?
        .to_owned();
    let fields = &configuration.values;
    for field in [
        "network_ref",
        "capability_id",
        "node_ref",
        "model_ref",
        "policy_ref",
        "input_refs",
        "memory_refs",
        "evidence_refs",
        "handoff_refs",
        "review_packet_refs",
        "payload_classification",
        "public_features",
        "plan_ref",
        "authorized_approvers",
        "qualified_reviewers",
    ] {
        if !fields.contains_key(field) {
            bail!("enabled node-intelligence task is missing {field}");
        }
    }
    let network_ref = required_digest(fields, "network_ref")?;
    let capability_id = required_string(fields, "capability_id")?;
    let node_ref = required_digest(fields, "node_ref")?;
    let model_ref = required_digest(fields, "model_ref")?;
    let policy_ref = required_digest(fields, "policy_ref")?;
    let input_refs = sorted_digest_array(fields, "input_refs")?;
    let memory_refs = sorted_memory_refs(fields)?;
    let evidence_refs = sorted_digest_array(fields, "evidence_refs")?;
    let handoff_refs = sorted_digest_array(fields, "handoff_refs")?;
    let review_packet_refs = sorted_digest_array(fields, "review_packet_refs")?;
    let plan_ref = required_digest(fields, "plan_ref")?;
    for field in ["authorized_approvers", "qualified_reviewers"] {
        required_string_array(fields, field)?;
    }
    let classification = required_string(fields, "payload_classification")?;
    if !matches!(
        classification.as_str(),
        "public" | "synthetic" | "project_private"
    ) {
        bail!("node-intelligence payload_classification is unsupported");
    }
    if !matches!(operation, "prepare" | "analysis" | "review") {
        bail!("unsupported node-intelligence operation");
    }
    if review_operation && review_packet_refs.is_empty() {
        bail!("review admission requires at least one pinned review packet");
    }
    if operation == "analysis" {
        validate_analysis_configuration(node, &configuration.values)?;
    }
    if let Some(configured) = fields.get("operation").and_then(Value::as_str) {
        let analysis_preflight = operation == "prepare"
            && configured == "analysis"
            && node.get("capability").and_then(Value::as_str) == Some(ANALYSIS_CAPABILITY)
            && runtime
                .get("analysis")
                .and_then(|analysis| analysis.get("enabled"))
                .and_then(Value::as_bool)
                == Some(true);
        let review_preflight = operation == "review"
            && !review_packet_refs.is_empty()
            && matches!(configured, "prepare" | "analysis");
        if configured != operation && !analysis_preflight && !review_preflight {
            bail!("node-intelligence task operation does not match its managed node capability");
        }
    }
    if !fields.get("public_features").is_some_and(Value::is_object) {
        bail!("node-intelligence public_features must be an object");
    }
    if let Some(pin) = fields.get("explicit_pin") {
        if !pin.is_boolean() {
            bail!("node-intelligence explicit_pin must be boolean");
        }
    }
    let node_is_pinned = node
        .pointer("/executor/agent")
        .and_then(Value::as_str)
        .is_some_and(|agent| !agent.trim().is_empty());
    let explicit_pin = node_is_pinned
        || fields
            .get("explicit_pin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let role = if node
        .get("capability")
        .and_then(Value::as_str)
        .is_some_and(crate::execute::is_verify)
    {
        "verifier"
    } else if node
        .get("capability")
        .and_then(Value::as_str)
        .is_some_and(|capability| capability.starts_with("control."))
    {
        "control"
    } else {
        "worker"
    };
    let baseline_route_id = baseline_route_id(invocation);
    let attempt_id = attempt_id(&project_id, &document.graph_hash, node_id, attempt_number);
    let attempt_pin = json!({
        "schema": "fractal.node.attempt.v1",
        "id": attempt_id,
        "project_id": project_id,
        "graph_ref": document.graph_hash,
        "task_id": node_id,
        "network_ref": network_ref,
        "capability_id": capability_id,
        "node_ref": node_ref,
        "model_ref": model_ref,
        "policy_ref": policy_ref,
        "input_refs": input_refs,
        "memory_refs": memory_refs,
        "evidence_refs": evidence_refs,
    });
    let attempt_ref = fractal_contracts::canonical_sha256(&attempt_pin)
        .map_err(|error| anyhow::anyhow!("hash expected node-intelligence attempt: {error}"))?;
    let mut gate_context = GateContext {
        project_id: project_id.clone(),
        graph_hash: document.graph_hash.clone(),
        node_id: node_id.to_owned(),
        attempt_ref,
        network_ref: network_ref.clone(),
        plan_ref,
        input_refs: input_refs.clone(),
        handoff_refs: handoff_refs.clone(),
        review_packet_refs: review_packet_refs.clone(),
    };
    let approved_gate_refs = if review_operation {
        Vec::new()
    } else {
        validated_gate_bindings(workspace, &document, node, &gate_context)?
    };
    if let Some(runtime_handoffs) = runtime.get("handoff_refs") {
        if runtime_handoffs != &json!(handoff_refs) {
            bail!("top-level and runtime handoff refs differ");
        }
    } else {
        bail!("node-intelligence runtime is missing handoff_refs");
    }
    if let Some(runtime_reviews) = runtime.get("review_packet_refs") {
        if runtime_reviews != &json!(review_packet_refs) {
            bail!("top-level and runtime review packet refs differ");
        }
    } else {
        bail!("node-intelligence runtime is missing review_packet_refs");
    }
    if runtime.get("plan_ref") != Some(&Value::String(gate_context.plan_ref.clone())) {
        bail!("top-level and runtime plan refs differ");
    }
    if runtime.get("authorized_approvers") != fields.get("authorized_approvers")
        || runtime.get("qualified_reviewers") != fields.get("qualified_reviewers")
    {
        bail!("runtime and task review rosters differ");
    }
    let host_policy = load_host_policy(workspace, node_id, &project_id, &document.graph_hash)?;
    let host_task = host_policy
        .task
        .as_object()
        .context("host policy task entry must be an object")?;
    if host_task.get("authorization") != runtime.get("authorization")
        || host_task.get("authorized_approvers") != fields.get("authorized_approvers")
        || host_task.get("qualified_reviewers") != fields.get("qualified_reviewers")
        || !host_policy_memory_matches(host_task, runtime)
    {
        bail!("project node-intelligence grants do not match the independent host policy");
    }
    let mut runtime_value = runtime.clone();
    runtime_value.insert(
        "approved_gate_refs".to_owned(),
        Value::Array(
            approved_gate_refs
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    let workspace = fs::canonicalize(workspace).context("resolve managed workspace")?;
    let config_digest = task_config_digest(&configuration)?;
    let request_id = format!(
        "node-intelligence-{}",
        digest_hex(
            format!(
                "{project_id}|{}|{node_id}|{attempt_number}",
                document.graph_hash
            )
            .as_bytes()
        )
    );
    let route = json!({
        "baseline_route_id": baseline_route_id,
        "cli_family": invocation.cli_family,
        "selected_model": invocation.selected_model,
        "selected_effort": invocation.selected_effort,
        "configuration_source": invocation.configuration_source,
    });
    let mut request = json!({
        "schema": REQUEST_SCHEMA,
        "operation": operation,
        "workspace": workspace.to_string_lossy(),
        "request_id": request_id,
        "project_id": project_id,
        "graph_id": graph_id,
        "graph_hash": document.graph_hash,
        "graph": document.graph,
        "attempt": {"id": attempt_id, "node_id": node_id, "number": attempt_number},
        "network_ref": network_ref,
        "capability_id": capability_id,
        "node_ref": node_ref,
        "model_ref": model_ref,
        "policy_ref": policy_ref,
        "input_refs": input_refs,
        "memory_refs": memory_refs,
        "evidence_refs": evidence_refs,
        "handoff_refs": handoff_refs,
        "review_packet_refs": review_packet_refs,
        "explicit_pin": explicit_pin,
        "role": role,
        "payload_classification": classification,
        "public_features": fields["public_features"],
        "route": route,
        "runtime": Value::Object(runtime_value),
    });
    let request_hash = fractal_contracts::canonical_sha256(&request)
        .map_err(|error| anyhow::anyhow!("hash node-intelligence request: {error}"))?;
    request
        .as_object_mut()
        .expect("request object")
        .insert("request_hash".to_owned(), Value::String(request_hash));
    // Retain normalized refs and expected attempt identity in the immutable
    // pre-effect binding used to recheck approvals after Python completes.
    gate_context.input_refs = input_refs;
    gate_context.handoff_refs = handoff_refs;
    gate_context.review_packet_refs = review_packet_refs;
    Ok(BridgeRequest {
        value: request,
        gate_context,
        attempt_number,
        graph_hash: document.graph_hash,
        node_id: node_id.to_owned(),
        worker_id: worker_id.to_owned(),
        receipt_path,
        timeout: node
            .pointer("/hard_limits/max_elapsed_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .map(|node_budget| configuration.timeout.min(node_budget))
            .unwrap_or(configuration.timeout),
        config_digest,
        host_policy_path: host_policy.path,
        host_policy_digest: host_policy.digest,
        pre_admission,
        review_operation,
        previous_attempt_count,
    })
}

fn task_config_digest(configuration: &TaskConfiguration) -> Result<String> {
    let value = json!({
        "task": configuration.values,
        "timeout_ms": configuration.timeout.as_millis(),
    });
    fractal_contracts::canonical_sha256(&value)
        .map_err(|error| anyhow::anyhow!("hash merged node-intelligence config: {error}"))
}

fn host_policy_memory_matches(
    host_task: &Map<String, Value>,
    runtime: &Map<String, Value>,
) -> bool {
    let null_memory = Value::Null;
    host_task.get("memory").unwrap_or(&null_memory) == runtime.get("memory").unwrap_or(&null_memory)
}

fn load_host_policy(
    workspace: &Path,
    node_id: &str,
    project_id: &str,
    graph_hash: &str,
) -> Result<HostPolicy> {
    let configured_path = std::env::var_os(HOST_POLICY_ENV)
        .with_context(|| format!("enabled node-intelligence task requires {HOST_POLICY_ENV}"))?;
    let path = PathBuf::from(configured_path);
    if !path.is_absolute() {
        bail!("node-intelligence host policy path must be absolute");
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).with_context(|| {
            format!(
                "inspect node-intelligence host policy path {}",
                current.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            bail!("node-intelligence host policy path contains a symlink");
        }
    }
    let canonical_path = fs::canonicalize(&path)
        .with_context(|| format!("resolve node-intelligence host policy {}", path.display()))?;
    let canonical_workspace = fs::canonicalize(workspace).context("resolve managed workspace")?;
    if canonical_path.starts_with(&canonical_workspace) {
        bail!("node-intelligence host policy must live outside the project workspace");
    }
    let metadata = fs::symlink_metadata(&canonical_path)?;
    if !metadata.is_file() || metadata.len() > MAX_HOST_POLICY_BYTES || !private_metadata(&metadata)
    {
        bail!("node-intelligence host policy must be a bounded owner-private regular file");
    }
    let file = open_nofollow(&canonical_path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_HOST_POLICY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_HOST_POLICY_BYTES {
        bail!("node-intelligence host policy exceeds size limit");
    }
    let policy = parse_unique_json(&bytes).context("decode node-intelligence host policy")?;
    let object = policy
        .as_object()
        .context("node-intelligence host policy must be an object")?;
    let expected_keys = ["schema", "project_id", "graph_hash", "tasks"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    if object.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_keys
        || object.get("schema").and_then(Value::as_str) != Some(HOST_POLICY_SCHEMA)
        || object.get("project_id").and_then(Value::as_str) != Some(project_id)
        || object.get("graph_hash").and_then(Value::as_str) != Some(graph_hash)
    {
        bail!("node-intelligence host policy is not bound to the current project graph");
    }
    let tasks = object
        .get("tasks")
        .and_then(Value::as_object)
        .context("node-intelligence host policy requires a task map")?;
    let task = tasks
        .get(node_id)
        .context("node-intelligence host policy has no entry for this task")?;
    let task_object = task
        .as_object()
        .context("node-intelligence host policy task entry must be an object")?;
    let task_keys = task_object
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let legacy_task_keys = [
        "authorization",
        "authorized_approvers",
        "qualified_reviewers",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let memory_task_keys = [
        "authorization",
        "authorized_approvers",
        "qualified_reviewers",
        "memory",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if task_keys != legacy_task_keys && task_keys != memory_task_keys {
        bail!("node-intelligence host policy task entry has an invalid schema");
    }
    if task_object
        .get("memory")
        .is_some_and(|memory| !memory.is_null() && !memory.is_object())
    {
        bail!("node-intelligence host policy memory grant must be null or an object");
    }
    let authorization = task_object
        .get("authorization")
        .and_then(Value::as_object)
        .context("node-intelligence host policy authorization must be an object")?;
    let authorization_keys = [
        "authorized_artifacts",
        "current_artifacts",
        "current_sources",
        "authorized_permissions",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if authorization
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != authorization_keys
    {
        bail!("node-intelligence host policy authorization has an invalid schema");
    }
    for field in ["authorized_artifacts", "authorized_permissions"] {
        let values = authorization
            .get(field)
            .and_then(Value::as_array)
            .context("node-intelligence host policy authorization refs must be arrays")?;
        if values
            .iter()
            .any(|value| value.as_str().is_none_or(|reference| !is_digest(reference)))
        {
            bail!("node-intelligence host policy contains an invalid authorized ref");
        }
    }
    for field in ["current_artifacts", "current_sources"] {
        let values = authorization
            .get(field)
            .and_then(Value::as_object)
            .context("node-intelligence host policy current refs must be objects")?;
        if values
            .values()
            .any(|value| value.as_str().is_none_or(|reference| !is_digest(reference)))
        {
            bail!("node-intelligence host policy contains an invalid current ref");
        }
    }
    for field in ["authorized_approvers", "qualified_reviewers"] {
        let mut values = Map::new();
        values.insert(field.to_owned(), task_object[field].clone());
        required_string_array(&values, field)?;
    }
    Ok(HostPolicy {
        path: canonical_path,
        digest: hash_bytes(&bytes),
        task: task.clone(),
    })
}

fn validate_analysis_configuration(node: &Value, fields: &Map<String, Value>) -> Result<Duration> {
    if node.get("capability").and_then(Value::as_str) != Some(ANALYSIS_CAPABILITY) {
        bail!("analysis operation is allowed only for intelligence.measurement.analyze nodes");
    }
    if node
        .pointer("/hard_limits/max_calls")
        .and_then(Value::as_u64)
        .is_none_or(|calls| calls < 1)
    {
        bail!("analysis node has no remaining call budget");
    }
    let max_elapsed_ms = node
        .pointer("/hard_limits/max_elapsed_ms")
        .and_then(Value::as_u64)
        .context("analysis node has no elapsed-time budget")?;
    let analysis = fields
        .get("runtime")
        .and_then(|runtime| runtime.get("analysis"))
        .and_then(Value::as_object)
        .context("analysis task requires runtime.analysis configuration")?;
    if analysis.get("enabled").and_then(Value::as_bool) != Some(true) {
        bail!("analysis task runtime must explicitly enable the adapter");
    }
    let database_path = analysis
        .get("database_path")
        .and_then(Value::as_str)
        .context("analysis task requires a project-relative database_path")?;
    if safe_project_path(Path::new("/workspace"), database_path).is_err() {
        bail!("analysis database_path must be project-relative");
    }
    for field in ["output_permission_ref", "output_retention_ref"] {
        let value = analysis
            .get(field)
            .and_then(Value::as_str)
            .context("analysis task lacks output permission/retention refs")?;
        if !is_digest(value) {
            bail!("analysis output permission and retention refs must be sha256 digests");
        }
    }
    let output_permission_ref = analysis["output_permission_ref"]
        .as_str()
        .expect("validated above");
    let authorized_permissions = fields
        .get("runtime")
        .and_then(|runtime| runtime.get("authorization"))
        .and_then(|authorization| authorization.get("authorized_permissions"))
        .and_then(Value::as_array)
        .context("analysis task requires host-authorized output permissions")?;
    if !authorized_permissions
        .iter()
        .any(|reference| reference.as_str() == Some(output_permission_ref))
    {
        bail!("analysis output permission is not in the host-authorized permission set");
    }
    let timeout = analysis
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .context("analysis timeout_ms must be an integer")?;
    if !(100..=5_000).contains(&timeout) || timeout > max_elapsed_ms {
        bail!("analysis timeout_ms must be between 100 and 5000");
    }
    Ok(Duration::from_millis(timeout))
}

fn validated_gate_bindings(
    workspace: &Path,
    document: &crate::project_file::FractalProject,
    node: &Value,
    context: &GateContext,
) -> Result<Vec<String>> {
    crate::external_gates::enforce_checkout(
        workspace,
        document,
        &context.node_id,
        document
            .execution
            .as_ref()
            .and_then(|state| state.assignments.get(&context.node_id))
            .map(|assignment| assignment.agent_id.as_str())
            .unwrap_or(""),
    )
    .context("external gate denied node-intelligence attempt")?;
    let gates = crate::external_gates::required_gates(node)?;
    if gates.is_empty() {
        return Ok(Vec::new());
    }
    let ledger = document
        .external_gate_ledger
        .as_ref()
        .context("external gate ledger missing for node-intelligence attempt")?;
    crate::external_gates::validate_ledger(ledger)
        .context("external gate ledger invalid for node-intelligence attempt")?;
    let revoked = ledger
        .records
        .iter()
        .filter(|record| record.kind == "revocation")
        .filter_map(|record| record.revokes.as_deref())
        .collect::<BTreeSet<_>>();
    let mut refs = Vec::with_capacity(gates.len());
    for gate in gates {
        let approval = ledger
            .records
            .iter()
            .rev()
            .find(|record| {
                record.kind == "approval"
                    && record.graph_hash == context.graph_hash
                    && record.node_id == context.node_id
                    && record.gate == gate
                    && !revoked.contains(record.content_hash.as_str())
            })
            .with_context(|| format!("external gate {gate} has no active approval"))?;
        let expected_role = crate::external_gates::required_role(&gate);
        if approval.role != expected_role {
            bail!("external gate {gate} approval role is invalid");
        }
        let (rendered, evidence) = crate::external_gates::read_safe_evidence(
            workspace,
            Path::new(&approval.evidence_path),
        )
        .with_context(|| format!("external gate {gate} evidence is unsafe"))?;
        if rendered != approval.evidence_path
            || evidence.len() as u64 != approval.evidence_length
            || hash_bytes(&evidence) != approval.evidence_hash
            || evidence.len() > MAX_APPROVAL_MANIFEST_BYTES
        {
            bail!("external gate {gate} evidence changed");
        }
        let manifest = parse_unique_json(&evidence).with_context(|| {
            format!("external gate {gate} evidence is not a bound approval manifest")
        })?;
        validate_approval_manifest(&manifest, context, &gate)?;
        refs.push(stable_gate_ref(context, &gate)?);
    }
    refs.sort();
    refs.dedup();
    Ok(refs)
}

fn validate_approval_manifest(value: &Value, context: &GateContext, gate: &str) -> Result<()> {
    let object = value
        .as_object()
        .context("external gate approval manifest must be an object")?;
    let expected_keys = [
        "schema",
        "project_id",
        "graph_hash",
        "node_id",
        "gate",
        "gate_ref",
        "attempt_ref",
        "network_ref",
        "plan_ref",
        "input_refs",
        "handoff_refs",
        "review_packet_refs",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if object.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_keys {
        bail!("external gate approval manifest has an invalid schema");
    }
    let expected = json!({
        "schema": APPROVAL_MANIFEST_SCHEMA,
        "project_id": context.project_id,
        "graph_hash": context.graph_hash,
        "node_id": context.node_id,
        "gate": gate,
        "gate_ref": stable_gate_ref(context, gate)?,
        "attempt_ref": context.attempt_ref,
        "network_ref": context.network_ref,
        "plan_ref": context.plan_ref,
        "input_refs": context.input_refs,
        "handoff_refs": context.handoff_refs,
        "review_packet_refs": context.review_packet_refs,
    });
    if value != &expected {
        bail!("external gate approval manifest is stale for this plan, input, attempt, or packet");
    }
    Ok(())
}

fn stable_gate_ref(context: &GateContext, gate: &str) -> Result<String> {
    let value = json!({
        "schema": "fractal.node_intelligence.gate_ref.v1",
        "project_id": context.project_id,
        "graph_hash": context.graph_hash,
        "node_id": context.node_id,
        "gate": gate,
    });
    fractal_contracts::canonical_sha256(&value)
        .map_err(|error| anyhow::anyhow!("hash declared gate identity: {error}"))
}

fn run_bridge_child(
    workspace: &Path,
    request: &[u8],
    timeout: Duration,
    launcher: &Launcher,
) -> Result<Value> {
    let _ = workspace;
    if request.len() > MAX_REQUEST_BYTES {
        bail!("node-intelligence request exceeds size limit");
    }
    crate::run_control::check_current_run_before_spawn()?;
    let mut command = Command::new(&launcher.program);
    command
        .args(&launcher.args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in [
        "PATH",
        "PYTHONPATH",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "CUDA_VISIBLE_DEVICES",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("failed to start configured local node-intelligence runtime")?;
    let mut input = child.stdin.take().context("open node-intelligence stdin")?;
    let request = request.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        input.write_all(&request)?;
        input.flush()
    });
    let stdout = child
        .stdout
        .take()
        .context("capture node-intelligence stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("capture node-intelligence stderr")?;
    let reader = std::thread::spawn(move || read_bounded(stdout, MAX_RESPONSE_BYTES));
    let stderr_reader = std::thread::spawn(move || drain(stderr));
    let _worker = register_bridge_worker(&mut child)?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child
            .try_wait()
            .context("wait for node-intelligence runtime")?
        {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                terminate_bridge_group(child.id());
                let _ = child.wait();
                bail!("node-intelligence runtime timed out");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    // Python can exit while a helper process still holds its stdout/stderr
    // descriptors. Close the exact process group before joining pipe readers.
    terminate_bridge_group(child.id());
    drop(_worker);
    join_until(writer, deadline, "node-intelligence request writer")?
        .context("write node-intelligence request")?;
    let (stdout, overflowed) = join_until(reader, deadline, "node-intelligence stdout reader")?
        .context("read node-intelligence response")?;
    join_until(stderr_reader, deadline, "node-intelligence stderr reader")?
        .context("drain node-intelligence stderr")?;
    if !status.success() {
        bail!(
            "node-intelligence runtime exited unsuccessfully (code {:?})",
            status.code()
        );
    }
    if overflowed || stdout.is_empty() {
        bail!("node-intelligence runtime produced an empty or oversized response");
    }
    parse_unique_json(&stdout).context("decode node-intelligence response")
}

fn join_until<T>(
    handle: std::thread::JoinHandle<T>,
    deadline: Instant,
    description: &str,
) -> Result<T> {
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            bail!("{description} exceeded node-intelligence deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("{description} panicked"))
}

fn terminate_bridge_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        let process_group = -(pid as i32);
        libc::kill(process_group, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(100));
        // Kill the group unconditionally: its original leader can have exited
        // while a local helper keeps inherited output descriptors open.
        libc::kill(process_group, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn register_bridge_worker(
    child: &mut std::process::Child,
) -> Result<crate::run_control::WorkerGuard> {
    crate::run_control::WorkerGuard::register_current(child.id()).map_err(|error| {
        crate::run_control::terminate_worker(child.id());
        let _ = child.wait();
        error.context("node-intelligence worker registration failed")
    })
}

fn read_bounded(mut reader: impl Read, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut data = Vec::new();
    let mut overflowed = false;
    let mut buffer = [0_u8; 8_192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(data.len());
        data.extend_from_slice(&buffer[..count.min(remaining)]);
        overflowed |= count > remaining;
    }
    Ok((data, overflowed))
}

fn drain(mut reader: impl Read) -> std::io::Result<()> {
    let mut buffer = [0_u8; 8_192];
    while reader.read(&mut buffer)? != 0 {}
    Ok(())
}

fn validate_response(
    workspace: &Path,
    request: &BridgeRequest,
    response: &Value,
) -> Result<BridgeEvidence> {
    let object = response
        .as_object()
        .context("node-intelligence response must be an object")?;
    if object.get("schema").and_then(Value::as_str) != Some(RESPONSE_SCHEMA) {
        bail!("node-intelligence response schema mismatch");
    }
    let request_value = &request.value;
    let mut request_without_hash = request_value.clone();
    let expected_hash = request_without_hash
        .as_object_mut()
        .and_then(|object| object.remove("request_hash"))
        .context("node-intelligence request lacks request_hash")?;
    let computed_hash = fractal_contracts::canonical_sha256(&request_without_hash)
        .map_err(|error| anyhow::anyhow!("hash node-intelligence request: {error}"))?;
    if expected_hash.as_str() != Some(computed_hash.as_str()) {
        bail!("node-intelligence request hash is not canonical");
    }
    for field in ECHO_FIELDS {
        if object.get(field) != request_value.get(field) {
            bail!("node-intelligence response binding mismatch for {field}");
        }
    }
    let status = object.get("status").and_then(Value::as_str);
    let review_only = request.review_operation;
    let allowed_status = if review_only {
        matches!(status, Some("ready" | "pending_review"))
    } else {
        status == Some("ready")
    };
    if !allowed_status || !object.get("error_code").is_some_and(Value::is_null) {
        let code = object
            .get("error_code")
            .and_then(Value::as_str)
            .unwrap_or("runtime_blocked");
        if matches!(
            code,
            "review_pending_or_invalid" | "review_attempt_mismatch"
        ) {
            return Err(ReviewBlockedBeforeEffect {
                code: code.to_owned(),
            }
            .into());
        }
        bail!("node-intelligence runtime blocked the configured attempt ({code})");
    }
    let attempt_ref = response_digest(object, "attempt_ref")?;
    if attempt_ref != request.gate_context.attempt_ref {
        bail!("node-intelligence returned a different pinned attempt");
    }
    let receipt_ref = response_digest(object, "receipt_ref")?;
    let context_ref = response_digest(object, "context_manifest_ref")?;
    let decision = object
        .get("decision")
        .context("node-intelligence response lacks decision")?;
    validate_decision(decision, request_value)?;
    if review_only
        && decision.get("reason").and_then(Value::as_str) != Some("review_admission_only")
    {
        bail!("review-only response contains a decision from another operation");
    }
    if review_only && !object.get("analysis").is_some_and(Value::is_null) {
        bail!("review-only response unexpectedly contains an analysis action");
    }
    let analysis = if request_value.get("operation").and_then(Value::as_str) == Some("analysis") {
        let result = object
            .get("analysis")
            .context("analysis response lacks typed analysis status")?;
        validate_analysis_result(result, &attempt_ref)?;
        Some(result.clone())
    } else {
        if !object.get("analysis").is_some_and(Value::is_null) {
            bail!("prepare response unexpectedly contains an analysis action");
        }
        None
    };
    let remaining_elapsed_ms = object
        .get("remaining_elapsed_ms")
        .and_then(Value::as_u64)
        .context("node-intelligence response lacks remaining elapsed-time budget")?;
    let remaining_calls = object
        .get("remaining_calls")
        .and_then(Value::as_u64)
        .context("node-intelligence response lacks remaining call budget")?;
    let context_path = object
        .get("context_manifest_path")
        .and_then(Value::as_str)
        .context("node-intelligence response lacks a context manifest path")?;
    let expected_context_path = content_path(&request.receipt_path, &context_ref)?;
    if context_path != expected_context_path {
        bail!("node-intelligence context path is not its content-addressed receipt path");
    }
    let context_bytes = read_private_content(
        workspace,
        context_path,
        &context_ref,
        MAX_PRIVATE_RECEIPT_BYTES,
    )?;
    let context =
        parse_unique_json(&context_bytes).context("decode node-intelligence context manifest")?;
    if context.get("schema").and_then(Value::as_str) != Some(CONTEXT_SCHEMA)
        || context.get("attempt_ref").and_then(Value::as_str) != Some(attempt_ref.as_str())
        || context.get("project_id") != request_value.get("project_id")
        || context.get("graph_hash") != request_value.get("graph_hash")
        || context.get("input_refs") != request_value.get("input_refs")
        || context.get("handoff_refs") != request_value.get("handoff_refs")
        || context.get("review_packet_refs") != request_value.get("review_packet_refs")
    {
        bail!("node-intelligence context manifest is not bound to the current attempt");
    }
    let receipt_path = request
        .value
        .pointer("/runtime/receipt_path")
        .and_then(Value::as_str)
        .context("node-intelligence request lacks receipt_path")?;
    let receipt_file = content_path(receipt_path, &receipt_ref)?;
    let receipt_bytes = read_private_content(
        workspace,
        &receipt_file,
        &receipt_ref,
        MAX_PRIVATE_RECEIPT_BYTES,
    )?;
    let receipt =
        parse_unique_json(&receipt_bytes).context("decode node-intelligence durable receipt")?;
    if receipt.get("schema").and_then(Value::as_str) != Some(RECEIPT_SCHEMA)
        || receipt.get("status") != object.get("status")
        || receipt.get("request_hash") != request_value.get("request_hash")
        || receipt.get("attempt_ref").and_then(Value::as_str) != Some(attempt_ref.as_str())
        || receipt.get("context_manifest_ref").and_then(Value::as_str) != Some(context_ref.as_str())
        || receipt.get("decision") != Some(decision)
        || receipt.get("analysis") != object.get("analysis")
        || receipt.get("remaining_elapsed_ms") != object.get("remaining_elapsed_ms")
        || receipt.get("remaining_calls") != object.get("remaining_calls")
        || receipt.get("provider_calls").and_then(Value::as_u64) != Some(0)
        || !receipt.get("verified_outcome").is_some_and(Value::is_null)
        || receipt.get("training_eligible").and_then(Value::as_bool) != Some(false)
        || receipt.get("mode").and_then(Value::as_str) != Some("shadow")
    {
        bail!("node-intelligence durable receipt does not bind to this response");
    }
    for field in ECHO_FIELDS {
        if receipt.get(field) != request_value.get(field) {
            bail!("node-intelligence durable receipt binding mismatch for {field}");
        }
    }
    let response_hash = fractal_contracts::canonical_sha256(response)
        .map_err(|error| anyhow::anyhow!("hash node-intelligence response: {error}"))?;
    Ok(BridgeEvidence {
        request_hash: request_value["request_hash"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        response_hash,
        attempt_ref,
        receipt_ref,
        context_manifest_ref: context_ref,
        context_manifest_path: context_path.to_owned(),
        policy_ref: request_value["policy_ref"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        model_ref: request_value["model_ref"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        network_ref: request_value["network_ref"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        host_policy_digest: request.host_policy_digest.clone(),
        input_refs: request.gate_context.input_refs.clone(),
        handoff_refs: request.gate_context.handoff_refs.clone(),
        review_packet_refs: request.gate_context.review_packet_refs.clone(),
        decision: decision.clone(),
        analysis,
        remaining_elapsed_ms,
        remaining_calls,
        approved_gate_refs: request
            .value
            .pointer("/runtime/approved_gate_refs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        pending_review: status == Some("pending_review"),
        gate_context: request.gate_context.clone(),
        node_id: request.node_id.clone(),
        worker_id: request.worker_id.clone(),
        attempt_number: request.attempt_number,
        graph_hash: request.graph_hash.clone(),
        config_digest: request.config_digest.clone(),
        host_policy_path: request.host_policy_path.clone(),
        pre_admission: request.pre_admission,
    })
}

fn validate_analysis_result(value: &Value, expected_attempt_ref: &str) -> Result<()> {
    let action = value
        .get("action")
        .context("analysis result lacks action receipt")?;
    let collection = value
        .get("collection")
        .context("analysis result lacks collection status")?;
    let state = action
        .get("state")
        .and_then(Value::as_str)
        .context("analysis action state is missing")?;
    let action_id = format!(
        "fractal:action:{}",
        digest_hex(
            fractal_contracts::canonical_json(&json!({
                "attempt_ref": expected_attempt_ref,
                "operation": "measurement-analyze",
            }))?
            .as_slice(),
        )
    );
    if action.get("action_id").and_then(Value::as_str) != Some(action_id.as_str())
        || action.get("attempt_ref").and_then(Value::as_str) != Some(expected_attempt_ref)
        || collection.get("action_id").and_then(Value::as_str) != Some(action_id.as_str())
    {
        bail!("analysis action receipt is bound to a different attempt");
    }
    if !matches!(
        state,
        "running" | "complete" | "failed" | "timed_out" | "cancelled" | "unknown"
    ) || collection
        .get("available")
        .and_then(Value::as_bool)
        .is_none()
        || action
            .get("verified_outcome")
            .is_none_or(|outcome| !outcome.is_null())
        || action
            .get("receipt_ref")
            .and_then(Value::as_str)
            .is_none_or(|reference| !is_digest(reference))
    {
        bail!("analysis result violates the typed, unverified action contract");
    }
    let refs = action
        .get("artifact_refs")
        .and_then(Value::as_array)
        .context("analysis action artifact refs are missing")?;
    if refs.iter().any(|reference| {
        reference
            .as_str()
            .is_none_or(|reference| !is_digest(reference))
    }) {
        bail!("analysis action contains an invalid artifact ref");
    }
    if let Some(input_ref) = action.get("input_ref") {
        if !input_ref.is_null() && input_ref.as_str().is_none_or(|value| !is_digest(value)) {
            bail!("analysis action contains an invalid input ref");
        }
    }
    let receipt_ref = action["receipt_ref"].as_str().expect("validated above");
    let mut action_without_receipt_ref = action.clone();
    action_without_receipt_ref
        .as_object_mut()
        .context("analysis action receipt must be an object")?
        .remove("receipt_ref");
    if fractal_contracts::canonical_sha256(&action_without_receipt_ref)
        .map_err(|error| anyhow::anyhow!("hash analysis action receipt: {error}"))?
        != receipt_ref
    {
        bail!("analysis action receipt hash mismatch");
    }
    let collection_refs = collection
        .get("artifact_refs")
        .and_then(Value::as_array)
        .context("analysis collection artifact refs are missing")?;
    if collection_refs.iter().any(|reference| {
        reference
            .as_str()
            .is_none_or(|reference| !is_digest(reference))
    }) {
        bail!("analysis collection contains an invalid artifact ref");
    }
    if collection.get("artifact_refs") != action.get("artifact_refs") {
        bail!("analysis collection does not bind the action's artifact refs");
    }
    if collection.get("available").and_then(Value::as_bool) == Some(true) && state != "complete" {
        bail!("analysis collection claims availability for a non-complete action");
    }
    let usage = value
        .get("usage")
        .and_then(Value::as_object)
        .context("analysis usage receipt is missing")?;
    let usage_keys = [
        "action_id",
        "elapsed_ms",
        "cpu_ms",
        "peak_rss_bytes",
        "input_bytes",
        "output_bytes",
        "provider_calls",
        "cost_microusd",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if usage.keys().map(String::as_str).collect::<BTreeSet<_>>() != usage_keys
        || usage.get("action_id") != action.get("action_id")
        || usage.get("provider_calls").and_then(Value::as_u64) != Some(0)
        || !usage.get("cost_microusd").is_some_and(Value::is_null)
    {
        bail!("analysis usage receipt is not bound to the local zero-cost adapter");
    }
    for field in [
        "elapsed_ms",
        "cpu_ms",
        "peak_rss_bytes",
        "input_bytes",
        "output_bytes",
        "provider_calls",
        "cost_microusd",
    ] {
        if usage
            .get(field)
            .is_none_or(|value| !value.is_null() && value.as_u64().is_none())
        {
            bail!("analysis usage receipt contains an invalid quantity");
        }
    }
    Ok(())
}

fn validate_decision(decision: &Value, request: &Value) -> Result<()> {
    if decision.get("family").and_then(Value::as_str) != Some("routing")
        || decision.get("mode").and_then(Value::as_str) != Some("shadow")
        || decision.get("baseline") != request.pointer("/route/baseline_route_id")
        || !decision.get("proposal").is_some_and(Value::is_null)
        || decision
            .get("reason")
            .and_then(Value::as_str)
            .is_none_or(|reason| reason.is_empty() || reason.len() > 120)
    {
        bail!("node-intelligence decision is not an abstaining shadow recommendation");
    }
    Ok(())
}

fn response_digest(object: &Map<String, Value>, field: &str) -> Result<String> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .context("node-intelligence response is missing a digest")?;
    if !is_digest(value) {
        bail!("node-intelligence response contains an invalid digest");
    }
    Ok(value.to_owned())
}

fn read_private_content(
    workspace: &Path,
    relative: &str,
    expected_digest: &str,
    limit: usize,
) -> Result<Vec<u8>> {
    let path = safe_project_path(workspace, relative)?;
    let mut current = PathBuf::from(workspace);
    let components = Path::new(relative)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for (index, part) in components.iter().enumerate() {
        current.push(part);
        let metadata = fs::symlink_metadata(&current).with_context(|| {
            format!(
                "inspect node-intelligence private path {}",
                current.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            bail!("node-intelligence private path contains a symlink");
        }
        if index + 1 < components.len() {
            if !metadata.is_dir() {
                bail!("node-intelligence receipt parent is not a directory");
            }
        } else if !metadata.is_file() || !private_metadata(&metadata) {
            bail!("node-intelligence receipt is not a private regular file");
        }
    }
    let receipt_root = path
        .parent()
        .context("node-intelligence receipt has no directory")?;
    let receipt_directory = fs::symlink_metadata(receipt_root)?;
    if receipt_directory.file_type().is_symlink()
        || !receipt_directory.is_dir()
        || !private_metadata(&receipt_directory)
    {
        bail!("node-intelligence receipt directory is not private");
    }
    let file = open_nofollow(&path)?;
    let mut data = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut data)?;
    if data.len() > limit || hash_bytes(&data) != expected_digest {
        bail!("node-intelligence content-addressed receipt hash mismatch");
    }
    Ok(data)
}

fn safe_project_path(workspace: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || relative.as_os_str().is_empty()
    {
        bail!("node-intelligence receipt path must be a safe project-relative path");
    }
    Ok(workspace.join(relative))
}

fn private_metadata(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

fn content_path(root: &str, digest: &str) -> Result<String> {
    if !is_digest(digest) {
        bail!("invalid node-intelligence content digest");
    }
    let root = Path::new(root);
    if root.is_absolute()
        || root
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("node-intelligence receipt root must be project-relative");
    }
    Ok(root
        .join(format!("{}.json", &digest[7..]))
        .to_string_lossy()
        .into_owned())
}

fn open_nofollow(path: &Path) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options
        .open(path)
        .with_context(|| format!("open node-intelligence file {}", path.display()))
}

fn graph_node<'a>(graph: &'a Value, node_id: &str) -> Result<&'a Value> {
    graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(node_id))
        .with_context(|| format!("current canonical graph has no node {node_id}"))
}

fn baseline_route_id(invocation: &crate::chain::jev_receipt::RouteInvocation) -> String {
    format!(
        "legacy:{}:backend-unknown:model-unknown:{}",
        invocation.cli_family,
        invocation.selected_effort.as_deref().unwrap_or("unknown")
    )
}

fn attempt_id(project_id: &str, graph_hash: &str, node_id: &str, attempt: u32) -> String {
    let value = format!("{project_id}|{graph_hash}|{node_id}|{attempt}");
    format!("fractal:attempt:{}", digest_hex(value.as_bytes()))
}

fn required_digest(fields: &Map<String, Value>, field: &str) -> Result<String> {
    let value = required_string(fields, field)?;
    if !is_digest(&value) {
        bail!("node-intelligence {field} must be a sha256 reference");
    }
    Ok(value)
}

fn required_string(fields: &Map<String, Value>, field: &str) -> Result<String> {
    fields
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .with_context(|| format!("node-intelligence {field} must be a non-empty string"))
}

fn sorted_digest_array(fields: &Map<String, Value>, field: &str) -> Result<Vec<String>> {
    let mut values = fields
        .get(field)
        .and_then(Value::as_array)
        .with_context(|| format!("node-intelligence {field} must be an array"))?
        .iter()
        .map(|value| {
            let value = value
                .as_str()
                .context("node-intelligence content refs must be strings")?;
            if !is_digest(value) {
                bail!("node-intelligence content refs must be sha256 digests");
            }
            Ok(value.to_owned())
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort();
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("node-intelligence {field} must not contain duplicate refs");
    }
    Ok(values)
}

fn sorted_memory_refs(fields: &Map<String, Value>) -> Result<Vec<Value>> {
    let mut values = fields
        .get("memory_refs")
        .and_then(Value::as_array)
        .context("node-intelligence memory_refs must be an array")?
        .clone();
    for value in &values {
        let object = value
            .as_object()
            .context("node-intelligence memory refs must be objects")?;
        if object.len() != 2
            || object.get("id").and_then(Value::as_str).is_none()
            || object
                .get("revision")
                .and_then(Value::as_str)
                .is_none_or(|revision| !is_digest(revision))
        {
            bail!("node-intelligence memory ref is malformed");
        }
    }
    values.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    if values.windows(2).any(|pair| pair[0]["id"] == pair[1]["id"]) {
        bail!("node-intelligence memory refs must not repeat an identity");
    }
    Ok(values)
}

fn required_string_array(fields: &Map<String, Value>, field: &str) -> Result<Vec<String>> {
    let mut values = fields
        .get(field)
        .and_then(Value::as_array)
        .with_context(|| format!("node-intelligence {field} must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
                .context("node-intelligence reviewer IDs must be non-empty strings")
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort();
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("node-intelligence reviewer lists must not contain duplicates");
    }
    Ok(values)
}

fn is_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn digest_hex(value: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hash_bytes(value: &[u8]) -> String {
    format!("sha256:{}", digest_hex(value))
}

fn parse_unique_json(bytes: &[u8]) -> Result<Value> {
    let parsed = serde_json::from_slice::<UniqueValue>(bytes)?;
    Ok(parsed.0)
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueValue;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate object keys")
            }
            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UniqueValue(Value::Bool(value)))
            }
            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UniqueValue(Value::Number(value.into())))
            }
            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UniqueValue(Value::Number(value.into())))
            }
            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(|number| UniqueValue(Value::Number(number)))
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }
            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UniqueValue(Value::String(value.to_owned())))
            }
            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UniqueValue(Value::String(value)))
            }
            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<UniqueValue>()? {
                    values.push(value.0);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = Map::new();
                while let Some((key, value)) = map.next_entry::<String, UniqueValue>()? {
                    if values.insert(key.clone(), value.0).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON key {key}"
                        )));
                    }
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "fractal-node-intelligence-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn digest(value: &[u8]) -> String {
        hash_bytes(value)
    }

    #[test]
    fn owner_policy_binds_private_memory_scope_and_defaults_to_null() {
        let runtime = serde_json::from_value::<Map<String, Value>>(serde_json::json!({})).unwrap();
        let missing_policy =
            serde_json::from_value::<Map<String, Value>>(serde_json::json!({})).unwrap();
        assert!(host_policy_memory_matches(&missing_policy, &runtime));

        let runtime = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
            "memory": {
                "repository_path": ".fractal/private-memory.sqlite3",
                "context_request": {"scope": "local_private", "requester": "owner"},
                "current_revisions": {"decision-1": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
            }
        }))
        .unwrap();
        assert!(
            !host_policy_memory_matches(&missing_policy, &runtime),
            "project configuration must not grant private memory scope by itself"
        );

        let matching_policy = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
            "memory": runtime["memory"]
        }))
        .unwrap();
        assert!(host_policy_memory_matches(&matching_policy, &runtime));

        let changed_policy = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
            "memory": {
                "repository_path": ".fractal/private-memory.sqlite3",
                "context_request": {"scope": "public", "requester": "owner"},
                "current_revisions": {"decision-1": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
            }
        }))
        .unwrap();
        assert!(!host_policy_memory_matches(&changed_policy, &runtime));
    }

    fn request_fixture(workspace: &Path) -> BridgeRequest {
        let mut request = json!({
            "schema": REQUEST_SCHEMA,
            "request_id": "request-1",
            "request_hash": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "project_id": "fractal:project:fixture",
            "graph_id": "graph-fixture",
            "graph_hash": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "attempt": {"id":"fractal:attempt:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", "node_id":"build", "number":1},
            "network_ref": "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "capability_id": "fractal:capability:code",
            "node_ref": "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "model_ref": "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "policy_ref": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "input_refs": [], "memory_refs": [], "evidence_refs": [], "handoff_refs": [], "review_packet_refs": [],
            "workspace": workspace.to_string_lossy(),
            "graph": {"graph_hash":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},
            "explicit_pin": false, "role":"worker", "payload_classification":"synthetic", "public_features":{},
            "route":{"baseline_route_id":"legacy:codex:backend-unknown:model-unknown:high"},
            "runtime":{"receipt_path":".fractal/node-receipts", "approved_gate_refs":[]}
        });
        let hash = fractal_contracts::canonical_sha256(&request_without_hash(&request)).unwrap();
        request["request_hash"] = Value::String(hash);
        BridgeRequest {
            value: request,
            gate_context: GateContext {
                project_id: "fractal:project:fixture".to_owned(),
                graph_hash:
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .to_owned(),
                node_id: "build".to_owned(),
                attempt_ref:
                    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                        .to_owned(),
                network_ref:
                    "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                        .to_owned(),
                plan_ref: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .to_owned(),
                input_refs: Vec::new(),
                handoff_refs: Vec::new(),
                review_packet_refs: Vec::new(),
            },
            attempt_number: 1,
            graph_hash: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .to_owned(),
            node_id: "build".to_owned(),
            worker_id: "codex".to_owned(),
            receipt_path: ".fractal/node-receipts".to_owned(),
            timeout: Duration::from_secs(5),
            config_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            host_policy_path: workspace.join("host-policy.json"),
            host_policy_digest:
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            pre_admission: false,
            review_operation: false,
            previous_attempt_count: 0,
        }
    }

    fn request_without_hash(request: &Value) -> Value {
        let mut value = request.clone();
        value.as_object_mut().unwrap().remove("request_hash");
        value
    }

    fn write_content(workspace: &Path, relative: &str, bytes: &[u8]) -> String {
        let path = workspace.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        digest(bytes)
    }

    fn response_fixture(workspace: &Path, request: &BridgeRequest) -> Value {
        response_fixture_with_status(workspace, request, "ready")
    }

    fn response_fixture_with_status(
        workspace: &Path,
        request: &BridgeRequest,
        status: &str,
    ) -> Value {
        let context = json!({
            "schema": CONTEXT_SCHEMA,
            "attempt_ref": request.gate_context.attempt_ref,
            "project_id": request.value["project_id"],
            "graph_hash": request.value["graph_hash"],
            "input_refs": request.value["input_refs"],
            "handoff_refs": request.value["handoff_refs"],
            "review_packet_refs": request.value["review_packet_refs"],
        });
        let context_bytes = fractal_contracts::canonical_json(&context).unwrap();
        let context_ref = write_content(
            workspace,
            &content_path(".fractal/node-receipts", &digest(&context_bytes)).unwrap(),
            &context_bytes,
        );
        let decision = json!({"family":"routing", "mode":"shadow", "reason":if request.review_operation { "review_admission_only" } else { "approved_roster_empty" }, "baseline":request.value.pointer("/route/baseline_route_id").unwrap(), "proposal":null});
        let mut receipt = json!({
            "schema": RECEIPT_SCHEMA,
            "attempt_ref": request.gate_context.attempt_ref,
            "context_manifest_ref": context_ref,
            "decision": decision,
            "analysis": null,
            "remaining_elapsed_ms": 1000,
            "remaining_calls": 1,
            "status":status,
            "signals_ref": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            "mode":"shadow","provider_calls":0,"verified_outcome":null,"training_eligible":false
        });
        for field in ECHO_FIELDS {
            receipt[field] = request.value[field].clone();
        }
        let receipt_bytes = fractal_contracts::canonical_json(&receipt).unwrap();
        let receipt_ref = write_content(
            workspace,
            &content_path(".fractal/node-receipts", &digest(&receipt_bytes)).unwrap(),
            &receipt_bytes,
        );
        let mut response = json!({
            "schema": RESPONSE_SCHEMA,
            "decision": decision,
            "attempt_ref": request.gate_context.attempt_ref,
            "receipt_ref": receipt_ref,
            "context_manifest_ref": context_ref,
            "context_manifest_path": content_path(".fractal/node-receipts", &context_ref).unwrap(),
            "analysis": null,
            "remaining_elapsed_ms": 1000,
            "remaining_calls": 1,
            "status":status,"error_code":null
        });
        for field in ECHO_FIELDS {
            response[field] = request.value[field].clone();
        }
        response
    }

    fn python_launcher(script: &str) -> Launcher {
        let program = std::env::var_os("PYTHON3").unwrap_or_else(|| "python3".into());
        Launcher {
            program: PathBuf::from(program),
            args: vec!["-c".to_owned(), script.to_owned()],
        }
    }

    #[test]
    fn duplicate_json_keys_are_rejected() {
        assert!(parse_unique_json(br#"{"x":1,"x":2}"#).is_err());
    }

    #[test]
    fn unavailable_child_fails_enabled_bridge() {
        let dir = TempDir::new();
        let launcher = Launcher {
            program: dir.path().join("missing-python"),
            args: vec![],
        };
        assert!(run_bridge_child(dir.path(), b"{}", Duration::from_secs(1), &launcher).is_err());
    }

    #[test]
    fn child_timeout_is_bounded_and_killed() {
        let dir = TempDir::new();
        let launcher = python_launcher("import time; time.sleep(2)");
        let result = run_bridge_child(dir.path(), b"{}", Duration::from_millis(100), &launcher);
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[cfg(unix)]
    #[test]
    fn exited_child_cannot_leave_descendant_holding_output_pipe() {
        let dir = TempDir::new();
        let launcher = python_launcher(
            "import os,time; pid=os.fork();\nif pid == 0: time.sleep(30); os._exit(0)\nprint('{}', flush=True)",
        );
        let started = Instant::now();
        let result = run_bridge_child(dir.path(), b"{}", Duration::from_secs(3), &launcher);
        assert_eq!(result.unwrap(), json!({}));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn real_child_malformed_response_is_rejected() {
        let dir = TempDir::new();
        let launcher = python_launcher("print('{\"schema\":1,\"schema\":2}')");
        let raw = serde_json::to_vec(&json!({})).unwrap();
        let result = run_bridge_child(dir.path(), &raw, Duration::from_secs(5), &launcher);
        assert!(result.is_err());
    }

    #[test]
    fn response_echo_and_attempt_pin_are_checked() {
        let dir = TempDir::new();
        let mut request = request_fixture(dir.path());
        let mut response = response_fixture(dir.path(), &request);
        response["attempt_ref"] = Value::String(
            "sha256:9999999999999999999999999999999999999999999999999999999999999999".to_owned(),
        );
        assert!(validate_response(dir.path(), &request, &response).is_err());
        response = response_fixture(dir.path(), &request);
        response["graph_hash"] = Value::String(
            "sha256:9999999999999999999999999999999999999999999999999999999999999999".to_owned(),
        );
        assert!(validate_response(dir.path(), &request, &response).is_err());
        request.value["request_hash"] = Value::String(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        );
    }

    #[test]
    fn receipt_and_context_files_are_verified_against_response_bindings() {
        let dir = TempDir::new();
        let request = request_fixture(dir.path());
        let response = response_fixture(dir.path(), &request);
        assert!(validate_response(dir.path(), &request, &response).is_ok());
        let mut forged = response.clone();
        forged["receipt_ref"] = Value::String(
            "sha256:9999999999999999999999999999999999999999999999999999999999999999".to_owned(),
        );
        assert!(validate_response(dir.path(), &request, &forged).is_err());
        let mut mismatched_status = response_fixture(dir.path(), &request);
        mismatched_status["status"] = Value::String("pending_review".to_owned());
        assert!(validate_response(dir.path(), &request, &mismatched_status).is_err());
    }

    #[test]
    fn review_response_preserves_pending_status_and_receipt_binding() {
        let dir = TempDir::new();
        let mut request = request_fixture(dir.path());
        let packet = "sha256:9999999999999999999999999999999999999999999999999999999999999999";
        request.value["operation"] = Value::String("review".to_owned());
        request.value["review_packet_refs"] = json!([packet]);
        request.value["runtime"]["review_packet_refs"] = json!([packet]);
        request.gate_context.review_packet_refs = vec![packet.to_owned()];
        request.pre_admission = true;
        request.review_operation = true;
        request.previous_attempt_count = 0;
        let request_hash =
            fractal_contracts::canonical_sha256(&request_without_hash(&request.value)).unwrap();
        request.value["request_hash"] = Value::String(request_hash);
        let response = response_fixture_with_status(dir.path(), &request, "pending_review");
        let evidence = validate_response(dir.path(), &request, &response).unwrap();
        assert!(evidence.pending_review);

        let mut forged = response_fixture_with_status(dir.path(), &request, "pending_review");
        let receipt_ref = forged["receipt_ref"].as_str().unwrap().to_owned();
        let path = dir
            .path()
            .join(content_path(".fractal/node-receipts", &receipt_ref).unwrap());
        let mut receipt: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        receipt["status"] = Value::String("ready".to_owned());
        let receipt_bytes = fractal_contracts::canonical_json(&receipt).unwrap();
        let forged_receipt_ref = write_content(
            dir.path(),
            &content_path(".fractal/node-receipts", &digest(&receipt_bytes)).unwrap(),
            &receipt_bytes,
        );
        forged["receipt_ref"] = Value::String(forged_receipt_ref);
        assert!(validate_response(dir.path(), &request, &forged).is_err());
    }
}
