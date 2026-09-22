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
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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
pub(crate) const INTAKE_CAPABILITY: &str = "intelligence.measurement.intake";
pub(crate) const CHECK_CAPABILITY: &str = "intelligence.measurement.check";
const EFFECT_INTENT_SCHEMA: &str = "fractal.node_intelligence.effect_intent.v1";
const EFFECT_SNAPSHOT_SCHEMA: &str = "fractal.node_intelligence.effect_snapshot.v1";
const MATERIALIZATION_RECEIPT_SCHEMA: &str = "fractal.node_intelligence.materialization.v1";
const NETWORK_RESOLUTION_SCHEMA: &str = "fractal.node_intelligence.network_resolution.v1";
const EFFECT_INTENT_DIR: &str = ".fractal/node-intelligence-intents";
const EFFECT_SNAPSHOT_DIR: &str = ".fractal/node-intelligence-effects";
const EFFECT_RESOLUTION_DIR: &str = ".fractal/node-intelligence-resolutions";
const MATERIALIZED_TASK_DIR: &str = ".fractal/node-intelligence-materialized";
const RESOLVED_TASK_DIR: &str = ".fractal/node-intelligence-resolved";
const NETWORK_RESOLVER_ENV: &str = "FRACTAL_NODE_NETWORK_RESOLVER";
const FEEDBACK_POLICY_ENV: &str = "FRACTAL_NODE_FEEDBACK_POLICY";
const RESOLVED_TASK_SCHEMA: &str = "fractal.node_intelligence.resolved_task.v1";
const MAX_CONFIG_BYTES: u64 = 1_048_576;
const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_PRIVATE_RECEIPT_BYTES: usize = 256 * 1024;
const MAX_APPROVAL_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_HOST_POLICY_BYTES: u64 = 1_048_576;
const MAX_EFFECT_RECORD_BYTES: usize = 64 * 1024;
const MAX_EFFECT_RECORDS: usize = 4096;
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
    pub(crate) operation: String,
    pub(crate) effect: Option<Value>,
    /// Kept as an alias while existing analysis callers migrate to `effect`.
    pub(crate) analysis: Option<Value>,
    pub(crate) recovered: bool,
    pub(crate) intent_ref: Option<String>,
    pub(crate) current_attempt_number: Option<u32>,
    pub(crate) materialization_ref: Option<String>,
    pub(crate) network_resolution_ref: Option<String>,
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

/// The managed effect may outlive the Rust coordinator that launched it. This
/// immutable record is written before the child can execute so a later Rust
/// checkout can query the original attempt's durable action journal without
/// launching a second effect.
#[derive(Clone, Debug)]
struct EffectIntent {
    intent_ref: String,
    body: Value,
}

/// A validated predecessor output receipt passed to the deterministic workflow
/// materializer. It contains references only; artifact bodies remain in the
/// private contract store and are revalidated by the Python host bridge.
#[derive(Clone, Debug)]
pub(crate) struct EffectSnapshot {
    pub(crate) node_id: String,
    pub(crate) attempt_number: u32,
    pub(crate) scheduler_attempt_number: u32,
    pub(crate) attempt_ref: String,
    pub(crate) operation: String,
    pub(crate) receipt_ref: String,
    pub(crate) artifact_refs: Vec<String>,
    pub(crate) verified_outcome: Option<bool>,
    pub(crate) network_resolution_ref: Option<String>,
    pub(crate) snapshot_ref: String,
}

/// Materialized task configuration is an in-memory, host-validated overlay.
/// It is never written back into project configuration.
#[derive(Clone, Debug)]
pub(crate) struct MaterializedTask {
    pub(crate) node_id: String,
    pub(crate) attempt_number: u32,
    pub(crate) original_task_ref: String,
    pub(crate) task: Value,
    pub(crate) materialization_ref: String,
    pub(crate) producer_receipt_refs: Vec<String>,
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
    original_task: Option<Value>,
    materialization_ref: Option<String>,
    network_resolution_ref: Option<String>,
}

#[derive(Clone, Debug)]
struct ResolvedNetworkTask {
    node_id: String,
    graph_hash: String,
    attempt_number: u32,
    original_task_ref: String,
    task: Value,
    resolution_ref: String,
    resolver_config_ref: String,
    request_hash: String,
}

#[derive(Clone, Debug)]
struct NetworkResolverConfig {
    path: PathBuf,
    digest: String,
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
    recheck_gate_context: GateContext,
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
    intent_ref: Option<String>,
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
    prepare_effect(
        workspace,
        node_id,
        worker_id,
        invocation,
        "analysis",
        expected_review,
        deadline,
    )
}

pub(crate) fn prepare_effect(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    invocation: &crate::chain::jev_receipt::RouteInvocation,
    operation: &str,
    expected_review: Option<&ReviewBinding>,
    deadline: Option<Instant>,
) -> Result<BridgeEvidence> {
    let deadline = match deadline {
        Some(deadline) => Some(deadline),
        None => attempt_deadline(workspace, node_id)?,
    };
    let document = crate::project_file::load(workspace)?;
    let node = graph_node(&document.graph, node_id)?;
    validate_measurement_configuration(
        node,
        &task_configuration(workspace, node_id)?
            .context("measurement operation requires an enabled task")?
            .values,
        operation,
    )?;
    if let Some(intent) = pending_effect_intent(workspace, node_id, &document.graph_hash, false)? {
        let evidence = recover_effect(
            workspace, node_id, worker_id, invocation, operation, intent, deadline,
        )?;
        if let Some(expected) = expected_review {
            require_review_binding(&evidence, expected)?;
        }
        return Ok(evidence);
    }
    let preflight = prepare_for_operation_with_timeout(
        workspace,
        node_id,
        worker_id,
        invocation,
        "prepare",
        remaining_deadline(deadline)?,
    )?
    .context("measurement adapter requires an explicitly enabled task")?;
    if let Some(expected) = expected_review {
        require_review_binding(&preflight, expected)?;
    }
    let budget_started = Instant::now();
    let preflight_remaining = preflight
        .remaining_elapsed_ms
        .saturating_sub(budget_started.elapsed().as_millis() as u64)
        .min(remaining_deadline(deadline)?.map_or(u64::MAX, |value| value.as_millis() as u64));
    recheck_before_effect_with_budget(workspace, &preflight, worker_id, preflight_remaining)
        .with_context(|| format!("host gates changed after {operation} preflight"))?;
    let remaining = preflight
        .remaining_elapsed_ms
        .saturating_sub(budget_started.elapsed().as_millis() as u64)
        .min(remaining_deadline(deadline)?.map_or(u64::MAX, |value| value.as_millis() as u64));
    if remaining == 0 {
        bail!("node-intelligence elapsed budget is exhausted before local effect");
    }
    let effect = prepare_for_operation_with_intent(
        workspace,
        node_id,
        worker_id,
        invocation,
        operation,
        Some(Duration::from_millis(remaining)),
        Some(operation),
    )?
    .context("measurement task became disabled before its effect")?;
    save_effect_snapshot(
        workspace,
        &effect,
        effect
            .current_attempt_number
            .unwrap_or(effect.attempt_number),
    )?;
    Ok(effect)
}

fn recover_effect(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    invocation: &crate::chain::jev_receipt::RouteInvocation,
    requested_operation: &str,
    intent: EffectIntent,
    deadline: Option<Instant>,
) -> Result<BridgeEvidence> {
    let document = crate::project_file::load(workspace)?;
    let old_operation = intent
        .body
        .get("operation")
        .and_then(Value::as_str)
        .context("effect intent lacks operation")?;
    if old_operation != requested_operation {
        bail!("unresolved effect intent operation differs from the canonical node capability");
    }
    let old_number = intent
        .body
        .get("attempt_number")
        .and_then(Value::as_u64)
        .context("effect intent lacks attempt number")? as u32;
    let current_number = document
        .learning
        .nodes
        .get(node_id)
        .map(|record| record.attempt_count)
        .unwrap_or_default();
    if current_number <= old_number {
        bail!("effect recovery requires a later canonical checkout attempt");
    }
    let config =
        task_configuration(workspace, node_id)?.context("effect recovery task was disabled")?;
    let mut request = build_request(workspace, node_id, worker_id, invocation, config, "prepare")?;
    if request.attempt_number != current_number {
        bail!("effect recovery checkout attempt changed during request construction");
    }
    if request.config_digest != intent.body["config_digest"].as_str().unwrap_or_default()
        || request.host_policy_path.to_string_lossy()
            != intent.body["host_policy_path"].as_str().unwrap_or_default()
        || request.host_policy_digest
            != intent.body["host_policy_digest"]
                .as_str()
                .unwrap_or_default()
        || request.value["network_ref"] != intent.body["network_ref"]
        || request.value["capability_id"] != intent.body["capability_id"]
        || request.value["node_ref"] != intent.body["node_ref"]
        || request.value["model_ref"] != intent.body["model_ref"]
        || request.value["policy_ref"] != intent.body["policy_ref"]
        || request
            .value
            .pointer("/runtime/network_resolution_ref")
            .and_then(Value::as_str)
            != intent
                .body
                .get("network_resolution_ref")
                .and_then(Value::as_str)
        || request.gate_context.input_refs
            != intent.body["input_refs"]
                .as_array()
                .context("intent input refs are invalid")?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        || request.gate_context.handoff_refs
            != intent.body["handoff_refs"]
                .as_array()
                .context("intent handoff refs are invalid")?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        || request.gate_context.review_packet_refs
            != intent.body["review_packet_refs"]
                .as_array()
                .context("intent review refs are invalid")?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
    {
        bail!("current task, route pins, owner policy, or producer handoffs differ from the committed effect intent");
    }
    let project_id = request.value["project_id"]
        .as_str()
        .context("request project id is invalid")?
        .to_owned();
    let graph_hash = request.graph_hash.clone();
    let old_id = attempt_id(&project_id, &graph_hash, node_id, old_number);
    let old_attempt_ref = attempt_ref_for_request(&request.value, &old_id, old_number)?;
    if intent.body.get("attempt_ref").and_then(Value::as_str) != Some(old_attempt_ref.as_str())
        || intent.body.get("action_id").and_then(Value::as_str)
            != Some(measurement_action_id(&old_attempt_ref, old_operation)?.as_str())
    {
        bail!("effect intent attempt or action identity is invalid");
    }
    request.value["attempt"] = json!({"id": old_id, "node_id": node_id, "number": old_number});
    request.value["request_id"] = Value::String(format!(
        "node-intelligence-{}",
        digest_hex(format!("{}|{}|{}|{}", project_id, graph_hash, node_id, old_number).as_bytes())
    ));
    request.value["operation"] = Value::String("recover".to_owned());
    request.value["recovery"] = json!({
        "operation": old_operation,
        "intent_ref": intent.intent_ref,
        "current_attempt": {
            "id": attempt_id(&project_id, &graph_hash, node_id, current_number),
            "number": current_number,
        },
    });
    request.gate_context.attempt_ref = old_attempt_ref;
    request.intent_ref = Some(intent.intent_ref.clone());
    let expected_binding = intent
        .body
        .get("request_binding_hash")
        .and_then(Value::as_str)
        .context("effect intent lacks request binding")?;
    if request_binding_hash(&request.value)? != expected_binding {
        bail!("effect recovery request does not reproduce the original immutable action binding");
    }
    request
        .value
        .as_object_mut()
        .context("effect recovery request must be an object")?
        .remove("request_hash");
    let request_hash = fractal_contracts::canonical_sha256(&request.value)
        .map_err(|error| anyhow::anyhow!("hash effect recovery request: {error}"))?;
    request.value["request_hash"] = Value::String(request_hash);
    request.timeout = remaining_deadline(deadline)?
        .unwrap_or(request.timeout)
        .min(request.timeout);
    recheck_request_binding(workspace, &request)?;
    let encoded = serde_json::to_vec(&request.value).context("encode effect recovery request")?;
    if encoded.len() > MAX_REQUEST_BYTES {
        bail!("effect recovery request exceeds size limit");
    }
    let response = run_bridge_child(
        workspace,
        &encoded,
        request.timeout,
        &Launcher {
            program: PathBuf::from("python3"),
            args: vec!["-m".into(), "intelligence_graph.node_runtime".into()],
        },
    )?;
    let evidence = validate_response(workspace, &request, &response)?;
    if !evidence.recovered
        || evidence.operation != old_operation
        || evidence.intent_ref.as_deref() != Some(intent.intent_ref.as_str())
        || evidence
            .effect
            .as_ref()
            .and_then(|effect| effect.pointer("/action/state"))
            .and_then(Value::as_str)
            != Some("complete")
        || evidence
            .effect
            .as_ref()
            .and_then(|effect| effect.pointer("/collection/available"))
            .and_then(Value::as_bool)
            != Some(true)
    {
        bail!("durable measurement action is not complete and recoverable");
    }
    recheck_before_effect(workspace, &evidence, worker_id)
        .context("host authority changed while reconciling the committed effect")?;
    save_effect_snapshot(workspace, &evidence, current_number)?;
    Ok(evidence)
}

fn attempt_ref_for_request(request: &Value, id: &str, attempt_number: u32) -> Result<String> {
    let pin = json!({
        "schema": "fractal.node.attempt.v1",
        "id": id,
        "project_id": request["project_id"],
        "graph_ref": request["graph_hash"],
        "task_id": request.pointer("/attempt/node_id"),
        "network_ref": request["network_ref"],
        "capability_id": request["capability_id"],
        "node_ref": request["node_ref"],
        "model_ref": request["model_ref"],
        "policy_ref": request["policy_ref"],
        "input_refs": request["input_refs"],
        "memory_refs": request["memory_refs"],
        "evidence_refs": request["evidence_refs"],
    });
    let _ = attempt_number;
    fractal_contracts::canonical_sha256(&pin)
        .map_err(|error| anyhow::anyhow!("hash recovered attempt pin: {error}"))
}

/// True when an enabled task pins at least one packet whose human review must
/// be admitted before Rust checks out the node. Malformed config is an error,
/// so it cannot silently bypass this pre-admission boundary.
pub(crate) fn requires_review_admission(workspace: &Path, node_id: &str) -> Result<bool> {
    let document = crate::project_file::load(workspace)?;
    if pending_effect_intent(workspace, node_id, &document.graph_hash, false)?.is_some() {
        // The original attempt's approved packet is rechecked during
        // read-only effect recovery; asking for admission on the next attempt
        // would incorrectly bind that existing action to a fresh packet.
        return Ok(false);
    }
    let Some(configuration) = task_configuration(workspace, node_id)? else {
        return Ok(false);
    };
    if configuration
        .values
        .get("runtime")
        .and_then(|runtime| runtime.get("materialize_from"))
        .and_then(Value::as_array)
        .is_some_and(|refs| !refs.is_empty())
    {
        return Ok(true);
    }
    if configuration
        .values
        .get("runtime")
        .and_then(|runtime| runtime.get("network_resolver_config"))
        .is_some()
    {
        return Ok(true);
    }
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
    if let Some(raw) = task_configuration_raw(workspace, node_id)? {
        let current = crate::project_file::load(workspace)?;
        let pending_recovery =
            pending_effect_intent(workspace, node_id, &current.graph_hash, false)?;
        let producers = raw
            .values
            .get("runtime")
            .and_then(|runtime| runtime.get("materialize_from"))
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty());
        if producers && pending_recovery.is_none() {
            let ready = task_configuration(workspace, node_id)?
                .is_some_and(|configuration| configuration.materialization_ref.is_some());
            if !ready {
                materialize_task(workspace, node_id, worker_id, &raw)?;
            }
        }
        if pending_recovery.is_none() {
            let resolved = task_configuration(workspace, node_id)?
                .is_some_and(|configuration| configuration.network_resolution_ref.is_some());
            if !resolved
                && raw
                    .values
                    .get("runtime")
                    .and_then(|runtime| runtime.get("network_resolver_config"))
                    .is_some()
            {
                resolve_network_task(workspace, node_id, &raw)?;
            }
        }
    }
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

fn materialize_task(
    workspace: &Path,
    node_id: &str,
    _worker_id: &str,
    configuration: &TaskConfiguration,
) -> Result<MaterializedTask> {
    let original_task = configuration
        .original_task
        .as_ref()
        .context("materialization requires the original task config")?;
    let document = crate::project_file::load(workspace)?;
    let runtime = original_task
        .get("runtime")
        .and_then(Value::as_object)
        .context("materialized task runtime is missing")?;
    let declared = sorted_node_ids(runtime.get("materialize_from"), "materialize_from")?;
    if declared.is_empty() || declared.len() > 128 || declared.contains(&node_id.to_owned()) {
        bail!("materialize_from must name a bounded set of distinct predecessor nodes");
    }
    let project_id = format!("fractal:project:{}", document.project.slug);
    let policy = load_host_policy(workspace, node_id, &project_id, &document.graph_hash)?;
    if policy.task.get("materialize_from") != Some(&json!(declared)) {
        bail!("project materialize_from differs from the owner host policy");
    }
    for producer in &declared {
        if !graph_has_edge(&document.graph, producer, node_id) {
            bail!("materialize_from is not a canonical graph dependency");
        }
    }
    let previous_attempt = document
        .learning
        .nodes
        .get(node_id)
        .map(|record| record.attempt_count)
        .unwrap_or_default();
    if document
        .execution
        .as_ref()
        .and_then(|execution| execution.assignments.get(node_id))
        .is_some_and(|assignment| assignment.state == "checked_out")
    {
        bail!("task materialization must occur before checkout");
    }
    let attempt_number = previous_attempt
        .checked_add(1)
        .context("node attempt count is exhausted")?;
    let original_task_ref = fractal_contracts::canonical_sha256(original_task)
        .map_err(|error| anyhow::anyhow!("hash materialization template: {error}"))?;
    if let Some(existing) = load_materialized_task(
        workspace,
        node_id,
        &document.graph_hash,
        attempt_number,
        &original_task_ref,
    )? {
        return Ok(existing);
    }
    let mut producer_snapshots = Vec::with_capacity(declared.len());
    for producer in &declared {
        producer_snapshots.push(completed_effect_snapshot(workspace, &document, producer)?);
    }
    let producer_bindings = producer_snapshots
        .iter()
        .map(|snapshot| json!({ "node_id": snapshot.node_id, "receipt_ref": snapshot.receipt_ref }))
        .collect::<Vec<_>>();
    let graph_id = document
        .graph
        .get("graph_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .context("canonical graph lacks graph_id")?;
    let attempt = json!({
        "id": attempt_id(&project_id, &document.graph_hash, node_id, attempt_number),
        "node_id": node_id,
        "number": attempt_number,
    });
    let mut request = json!({
        "schema": "fractal.node_intelligence.materialize_request.v1",
        "operation": "materialize",
        "workspace": fs::canonicalize(workspace)?.to_string_lossy(),
        "project_id": project_id,
        "graph_id": graph_id,
        "graph_hash": document.graph_hash,
        "graph": document.graph,
        "attempt": attempt,
        "task": original_task,
        "producers": producer_bindings,
    });
    let request_hash = fractal_contracts::canonical_sha256(&request)
        .map_err(|error| anyhow::anyhow!("hash materialization request: {error}"))?;
    request["request_hash"] = Value::String(request_hash.clone());
    let bytes = serde_json::to_vec(&request).context("encode materialization request")?;
    if bytes.len() > MAX_REQUEST_BYTES {
        bail!("materialization request exceeds the bridge size bound");
    }
    let node = graph_node(&document.graph, node_id)?;
    let node_budget = node
        .pointer("/hard_limits/max_elapsed_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(configuration.timeout);
    let response = run_bridge_child(
        workspace,
        &bytes,
        configuration.timeout.min(node_budget),
        &Launcher {
            program: PathBuf::from("python3"),
            args: vec!["-m".into(), "intelligence_graph.node_runtime".into()],
        },
    )?;
    let object = response
        .as_object()
        .context("materializer response must be an object")?;
    if object.get("schema").and_then(Value::as_str)
        != Some("fractal.node_intelligence.materialize_response.v1")
        || object.get("request_hash").and_then(Value::as_str) != Some(request_hash.as_str())
        || object.get("attempt") != Some(&attempt)
        || object.get("original_task_ref").and_then(Value::as_str)
            != Some(original_task_ref.as_str())
        || object.get("status").and_then(Value::as_str) != Some("ready")
        || object.get("provider_calls").and_then(Value::as_u64) != Some(0)
    {
        bail!("materializer response is not a ready, correctly bound result");
    }
    let materialization_ref = object
        .get("materialization_ref")
        .and_then(Value::as_str)
        .filter(|reference| is_digest(reference))
        .context("materializer response lacks a valid receipt ref")?
        .to_owned();
    let mut task = object
        .get("task")
        .cloned()
        .context("materializer omitted task overlay")?;
    validate_materialized_task(
        workspace,
        original_task,
        &task,
        &producer_snapshots,
        &request,
        &materialization_ref,
    )?;
    let latest = crate::project_file::load(workspace)?;
    let latest_policy = load_host_policy(workspace, node_id, &project_id, &document.graph_hash)?;
    if latest.graph_hash != document.graph_hash
        || latest_policy.path != policy.path
        || latest_policy.digest != policy.digest
        || latest
            .learning
            .nodes
            .get(node_id)
            .map(|record| record.attempt_count)
            .unwrap_or_default()
            != previous_attempt
        || latest
            .execution
            .as_ref()
            .and_then(|execution| execution.assignments.get(node_id))
            .is_some_and(|assignment| assignment.state == "checked_out")
    {
        bail!("graph, attempt, or owner policy changed during task materialization");
    }
    for producer in &producer_snapshots {
        let current = completed_effect_snapshot(workspace, &latest, &producer.node_id)?;
        if current.receipt_ref != producer.receipt_ref
            || current.attempt_ref != producer.attempt_ref
            || current.artifact_refs != producer.artifact_refs
        {
            bail!("completed producer output changed during materialization");
        }
    }
    task["runtime"]["materialization_ref"] = Value::String(materialization_ref.clone());
    let materialized = MaterializedTask {
        node_id: node_id.to_owned(),
        attempt_number,
        original_task_ref,
        task,
        materialization_ref,
        producer_receipt_refs: producer_snapshots
            .into_iter()
            .map(|snapshot| snapshot.receipt_ref)
            .collect(),
    };
    save_materialized_task(workspace, &materialized, &document.graph_hash)?;
    Ok(materialized)
}

fn sorted_node_ids(value: Option<&Value>, field: &str) -> Result<Vec<String>> {
    let mut values = value
        .and_then(Value::as_array)
        .with_context(|| format!("{field} must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .with_context(|| format!("{field} has an invalid node id"))
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort();
    if values.is_empty() || values.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("{field} must contain distinct node ids");
    }
    Ok(values)
}

fn graph_has_edge(graph: &Value, from: &str, to: &str) -> bool {
    graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|edge| {
            edge.get("from").and_then(Value::as_str) == Some(from)
                && edge.get("to").and_then(Value::as_str) == Some(to)
        })
}

fn validate_materialized_task(
    workspace: &Path,
    original: &Value,
    task: &Value,
    producers: &[EffectSnapshot],
    request: &Value,
    materialization_ref: &str,
) -> Result<()> {
    let original_obj = original
        .as_object()
        .context("original materialization task is invalid")?;
    let task_obj = task.as_object().context("materialized task is invalid")?;
    let outputs = producers
        .iter()
        .flat_map(|producer| producer.artifact_refs.iter().cloned())
        .collect::<BTreeSet<_>>();
    let inputs = task
        .get("input_refs")
        .and_then(Value::as_array)
        .context("materialized inputs are invalid")?;
    if inputs
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        != outputs
    {
        bail!("materialized inputs do not equal committed producer outputs");
    }
    let mut normalized = task.clone();
    for field in ["input_refs", "handoff_refs", "review_packet_refs"] {
        normalized[field] = original_obj[field].clone();
    }
    let orig_runtime = original
        .get("runtime")
        .and_then(Value::as_object)
        .context("original runtime config is invalid")?;
    let runtime = normalized
        .get_mut("runtime")
        .and_then(Value::as_object_mut)
        .context("materialized runtime config is invalid")?;
    for field in ["handoff_refs", "review_packet_refs"] {
        runtime.insert(field.into(), orig_runtime[field].clone());
    }
    let original_auth = orig_runtime
        .get("authorization")
        .and_then(Value::as_object)
        .context("original auth is invalid")?;
    let auth = runtime
        .get_mut("authorization")
        .and_then(Value::as_object_mut)
        .context("materialized auth is invalid")?;
    if auth.get("current_sources") != original_auth.get("current_sources")
        || auth.get("authorized_permissions") != original_auth.get("authorized_permissions")
    {
        bail!("materializer changed source authority or permissions");
    }
    let expected_artifacts = original_auth["authorized_artifacts"]
        .as_array()
        .context("original authorized refs are invalid")?
        .iter()
        .chain(inputs.iter())
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("artifact ref is not a string")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let actual_artifacts = auth
        .get("authorized_artifacts")
        .and_then(Value::as_array)
        .context("materialized authorized refs are invalid")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("artifact ref is not a string")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if expected_artifacts != actual_artifacts {
        bail!("materializer added authorization outside producer outputs");
    }
    let current = auth
        .get("current_artifacts")
        .and_then(Value::as_object)
        .context("materialized current artifacts are invalid")?;
    let original_current = original_auth["current_artifacts"]
        .as_object()
        .context("original current artifacts are invalid")?;
    if original_current
        .iter()
        .any(|(key, value)| current.get(key) != Some(value))
        || current.values().any(|value| {
            !original_current.values().any(|old| old == value)
                && value
                    .as_str()
                    .is_none_or(|reference| !outputs.contains(reference))
        })
        || outputs
            .iter()
            .any(|output| !current.values().any(|value| value.as_str() == Some(output)))
    {
        bail!("materializer changed or omitted current artifact bindings");
    }
    normalized["runtime"]["authorization"] = orig_runtime["authorization"].clone();
    if &normalized != original
        || task_obj
            .get("runtime")
            .and_then(|runtime| runtime.get("materialization_ref"))
            .is_some()
    {
        bail!("materializer changed fields outside the producer-derived handoff delta");
    }
    let receipt_path = orig_runtime
        .get("receipt_path")
        .and_then(Value::as_str)
        .context("receipt path missing")?;
    let bytes = read_private_content(
        workspace,
        &content_path(receipt_path, materialization_ref)?,
        materialization_ref,
        MAX_PRIVATE_RECEIPT_BYTES,
    )?;
    let receipt = parse_unique_json(&bytes)?;
    let task_ref = fractal_contracts::canonical_sha256(task)
        .map_err(|error| anyhow::anyhow!("hash materialized task: {error}"))?;
    if receipt.get("schema").and_then(Value::as_str) != Some(MATERIALIZATION_RECEIPT_SCHEMA)
        || receipt.get("task_ref").and_then(Value::as_str) != Some(task_ref.as_str())
        || receipt.get("request_hash") != request.get("request_hash")
        || receipt.get("producers") != request.get("producers")
        || receipt.get("input_refs") != task.get("input_refs")
        || receipt.get("handoff_refs") != task.get("handoff_refs")
        || receipt.get("review_packet_refs") != task.get("review_packet_refs")
        || receipt.get("provider_calls").and_then(Value::as_u64) != Some(0)
    {
        bail!("materialization receipt does not bind the exact returned task");
    }
    Ok(())
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

fn save_effect_snapshot(
    workspace: &Path,
    evidence: &BridgeEvidence,
    scheduler_attempt_number: u32,
) -> Result<()> {
    let Some(effect) = evidence.effect.as_ref() else {
        return Ok(());
    };
    let action = effect
        .get("action")
        .context("node-intelligence effect lacks action receipt")?;
    let collection = effect
        .get("collection")
        .context("node-intelligence effect lacks collection receipt")?;
    if action.get("state").and_then(Value::as_str) != Some("complete")
        || collection.get("available").and_then(Value::as_bool) != Some(true)
    {
        return Ok(());
    }
    let artifact_refs = collection
        .get("artifact_refs")
        .and_then(Value::as_array)
        .context("effect collection lacks artifact refs")?
        .clone();
    let body = json!({
        "schema": EFFECT_SNAPSHOT_SCHEMA,
        "node_id": evidence.node_id,
        "graph_hash": evidence.graph_hash,
        "attempt_number": evidence.attempt_number,
        "scheduler_attempt_number": scheduler_attempt_number,
        "attempt_ref": evidence.attempt_ref,
        "operation": evidence.operation,
        "receipt_ref": evidence.receipt_ref,
        "artifact_refs": artifact_refs,
        "verified_outcome": effect.get("verified_outcome").cloned().unwrap_or(Value::Null),
        "context_manifest_ref": evidence.context_manifest_ref,
        "materialization_ref": evidence.materialization_ref,
        "network_resolution_ref": evidence.network_resolution_ref,
        "intent_ref": evidence.intent_ref,
        "recovered": evidence.recovered,
    });
    write_private_record(workspace, EFFECT_SNAPSHOT_DIR, &body)?;
    Ok(())
}

pub(crate) fn mark_effect_completed(workspace: &Path, evidence: &BridgeEvidence) -> Result<()> {
    if let Some(intent_ref) = evidence.intent_ref.as_deref() {
        resolve_effect_intent(
            workspace,
            intent_ref,
            evidence
                .current_attempt_number
                .unwrap_or(evidence.attempt_number),
        )?;
    }
    Ok(())
}

fn completed_effect_snapshot(
    workspace: &Path,
    document: &crate::project_file::FractalProject,
    node_id: &str,
) -> Result<EffectSnapshot> {
    let directory = safe_project_path(workspace, EFFECT_SNAPSHOT_DIR)?;
    let metadata = fs::symlink_metadata(&directory)
        .context("completed predecessor has no durable effect snapshots")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !private_metadata(&metadata) {
        bail!("node-intelligence effect snapshot store is unsafe");
    }
    let current_attempt = document
        .learning
        .nodes
        .get(node_id)
        .map(|record| record.attempt_count)
        .unwrap_or_default();
    let assignment = document
        .execution
        .as_ref()
        .and_then(|state| state.assignments.get(node_id))
        .context("materialization predecessor assignment is missing")?;
    if assignment.state != "completed" {
        bail!("materialization predecessor is not canonically complete");
    }
    let mut found = None;
    for entry in fs::read_dir(&directory)? {
        let name = entry?.file_name();
        let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        let reference = format!("sha256:{stem}");
        if !is_digest(&reference) {
            bail!("node-intelligence effect snapshot filename is invalid");
        }
        let value = read_private_record(workspace, EFFECT_SNAPSHOT_DIR, &reference)?;
        if value.get("schema").and_then(Value::as_str) != Some(EFFECT_SNAPSHOT_SCHEMA)
            || value.get("node_id").and_then(Value::as_str) != Some(node_id)
            || value.get("graph_hash").and_then(Value::as_str) != Some(document.graph_hash.as_str())
            || value
                .get("scheduler_attempt_number")
                .and_then(Value::as_u64)
                != Some(u64::from(current_attempt))
        {
            continue;
        }
        let artifact_refs = value
            .get("artifact_refs")
            .and_then(Value::as_array)
            .context("effect snapshot artifact refs are missing")?
            .iter()
            .map(|reference| {
                reference
                    .as_str()
                    .filter(|reference| is_digest(reference))
                    .map(str::to_owned)
                    .context("effect snapshot contains an invalid artifact ref")
            })
            .collect::<Result<Vec<_>>>()?;
        let snapshot = EffectSnapshot {
            node_id: node_id.to_owned(),
            attempt_number: value["attempt_number"].as_u64().unwrap_or_default() as u32,
            scheduler_attempt_number: current_attempt,
            attempt_ref: value["attempt_ref"]
                .as_str()
                .context("effect snapshot lacks attempt ref")?
                .to_owned(),
            operation: value["operation"]
                .as_str()
                .context("effect snapshot lacks operation")?
                .to_owned(),
            receipt_ref: value["receipt_ref"]
                .as_str()
                .filter(|reference| is_digest(reference))
                .context("effect snapshot lacks runtime receipt ref")?
                .to_owned(),
            artifact_refs,
            verified_outcome: value.get("verified_outcome").and_then(Value::as_bool),
            network_resolution_ref: value
                .get("network_resolution_ref")
                .and_then(Value::as_str)
                .map(str::to_owned),
            snapshot_ref: reference,
        };
        validate_effect_snapshot_receipt(workspace, document, &value, &snapshot)?;
        if found.is_some() {
            bail!("multiple effect snapshots bind the completed predecessor attempt");
        }
        found = Some(snapshot);
    }
    found.context("completed predecessor has no validated effect snapshot")
}

fn validate_effect_snapshot_receipt(
    workspace: &Path,
    document: &crate::project_file::FractalProject,
    snapshot_value: &Value,
    snapshot: &EffectSnapshot,
) -> Result<()> {
    if snapshot.snapshot_ref
        != fractal_contracts::canonical_sha256(snapshot_value)
            .map_err(|error| anyhow::anyhow!("hash effect snapshot: {error}"))?
        || snapshot.scheduler_attempt_number
            != document
                .learning
                .nodes
                .get(&snapshot.node_id)
                .map(|record| record.attempt_count)
                .unwrap_or_default()
        || snapshot.attempt_number == 0
        || snapshot.attempt_number > snapshot.scheduler_attempt_number
        || !matches!(snapshot.operation.as_str(), "analysis" | "intake" | "check")
        || snapshot_value
            .get("verified_outcome")
            .and_then(Value::as_bool)
            != snapshot.verified_outcome
        || snapshot_value
            .get("network_resolution_ref")
            .and_then(Value::as_str)
            .map(str::to_owned)
            != snapshot.network_resolution_ref
    {
        bail!("completed effect snapshot identity is inconsistent");
    }

    let recovered = snapshot_value
        .get("recovered")
        .and_then(Value::as_bool)
        .context("effect snapshot lacks its recovery status")?;
    let intent_ref = snapshot_value
        .get("intent_ref")
        .and_then(Value::as_str)
        .filter(|reference| is_digest(reference))
        .context("effect snapshot lacks a durable intent reference")?;
    let intent = read_private_record(workspace, EFFECT_INTENT_DIR, intent_ref)?;
    if intent.get("schema").and_then(Value::as_str) != Some(EFFECT_INTENT_SCHEMA)
        || intent.get("node_id").and_then(Value::as_str) != Some(snapshot.node_id.as_str())
        || intent.get("graph_hash").and_then(Value::as_str) != Some(document.graph_hash.as_str())
        || intent.get("attempt_number").and_then(Value::as_u64)
            != Some(u64::from(snapshot.attempt_number))
        || intent.get("attempt_ref").and_then(Value::as_str) != Some(snapshot.attempt_ref.as_str())
        || intent.get("operation").and_then(Value::as_str) != Some(snapshot.operation.as_str())
        || intent.get("materialization_ref") != snapshot_value.get("materialization_ref")
        || intent.get("network_resolution_ref") != snapshot_value.get("network_resolution_ref")
    {
        bail!("effect snapshot does not match its immutable intent");
    }

    let task = task_configuration_raw(workspace, &snapshot.node_id)?
        .context("completed producer task configuration is missing")?;
    let receipt_path = task
        .values
        .get("runtime")
        .and_then(|runtime| runtime.get("receipt_path"))
        .and_then(Value::as_str)
        .context("completed producer receipt path is missing")?;
    let receipt_bytes = read_private_content(
        workspace,
        &content_path(receipt_path, &snapshot.receipt_ref)?,
        &snapshot.receipt_ref,
        MAX_PRIVATE_RECEIPT_BYTES,
    )?;
    let receipt =
        parse_unique_json(&receipt_bytes).context("decode completed producer runtime receipt")?;
    let attempt = receipt
        .get("attempt")
        .and_then(Value::as_object)
        .context("completed producer receipt lacks attempt identity")?;
    let observed_attempt_id = attempt
        .get("id")
        .and_then(Value::as_str)
        .context("completed producer receipt lacks attempt id")?;
    let attempt_number = attempt
        .get("number")
        .and_then(Value::as_u64)
        .context("completed producer receipt lacks attempt number")?;
    if receipt.get("schema").and_then(Value::as_str) != Some(RECEIPT_SCHEMA)
        || receipt.get("status").and_then(Value::as_str) != Some("ready")
        || receipt.get("project_id") != intent.get("project_id")
        || receipt.get("graph_hash").and_then(Value::as_str) != Some(document.graph_hash.as_str())
        || receipt.get("attempt_ref").and_then(Value::as_str) != Some(snapshot.attempt_ref.as_str())
        || attempt.get("node_id").and_then(Value::as_str) != Some(snapshot.node_id.as_str())
        || attempt_number != u64::from(snapshot.attempt_number)
        || receipt.get("context_manifest_ref") != snapshot_value.get("context_manifest_ref")
        || receipt.get("materialization_ref") != snapshot_value.get("materialization_ref")
        || receipt
            .get("network_resolution_ref")
            .unwrap_or(&Value::Null)
            != snapshot_value
                .get("network_resolution_ref")
                .unwrap_or(&Value::Null)
        || receipt.get("network_ref") != intent.get("network_ref")
        || receipt.get("capability_id") != intent.get("capability_id")
        || receipt.get("node_ref") != intent.get("node_ref")
        || receipt.get("model_ref") != intent.get("model_ref")
        || receipt.get("policy_ref") != intent.get("policy_ref")
        || receipt.get("input_refs") != intent.get("input_refs")
        || receipt.get("memory_refs") != intent.get("memory_refs")
        || receipt.get("evidence_refs") != intent.get("evidence_refs")
        || receipt.get("handoff_refs") != intent.get("handoff_refs")
        || receipt.get("review_packet_refs") != intent.get("review_packet_refs")
    {
        bail!("completed producer runtime receipt does not match its pinned intent");
    }
    let expected_attempt_id = attempt_id(
        intent["project_id"]
            .as_str()
            .context("effect intent lacks project id")?,
        &document.graph_hash,
        &snapshot.node_id,
        snapshot.attempt_number,
    );
    if observed_attempt_id != expected_attempt_id {
        bail!("completed producer receipt attempt id is not canonical");
    }
    let attempt_ref =
        attempt_ref_for_request(&receipt, observed_attempt_id, snapshot.attempt_number)?;
    if attempt_ref != snapshot.attempt_ref {
        bail!("completed producer receipt pin does not reproduce its attempt ref");
    }
    let intent_action_id = intent
        .get("action_id")
        .and_then(Value::as_str)
        .context("effect intent lacks action id")?;
    if intent_action_id != measurement_action_id(&snapshot.attempt_ref, &snapshot.operation)? {
        bail!("effect intent action id does not match its operation and attempt");
    }
    let result = if recovered {
        receipt.get("recovery").filter(|value| {
            value.get("operation").and_then(Value::as_str) == Some(snapshot.operation.as_str())
        })
    } else {
        receipt.get(&snapshot.operation)
    }
    .context("completed producer receipt lacks its measured result")?;
    validate_measurement_result(result, &snapshot.attempt_ref, &snapshot.operation)?;
    if result
        .get("action")
        .and_then(|action| action.get("action_id"))
        .and_then(Value::as_str)
        != Some(intent_action_id)
        || result
            .get("action")
            .and_then(|action| action.get("state"))
            .and_then(Value::as_str)
            != Some("complete")
        || result
            .get("collection")
            .and_then(|collection| collection.get("available"))
            .and_then(Value::as_bool)
            != Some(true)
        || result
            .get("collection")
            .and_then(|collection| collection.get("artifact_refs"))
            != Some(&Value::Array(
                snapshot
                    .artifact_refs
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ))
        || result
            .get("verified_outcome")
            .cloned()
            .unwrap_or(Value::Null)
            != snapshot_value
                .get("verified_outcome")
                .cloned()
                .unwrap_or(Value::Null)
    {
        bail!("completed producer action does not match its durable effect snapshot");
    }
    if snapshot.operation == "check" && snapshot.verified_outcome != Some(true) {
        bail!(
            "measurement check without a positive verified result cannot produce workflow inputs"
        );
    }
    if !intent_is_resolved(workspace, intent_ref)? {
        // The graph transition is authoritative proof that the effect's node
        // outcome committed. Repair the small crash window between that
        // transition and the additive resolution pointer without re-running
        // the Python adapter.
        resolve_effect_intent(workspace, intent_ref, snapshot.scheduler_attempt_number)?;
    }
    Ok(())
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
    prepare_for_operation_with_intent(
        workspace,
        node_id,
        worker_id,
        invocation,
        operation,
        timeout_cap,
        None,
    )
}

fn prepare_for_operation_with_intent(
    workspace: &Path,
    node_id: &str,
    worker_id: &str,
    invocation: &crate::chain::jev_receipt::RouteInvocation,
    operation: &str,
    timeout_cap: Option<Duration>,
    intent_operation: Option<&str>,
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
    if let Some(intent_operation) = intent_operation {
        request.intent_ref = Some(write_effect_intent(workspace, &request, intent_operation)?);
    }
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
    if intent_operation.is_some() {
        save_effect_snapshot(workspace, &evidence, evidence.attempt_number)?;
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
    if remaining_elapsed_ms == 0 && !evidence.recovered {
        bail!("node-intelligence elapsed budget is exhausted before effect");
    }
    if !evidence.recovered {
        recheck_current_workflow_review(
            workspace,
            evidence,
            worker_id,
            Duration::from_millis(remaining_elapsed_ms.min(evidence.remaining_elapsed_ms)),
        )?;
    }
    recheck_current_binding(
        workspace,
        &evidence.node_id,
        worker_id,
        evidence
            .current_attempt_number
            .unwrap_or(evidence.attempt_number),
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
        &request.recheck_gate_context,
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
    if current_configuration.network_resolution_ref.is_some() {
        recheck_network_resolution(workspace, node_id, expected_attempt, &current_configuration)
            .context("pinned network is no longer authorized by the selected host resolver")?;
    } else if current_configuration
        .values
        .get("runtime")
        .and_then(|runtime| runtime.get("network_resolver_config"))
        .is_some()
    {
        bail!("selected network resolver has no durable pin for the current attempt");
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

/// Replay the resolver's immutable request for its existing attempt row. The
/// Python owner governor checks current policy/registry authority but returns
/// the saved pins; Rust requires the same request, receipt, and task overlay.
fn recheck_network_resolution(
    workspace: &Path,
    node_id: &str,
    current_attempt: u32,
    configuration: &TaskConfiguration,
) -> Result<()> {
    let document = crate::project_file::load(workspace)?;
    let project_id = format!("fractal:project:{}", document.project.slug);
    let original_task = configuration
        .original_task
        .as_ref()
        .context("resolved task lost its original project config")?;
    let configured_path = original_task
        .pointer("/runtime/network_resolver_config")
        .and_then(Value::as_str)
        .context("resolved task lacks its selected resolver path")?;
    let resolver_config = load_network_resolver_config(workspace, configured_path, &project_id)?;
    let attempt_number = pending_effect_intent(workspace, node_id, &document.graph_hash, false)?
        .and_then(|intent| intent.body.get("attempt_number").and_then(Value::as_u64))
        .map(|number| number as u32)
        .unwrap_or(current_attempt);
    let original_task_ref = fractal_contracts::canonical_sha256(original_task)
        .map_err(|error| anyhow::anyhow!("hash original network task: {error}"))?;
    let saved = load_resolved_network_task(
        workspace,
        node_id,
        &document.graph_hash,
        attempt_number,
        &original_task_ref,
        &resolver_config,
        original_task,
    )?
    .context("saved network-resolution pin is missing")?;
    if Some(saved.resolution_ref.as_str()) != configuration.network_resolution_ref.as_deref()
        || saved.task.as_object() != Some(&configuration.values)
    {
        bail!("saved network-resolution pins changed before effect");
    }
    let attempt = json!({
        "id": attempt_id(&project_id, &document.graph_hash, node_id, attempt_number),
        "node_id": node_id,
        "number": attempt_number,
    });
    let mut request = json!({
        "schema": "fractal.node_intelligence.resolve_request.v1",
        "operation": "resolve",
        "workspace": fs::canonicalize(workspace)?.to_string_lossy(),
        "project_id": project_id,
        "graph_hash": document.graph_hash,
        "attempt": attempt,
        "task": original_task,
    });
    let request_hash = fractal_contracts::canonical_sha256(&request)
        .map_err(|error| anyhow::anyhow!("hash network resolution request: {error}"))?;
    if request_hash != saved.request_hash {
        bail!("saved resolver request no longer matches the original attempt");
    }
    request["request_hash"] = Value::String(request_hash.clone());
    let bytes = serde_json::to_vec(&request).context("encode network-resolution recheck")?;
    let node = graph_node(&document.graph, node_id)?;
    let timeout = node
        .pointer("/hard_limits/max_elapsed_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(configuration.timeout)
        .min(configuration.timeout);
    let response = run_bridge_child(
        workspace,
        &bytes,
        timeout,
        &Launcher {
            program: PathBuf::from("python3"),
            args: vec!["-m".into(), "intelligence_graph.node_runtime".into()],
        },
    )?;
    let object = response
        .as_object()
        .context("network resolver recheck response must be an object")?;
    if object.get("schema").and_then(Value::as_str)
        != Some("fractal.node_intelligence.resolve_response.v1")
        || object.get("request_hash").and_then(Value::as_str) != Some(request_hash.as_str())
        || object.get("attempt") != Some(&attempt)
        || object.get("original_task_ref").and_then(Value::as_str)
            != Some(original_task_ref.as_str())
        || object.get("task") != Some(&saved.task)
        || object.get("resolution_ref").and_then(Value::as_str)
            != Some(saved.resolution_ref.as_str())
        || object.get("status").and_then(Value::as_str) != Some("ready")
        || object.get("provider_calls").and_then(Value::as_u64) != Some(0)
        || object.get("model_starts").and_then(Value::as_u64) != Some(0)
    {
        bail!("resolver no longer authorizes the exact saved network pin");
    }
    Ok(())
}

fn task_configuration(workspace: &Path, node_id: &str) -> Result<Option<TaskConfiguration>> {
    let Some(mut configuration) = task_configuration_raw(workspace, node_id)? else {
        return Ok(None);
    };
    let materialize_from = configuration
        .values
        .get("runtime")
        .and_then(|runtime| runtime.get("materialize_from"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !materialize_from.is_empty() {
        let document = crate::project_file::load(workspace)?;
        let predicted_attempt_number = document
            .learning
            .nodes
            .get(node_id)
            .map(|record| record.attempt_count)
            .unwrap_or_default()
            .saturating_add(u32::from(
                !document
                    .execution
                    .as_ref()
                    .and_then(|state| state.assignments.get(node_id))
                    .is_some_and(|assignment| assignment.state == "checked_out"),
            ));
        let attempt_number =
            pending_effect_intent(workspace, node_id, &document.graph_hash, false)?
                .and_then(|intent| intent.body.get("attempt_number").and_then(Value::as_u64))
                .map(|number| number as u32)
                .unwrap_or(predicted_attempt_number);
        let original_task = Value::Object(configuration.values.clone());
        let original_task_ref = fractal_contracts::canonical_sha256(&original_task)
            .map_err(|error| anyhow::anyhow!("hash original materialized task: {error}"))?;
        if let Some(materialized) = load_materialized_task(
            workspace,
            node_id,
            &document.graph_hash,
            attempt_number,
            &original_task_ref,
        )? {
            let declared = sorted_node_ids(
                Some(&Value::Array(materialize_from.clone())),
                "materialize_from",
            )?;
            if materialized.producer_receipt_refs.len() != declared.len() {
                bail!("materialized producer receipt set no longer matches its declaration");
            }
            for (producer, expected_receipt_ref) in
                declared.iter().zip(&materialized.producer_receipt_refs)
            {
                if !graph_has_edge(&document.graph, producer, node_id) {
                    bail!("materialized input producer is no longer a canonical graph dependency");
                }
                let current = completed_effect_snapshot(workspace, &document, producer)?;
                if current.receipt_ref != *expected_receipt_ref {
                    bail!("materialized producer receipt changed before receiver reuse");
                }
            }
            configuration.values = materialized
                .task
                .as_object()
                .context("materialized task must be an object")?
                .clone();
            configuration.original_task = Some(original_task);
            configuration.materialization_ref = Some(materialized.materialization_ref);
        } else {
            configuration.original_task = Some(original_task);
        }
    }
    let runtime = configuration
        .values
        .get("runtime")
        .and_then(Value::as_object)
        .context("enabled node-intelligence task runtime is missing")?;
    if let Some(configured_path) = runtime.get("network_resolver_config") {
        let configured_path = configured_path
            .as_str()
            .filter(|path| !path.trim().is_empty())
            .context("network_resolver_config must be a non-empty absolute path")?;
        if configuration.materialization_ref.is_some()
            || !materialize_from.is_empty()
            || configuration.values["handoff_refs"]
                .as_array()
                .is_some_and(|refs| !refs.is_empty())
            || configuration.values["review_packet_refs"]
                .as_array()
                .is_some_and(|refs| !refs.is_empty())
        {
            bail!("network resolver is limited to unbound root attempts without handoffs or review packets");
        }
        let document = crate::project_file::load(workspace)?;
        let project_id = format!("fractal:project:{}", document.project.slug);
        let owner_config = load_network_resolver_config(workspace, configured_path, &project_id)?;
        let original_task = configuration
            .original_task
            .as_ref()
            .context("network resolver requires the original project task")?;
        let original_task_ref = fractal_contracts::canonical_sha256(original_task)
            .map_err(|error| anyhow::anyhow!("hash original resolver task: {error}"))?;
        let current_attempt = document
            .learning
            .nodes
            .get(node_id)
            .map(|record| record.attempt_count)
            .unwrap_or_default();
        let pending_intent =
            pending_effect_intent(workspace, node_id, &document.graph_hash, false)?;
        let attempt_number = if let Some(intent) = pending_intent.as_ref() {
            intent
                .body
                .get("attempt_number")
                .and_then(Value::as_u64)
                .context("resolver recovery intent lacks attempt number")? as u32
        } else if document
            .execution
            .as_ref()
            .and_then(|state| state.assignments.get(node_id))
            .is_some_and(|assignment| assignment.state == "checked_out")
        {
            current_attempt
        } else {
            current_attempt
                .checked_add(1)
                .context("node attempt counter is exhausted")?
        };
        if let Some(resolved) = load_resolved_network_task(
            workspace,
            node_id,
            &document.graph_hash,
            attempt_number,
            &original_task_ref,
            &owner_config,
            original_task,
        )? {
            configuration.values = resolved
                .task
                .as_object()
                .context("resolved network task must be an object")?
                .clone();
            configuration.network_resolution_ref = Some(resolved.resolution_ref);
        } else if pending_intent.is_some() {
            bail!("committed effect recovery has no saved network-resolution pin");
        }
    }
    Ok(Some(configuration))
}

fn load_network_resolver_config(
    workspace: &Path,
    configured_path: &str,
    project_id: &str,
) -> Result<NetworkResolverConfig> {
    let selected = std::env::var(NETWORK_RESOLVER_ENV)
        .with_context(|| format!("network resolver requires {NETWORK_RESOLVER_ENV}"))?;
    if selected != configured_path {
        bail!("project resolver path does not match the operator-selected resolver config");
    }
    let path = PathBuf::from(&selected);
    if !path.is_absolute() {
        bail!("network resolver config path must be absolute");
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).with_context(|| {
            format!("inspect network resolver config path {}", current.display())
        })?;
        if metadata.file_type().is_symlink() {
            bail!("network resolver config path contains a symlink");
        }
    }
    let canonical_path = fs::canonicalize(&path)
        .with_context(|| format!("resolve network resolver config {}", path.display()))?;
    let canonical_workspace = fs::canonicalize(workspace).context("resolve managed workspace")?;
    if canonical_path.starts_with(&canonical_workspace) {
        bail!("network resolver config must live outside the project workspace");
    }
    let metadata = fs::symlink_metadata(&canonical_path)?;
    if !metadata.is_file() || metadata.len() > MAX_HOST_POLICY_BYTES || !private_metadata(&metadata)
    {
        bail!("network resolver config must be a bounded owner-private regular file");
    }
    let file = open_nofollow(&canonical_path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_HOST_POLICY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_HOST_POLICY_BYTES {
        bail!("network resolver config exceeds size limit");
    }
    let config = parse_unique_json(&bytes).context("decode network resolver config")?;
    let object = config
        .as_object()
        .context("network resolver config must be an object")?;
    let expected_keys = [
        "schema",
        "enabled",
        "project_id",
        "registry_path",
        "rollout_policy_path",
        "state_path",
        "baseline_model_refs",
        "head_artifact_roots",
        "laya_installation",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if object.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_keys
        || object.get("schema").and_then(Value::as_str) != Some(NETWORK_RESOLUTION_SCHEMA)
        || object.get("enabled").and_then(Value::as_bool) != Some(true)
        || object.get("project_id").and_then(Value::as_str) != Some(project_id)
    {
        bail!("network resolver config is not enabled for the current project");
    }
    for field in ["registry_path", "rollout_policy_path", "state_path"] {
        if object
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
        {
            bail!("network resolver config has an invalid {field}");
        }
    }
    let baselines = object
        .get("baseline_model_refs")
        .and_then(Value::as_array)
        .context("network resolver baseline model refs must be an array")?;
    if baselines.len() > 128
        || baselines
            .iter()
            .any(|value| value.as_str().is_none_or(|reference| !is_digest(reference)))
    {
        bail!("network resolver baseline model refs are invalid");
    }
    let artifact_roots = object
        .get("head_artifact_roots")
        .and_then(Value::as_array)
        .context("network resolver artifact roots must be an array")?;
    if artifact_roots.len() > 128
        || artifact_roots
            .iter()
            .any(|value| value.as_str().is_none_or(|root| root.trim().is_empty()))
    {
        bail!("network resolver artifact roots are invalid");
    }
    let digest = fractal_contracts::canonical_sha256(&config)
        .map_err(|error| anyhow::anyhow!("hash network resolver config: {error}"))?;
    Ok(NetworkResolverConfig {
        path: canonical_path,
        digest,
    })
}

fn validate_resolved_network_task(original: &Value, resolved: &Value) -> Result<()> {
    let original = original
        .as_object()
        .context("original resolver task must be an object")?;
    let resolved = resolved
        .as_object()
        .context("resolved task must be an object")?;
    if original.keys().collect::<BTreeSet<_>>() != resolved.keys().collect::<BTreeSet<_>>() {
        bail!("resolver changed the task schema");
    }
    for key in original.keys() {
        if !matches!(
            key.as_str(),
            "network_ref" | "node_ref" | "model_ref" | "policy_ref"
        ) && original.get(key) != resolved.get(key)
        {
            bail!("resolver changed a task field outside the four pinned network refs");
        }
    }
    for key in ["network_ref", "node_ref", "model_ref", "policy_ref"] {
        if resolved
            .get(key)
            .and_then(Value::as_str)
            .is_none_or(|reference| !is_digest(reference))
        {
            bail!("resolver returned an invalid {key}");
        }
    }
    if resolved
        .get("runtime")
        .and_then(Value::as_object)
        .is_some_and(|runtime| runtime.contains_key("network_resolution_ref"))
    {
        bail!("resolver cannot set its host-owned resolution receipt ref");
    }
    Ok(())
}

fn load_resolved_network_task(
    workspace: &Path,
    node_id: &str,
    graph_hash: &str,
    attempt_number: u32,
    original_task_ref: &str,
    resolver_config: &NetworkResolverConfig,
    original_task: &Value,
) -> Result<Option<ResolvedNetworkTask>> {
    let directory = safe_project_path(workspace, RESOLVED_TASK_DIR)?;
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !private_metadata(&metadata) {
        bail!("resolved network task store is unsafe");
    }
    let mut found = None;
    for entry in fs::read_dir(&directory)? {
        let name = entry?.file_name();
        let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        let record_ref = format!("sha256:{stem}");
        if !is_digest(&record_ref) {
            bail!("resolved network task filename is invalid");
        }
        let record = read_private_record(workspace, RESOLVED_TASK_DIR, &record_ref)?;
        if record.get("schema").and_then(Value::as_str) != Some(RESOLVED_TASK_SCHEMA) {
            bail!("resolved network task record schema mismatch");
        }
        if record.get("node_id").and_then(Value::as_str) != Some(node_id)
            || record.get("graph_hash").and_then(Value::as_str) != Some(graph_hash)
            || record.get("attempt_number").and_then(Value::as_u64)
                != Some(u64::from(attempt_number))
        {
            continue;
        }
        if record.get("original_task_ref").and_then(Value::as_str) != Some(original_task_ref)
            || record.get("resolver_config_path").and_then(Value::as_str)
                != Some(resolver_config.path.to_string_lossy().as_ref())
            || record.get("resolver_config_ref").and_then(Value::as_str)
                != Some(resolver_config.digest.as_str())
        {
            bail!("resolved network task no longer matches its original task or owner config");
        }
        let task = record
            .get("task")
            .cloned()
            .context("resolved network task record lacks task")?;
        validate_resolved_network_task(original_task, &task)?;
        let resolution_ref = record
            .get("resolution_ref")
            .and_then(Value::as_str)
            .filter(|reference| is_digest(reference))
            .context("resolved network task lacks receipt ref")?
            .to_owned();
        let request_hash = record
            .get("request_hash")
            .and_then(Value::as_str)
            .filter(|reference| is_digest(reference))
            .context("resolved network task lacks its original request hash")?
            .to_owned();
        let runtime = original_task
            .pointer("/runtime")
            .and_then(Value::as_object)
            .context("resolver task runtime is invalid")?;
        let receipt_root = runtime
            .get("receipt_path")
            .and_then(Value::as_str)
            .context("resolver task receipt path is missing")?;
        let receipt_path = content_path(receipt_root, &resolution_ref)?;
        let receipt_bytes = read_private_content(
            workspace,
            &receipt_path,
            &resolution_ref,
            MAX_PRIVATE_RECEIPT_BYTES,
        )?;
        let receipt =
            parse_unique_json(&receipt_bytes).context("decode saved network-resolution receipt")?;
        let document = crate::project_file::load(workspace)?;
        let project_id = format!("fractal:project:{}", document.project.slug);
        let expected_attempt = json!({
            "id": attempt_id(&project_id, graph_hash, node_id, attempt_number),
            "node_id": node_id,
            "number": attempt_number,
        });
        let expected_task_ref = fractal_contracts::canonical_sha256(&task)
            .map_err(|error| anyhow::anyhow!("hash resolved task: {error}"))?;
        if receipt.get("schema").and_then(Value::as_str) != Some(NETWORK_RESOLUTION_SCHEMA)
            || receipt.get("request_hash").and_then(Value::as_str) != Some(request_hash.as_str())
            || receipt.get("project_id").and_then(Value::as_str) != Some(project_id.as_str())
            || receipt.get("graph_hash").and_then(Value::as_str) != Some(graph_hash)
            || receipt.get("attempt") != Some(&expected_attempt)
            || receipt.get("config_ref").and_then(Value::as_str)
                != Some(resolver_config.digest.as_str())
            || receipt.get("original_task_ref").and_then(Value::as_str) != Some(original_task_ref)
            || receipt.get("task_ref").and_then(Value::as_str) != Some(expected_task_ref.as_str())
            || receipt.get("network_ref") != task.get("network_ref")
            || receipt.get("node_ref") != task.get("node_ref")
            || receipt.get("model_ref") != task.get("model_ref")
            || receipt.get("policy_ref") != task.get("policy_ref")
            || receipt.get("provider_calls").and_then(Value::as_u64) != Some(0)
            || receipt.get("model_starts").and_then(Value::as_u64) != Some(0)
        {
            bail!("network-resolution receipt does not bind its exact attempt and pins");
        }
        let resolved = ResolvedNetworkTask {
            node_id: node_id.to_owned(),
            graph_hash: graph_hash.to_owned(),
            attempt_number,
            original_task_ref: original_task_ref.to_owned(),
            task,
            resolution_ref,
            resolver_config_ref: resolver_config.digest.clone(),
            request_hash,
        };
        if found.replace(resolved).is_some() {
            bail!("multiple network-resolution overlays bind the same attempt");
        }
    }
    Ok(found)
}

fn resolve_network_task(
    workspace: &Path,
    node_id: &str,
    configuration: &TaskConfiguration,
) -> Result<ResolvedNetworkTask> {
    let original_task = configuration
        .original_task
        .as_ref()
        .context("network resolution requires original task config")?;
    let runtime = original_task
        .get("runtime")
        .and_then(Value::as_object)
        .context("network resolver runtime is missing")?;
    let configured_path = runtime
        .get("network_resolver_config")
        .and_then(Value::as_str)
        .context("network resolver config path is missing")?;
    if runtime
        .get("materialize_from")
        .and_then(Value::as_array)
        .is_some_and(|refs| !refs.is_empty())
        || configuration.values["handoff_refs"]
            .as_array()
            .is_some_and(|refs| !refs.is_empty())
        || configuration.values["review_packet_refs"]
            .as_array()
            .is_some_and(|refs| !refs.is_empty())
    {
        bail!("network resolution cannot change pins for a dependency or reviewed task");
    }
    let document = crate::project_file::load(workspace)?;
    let project_id = format!("fractal:project:{}", document.project.slug);
    let node = graph_node(&document.graph, node_id)?;
    let capability_id = required_string(&configuration.values, "capability_id")?;
    if node.get("capability").and_then(Value::as_str) != Some(capability_id.as_str()) {
        bail!("network resolver task capability differs from the canonical graph node");
    }
    if document
        .execution
        .as_ref()
        .and_then(|state| state.assignments.get(node_id))
        .is_some_and(|assignment| assignment.state == "checked_out")
    {
        bail!("network resolution must finish before managed checkout");
    }
    let previous_attempt = document
        .learning
        .nodes
        .get(node_id)
        .map(|record| record.attempt_count)
        .unwrap_or_default();
    let attempt_number = previous_attempt
        .checked_add(1)
        .context("node attempt counter is exhausted")?;
    let original_task_ref = fractal_contracts::canonical_sha256(original_task)
        .map_err(|error| anyhow::anyhow!("hash original network task: {error}"))?;
    let resolver_config = load_network_resolver_config(workspace, configured_path, &project_id)?;
    if let Some(existing) = load_resolved_network_task(
        workspace,
        node_id,
        &document.graph_hash,
        attempt_number,
        &original_task_ref,
        &resolver_config,
        original_task,
    )? {
        return Ok(existing);
    }
    let policy = load_host_policy(workspace, node_id, &project_id, &document.graph_hash)?;
    let host_task = policy
        .task
        .as_object()
        .context("host policy task entry must be an object")?;
    if !host_policy_authorization_matches(host_task, configuration, runtime)
        || host_task.get("authorized_approvers") != configuration.values.get("authorized_approvers")
        || host_task.get("qualified_reviewers") != configuration.values.get("qualified_reviewers")
        || !host_policy_memory_matches(host_task, runtime)
    {
        bail!("network resolution task does not match independent host authorization");
    }
    let graph_hash = document.graph_hash.clone();
    let attempt = json!({
        "id": attempt_id(&project_id, &graph_hash, node_id, attempt_number),
        "node_id": node_id,
        "number": attempt_number,
    });
    let mut request = json!({
        "schema": "fractal.node_intelligence.resolve_request.v1",
        "operation": "resolve",
        "workspace": fs::canonicalize(workspace)?.to_string_lossy(),
        "project_id": project_id,
        "graph_hash": graph_hash,
        "attempt": attempt,
        "task": original_task,
    });
    let request_hash = fractal_contracts::canonical_sha256(&request)
        .map_err(|error| anyhow::anyhow!("hash network resolution request: {error}"))?;
    request["request_hash"] = Value::String(request_hash.clone());
    let bytes = serde_json::to_vec(&request).context("encode network resolution request")?;
    if bytes.len() > MAX_REQUEST_BYTES {
        bail!("network resolution request exceeds the bounded JSON contract");
    }
    let node_budget = node
        .pointer("/hard_limits/max_elapsed_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(configuration.timeout);
    let response = run_bridge_child(
        workspace,
        &bytes,
        configuration.timeout.min(node_budget),
        &Launcher {
            program: PathBuf::from("python3"),
            args: vec!["-m".into(), "intelligence_graph.node_runtime".into()],
        },
    )?;
    let object = response
        .as_object()
        .context("network resolver response must be an object")?;
    let response_keys = [
        "schema",
        "request_hash",
        "attempt",
        "original_task_ref",
        "task",
        "resolution_ref",
        "status",
        "provider_calls",
        "model_starts",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if object.keys().map(String::as_str).collect::<BTreeSet<_>>() != response_keys
        || object.get("schema").and_then(Value::as_str)
            != Some("fractal.node_intelligence.resolve_response.v1")
        || object.get("request_hash").and_then(Value::as_str) != Some(request_hash.as_str())
        || object.get("attempt") != Some(&attempt)
        || object.get("original_task_ref").and_then(Value::as_str)
            != Some(original_task_ref.as_str())
        || object.get("status").and_then(Value::as_str) != Some("ready")
        || object.get("provider_calls").and_then(Value::as_u64) != Some(0)
        || object.get("model_starts").and_then(Value::as_u64) != Some(0)
    {
        bail!("network resolver response is not ready or correctly bound");
    }
    let task = object
        .get("task")
        .cloned()
        .context("network resolver omitted its pinned task")?;
    validate_resolved_network_task(original_task, &task)?;
    let resolution_ref = object
        .get("resolution_ref")
        .and_then(Value::as_str)
        .filter(|reference| is_digest(reference))
        .context("network resolver omitted a valid receipt ref")?
        .to_owned();
    let receipt_path = runtime
        .get("receipt_path")
        .and_then(Value::as_str)
        .context("network resolver runtime receipt path is missing")?;
    let receipt_bytes = read_private_content(
        workspace,
        &content_path(receipt_path, &resolution_ref)?,
        &resolution_ref,
        MAX_PRIVATE_RECEIPT_BYTES,
    )?;
    let receipt = parse_unique_json(&receipt_bytes).context("decode network resolution receipt")?;
    let task_ref = fractal_contracts::canonical_sha256(&task)
        .map_err(|error| anyhow::anyhow!("hash resolved network task: {error}"))?;
    if receipt.get("schema").and_then(Value::as_str) != Some(NETWORK_RESOLUTION_SCHEMA)
        || receipt.get("request_hash").and_then(Value::as_str) != Some(request_hash.as_str())
        || receipt.get("project_id").and_then(Value::as_str) != Some(project_id.as_str())
        || receipt.get("graph_hash").and_then(Value::as_str) != Some(graph_hash.as_str())
        || receipt.get("attempt") != Some(&attempt)
        || receipt.get("config_ref").and_then(Value::as_str)
            != Some(resolver_config.digest.as_str())
        || receipt.get("original_task_ref").and_then(Value::as_str)
            != Some(original_task_ref.as_str())
        || receipt.get("task_ref").and_then(Value::as_str) != Some(task_ref.as_str())
        || receipt.get("network_ref") != task.get("network_ref")
        || receipt.get("node_ref") != task.get("node_ref")
        || receipt.get("model_ref") != task.get("model_ref")
        || receipt.get("policy_ref") != task.get("policy_ref")
        || receipt.get("provider_calls").and_then(Value::as_u64) != Some(0)
        || receipt.get("model_starts").and_then(Value::as_u64) != Some(0)
    {
        bail!("network resolution receipt does not bind the selected task pins");
    }
    let current_document = crate::project_file::load(workspace)?;
    let current_raw = task_configuration_raw(workspace, node_id)?
        .context("resolver task was disabled while resolving network")?;
    let current_original = current_raw
        .original_task
        .as_ref()
        .context("resolver task lost its original config")?;
    let current_policy = load_host_policy(workspace, node_id, &project_id, &graph_hash)?;
    let current_config = load_network_resolver_config(workspace, configured_path, &project_id)?;
    if current_document.graph_hash != graph_hash
        || current_document
            .learning
            .nodes
            .get(node_id)
            .map(|record| record.attempt_count)
            .unwrap_or_default()
            != previous_attempt
        || current_document
            .execution
            .as_ref()
            .and_then(|state| state.assignments.get(node_id))
            .is_some_and(|assignment| assignment.state == "checked_out")
        || fractal_contracts::canonical_sha256(current_original)
            .map_err(|error| anyhow::anyhow!("hash current resolver task: {error}"))?
            != original_task_ref
        || current_policy.path != policy.path
        || current_policy.digest != policy.digest
        || current_config.path != resolver_config.path
        || current_config.digest != resolver_config.digest
    {
        bail!("graph, attempt, task config, or owner policy changed during network resolution");
    }
    let resolved = ResolvedNetworkTask {
        node_id: node_id.to_owned(),
        graph_hash,
        attempt_number,
        original_task_ref,
        task,
        resolution_ref,
        resolver_config_ref: resolver_config.digest,
        request_hash,
    };
    save_resolved_network_task(workspace, &resolved, &resolver_config.path)?;
    Ok(resolved)
}

fn save_resolved_network_task(
    workspace: &Path,
    resolved: &ResolvedNetworkTask,
    resolver_config_path: &Path,
) -> Result<String> {
    write_private_record(
        workspace,
        RESOLVED_TASK_DIR,
        &json!({
            "schema": RESOLVED_TASK_SCHEMA,
            "node_id": resolved.node_id,
            "graph_hash": resolved.graph_hash,
            "attempt_number": resolved.attempt_number,
            "original_task_ref": resolved.original_task_ref,
            "task": resolved.task,
            "resolution_ref": resolved.resolution_ref,
            "request_hash": resolved.request_hash,
            "resolver_config_path": resolver_config_path.to_string_lossy(),
            "resolver_config_ref": resolved.resolver_config_ref,
        }),
    )
}

fn task_configuration_raw(workspace: &Path, node_id: &str) -> Result<Option<TaskConfiguration>> {
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
    let original_task = Value::Object(merged.clone());
    if merged
        .get("runtime")
        .and_then(Value::as_object)
        .is_some_and(|runtime| {
            runtime.contains_key("materialization_ref")
                || runtime.contains_key("network_resolution_ref")
        })
    {
        bail!("project node-intelligence config cannot set host materialization or network-resolution refs");
    }
    Ok(Some(TaskConfiguration {
        values: merged,
        timeout: Duration::from_millis(timeout_ms),
        original_task: Some(original_task),
        materialization_ref: None,
        network_resolution_ref: None,
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
    if !matches!(
        operation,
        "prepare" | "analysis" | "intake" | "check" | "review" | "recover"
    ) {
        bail!("unsupported node-intelligence operation");
    }
    if matches!(operation, "analysis" | "intake" | "check") {
        validate_measurement_configuration(node, &configuration.values, operation)?;
    }
    if let Some(configured) = fields.get("operation").and_then(Value::as_str) {
        let effect_preflight = operation == "prepare"
            && matches!(configured, "analysis" | "intake" | "check")
            && configured
                == node
                    .get("capability")
                    .and_then(Value::as_str)
                    .and_then(|capability| match capability {
                        ANALYSIS_CAPABILITY => Some("analysis"),
                        INTAKE_CAPABILITY => Some("intake"),
                        CHECK_CAPABILITY => Some("check"),
                        _ => None,
                    })
                    .unwrap_or_default()
            && runtime
                .get(configured)
                .and_then(|effect| effect.get("enabled"))
                .and_then(Value::as_bool)
                == Some(true);
        let review_preflight = operation == "review"
            && matches!(configured, "prepare" | "analysis" | "intake" | "check");
        if configured != operation && !effect_preflight && !review_preflight {
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
    if !host_policy_authorization_matches(host_task, &configuration, runtime)
        || host_task.get("authorized_approvers") != fields.get("authorized_approvers")
        || host_task.get("qualified_reviewers") != fields.get("qualified_reviewers")
        || host_task.get("materialize_from") != runtime.get("materialize_from")
        || !host_policy_memory_matches(host_task, runtime)
    {
        bail!("project node-intelligence grants do not match the independent host policy");
    }
    let mut runtime_value = runtime.clone();
    if let Some(network_resolution_ref) = configuration.network_resolution_ref.as_ref() {
        runtime_value.insert(
            "network_resolution_ref".to_owned(),
            Value::String(network_resolution_ref.clone()),
        );
    } else if runtime.contains_key("network_resolution_ref") {
        bail!("project node-intelligence config cannot set network_resolution_ref");
    } else if runtime.contains_key("network_resolver_config") {
        bail!("operator-selected network resolver has no saved resolution pin for this attempt");
    }
    if let Some(materialization_ref) = configuration.materialization_ref.as_ref() {
        runtime_value.insert(
            "materialization_ref".to_owned(),
            Value::String(materialization_ref.clone()),
        );
    } else if runtime.contains_key("materialize_from") {
        bail!("producer-bound node task has not been materialized for this attempt");
    }
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
        recheck_gate_context: gate_context.clone(),
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
        intent_ref: None,
    })
}

fn host_policy_authorization_matches(
    host_task: &Map<String, Value>,
    configuration: &TaskConfiguration,
    effective_runtime: &Map<String, Value>,
) -> bool {
    let Some(original_runtime) = configuration
        .original_task
        .as_ref()
        .and_then(|task| task.get("runtime"))
        .and_then(Value::as_object)
    else {
        return false;
    };
    if host_task.get("authorization") != original_runtime.get("authorization") {
        return false;
    }
    if configuration.materialization_ref.is_none() {
        return effective_runtime.get("authorization") == original_runtime.get("authorization");
    }
    let Some(original_auth) = original_runtime
        .get("authorization")
        .and_then(Value::as_object)
    else {
        return false;
    };
    let Some(effective_auth) = effective_runtime
        .get("authorization")
        .and_then(Value::as_object)
    else {
        return false;
    };
    if effective_auth.get("current_sources") != original_auth.get("current_sources")
        || effective_auth.get("authorized_permissions")
            != original_auth.get("authorized_permissions")
    {
        return false;
    }
    let Some(original_refs) = original_auth
        .get("authorized_artifacts")
        .and_then(Value::as_array)
    else {
        return false;
    };
    let Some(effective_refs) = effective_auth
        .get("authorized_artifacts")
        .and_then(Value::as_array)
    else {
        return false;
    };
    let Some(input_refs) = configuration
        .values
        .get("input_refs")
        .and_then(Value::as_array)
    else {
        return false;
    };
    let expected = original_refs
        .iter()
        .chain(input_refs)
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let actual = effective_refs
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    if expected != actual {
        return false;
    }
    effective_auth
        .get("current_artifacts")
        .and_then(Value::as_object)
        .zip(
            original_auth
                .get("current_artifacts")
                .and_then(Value::as_object),
        )
        .is_some_and(|(effective, original)| {
            original
                .iter()
                .all(|(key, value)| effective.get(key) == Some(value))
                && effective.values().all(|value| {
                    original.values().any(|old| old == value)
                        || value.as_str().is_some_and(|reference| {
                            input_refs
                                .iter()
                                .any(|input| input.as_str() == Some(reference))
                        })
                })
        })
}

fn task_config_digest(configuration: &TaskConfiguration) -> Result<String> {
    let value = json!({
        "task": configuration.values,
        "timeout_ms": configuration.timeout.as_millis(),
        "materialization_ref": configuration.materialization_ref,
        "network_resolution_ref": configuration.network_resolution_ref,
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
    let materialization_task_keys = [
        "authorization",
        "authorized_approvers",
        "qualified_reviewers",
        "materialize_from",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let memory_materialization_task_keys = [
        "authorization",
        "authorized_approvers",
        "qualified_reviewers",
        "memory",
        "materialize_from",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if task_keys != legacy_task_keys
        && task_keys != memory_task_keys
        && task_keys != materialization_task_keys
        && task_keys != memory_materialization_task_keys
    {
        bail!("node-intelligence host policy task entry has an invalid schema");
    }
    if task_object.contains_key("materialize_from") {
        let ids = sorted_node_ids(
            task_object.get("materialize_from"),
            "host_policy.materialize_from",
        )?;
        if task_object.get("materialize_from") != Some(&json!(ids)) {
            bail!("host policy materialize_from must be sorted and unique");
        }
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

fn validate_measurement_configuration(
    node: &Value,
    fields: &Map<String, Value>,
    operation: &str,
) -> Result<Duration> {
    let capability = match operation {
        "analysis" => ANALYSIS_CAPABILITY,
        "intake" => INTAKE_CAPABILITY,
        "check" => CHECK_CAPABILITY,
        _ => bail!("unsupported deterministic measurement operation"),
    };
    if node.get("capability").and_then(Value::as_str) != Some(capability) {
        bail!("{operation} is allowed only for its canonical measurement capability");
    }
    if operation == "analysis" {
        return validate_analysis_configuration(node, fields);
    }
    if node
        .pointer("/hard_limits/max_calls")
        .and_then(Value::as_u64)
        .is_none_or(|calls| calls < 1)
    {
        bail!("{operation} node has no remaining call budget");
    }
    let max_elapsed_ms = node
        .pointer("/hard_limits/max_elapsed_ms")
        .and_then(Value::as_u64)
        .context("measurement node has no elapsed-time budget")?;
    let runtime = fields
        .get("runtime")
        .and_then(Value::as_object)
        .context("task runtime is missing")?;
    let effect = runtime
        .get(operation)
        .and_then(Value::as_object)
        .with_context(|| format!("{operation} task requires runtime.{operation}"))?;
    if effect.get("enabled").and_then(Value::as_bool) != Some(true) {
        bail!("runtime.{operation}.enabled must be true");
    }
    let database_path = effect
        .get("database_path")
        .and_then(Value::as_str)
        .context("measurement task requires a database_path")?;
    if safe_project_path(Path::new("/workspace"), database_path).is_err() {
        bail!("measurement database_path must be project-relative");
    }
    for field in ["output_permission_ref", "output_retention_ref"] {
        let value = effect
            .get(field)
            .and_then(Value::as_str)
            .with_context(|| format!("runtime.{operation} requires {field}"))?;
        if !is_digest(value) {
            bail!("measurement output rights must be sha256 refs");
        }
    }
    let permission = effect["output_permission_ref"].as_str().unwrap_or_default();
    let granted = runtime
        .get("authorization")
        .and_then(|value| value.get("authorized_permissions"))
        .and_then(Value::as_array)
        .context("measurement output rights need host authorization")?;
    if !granted
        .iter()
        .any(|value| value.as_str() == Some(permission))
    {
        bail!("measurement output permission is not host-authorized");
    }
    let timeout_ms = effect
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(2_000);
    if !(100..=5_000).contains(&timeout_ms) || timeout_ms > max_elapsed_ms {
        bail!("measurement timeout must fit the node's bounded elapsed-time limit");
    }
    Ok(Duration::from_millis(timeout_ms))
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
        NETWORK_RESOLVER_ENV,
        FEEDBACK_POLICY_ENV,
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
    let request_operation = request_value
        .get("operation")
        .and_then(Value::as_str)
        .context("node-intelligence request lacks operation")?;
    let effect_operation = if request_operation == "recover" {
        request_value
            .pointer("/recovery/operation")
            .and_then(Value::as_str)
            .context("recovery request lacks original operation")?
    } else {
        request_operation
    };
    let effect_field = if request_operation == "recover" {
        "recovery"
    } else {
        effect_operation
    };
    let effect = if matches!(effect_operation, "analysis" | "intake" | "check") {
        let result = object
            .get(effect_field)
            .context("node-intelligence response lacks typed effect status")?;
        validate_measurement_result(result, &attempt_ref, effect_operation)?;
        Some(result.clone())
    } else {
        for field in ["analysis", "intake", "check", "recovery"] {
            if !object.get(field).is_some_and(Value::is_null) {
                bail!("non-effect node-intelligence response unexpectedly contains {field}");
            }
        }
        None
    };
    if request_operation == "recover" {
        let recovery = object
            .get("recovery")
            .and_then(Value::as_object)
            .context("recovery response lacks recovery binding")?;
        if recovery.get("operation").and_then(Value::as_str) != Some(effect_operation)
            || recovery.get("intent_ref") != request_value.pointer("/recovery/intent_ref")
            || recovery.get("current_attempt") != request_value.pointer("/recovery/current_attempt")
        {
            bail!("recovery response does not match the Rust intent and current attempt");
        }
    }
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
        || receipt.get("intake") != object.get("intake")
        || receipt.get("check") != object.get("check")
        || receipt.get("recovery") != object.get("recovery")
        || receipt
            .get("network_resolution_ref")
            .unwrap_or(&Value::Null)
            != request_value
                .pointer("/runtime/network_resolution_ref")
                .unwrap_or(&Value::Null)
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
        operation: effect_operation.to_owned(),
        effect: effect.clone(),
        analysis: (effect_operation == "analysis")
            .then(|| effect.clone())
            .flatten(),
        recovered: request_operation == "recover",
        intent_ref: if request_operation == "recover" {
            request_value
                .pointer("/recovery/intent_ref")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else {
            request.intent_ref.clone()
        },
        current_attempt_number: if request_operation == "recover" {
            request_value
                .pointer("/recovery/current_attempt/number")
                .and_then(Value::as_u64)
                .map(|value| value as u32)
        } else {
            None
        },
        materialization_ref: request_value
            .pointer("/runtime/materialization_ref")
            .and_then(Value::as_str)
            .map(str::to_owned),
        network_resolution_ref: request_value
            .pointer("/runtime/network_resolution_ref")
            .and_then(Value::as_str)
            .map(str::to_owned),
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
        gate_context: request.recheck_gate_context.clone(),
        node_id: request.node_id.clone(),
        worker_id: request.worker_id.clone(),
        attempt_number: request
            .value
            .pointer("/attempt/number")
            .and_then(Value::as_u64)
            .context("node-intelligence response binding lacks attempt number")?
            as u32,
        graph_hash: request.graph_hash.clone(),
        config_digest: request.config_digest.clone(),
        host_policy_path: request.host_policy_path.clone(),
        pre_admission: request.pre_admission,
    })
}

fn validate_measurement_result(
    value: &Value,
    expected_attempt_ref: &str,
    operation: &str,
) -> Result<()> {
    let action = value
        .get("action")
        .context("measurement result lacks action receipt")?;
    let collection = value
        .get("collection")
        .context("measurement result lacks collection status")?;
    let state = action
        .get("state")
        .and_then(Value::as_str)
        .context("measurement action state is missing")?;
    let action_id = measurement_action_id(expected_attempt_ref, operation)?;
    if action.get("action_id").and_then(Value::as_str) != Some(action_id.as_str())
        || action.get("attempt_ref").and_then(Value::as_str) != Some(expected_attempt_ref)
        || collection.get("action_id").and_then(Value::as_str) != Some(action_id.as_str())
    {
        bail!("measurement action receipt is bound to a different attempt");
    }
    let checked_outcome = value.get("verified_outcome");
    let action_outcome = action.get("verified_outcome");
    let outcome_valid = match operation {
        "check" => {
            checked_outcome.is_some_and(Value::is_boolean) && action_outcome == checked_outcome
        }
        "analysis" | "intake" => {
            checked_outcome.is_some_and(Value::is_null)
                && action_outcome.is_some_and(Value::is_null)
        }
        _ => false,
    };
    if !matches!(
        state,
        "running" | "complete" | "failed" | "timed_out" | "cancelled" | "unknown"
    ) || collection
        .get("available")
        .and_then(Value::as_bool)
        .is_none()
        || !outcome_valid
        || action
            .get("receipt_ref")
            .and_then(Value::as_str)
            .is_none_or(|reference| !is_digest(reference))
    {
        bail!("measurement result violates the typed local action contract");
    }
    let refs = action
        .get("artifact_refs")
        .and_then(Value::as_array)
        .context("measurement action artifact refs are missing")?;
    if refs.iter().any(|reference| {
        reference
            .as_str()
            .is_none_or(|reference| !is_digest(reference))
    }) {
        bail!("measurement action contains an invalid artifact ref");
    }
    if let Some(input_ref) = action.get("input_ref") {
        if !input_ref.is_null() && input_ref.as_str().is_none_or(|value| !is_digest(value)) {
            bail!("measurement action contains an invalid input ref");
        }
    }
    let receipt_ref = action["receipt_ref"].as_str().expect("validated above");
    let mut action_without_receipt_ref = action.clone();
    action_without_receipt_ref
        .as_object_mut()
        .context("measurement action receipt must be an object")?
        .remove("receipt_ref");
    if fractal_contracts::canonical_sha256(&action_without_receipt_ref)
        .map_err(|error| anyhow::anyhow!("hash measurement action receipt: {error}"))?
        != receipt_ref
    {
        bail!("measurement action receipt hash mismatch");
    }
    let collection_refs = collection
        .get("artifact_refs")
        .and_then(Value::as_array)
        .context("measurement collection artifact refs are missing")?;
    if collection_refs.iter().any(|reference| {
        reference
            .as_str()
            .is_none_or(|reference| !is_digest(reference))
    }) {
        bail!("measurement collection contains an invalid artifact ref");
    }
    if collection.get("artifact_refs") != action.get("artifact_refs") {
        bail!("measurement collection does not bind the action's artifact refs");
    }
    if collection.get("available").and_then(Value::as_bool) == Some(true) && state != "complete" {
        bail!("measurement collection claims availability for a non-complete action");
    }
    let usage = value
        .get("usage")
        .and_then(Value::as_object)
        .context("measurement usage receipt is missing")?;
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
        bail!("measurement usage receipt is not bound to the local zero-provider adapter");
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
            bail!("measurement usage receipt contains an invalid quantity");
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

fn ensure_private_store(workspace: &Path, relative: &str) -> Result<PathBuf> {
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
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!("node-intelligence private store contains an unsafe path component");
                }
                if index + 1 == components.len() && !private_metadata(&metadata) {
                    bail!("node-intelligence private store is not owner-private");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if index + 1 != components.len() {
                    bail!("node-intelligence private store parent is missing");
                }
                fs::create_dir(&current).with_context(|| {
                    format!(
                        "create node-intelligence private store {}",
                        current.display()
                    )
                })?;
                #[cfg(unix)]
                fs::set_permissions(&current, fs::Permissions::from_mode(0o700))?;
                let metadata = fs::symlink_metadata(&current)?;
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                    || !private_metadata(&metadata)
                {
                    bail!("created node-intelligence private store is not owner-private");
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn write_private_record(workspace: &Path, relative_dir: &str, value: &Value) -> Result<String> {
    let bytes = fractal_contracts::canonical_json(value)
        .context("encode canonical node-intelligence private record")?;
    if bytes.len() > MAX_EFFECT_RECORD_BYTES {
        bail!("node-intelligence private record exceeds size limit");
    }
    let reference = hash_bytes(&bytes);
    let directory = ensure_private_store(workspace, relative_dir)?;
    let path = directory.join(format!("{}.json", &reference[7..]));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::File::open(&directory)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let rel = Path::new(relative_dir).join(format!("{}.json", &reference[7..]));
            let existing = read_private_content(
                workspace,
                &rel.to_string_lossy(),
                &reference,
                MAX_EFFECT_RECORD_BYTES,
            )?;
            if existing != bytes {
                bail!("node-intelligence private record digest collision");
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("write node-intelligence private record {}", path.display())
            })
        }
    }
    Ok(reference)
}

fn read_private_record(workspace: &Path, relative_dir: &str, reference: &str) -> Result<Value> {
    if !is_digest(reference) {
        bail!("node-intelligence private record reference is invalid");
    }
    let rel = Path::new(relative_dir).join(format!("{}.json", &reference[7..]));
    let bytes = read_private_content(
        workspace,
        &rel.to_string_lossy(),
        reference,
        MAX_EFFECT_RECORD_BYTES,
    )?;
    parse_unique_json(&bytes).context("decode node-intelligence private record")
}

fn save_materialized_task(
    workspace: &Path,
    materialized: &MaterializedTask,
    graph_hash: &str,
) -> Result<String> {
    let body = json!({
        "schema": "fractal.node_intelligence.materialized_task.v1",
        "node_id": materialized.node_id,
        "graph_hash": graph_hash,
        "attempt_number": materialized.attempt_number,
        "original_task_ref": materialized.original_task_ref,
        "task": materialized.task,
        "materialization_ref": materialized.materialization_ref,
        "producer_receipt_refs": materialized.producer_receipt_refs,
    });
    write_private_record(workspace, MATERIALIZED_TASK_DIR, &body)
}

fn load_materialized_task(
    workspace: &Path,
    node_id: &str,
    graph_hash: &str,
    attempt_number: u32,
    original_task_ref: &str,
) -> Result<Option<MaterializedTask>> {
    let directory = safe_project_path(workspace, MATERIALIZED_TASK_DIR)?;
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !private_metadata(&metadata) {
        bail!("node-intelligence materialized-task store is unsafe");
    }
    let mut found = None;
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let reference = format!("sha256:{stem}");
        if !is_digest(&reference) {
            bail!("node-intelligence materialized-task filename is invalid");
        }
        let value = read_private_record(workspace, MATERIALIZED_TASK_DIR, &reference)?;
        if value.get("schema").and_then(Value::as_str)
            != Some("fractal.node_intelligence.materialized_task.v1")
        {
            bail!("node-intelligence materialized-task schema mismatch");
        }
        if value.get("node_id").and_then(Value::as_str) != Some(node_id)
            || value.get("graph_hash").and_then(Value::as_str) != Some(graph_hash)
            || value.get("attempt_number").and_then(Value::as_u64)
                != Some(u64::from(attempt_number))
            || value.get("original_task_ref").and_then(Value::as_str) != Some(original_task_ref)
        {
            continue;
        }
        let task = value
            .get("task")
            .cloned()
            .context("materialized task record lacks task")?;
        let materialization_ref = value
            .get("materialization_ref")
            .and_then(Value::as_str)
            .filter(|reference| is_digest(reference))
            .context("materialized task record lacks a valid receipt ref")?
            .to_owned();
        let producer_receipt_refs = value
            .get("producer_receipt_refs")
            .and_then(Value::as_array)
            .context("materialized task record lacks producer receipt refs")?
            .iter()
            .map(|reference| {
                reference
                    .as_str()
                    .filter(|reference| is_digest(reference))
                    .map(str::to_owned)
                    .context("materialized task producer receipt ref is invalid")
            })
            .collect::<Result<Vec<_>>>()?;
        if found.is_some() {
            bail!("multiple materialized task records match the current attempt");
        }
        found = Some(MaterializedTask {
            node_id: node_id.to_owned(),
            attempt_number,
            original_task_ref: original_task_ref.to_owned(),
            task,
            materialization_ref,
            producer_receipt_refs,
        });
    }
    Ok(found)
}

fn request_binding_hash(value: &Value) -> Result<String> {
    let mut binding = value.clone();
    let object = binding
        .as_object_mut()
        .context("node-intelligence request must be an object")?;
    object.remove("request_hash");
    object.remove("operation");
    object.remove("recovery");
    fractal_contracts::canonical_sha256(&binding)
        .map_err(|error| anyhow::anyhow!("hash node-intelligence effect binding: {error}"))
}

fn measurement_action_id(attempt_ref: &str, operation: &str) -> Result<String> {
    let adapter_operation = match operation {
        "analysis" => "measurement-analyze",
        "intake" => "measurement-intake",
        "check" => "measurement-check",
        _ => bail!("unsupported measurement effect operation"),
    };
    let body = json!({ "attempt_ref": attempt_ref, "operation": adapter_operation });
    let encoded = fractal_contracts::canonical_json(&body)
        .context("encode deterministic measurement action identity")?;
    Ok(format!("fractal:action:{}", digest_hex(&encoded)))
}

fn effect_intent_body(request: &BridgeRequest, operation: &str) -> Result<Value> {
    let value = &request.value;
    let attempt_ref = &request.gate_context.attempt_ref;
    let action_id = measurement_action_id(attempt_ref, operation)?;
    Ok(json!({
        "schema": EFFECT_INTENT_SCHEMA,
        "project_id": value["project_id"],
        "graph_id": value["graph_id"],
        "graph_hash": request.graph_hash,
        "node_id": request.node_id,
        "operation": operation,
        "attempt_number": request.attempt_number,
        "attempt_ref": attempt_ref,
        "action_id": action_id,
        "request_binding_hash": request_binding_hash(value)?,
        "config_digest": request.config_digest,
        "host_policy_path": request.host_policy_path.to_string_lossy(),
        "host_policy_digest": request.host_policy_digest,
        "network_ref": value["network_ref"],
        "capability_id": value["capability_id"],
        "node_ref": value["node_ref"],
        "model_ref": value["model_ref"],
        "policy_ref": value["policy_ref"],
        "input_refs": request.gate_context.input_refs,
        "memory_refs": value["memory_refs"],
        "evidence_refs": value["evidence_refs"],
        "handoff_refs": request.gate_context.handoff_refs,
        "review_packet_refs": request.gate_context.review_packet_refs,
        "approved_gate_refs": value.pointer("/runtime/approved_gate_refs").cloned().unwrap_or_else(|| json!([])),
        "materialization_ref": value.pointer("/runtime/materialization_ref").cloned().unwrap_or(Value::Null),
        "network_resolution_ref": value.pointer("/runtime/network_resolution_ref").cloned().unwrap_or(Value::Null),
    }))
}

fn write_effect_intent(
    workspace: &Path,
    request: &BridgeRequest,
    operation: &str,
) -> Result<String> {
    let body = effect_intent_body(request, operation)?;
    write_private_record(workspace, EFFECT_INTENT_DIR, &body)
}

fn intent_is_resolved(workspace: &Path, intent_ref: &str) -> Result<bool> {
    let dir = safe_project_path(workspace, EFFECT_RESOLUTION_DIR)?;
    let path = dir.join(format!("{}.json", &intent_ref[7..]));
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || !private_metadata(&metadata)
            {
                bail!("node-intelligence resolution record is unsafe");
            }
            let file = open_nofollow(&path)?;
            let mut bytes = Vec::new();
            file.take(MAX_EFFECT_RECORD_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_EFFECT_RECORD_BYTES {
                bail!("node-intelligence resolution pointer exceeds size limit");
            }
            let pointer = parse_unique_json(&bytes)?;
            if pointer.get("schema").and_then(Value::as_str)
                != Some("fractal.node_intelligence.effect_resolution_pointer.v1")
                || pointer.get("intent_ref").and_then(Value::as_str) != Some(intent_ref)
            {
                bail!("node-intelligence resolution record binding mismatch");
            }
            let resolution_ref = pointer
                .get("resolution_ref")
                .and_then(Value::as_str)
                .filter(|reference| is_digest(reference))
                .context("resolution pointer ref is invalid")?;
            let value = read_private_record(workspace, EFFECT_RESOLUTION_DIR, resolution_ref)?;
            if value.get("schema").and_then(Value::as_str)
                != Some("fractal.node_intelligence.effect_resolution.v1")
                || value.get("intent_ref").and_then(Value::as_str) != Some(intent_ref)
            {
                bail!("effect resolution does not bind its intent");
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn resolve_effect_intent(
    workspace: &Path,
    intent_ref: &str,
    completion_attempt: u32,
) -> Result<()> {
    let intent = read_private_record(workspace, EFFECT_INTENT_DIR, intent_ref)?;
    let completion = json!({
        "schema": "fractal.node_intelligence.effect_resolution.v1",
        "intent_ref": intent_ref,
        "node_id": intent.get("node_id"),
        "graph_hash": intent.get("graph_hash"),
        "effect_attempt_ref": intent.get("attempt_ref"),
        "completion_attempt": completion_attempt,
    });
    let resolution_ref = write_private_record(workspace, EFFECT_RESOLUTION_DIR, &completion)?;
    let directory = ensure_private_store(workspace, EFFECT_RESOLUTION_DIR)?;
    let path = directory.join(format!("{}.json", &intent_ref[7..]));
    let pointer = json!({
        "schema": "fractal.node_intelligence.effect_resolution_pointer.v1",
        "intent_ref": intent_ref,
        "resolution_ref": resolution_ref,
    });
    let bytes = fractal_contracts::canonical_json(&pointer)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::File::open(&directory)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !intent_is_resolved(workspace, intent_ref)? {
                return Err(error.into());
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn pending_effect_intent(
    workspace: &Path,
    node_id: &str,
    graph_hash: &str,
    completed: bool,
) -> Result<Option<EffectIntent>> {
    let directory = safe_project_path(workspace, EFFECT_INTENT_DIR)?;
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !private_metadata(&metadata) {
        bail!("node-intelligence effect-intent store is unsafe");
    }
    let current_document = crate::project_file::load(workspace)?;
    let current_project_id = format!("fractal:project:{}", current_document.project.slug);
    let mut found = Vec::new();
    for entry in fs::read_dir(&directory)? {
        if found.len() >= MAX_EFFECT_RECORDS {
            bail!("node-intelligence effect-intent store exceeds record limit");
        }
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".json") {
            continue;
        }
        let stem = name.strip_suffix(".json").unwrap_or_default();
        let intent_ref = format!("sha256:{stem}");
        if !is_digest(&intent_ref) {
            bail!("node-intelligence effect-intent filename is invalid");
        }
        let body = read_private_record(workspace, EFFECT_INTENT_DIR, &intent_ref)?;
        if body.get("schema").and_then(Value::as_str) != Some(EFFECT_INTENT_SCHEMA) {
            bail!("node-intelligence effect-intent schema mismatch");
        }
        if body.get("node_id").and_then(Value::as_str) == Some(node_id)
            && body.get("project_id").and_then(Value::as_str) == Some(current_project_id.as_str())
            && !intent_is_resolved(workspace, &intent_ref)?
        {
            if body.get("graph_hash").and_then(Value::as_str) != Some(graph_hash) {
                bail!("an unresolved effect intent belongs to a different graph; operator reconciliation is required");
            }
            let body_attempt = body
                .get("attempt_number")
                .and_then(Value::as_u64)
                .context("effect intent lacks attempt number")?;
            if completed {
                resolve_effect_intent(workspace, &intent_ref, body_attempt as u32)?;
            } else {
                found.push(EffectIntent { intent_ref, body });
            }
        }
        let _ = path;
    }
    found.sort_by_key(|intent| intent.body["attempt_number"].as_u64().unwrap_or_default());
    if found.len() > 1 {
        bail!("multiple unresolved node-intelligence effects require operator reconciliation");
    }
    Ok(found.pop())
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

    fn resolver_task() -> Value {
        json!({
            "network_ref": digest(b"network-0"),
            "capability_id": "intelligence.measurement.analyze",
            "node_ref": digest(b"node-0"),
            "model_ref": digest(b"model-0"),
            "policy_ref": digest(b"policy-0"),
            "input_refs": [digest(b"input-0")],
            "handoff_refs": [],
            "review_packet_refs": [],
            "public_features": {"sample_count": 3},
            "runtime": {
                "network_resolver_config": "/tmp/operator/network.json",
                "authorization": {"authorized_artifacts": [digest(b"input-0")]}
            }
        })
    }

    #[test]
    fn network_resolution_overlay_only_changes_the_four_pinned_refs() {
        let original = resolver_task();
        let mut resolved = original.clone();
        resolved["network_ref"] = json!(digest(b"network-1"));
        resolved["node_ref"] = json!(digest(b"node-1"));
        resolved["model_ref"] = json!(digest(b"model-1"));
        resolved["policy_ref"] = json!(digest(b"policy-1"));
        validate_resolved_network_task(&original, &resolved).unwrap();

        let mut changed_input = resolved.clone();
        changed_input["input_refs"] = json!([digest(b"other-input")]);
        assert!(validate_resolved_network_task(&original, &changed_input).is_err());

        let mut changed_grant = resolved.clone();
        changed_grant["runtime"]["authorization"]["authorized_artifacts"] =
            json!([digest(b"other-input")]);
        assert!(validate_resolved_network_task(&original, &changed_grant).is_err());

        let mut invalid_pin = resolved;
        invalid_pin["model_ref"] = json!("sha256:not-a-digest");
        assert!(validate_resolved_network_task(&original, &invalid_pin).is_err());
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

    #[test]
    fn unresolved_effect_from_an_old_graph_blocks_relaunch() {
        let dir = TempDir::new();
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_effect_intent_recovery",
            "nodes": [{"id": "analysis", "capability": ANALYSIS_CAPABILITY}],
            "edges": []
        });
        graph["graph_hash"] = json!(fractal_contracts::canonical_sha256(&graph).unwrap());
        crate::project_file::persist(dir.path(), &graph, "Effect intent recovery fixture").unwrap();
        let document = crate::project_file::load(dir.path()).unwrap();
        ensure_private_store(dir.path(), EFFECT_INTENT_DIR).unwrap();
        write_private_record(
            dir.path(),
            EFFECT_INTENT_DIR,
            &json!({
                "schema": EFFECT_INTENT_SCHEMA,
                "project_id": format!("fractal:project:{}", document.project.slug),
                "graph_hash": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "node_id": "analysis",
                "attempt_number": 1
            }),
        )
        .unwrap();

        let error =
            pending_effect_intent(dir.path(), "analysis", &document.graph_hash, false).unwrap_err();
        assert!(error.to_string().contains("different graph"));
    }

    fn request_fixture(workspace: &Path) -> BridgeRequest {
        let mut request = json!({
            "schema": REQUEST_SCHEMA,
            "operation": "prepare",
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
        let gate_context = GateContext {
            project_id: "fractal:project:fixture".to_owned(),
            graph_hash: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .to_owned(),
            node_id: "build".to_owned(),
            attempt_ref: "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                .to_owned(),
            network_ref: "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                .to_owned(),
            plan_ref: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .to_owned(),
            input_refs: Vec::new(),
            handoff_refs: Vec::new(),
            review_packet_refs: Vec::new(),
        };
        BridgeRequest {
            value: request,
            recheck_gate_context: gate_context.clone(),
            gate_context,
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
            intent_ref: None,
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
            "intake": null,
            "check": null,
            "recovery": null,
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
            "intake": null,
            "check": null,
            "recovery": null,
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
