//! Runtime enforcement for immutable node `policy_contract` values.
//!
//! Compilation is the authority boundary: every executable node carries a
//! versioned, hashed contract.  This module deliberately does not resolve a
//! policy from the repository at execution time.  It parses the contract that
//! is already part of the committed graph, validates its relationship to the
//! graph/node identity, and produces bounded evidence suitable for the
//! learning/failure projections.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::harness_policy::NODE_POLICY_CONTRACT_SCHEMA;

const REPORT_SCHEMA: &str = "fractal.policy_enforcement_report.v1";
const SHA256_PREFIX: &str = "sha256:";
const MAX_SNAPSHOT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Compact limits copied from a node contract.  Values are intentionally
/// integer-only and are never inferred from prompts or environment variables.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct EffectiveLimits {
    pub(crate) max_steps: u64,
    pub(crate) max_minutes: u64,
    pub(crate) max_attempts: u64,
    pub(crate) max_files_changed: u64,
    pub(crate) max_diff_lines: u64,
    #[serde(default)]
    pub(crate) max_input_tokens: u64,
    #[serde(default)]
    pub(crate) max_output_tokens: u64,
    #[serde(default)]
    pub(crate) max_cost_usd: u64,
}

impl EffectiveLimits {
    #[allow(dead_code)]
    pub(crate) fn timeout_ms(&self) -> u64 {
        self.max_minutes.saturating_mul(60_000)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct EffectiveNetwork {
    pub(crate) default: String,
    #[serde(default)]
    pub(crate) allowed_destinations: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EffectivePolicy {
    pub(crate) schema: String,
    pub(crate) policy_hash: String,
    pub(crate) provenance: String,
    pub(crate) capability: String,
    pub(crate) sandbox_profile: String,
    pub(crate) allowed_writes: Vec<String>,
    pub(crate) allowed_commands: Vec<String>,
    pub(crate) network: EffectiveNetwork,
    pub(crate) limits: EffectiveLimits,
    pub(crate) verifier_ids: Vec<String>,
    pub(crate) evidence_requirements: Vec<String>,
    pub(crate) external_side_effects: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ControlStatus {
    Enforced,
    Prevented,
    Detected,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PolicyViolation {
    pub(crate) kind: String,
    /// A normalized relative path or a digest.  Absolute paths are never
    /// stored in reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) digest: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PolicyEnforcementReport {
    pub(crate) schema: String,
    pub(crate) policy_hash: String,
    pub(crate) controls: BTreeMap<String, ControlStatus>,
    pub(crate) limits: EffectiveLimits,
    /// Sanitized provider identity from the read-only `--version` probe.  The
    /// version is evidence, not authority: eligibility still requires the
    /// provider's documented controls to match the immutable contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) violations: Vec<PolicyViolation>,
    pub(crate) pre_snapshot_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) post_snapshot_hash: Option<String>,
    pub(crate) evidence_hash: String,
    pub(crate) report_hash: String,
}

impl PolicyEnforcementReport {
    fn new(policy: &EffectivePolicy, pre_snapshot_hash: String) -> Self {
        let mut controls = BTreeMap::new();
        controls.insert("contract".to_owned(), ControlStatus::Enforced);
        controls.insert("policy_hash".to_owned(), ControlStatus::Enforced);
        controls.insert("capability".to_owned(), ControlStatus::Enforced);
        controls.insert("sandbox".to_owned(), ControlStatus::Enforced);
        controls.insert("network".to_owned(), ControlStatus::Enforced);
        controls.insert("approval".to_owned(), ControlStatus::Enforced);
        controls.insert("command_allowlist".to_owned(), ControlStatus::Enforced);
        controls.insert("workspace_paths".to_owned(), ControlStatus::Enforced);
        controls.insert("file_bounds".to_owned(), ControlStatus::Enforced);
        controls.insert("diff_bounds".to_owned(), ControlStatus::Enforced);
        controls.insert("secret_paths".to_owned(), ControlStatus::Enforced);
        controls.insert("symlink_escape".to_owned(), ControlStatus::Enforced);
        controls.insert("environment".to_owned(), ControlStatus::Enforced);
        controls.insert("provider_route".to_owned(), ControlStatus::Enforced);
        Self {
            schema: REPORT_SCHEMA.to_owned(),
            policy_hash: policy.policy_hash.clone(),
            controls,
            limits: policy.limits.clone(),
            provider_version: None,
            provider_reason: None,
            violations: Vec::new(),
            pre_snapshot_hash,
            post_snapshot_hash: None,
            evidence_hash: String::new(),
            report_hash: String::new(),
        }
    }

    pub(crate) fn compact_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({"schema": REPORT_SCHEMA}))
    }

    fn finalize(&mut self) {
        let mut body = self.compact_value();
        if let Some(object) = body.as_object_mut() {
            object.remove("report_hash");
        }
        self.report_hash = sha256_value(&body);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PolicyFailure {
    Denied,
    CapabilityMismatch,
    InvalidContract,
    InvalidHash,
    ExternalSideEffect,
    BudgetExceeded,
    ProviderUnavailable,
    PathViolation,
    FileBounds,
    DiffBounds,
    SymlinkEscape,
    SecretPath,
}

impl PolicyFailure {
    pub(crate) fn failure_code(&self) -> crate::learning_data::FailureCode {
        match self {
            Self::BudgetExceeded | Self::FileBounds | Self::DiffBounds => {
                crate::learning_data::FailureCode::BudgetExceeded
            }
            Self::InvalidContract | Self::InvalidHash | Self::CapabilityMismatch => {
                crate::learning_data::FailureCode::InvalidOutputSchema
            }
            Self::Denied
            | Self::ExternalSideEffect
            | Self::ProviderUnavailable
            | Self::PathViolation
            | Self::SymlinkEscape
            | Self::SecretPath => crate::learning_data::FailureCode::ToolFailure,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PolicyError {
    pub(crate) failure: PolicyFailure,
    pub(crate) report: Option<PolicyEnforcementReport>,
    pub(crate) message: String,
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PolicyError {}

#[derive(Clone, Debug, Default)]
pub(crate) struct WorkspaceSnapshot {
    files: BTreeMap<String, SnapshotEntry>,
    pub(crate) digest: String,
}

impl WorkspaceSnapshot {
    pub(crate) fn has_symlink_escape(&self) -> bool {
        self.files.values().any(|entry| {
            entry.kind == SnapshotKind::Symlink && entry.digest == sha256_bytes(b"symlink-escape")
        })
    }
}

#[derive(Clone, Debug)]
struct SnapshotEntry {
    kind: SnapshotKind,
    digest: String,
    bytes: Option<Vec<u8>>,
    target_digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotKind {
    File,
    Symlink,
}

#[derive(Clone, Debug)]
pub(crate) struct Postflight {
    pub(crate) report: PolicyEnforcementReport,
    pub(crate) failure: Option<PolicyError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderEligibility {
    pub(crate) status: ControlStatus,
    pub(crate) reason: Option<String>,
    /// Control-level evidence is kept separate from the aggregate route
    /// status so reports can say exactly which provider guarantee is missing.
    pub(crate) controls: BTreeMap<String, ControlStatus>,
    pub(crate) version: Option<String>,
}

/// Parse and validate one immutable node contract.  This deliberately rejects
/// missing fields rather than falling back to the repository policy.
#[allow(clippy::result_large_err)]
pub(crate) fn parse_contract(
    node: &Value,
    graph: Option<&Value>,
) -> Result<EffectivePolicy, PolicyError> {
    let contract = node.get("policy_contract").ok_or_else(|| {
        policy_error(
            PolicyFailure::InvalidContract,
            "node has no immutable policy_contract",
            None,
        )
    })?;
    let object = contract.as_object().ok_or_else(|| {
        policy_error(
            PolicyFailure::InvalidContract,
            "policy_contract must be an object",
            None,
        )
    })?;
    let schema = required_string(object, "schema")
        .map_err(|message| policy_error(PolicyFailure::InvalidContract, message, None))?;
    if schema != NODE_POLICY_CONTRACT_SCHEMA {
        return Err(policy_error(
            PolicyFailure::InvalidContract,
            format!("unsupported policy contract schema `{schema}`"),
            None,
        ));
    }
    let policy_hash = required_string(object, "policy_hash")
        .map_err(|message| policy_error(PolicyFailure::InvalidHash, message, None))?;
    if !is_sha256(&policy_hash) {
        return Err(policy_error(
            PolicyFailure::InvalidHash,
            "policy_contract policy_hash is not a sha256 digest",
            None,
        ));
    }
    if let Some(claimed) = node.get("policy_hash").and_then(Value::as_str) {
        if claimed != policy_hash {
            return Err(policy_error(
                PolicyFailure::InvalidHash,
                "node policy_hash does not match policy_contract",
                None,
            ));
        }
    } else {
        return Err(policy_error(
            PolicyFailure::InvalidHash,
            "node is missing policy_hash",
            None,
        ));
    }
    if let Some(graph_hash) = graph
        .and_then(|value| value.get("policy_hash"))
        .and_then(Value::as_str)
    {
        if graph_hash != policy_hash {
            return Err(policy_error(
                PolicyFailure::InvalidHash,
                "node policy hash does not match graph policy hash",
                None,
            ));
        }
    } else if graph.is_some() {
        return Err(policy_error(
            PolicyFailure::InvalidHash,
            "graph is missing policy_hash",
            None,
        ));
    }
    if let Some(graph_schema) = graph
        .and_then(|value| value.get("policy_schema"))
        .and_then(Value::as_str)
    {
        if graph_schema != crate::harness_policy::HARNESS_POLICY_SCHEMA {
            return Err(policy_error(
                PolicyFailure::InvalidContract,
                "graph policy_schema is unknown",
                None,
            ));
        }
    }
    let capability = required_string(object, "capability")
        .map_err(|message| policy_error(PolicyFailure::InvalidContract, message, None))?;
    let node_capability = node
        .get("capability")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if capability != node_capability {
        return Err(policy_error(
            PolicyFailure::CapabilityMismatch,
            format!(
                "node capability `{node_capability}` mismatches policy capability `{capability}`"
            ),
            None,
        ));
    }
    if object.get("decision").and_then(Value::as_str) != Some("allow") {
        return Err(policy_error(
            PolicyFailure::Denied,
            format!("policy denied capability `{capability}`"),
            None,
        ));
    }
    let sandbox_profile = required_string(object, "sandbox_profile")
        .map_err(|message| policy_error(PolicyFailure::InvalidContract, message, None))?;
    if sandbox_profile == "deny" {
        return Err(policy_error(
            PolicyFailure::Denied,
            "policy sandbox profile denies execution",
            None,
        ));
    }
    let allowed_writes = string_list(object.get("allowed_writes"), "allowed_writes")
        .map_err(|message| policy_error(PolicyFailure::InvalidContract, message, None))?;
    for path in &allowed_writes {
        validate_relative_glob(path).map_err(|message| {
            policy_error(PolicyFailure::InvalidContract, message.to_string(), None)
        })?;
    }
    let allowed_commands = string_list(object.get("allowed_commands"), "allowed_commands")
        .map_err(|message| policy_error(PolicyFailure::InvalidContract, message, None))?;
    let network = parse_network(object.get("network"))
        .map_err(|message| policy_error(PolicyFailure::InvalidContract, message, None))?;
    let limits = parse_limits(object.get("budgets"))
        .map_err(|message| policy_error(PolicyFailure::InvalidContract, message, None))?;
    let verifier_ids = string_list(object.get("verifier_ids"), "verifier_ids")
        .map_err(|message| policy_error(PolicyFailure::InvalidContract, message, None))?;
    let evidence_requirements =
        string_list(object.get("evidence_requirements"), "evidence_requirements")
            .map_err(|message| policy_error(PolicyFailure::InvalidContract, message, None))?;
    let external_side_effects = object
        .get("external_side_effects")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            policy_error(
                PolicyFailure::InvalidContract,
                "policy_contract external_side_effects must be boolean",
                None,
            )
        })?;
    let requested_external = node
        .get("external_side_effects")
        .or_else(|| node.get("external_side_effect"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if requested_external && !external_side_effects {
        return Err(policy_error(
            PolicyFailure::ExternalSideEffect,
            "node requests external side effects without a policy grant",
            None,
        ));
    }
    if node
        .get("side_effects")
        .and_then(Value::as_bool)
        .is_some_and(|requested| requested && !external_side_effects)
    {
        return Err(policy_error(
            PolicyFailure::ExternalSideEffect,
            "node side effects are not covered by the policy grant",
            None,
        ));
    }
    Ok(EffectivePolicy {
        schema,
        policy_hash,
        provenance: object
            .get("provenance")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        capability,
        sandbox_profile,
        allowed_writes,
        allowed_commands,
        network,
        limits,
        verifier_ids,
        evidence_requirements,
        external_side_effects,
    })
}

/// Capture a deterministic recursive preflight snapshot.  The snapshot is
/// deliberately independent of git state so pre-existing user changes are
/// treated as the baseline and are never reverted by postflight enforcement.
pub(crate) fn snapshot_workspace(workspace: &Path) -> Result<WorkspaceSnapshot> {
    let mut files = BTreeMap::new();
    walk_workspace(workspace, workspace, &mut files)?;
    let body: Vec<Value> = files
        .iter()
        .map(|(path, entry)| {
            json!({
                "path": path,
                "kind": match entry.kind { SnapshotKind::File => "file", SnapshotKind::Symlink => "symlink" },
                "digest": entry.digest,
                "target_digest": entry.target_digest,
            })
        })
        .collect();
    let digest = sha256_value(&Value::Array(body));
    Ok(WorkspaceSnapshot { files, digest })
}

/// Validate contract, provider route, and attempt/step limits before any
/// worker process is spawned.  The report returned on success is also used as
/// the basis for postflight evidence.
#[allow(clippy::result_large_err)]
pub(crate) fn preflight(
    node: &Value,
    graph: Option<&Value>,
    workspace: &Path,
    agent: &str,
    steps_used: u64,
) -> std::result::Result<(EffectivePolicy, WorkspaceSnapshot, PolicyEnforcementReport), PolicyError>
{
    let policy = parse_contract(node, graph)?;
    let capability_is_worker = policy.capability.contains("code.generate")
        || policy.capability.ends_with(".edit")
        || policy.capability.contains("code.write")
        || policy.capability == "content.analyze";
    if capability_is_worker && policy.allowed_writes.is_empty() {
        return Err(policy_error(
            PolicyFailure::Denied,
            "worker capability has no explicitly granted writable paths",
            None,
        ));
    }
    enforce_limits(&policy, node, workspace, steps_used)?;
    let snapshot = snapshot_workspace(workspace).map_err(|error| {
        policy_error(
            PolicyFailure::ProviderUnavailable,
            format!("cannot snapshot workspace: {error:#}"),
            None,
        )
    })?;
    if snapshot.has_symlink_escape() {
        return Err(policy_error(
            PolicyFailure::SymlinkEscape,
            "workspace contains a symlink escaping the workspace root",
            None,
        ));
    }
    let mut report = PolicyEnforcementReport::new(&policy, snapshot.digest.clone());
    let eligibility = provider_eligibility(agent, &policy);
    for (control, status) in &eligibility.controls {
        report.controls.insert(control.clone(), status.clone());
    }
    report
        .controls
        .insert("provider_route".to_owned(), eligibility.status.clone());
    report.provider_version = eligibility.version.clone();
    report.provider_reason = eligibility.reason.clone();
    if let Some(reason) = eligibility.reason {
        report.finalize();
        return Err(policy_error(
            PolicyFailure::ProviderUnavailable,
            reason,
            Some(report),
        ));
    }
    report.finalize();
    Ok((policy, snapshot, report))
}

/// Enforce observable step/attempt budgets without requiring a provider.  This
/// is used for verifier/control nodes that do not spawn an agent process.
#[allow(clippy::result_large_err)]
pub(crate) fn enforce_limits(
    policy: &EffectivePolicy,
    node: &Value,
    workspace: &Path,
    steps_used: u64,
) -> std::result::Result<(), PolicyError> {
    if steps_used >= policy.limits.max_steps {
        return Err(policy_error(
            PolicyFailure::BudgetExceeded,
            format!("max_steps {} exceeded", policy.limits.max_steps),
            None,
        ));
    }
    if let Ok(document) = crate::project_file::load(workspace) {
        if let Some(record) = document
            .learning
            .nodes
            .get(node.get("id").and_then(Value::as_str).unwrap_or_default())
        {
            // `run_multi_agent` checks out the node before invoking this
            // preflight, so the current attempt is already counted.  A retry
            // is refused only once it would become attempt max+1.
            let checked_out = document
                .execution
                .as_ref()
                .and_then(|execution| {
                    execution
                        .assignments
                        .get(node.get("id").and_then(Value::as_str).unwrap_or_default())
                })
                .is_some_and(|assignment| assignment.state == "checked_out");
            let exhausted = if checked_out {
                u64::from(record.attempt_count) > policy.limits.max_attempts
            } else {
                u64::from(record.attempt_count) >= policy.limits.max_attempts
            };
            if exhausted {
                return Err(policy_error(
                    PolicyFailure::BudgetExceeded,
                    format!("max_attempts {} exceeded", policy.limits.max_attempts),
                    None,
                ));
            }
        }
    }
    Ok(())
}

/// Compare a post-worker snapshot with the preflight snapshot.  Violations are
/// detected without changing user files; callers release the node through the
/// normal learning seam.
pub(crate) fn postflight(
    policy: &EffectivePolicy,
    before: &WorkspaceSnapshot,
    workspace: &Path,
    mut report: PolicyEnforcementReport,
) -> Result<Postflight> {
    let after = snapshot_workspace(workspace)?;
    let mut violations = Vec::new();
    let changed = changed_entries(before, &after);
    let mut file_count = 0_u64;
    let mut diff_lines = 0_u64;
    for path in &changed {
        let Some(entry) = after.files.get(path) else {
            // Deletion is still a changed path and must be authorized by the
            // write glob.  No absolute path is emitted.
            if !matches_globs(path, &policy.allowed_writes) {
                violations.push(PolicyViolation {
                    kind: "path_not_allowed".to_owned(),
                    path: Some(path.clone()),
                    digest: None,
                });
            }
            file_count += 1;
            if let Some(old) = before.files.get(path) {
                diff_lines = diff_lines.saturating_add(line_count(old.bytes.as_deref()));
            }
            continue;
        };
        file_count += 1;
        if !matches_globs(path, &policy.allowed_writes) {
            violations.push(PolicyViolation {
                kind: "path_not_allowed".to_owned(),
                path: Some(path.clone()),
                digest: Some(entry.digest.clone()),
            });
        }
        if is_secret_path(path) {
            violations.push(PolicyViolation {
                kind: "generated_secret_path".to_owned(),
                path: Some(path.clone()),
                digest: Some(entry.digest.clone()),
            });
        }
        if entry.kind == SnapshotKind::Symlink && entry.digest == sha256_bytes(b"symlink-escape") {
            violations.push(PolicyViolation {
                kind: "symlink_escape".to_owned(),
                path: Some(path.clone()),
                digest: entry.target_digest.clone(),
            });
        }
        diff_lines = diff_lines.saturating_add(line_count(
            before.files.get(path).and_then(|old| old.bytes.as_deref()),
        ));
        diff_lines = diff_lines.saturating_add(line_count(entry.bytes.as_deref()));
    }
    if file_count > policy.limits.max_files_changed {
        violations.push(PolicyViolation {
            kind: "max_files_changed".to_owned(),
            path: None,
            digest: Some(sha256_bytes(file_count.to_string().as_bytes())),
        });
    }
    if diff_lines > policy.limits.max_diff_lines {
        violations.push(PolicyViolation {
            kind: "max_diff_lines".to_owned(),
            path: None,
            digest: Some(sha256_bytes(diff_lines.to_string().as_bytes())),
        });
    }
    report.post_snapshot_hash = Some(after.digest.clone());
    report.violations.extend(violations.clone());
    report.evidence_hash = sha256_value(&json!({
        "pre": report.pre_snapshot_hash,
        "post": report.post_snapshot_hash,
        "violations": report.violations,
    }));
    report.finalize();
    let failure = if violations.is_empty() {
        None
    } else {
        let kind = violations.first().map(|v| v.kind.as_str()).unwrap_or("");
        let failure = match kind {
            "max_files_changed" => PolicyFailure::FileBounds,
            "max_diff_lines" => PolicyFailure::DiffBounds,
            "symlink_escape" => PolicyFailure::SymlinkEscape,
            "generated_secret_path" => PolicyFailure::SecretPath,
            _ => PolicyFailure::PathViolation,
        };
        Some(policy_error(
            failure,
            "postflight policy violation detected".to_owned(),
            Some(report.clone()),
        ))
    };
    Ok(Postflight { report, failure })
}

/// Construct the least-privilege invocation route for a provider.  A route is
/// ineligible if the provider cannot make the contract's controls real; callers
/// must not replace this with a bypass flag.
///
/// The v1 contract has no value that means "all shell commands are allowed".
/// Consequently a non-empty `allowed_commands` list is a bounded shell grant,
/// and providers without a native command allowlist remain ineligible.  An
/// empty list is an intentional no-shell grant and enables the file-only
/// routes for Claude and Hermes.
pub(crate) fn provider_eligibility(agent: &str, policy: &EffectivePolicy) -> ProviderEligibility {
    let kind = canonical_provider(agent);
    let mut controls = provider_control_defaults();
    let (version, version_reason) = provider_version(&kind);
    if let Some(reason) = version_reason {
        controls.insert("provider_version".to_owned(), ControlStatus::Unavailable);
        return ProviderEligibility {
            status: ControlStatus::Unavailable,
            reason: Some(reason),
            controls,
            version,
        };
    }
    controls.insert("provider_version".to_owned(), ControlStatus::Enforced);

    let shell_allowed = !policy.allowed_commands.is_empty();
    let network = policy.network.default.as_str();
    let network_denied = matches!(network, "deny" | "deny_by_default");
    let network_bounded = matches!(network, "allow_scoped" | "retrieval_only")
        || !policy.network.allowed_destinations.is_empty();

    // Network destinations are never inferred from an environment variable or
    // from a provider's generic "sandbox enabled" switch.  Only a provider
    // with a documented destination control could satisfy a bounded grant;
    // none of the installed worker CLIs has one.
    if network_bounded {
        controls.insert("network".to_owned(), ControlStatus::Unavailable);
        return ProviderEligibility {
            status: ControlStatus::Unavailable,
            reason: Some(format!(
                "provider `{agent}` cannot enforce scoped network destinations `{}`",
                if policy.network.allowed_destinations.is_empty() {
                    network
                } else {
                    "allowlist"
                }
            )),
            controls,
            version,
        };
    }

    match kind.as_str() {
        "codex" | "codex-luna" => {
            // Codex's workspace-write profile has a documented network toggle
            // but no command-name allowlist.  Existing Fractal policy execution
            // keeps Codex as the compatibility route for bounded test/build
            // grants; the command and diff guards remain authoritative outside
            // the provider process.
            controls.insert("approval".to_owned(), ControlStatus::Enforced);
            controls.insert("network".to_owned(), ControlStatus::Enforced);
            controls.insert("workspace_paths".to_owned(), ControlStatus::Detected);
            if shell_allowed {
                controls.insert("command_allowlist".to_owned(), ControlStatus::Detected);
            } else {
                controls.insert("command_allowlist".to_owned(), ControlStatus::Unavailable);
                return ProviderEligibility {
                    status: ControlStatus::Unavailable,
                    reason: Some(
                        "Codex cannot disable its shell tool for a no-shell policy contract"
                            .to_owned(),
                    ),
                    controls,
                    version,
                };
            }
            ProviderEligibility {
                status: ControlStatus::Enforced,
                reason: None,
                controls,
                version,
            }
        }
        "claude" => {
            controls.insert("approval".to_owned(), ControlStatus::Enforced);
            controls.insert("workspace_paths".to_owned(), ControlStatus::Detected);
            if shell_allowed && !contract_allows_unrestricted_shell(policy) {
                controls.insert("command_allowlist".to_owned(), ControlStatus::Unavailable);
                controls.insert("network".to_owned(), ControlStatus::Unavailable);
                return ProviderEligibility {
                    status: ControlStatus::Unavailable,
                    reason: Some(
                        "Claude has Bash but the v1 contract has no unrestricted-command sentinel; bounded shell grants fail closed"
                            .to_owned(),
                    ),
                    controls,
                    version,
                };
            }
            // `--tools` plus `--disallowedTools` removes Bash and web tools,
            // so a network-deny contract is real even though Claude has no
            // host-level network firewall switch.
            controls.insert("command_allowlist".to_owned(), ControlStatus::Enforced);
            controls.insert("network".to_owned(), ControlStatus::Enforced);
            if !network_denied {
                controls.insert("network".to_owned(), ControlStatus::Detected);
            }
            ProviderEligibility {
                status: ControlStatus::Enforced,
                reason: None,
                controls,
                version,
            }
        }
        "cursor-agent" => {
            controls.insert("approval".to_owned(), ControlStatus::Unavailable);
            controls.insert("workspace_paths".to_owned(), ControlStatus::Enforced);
            controls.insert("command_allowlist".to_owned(), ControlStatus::Unavailable);
            controls.insert("network".to_owned(), ControlStatus::Unavailable);
            // Cursor's `--sandbox enabled` does not document whether network
            // is denied, nor does the CLI expose a shell/tool allowlist.  It is
            // therefore never used for a bounded v1 contract.  Future policy
            // versions may add an explicit unrestricted-shell grant; until
            // then fail closed rather than guessing.
            let reason = if !shell_allowed && network_denied {
                "Cursor cannot disable its shell tool for a no-shell/network-deny policy contract"
            } else {
                "Cursor has no documented command allowlist or network control for this policy contract"
            };
            ProviderEligibility {
                status: ControlStatus::Unavailable,
                reason: Some(reason.to_owned()),
                controls,
                version,
            }
        }
        "hermes" => {
            controls.insert("workspace_paths".to_owned(), ControlStatus::Enforced);
            controls.insert("approval".to_owned(), ControlStatus::Detected);
            if shell_allowed {
                controls.insert("command_allowlist".to_owned(), ControlStatus::Unavailable);
                controls.insert("network".to_owned(), ControlStatus::Unavailable);
                return ProviderEligibility {
                    status: ControlStatus::Unavailable,
                    reason: Some(
                        "Hermes terminal has no enforceable command allowlist; the v1 bounded shell grant fails closed"
                            .to_owned(),
                    ),
                    controls,
                    version,
                };
            }
            controls.insert("command_allowlist".to_owned(), ControlStatus::Enforced);
            controls.insert("network".to_owned(), ControlStatus::Enforced);
            if !network_denied {
                controls.insert("network".to_owned(), ControlStatus::Detected);
            }
            ProviderEligibility {
                status: ControlStatus::Enforced,
                reason: None,
                controls,
                version,
            }
        }
        _ => {
            controls.insert("provider_version".to_owned(), ControlStatus::Unavailable);
            ProviderEligibility {
                status: ControlStatus::Unavailable,
                reason: Some(format!("unknown provider `{agent}`")),
                controls,
                version: None,
            }
        }
    }
}

/// Hardened provider command.  The prompt is passed as a direct argument and
/// is never interpolated into a shell command.
pub(crate) fn worker_command(
    kind: &str,
    prompt: &str,
    role: &str,
    policy: &EffectivePolicy,
) -> Result<Command> {
    let kind = canonical_provider(kind);
    if let Some(reason) = provider_eligibility(&kind, policy).reason {
        bail!("{reason}");
    }
    let mut command = Command::new(if kind == "codex-luna" { "codex" } else { &kind });
    match kind.as_str() {
        "codex" | "codex-luna" => {
            command.arg("exec");
            if role == "lead planner" {
                command.args([
                    "--model",
                    "gpt-5.6-sol",
                    "--config",
                    "model_reasoning_effort=\"high\"",
                ]);
            } else {
                command.args(["--model", "gpt-5.6-luna"]);
            }
            // Current Codex uses `approval_policy` as the config equivalent of
            // the older --ask-for-approval flag.  Workspace-write is explicit;
            // network access stays disabled in that profile.
            command.args(["--config", "approval_policy=\"never\""]);
            command.arg("--config").arg(format!(
                "sandbox_workspace_write.network_access={}",
                if matches!(policy.network.default.as_str(), "deny" | "deny_by_default") {
                    "false"
                } else {
                    "true"
                }
            ));
            command.args(["--sandbox", "workspace-write", "--color", "never"]);
        }
        "claude" => {
            command.args([
                "-p",
                "--bare",
                "--safe-mode",
                "--strict-mcp-config",
                "--no-chrome",
                "--permission-mode",
                "acceptEdits",
                "--no-session-persistence",
                "--tools",
                "Read,Edit,Glob,Grep",
                "--disallowedTools",
                "WebFetch",
                "WebSearch",
                "Bash",
            ]);
            if let Some(model) = model_for_provider("claude") {
                command.arg("--model").arg(model);
            }
        }
        "hermes" => {
            command.args([
                "chat",
                "-q",
                prompt,
                "-Q",
                "--ignore-user-config",
                "--ignore-rules",
                "--toolsets",
                "file",
            ]);
            if let Some(model) = model_for_provider("hermes") {
                let provider = env::var("FRACTAL_HERMES_PROVIDER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "openrouter".to_owned());
                command.args(["--model", &model, "--provider", &provider]);
            }
            // Prompt is already passed as `-q`; do not append it again below.
            return Ok(command);
        }
        "cursor-agent" => {
            command.args(["-p", "--sandbox", "enabled", "--workspace", "."]);
            if let Some(model) = model_for_provider("cursor") {
                command.args(["--model", &model]);
            }
        }
        _ => unreachable!("provider eligibility rejected unknown route"),
    }
    command.arg(prompt);
    Ok(command)
}

fn provider_control_defaults() -> BTreeMap<String, ControlStatus> {
    BTreeMap::from([
        ("approval".to_owned(), ControlStatus::Unavailable),
        ("command_allowlist".to_owned(), ControlStatus::Unavailable),
        ("network".to_owned(), ControlStatus::Unavailable),
        ("workspace_paths".to_owned(), ControlStatus::Unavailable),
        ("provider_version".to_owned(), ControlStatus::Unavailable),
    ])
}

fn canonical_provider(agent: &str) -> String {
    match agent.trim().to_ascii_lowercase().as_str() {
        "cursor" | "cursor-agent" | "agent" => "cursor-agent".to_owned(),
        "codex" | "codex-luna" => agent.trim().to_ascii_lowercase(),
        other => other.to_owned(),
    }
}

fn model_for_provider(kind: &str) -> Option<String> {
    let key = format!(
        "FRACTAL_{}_MODEL",
        kind.to_ascii_uppercase().replace('-', "_")
    );
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

/// The v1 schema intentionally has no unrestricted-shell sentinel.  Keep this
/// false until a future contract version adds one and it is validated by the
/// compiler; accepting `*` here would silently widen a bounded grant.
fn contract_allows_unrestricted_shell(_policy: &EffectivePolicy) -> bool {
    false
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProviderVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

fn provider_minimum(kind: &str) -> Option<ProviderVersion> {
    match kind {
        "claude" => Some(ProviderVersion {
            major: 2,
            minor: 0,
            patch: 0,
        }),
        "cursor-agent" => Some(ProviderVersion {
            major: 2026,
            minor: 7,
            patch: 23,
        }),
        "hermes" => Some(ProviderVersion {
            major: 0,
            minor: 13,
            patch: 0,
        }),
        "codex" | "codex-luna" => Some(ProviderVersion {
            major: 0,
            minor: 145,
            patch: 0,
        }),
        _ => None,
    }
}

fn provider_binary(kind: &str) -> Option<&'static str> {
    match kind {
        "codex" | "codex-luna" => Some("codex"),
        "cursor-agent" => Some("cursor-agent"),
        "claude" => Some("claude"),
        "hermes" => Some("hermes"),
        _ => None,
    }
}

fn parse_provider_version(text: &str) -> Option<ProviderVersion> {
    for token in text.split_whitespace() {
        let token = token.trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '.');
        let mut parts = token.split('.');
        let (Some(major), Some(minor), Some(patch)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(major) = major.parse::<u64>() else {
            continue;
        };
        let Ok(minor) = minor.parse::<u64>() else {
            continue;
        };
        let patch = patch
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        let Ok(patch) = patch.parse::<u64>() else {
            continue;
        };
        return Some(ProviderVersion {
            major,
            minor,
            patch,
        });
    }
    None
}

fn provider_version(kind: &str) -> (Option<String>, Option<String>) {
    let Some(binary) = provider_binary(kind) else {
        return (None, Some(format!("unknown provider `{kind}`")));
    };
    if !binary_on_path(binary) {
        return (
            None,
            Some(format!(
                "provider `{kind}` is unavailable: `{binary}` is not on PATH"
            )),
        );
    }
    // Version probing is an audit operation, not a worker launch.  Do not
    // expose the parent process's API keys or arbitrary environment values to
    // provider startup code while checking its installed capability surface.
    let mut version_command = Command::new(binary);
    version_command.arg("--version").env_clear();
    if let Some(path) = env::var_os("PATH") {
        version_command.env("PATH", path);
    }
    // A few provider wrapper scripts require HOME just to resolve their own
    // installation; use the system temp directory rather than the operator's
    // home so version startup cannot discover credential/config files.
    version_command.env("HOME", env::temp_dir());
    let output = match version_command.output() {
        Ok(output) => output,
        Err(error) => {
            return (
                None,
                Some(format!("provider `{kind}` version probe failed: {error}")),
            )
        }
    };
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let Some(version) = parse_provider_version(&text) else {
        return (
            None,
            Some(format!("provider `{kind}` reported an unparseable version")),
        );
    };
    let rendered = format!("{}.{}.{}", version.major, version.minor, version.patch);
    if !output.status.success() {
        return (
            Some(rendered),
            Some(format!(
                "provider `{kind}` version probe exited unsuccessfully"
            )),
        );
    }
    let Some(minimum) = provider_minimum(kind) else {
        return (Some(rendered), None);
    };
    if version < minimum || version.major != minimum.major {
        return (
            Some(rendered.clone()),
            Some(format!(
                "provider `{kind}` version `{rendered}` is unsupported; tested minimum is `{}.{}.{}`",
                minimum.major, minimum.minor, minimum.patch
            )),
        );
    }
    (Some(rendered), None)
}

fn binary_on_path(binary: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| {
            env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(binary);
                candidate.is_file() || candidate.with_extension("exe").is_file()
            })
        })
        .unwrap_or(false)
}

/// Return a sanitized child environment.  Values are intentionally kept out
/// of reports/logs; this map is consumed directly by Command::env_clear/envs.
pub(crate) fn sanitized_environment(kind: &str, network_denied: bool) -> BTreeMap<String, String> {
    const BASE: &[&str] = &[
        "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "USER",
    ];
    const RUNTIME: &[&str] = &[
        "FRACTAL_AGENT_ID",
        "FRACTAL_AGENT_LABEL",
        "FRACTAL_WORKER",
        "FRACTAL_MODEL",
        "FRACTAL_CODEX_MODEL",
        "FRACTAL_CLAUDE_MODEL",
        "FRACTAL_CURSOR_MODEL",
        "FRACTAL_HERMES_MODEL",
        "FRACTAL_HERMES_PROVIDER",
        "FRACTAL_OFFLINE",
    ];
    const AUTH: &[&str] = &[
        "CODEX_HOME",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "AWS_PROFILE",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "CURSOR_API_KEY",
        "CURSOR_API_ENDPOINT",
        "HERMES_INFERENCE_MODEL",
        "HERMES_INFERENCE_PROVIDER",
    ];
    let mut output = BTreeMap::new();
    for name in BASE.iter().chain(RUNTIME).chain(AUTH) {
        if let Ok(value) = env::var(name) {
            output.insert((*name).to_owned(), value);
        }
    }
    output.insert("FRACTAL_WORKER".to_owned(), kind.to_owned());
    if network_denied {
        output.insert("FRACTAL_OFFLINE".to_owned(), "1".to_owned());
        output.insert("NO_PROXY".to_owned(), "*".to_owned());
    }
    output
}

/// Extend the sanitized environment with provider-specific workspace roots.
/// Hermes otherwise reads `~/.hermes/.env` and can persist sessions/logs in a
/// user's home directory; an isolated HERMES_HOME prevents that ambient state
/// from becoming part of a Fractal run.  Its file tool receives the same root
/// through HERMES_WRITE_SAFE_ROOT.  The helper keeps the two-argument function
/// above stable for callers/tests that only need the generic environment.
pub(crate) fn sanitized_environment_for_workspace(
    kind: &str,
    network_denied: bool,
    workspace: &Path,
) -> BTreeMap<String, String> {
    let mut output = sanitized_environment(kind, network_denied);
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    output.insert(
        "TERMINAL_CWD".to_owned(),
        workspace.to_string_lossy().into_owned(),
    );
    if canonical_provider(kind) == "hermes" {
        let root_key = sha256_bytes(workspace.to_string_lossy().as_bytes());
        let isolated_home = env::temp_dir()
            .join("fractal-hermes")
            .join(root_key.trim_start_matches(SHA256_PREFIX));
        output.insert(
            "HERMES_HOME".to_owned(),
            isolated_home.to_string_lossy().into_owned(),
        );
        output.insert(
            "HERMES_WRITE_SAFE_ROOT".to_owned(),
            workspace.to_string_lossy().into_owned(),
        );
    }
    output
}

pub(crate) fn report_hash(report: &PolicyEnforcementReport) -> String {
    report.report_hash.clone()
}

fn policy_error(
    failure: PolicyFailure,
    message: impl Into<String>,
    report: Option<PolicyEnforcementReport>,
) -> PolicyError {
    PolicyError {
        failure,
        report,
        message: message.into(),
    }
}

fn required_string(object: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.to_owned())
        .ok_or_else(|| format!("policy_contract field `{key}` must be a non-empty string"))
}

fn string_list(value: Option<&Value>, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("policy_contract field `{key}` must be an array"))?;
    let mut values = Vec::with_capacity(array.len());
    for item in array {
        let item = item
            .as_str()
            .ok_or_else(|| format!("policy_contract field `{key}` contains a non-string"))?;
        if item.trim().is_empty() {
            return Err(format!(
                "policy_contract field `{key}` contains an empty string"
            ));
        }
        values.push(item.to_owned());
    }
    values.sort();
    values.dedup();
    Ok(values)
}

fn parse_network(value: Option<&Value>) -> Result<EffectiveNetwork, String> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| "policy_contract network must be an object".to_owned())?;
    let default = object
        .get("default")
        .and_then(Value::as_str)
        .ok_or_else(|| "policy_contract network.default missing".to_owned())?
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        default.as_str(),
        "deny" | "deny_by_default" | "allow" | "allow_scoped" | "retrieval_only"
    ) {
        return Err(format!("unsupported network policy `{default}`"));
    }
    let allowed_destinations = string_list(
        object.get("allowed_destinations"),
        "network.allowed_destinations",
    )?;
    Ok(EffectiveNetwork {
        default,
        allowed_destinations,
    })
}

fn parse_limits(value: Option<&Value>) -> Result<EffectiveLimits, String> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| "policy_contract budgets must be an object".to_owned())?;
    let number = |key: &str| -> Result<u64, String> {
        object
            .get(key)
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("policy_contract budgets.{key} must be a positive integer"))
    };
    Ok(EffectiveLimits {
        max_steps: number("max_steps")?,
        max_minutes: number("max_minutes")?,
        max_attempts: number("max_attempts")?,
        max_files_changed: number("max_files_changed")?,
        max_diff_lines: number("max_diff_lines")?,
        max_input_tokens: number("max_input_tokens")?,
        max_output_tokens: number("max_output_tokens")?,
        max_cost_usd: object
            .get("max_cost_usd")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn validate_relative_glob(value: &str) -> Result<()> {
    let normalized = value.replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.starts_with('~')
        || normalized.as_bytes().get(1) == Some(&b':')
        || normalized.split('/').any(|part| part == "..")
    {
        bail!("policy path glob is absolute or traverses outside the workspace")
    }
    Ok(())
}

fn walk_workspace(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, SnapshotEntry>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("read workspace directory {}", directory.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if relative == ".git" || relative.starts_with(".git/") {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).unwrap_or_else(|_| PathBuf::from("<unreadable>"));
            let canonical = fs::canonicalize(&path).ok();
            let escapes = canonical
                .as_ref()
                .is_some_and(|canonical| !canonical.starts_with(root));
            let target_text = target.to_string_lossy().replace('\\', "/");
            let target_digest = sha256_bytes(target_text.as_bytes());
            files.insert(
                relative,
                SnapshotEntry {
                    kind: SnapshotKind::Symlink,
                    digest: if escapes {
                        sha256_bytes(b"symlink-escape")
                    } else {
                        target_digest.clone()
                    },
                    bytes: None,
                    target_digest: Some(target_digest),
                },
            );
            continue;
        }
        if metadata.is_dir() {
            walk_workspace(root, &path, files)?;
        } else if metadata.is_file() {
            let bytes = fs::read(&path).unwrap_or_default();
            let stored = (bytes.len() as u64 <= MAX_SNAPSHOT_FILE_BYTES).then(|| bytes.clone());
            files.insert(
                relative,
                SnapshotEntry {
                    kind: SnapshotKind::File,
                    digest: sha256_bytes(&bytes),
                    bytes: stored,
                    target_digest: None,
                },
            );
        }
    }
    Ok(())
}

fn changed_entries(before: &WorkspaceSnapshot, after: &WorkspaceSnapshot) -> Vec<String> {
    let paths: BTreeSet<String> = before
        .files
        .keys()
        .chain(after.files.keys())
        .cloned()
        .collect();
    paths
        .into_iter()
        // Evidence manifests are controller-owned sidecars written after a
        // verifier run.  They must not be mistaken for agent edits or require
        // a project policy grant; their own content hash is the audit boundary.
        .filter(|path| path != ".fractal/evidence" && !path.starts_with(".fractal/evidence/"))
        .filter(|path| {
            before.files.get(path).map(snapshot_identity)
                != after.files.get(path).map(snapshot_identity)
        })
        .collect()
}

fn snapshot_identity(entry: &SnapshotEntry) -> (&SnapshotKind, &String, &Option<String>) {
    (&entry.kind, &entry.digest, &entry.target_digest)
}

fn line_count(bytes: Option<&[u8]>) -> u64 {
    bytes
        .map(|bytes| String::from_utf8_lossy(bytes).lines().count() as u64)
        .unwrap_or(0)
}

fn is_secret_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower == ".env"
        || lower.starts_with(".env.")
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.contains("/secrets/")
        || lower.starts_with("secrets/")
        || lower.ends_with("/credentials")
        || lower == "credentials"
}

fn matches_globs(path: &str, globs: &[String]) -> bool {
    globs.iter().any(|glob| glob_match(glob, path))
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut memo = BTreeMap::new();
    fn inner(
        pattern: &[u8],
        text: &[u8],
        p: usize,
        t: usize,
        memo: &mut BTreeMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(value) = memo.get(&(p, t)) {
            return *value;
        }
        let result = if p == pattern.len() {
            t == text.len()
        } else if pattern[p] == b'*' {
            if p + 1 < pattern.len() && pattern[p + 1] == b'*' {
                inner(pattern, text, p + 2, t, memo)
                    || (t < text.len() && inner(pattern, text, p, t + 1, memo))
            } else {
                inner(pattern, text, p + 1, t, memo)
                    || (t < text.len() && text[t] != b'/' && inner(pattern, text, p, t + 1, memo))
            }
        } else if pattern[p] == b'?' {
            t < text.len() && text[t] != b'/' && inner(pattern, text, p + 1, t + 1, memo)
        } else {
            t < text.len() && pattern[p] == text[t] && inner(pattern, text, p + 1, t + 1, memo)
        };
        memo.insert((p, t), result);
        result
    }
    inner(pattern, text, 0, 0, &mut memo)
}

fn is_sha256(value: &str) -> bool {
    let digest = value.strip_prefix(SHA256_PREFIX).unwrap_or(value);
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::from(SHA256_PREFIX);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn sha256_value(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    sha256_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn hash() -> String {
        format!("sha256:{}", "a".repeat(64))
    }
    fn node(capability: &str) -> Value {
        json!({
            "id": "n",
            "capability": capability,
            "policy_hash": hash(),
            "policy_contract": {
                "schema": NODE_POLICY_CONTRACT_SCHEMA,
                "policy_hash": hash(),
                "provenance": "builtin:safe-default.v1",
                "capability": capability,
                "decision": "allow",
                "sandbox_profile": "local-work-v1",
                "allowed_writes": ["src/**"],
                "allowed_commands": ["cargo test"],
                "network": {"default":"deny", "allowed_destinations":[]},
                "budgets": {"max_steps":4,"max_minutes":1,"max_attempts":1,"max_files_changed":2,"max_diff_lines":20,"max_input_tokens":1,"max_output_tokens":1,"max_cost_usd":0},
                "verifier_ids": [],
                "evidence_requirements": [],
                "external_side_effects": false
            }
        })
    }

    fn workspace() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = env::temp_dir().join(format!(
            "fractal-policy-exec-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn rejects_deny_and_capability_mismatch() {
        let mut denied = node("code.generate");
        denied["policy_contract"]["decision"] = json!("deny");
        assert!(matches!(
            parse_contract(&denied, None).unwrap_err().failure,
            PolicyFailure::Denied
        ));
        let mut mismatch = node("code.generate");
        mismatch["policy_contract"]["capability"] = json!("code.edit");
        assert!(matches!(
            parse_contract(&mismatch, None).unwrap_err().failure,
            PolicyFailure::CapabilityMismatch
        ));
    }

    #[test]
    fn rejects_invalid_hash_and_unknown_schema() {
        let mut invalid = node("code.generate");
        invalid["policy_hash"] = json!("sha256:bad");
        assert!(matches!(
            parse_contract(&invalid, None).unwrap_err().failure,
            PolicyFailure::InvalidHash
        ));
        let mut unknown = node("code.generate");
        unknown["policy_contract"]["schema"] = json!("fractal.node_policy_contract.v99");
        assert!(matches!(
            parse_contract(&unknown, None).unwrap_err().failure,
            PolicyFailure::InvalidContract
        ));
    }

    #[test]
    fn timeout_is_strict_node_limit_without_floor() {
        let policy = parse_contract(&node("code.generate"), None).unwrap();
        assert_eq!(policy.limits.timeout_ms(), 60_000);
    }

    #[test]
    fn glob_and_postflight_bounds_are_enforced_without_reverting() {
        let root = workspace();
        let policy = parse_contract(&node("code.generate"), None).unwrap();
        let before = snapshot_workspace(&root).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/ok.rs"), "one\ntwo\n").unwrap();
        fs::write(root.join("outside.txt"), "secret").unwrap();
        let report = PolicyEnforcementReport::new(&policy, before.digest.clone());
        let post = postflight(&policy, &before, &root, report).unwrap();
        assert!(post.failure.is_some());
        assert!(post
            .report
            .violations
            .iter()
            .any(|v| v.kind == "path_not_allowed"));
        assert!(root.join("outside.txt").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn command_has_no_bypass_and_sanitized_env_redacts_values_from_report() {
        let policy = parse_contract(&node("code.generate"), None).unwrap();
        let command = worker_command("codex-luna", "build", "worker", &policy).unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--sandbox", "workspace-write"]));
        assert!(!args
            .iter()
            .any(|arg| arg.contains("dangerously") || arg == "--yolo" || arg == "--force"));
        let environment = sanitized_environment("codex-luna", true);
        assert_eq!(environment.get("FRACTAL_OFFLINE"), Some(&"1".to_owned()));
        let report = PolicyEnforcementReport::new(&policy, hash());
        let rendered = serde_json::to_string(&report).unwrap();
        assert!(!rendered.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn provider_matrix_keeps_file_only_routes_and_rejects_bounded_shell() {
        let mut no_shell = node("code.generate");
        no_shell["policy_contract"]["allowed_commands"] = json!([]);
        let file_policy = parse_contract(&no_shell, None).unwrap();

        let claude = provider_eligibility("claude", &file_policy);
        assert_eq!(claude.status, ControlStatus::Enforced);
        assert_eq!(
            claude.controls.get("network"),
            Some(&ControlStatus::Enforced)
        );
        assert_eq!(
            claude.controls.get("command_allowlist"),
            Some(&ControlStatus::Enforced)
        );

        let hermes = provider_eligibility("hermes", &file_policy);
        assert_eq!(hermes.status, ControlStatus::Enforced);
        assert_eq!(
            hermes.controls.get("network"),
            Some(&ControlStatus::Enforced)
        );

        let cursor = provider_eligibility("cursor", &file_policy);
        assert_eq!(cursor.status, ControlStatus::Unavailable);
        assert!(cursor
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("cannot disable its shell"));

        let bounded = parse_contract(&node("code.generate"), None).unwrap();
        for provider in ["claude", "cursor", "hermes"] {
            let result = provider_eligibility(provider, &bounded);
            assert_eq!(result.status, ControlStatus::Unavailable, "{provider}");
            assert!(
                result.reason.is_some(),
                "{provider} must explain the denial"
            );
        }
        let codex = provider_eligibility("codex-luna", &bounded);
        assert_eq!(codex.status, ControlStatus::Enforced);
        assert_eq!(
            codex.controls.get("command_allowlist"),
            Some(&ControlStatus::Detected)
        );
    }

    #[test]
    fn provider_matrix_allows_only_broad_network_without_destinations() {
        let mut value = node("code.generate");
        value["policy_contract"]["allowed_commands"] = json!([]);
        value["policy_contract"]["network"] =
            json!({"default": "allow", "allowed_destinations": []});
        let policy = parse_contract(&value, None).unwrap();
        let claude = provider_eligibility("claude", &policy);
        assert_eq!(claude.status, ControlStatus::Enforced);
        assert_eq!(
            claude.controls.get("network"),
            Some(&ControlStatus::Detected)
        );
        let hermes = provider_eligibility("hermes", &policy);
        assert_eq!(hermes.status, ControlStatus::Enforced);
        assert_eq!(
            hermes.controls.get("network"),
            Some(&ControlStatus::Detected)
        );

        value["policy_contract"]["network"] =
            json!({"default": "allow_scoped", "allowed_destinations": ["api.example.com"]});
        let scoped = parse_contract(&value, None).unwrap();
        let result = provider_eligibility("claude", &scoped);
        assert_eq!(result.status, ControlStatus::Unavailable);
        assert!(result
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("scoped network"));
    }

    #[test]
    fn provider_commands_use_documented_noninteractive_flags_only() {
        let mut value = node("code.generate");
        value["policy_contract"]["allowed_commands"] = json!([]);
        let policy = parse_contract(&value, None).unwrap();
        let claude = worker_command("claude", "edit files", "worker", &policy).unwrap();
        let claude_args: Vec<String> = claude
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(claude_args
            .windows(2)
            .any(|pair| pair == ["--permission-mode".to_owned(), "acceptEdits".to_owned()]));
        assert!(claude_args
            .windows(2)
            .any(|pair| pair == ["--tools".to_owned(), "Read,Edit,Glob,Grep".to_owned()]));
        assert!(claude_args.iter().any(|arg| arg == "WebFetch"));
        assert!(claude_args.iter().any(|arg| arg == "WebSearch"));
        assert!(claude_args.iter().any(|arg| arg == "Bash"));
        assert!(!claude_args
            .iter()
            .any(|arg| { arg.contains("dangerously") || arg == "--yolo" || arg == "--force" }));

        let hermes = worker_command("hermes", "edit files", "worker", &policy).unwrap();
        let hermes_args: Vec<String> = hermes
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(hermes_args
            .windows(2)
            .any(|pair| pair == ["--toolsets".to_owned(), "file".to_owned()]));
        assert!(hermes_args
            .windows(2)
            .any(|pair| pair == ["-q".to_owned(), "edit files".to_owned()]));
        assert!(!hermes_args
            .iter()
            .any(|arg| { arg.contains("dangerously") || arg == "--yolo" || arg == "--force" }));
    }

    #[test]
    fn hermes_environment_isolated_to_workspace_and_does_not_pass_unknown_secrets() {
        let root = workspace();
        let environment = sanitized_environment_for_workspace("hermes", true, &root);
        assert_eq!(
            environment.get("HERMES_WRITE_SAFE_ROOT"),
            Some(&root.canonicalize().unwrap().to_string_lossy().into_owned())
        );
        assert!(environment
            .get("HERMES_HOME")
            .is_some_and(|value| value.contains("fractal-hermes")));
        assert_eq!(
            environment.get("TERMINAL_CWD"),
            environment.get("HERMES_WRITE_SAFE_ROOT")
        );
        assert_eq!(environment.get("FRACTAL_OFFLINE"), Some(&"1".to_owned()));
        assert!(!environment.contains_key("DATABASE_URL"));
        assert!(!environment.contains_key("AWS_SECRET_ACCESS_KEY"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn provider_version_parser_rejects_unparseable_and_normalizes_supported_output() {
        assert_eq!(
            parse_provider_version("Claude Code 2.1.220 (stable)"),
            Some(ProviderVersion {
                major: 2,
                minor: 1,
                patch: 220,
            })
        );
        assert_eq!(
            parse_provider_version("Hermes Agent v0.13.0 (2026.5.7)"),
            Some(ProviderVersion {
                major: 0,
                minor: 13,
                patch: 0,
            })
        );
        assert_eq!(parse_provider_version("not a version"), None);
    }

    #[test]
    fn report_is_deterministic_and_redacted() {
        let policy = parse_contract(&node("code.generate"), None).unwrap();
        let mut first = PolicyEnforcementReport::new(&policy, hash());
        first.finalize();
        let mut second = PolicyEnforcementReport::new(&policy, hash());
        second.finalize();
        assert_eq!(first.report_hash, second.report_hash);
        assert!(!first.compact_value().to_string().contains("/tmp"));
    }
}
