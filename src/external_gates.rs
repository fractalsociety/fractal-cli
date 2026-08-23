//! External human-review gates for graph execution.
//!
//! Gate declarations remain on graph nodes (the historical external_gates
//! field is still read), while approvals and revocations live in the additive
//! top-level external_gate_ledger project field. Ledger records are local,
//! content-hashed audit records: they are tamper-evident local governance, not
//! cryptographic signatures or remote non-repudiation.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::CString;
use std::fs;
#[cfg(unix)]
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub(crate) const LEDGER_SCHEMA: &str = "fractal.external_gate_ledger.v1";
pub(crate) const RECORD_SCHEMA: &str = "fractal.external_gate_record.v1";
const LOCAL_AUTHORITY: &str = "os-local";
const LOCAL_ASSURANCE: &str = "tamper-evident-local-governance";
const MAX_GATE_BYTES: usize = 160;
const MAX_REVIEWER_BYTES: usize = 256;
const MAX_ROLE_BYTES: usize = 128;
const MAX_ATTESTATION_BYTES: usize = 4_096;
const MAX_PATH_BYTES: usize = 1_024;
/// Bound evidence reads performed during recording and every scheduler check.
const MAX_EVIDENCE_BYTES: u64 = 16 * 1024 * 1024;

/// Append-only project-level ledger. It is deliberately additive and outside
/// the immutable execution graph, so adding a review does not change graph_hash.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(crate) struct ExternalGateLedger {
    #[serde(default = "default_ledger_schema")]
    pub(crate) schema: String,
    #[serde(default)]
    pub(crate) records: Vec<ExternalGateRecord>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

/// One immutable approval or exact revocation entry.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct ExternalGateRecord {
    #[serde(default = "default_record_schema")]
    pub(crate) schema: String,
    /// approval or revocation.
    pub(crate) kind: String,
    pub(crate) graph_hash: String,
    pub(crate) node_id: String,
    pub(crate) gate: String,
    pub(crate) reviewer_id: String,
    #[serde(default)]
    pub(crate) reviewer_label: String,
    pub(crate) role: String,
    pub(crate) attestation: String,
    /// Honest local provenance. This is not a cryptographic signature.
    pub(crate) authority: String,
    pub(crate) assurance: String,
    pub(crate) evidence_path: String,
    pub(crate) evidence_hash: String,
    pub(crate) evidence_length: u64,
    /// Canonical snapshot token used only to confirm a preview before apply.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) project_revision: String,
    pub(crate) recorded_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) previous_hash: Option<String>,
    /// Present only on a revocation; identifies one exact prior approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) revokes: Option<String>,
    /// SHA-256 of this record with content_hash omitted.
    pub(crate) content_hash: String,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug)]
pub(crate) struct RecordApprovalInput {
    pub(crate) node_id: String,
    pub(crate) gate: String,
    pub(crate) evidence_path: PathBuf,
    pub(crate) reviewer_id: String,
    pub(crate) reviewer_label: String,
    pub(crate) role: String,
    pub(crate) attestation: String,
}

#[derive(Clone, Debug)]
pub(crate) struct RevokeApprovalInput {
    pub(crate) approval_hash: String,
    pub(crate) reviewer_id: String,
    pub(crate) reviewer_label: String,
    pub(crate) role: String,
    pub(crate) attestation: String,
}

fn default_ledger_schema() -> String {
    LEDGER_SCHEMA.to_owned()
}

fn default_record_schema() -> String {
    RECORD_SCHEMA.to_owned()
}

impl ExternalGateLedger {
    pub(crate) fn empty() -> Self {
        Self {
            schema: LEDGER_SCHEMA.to_owned(),
            records: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

fn parse_gate_declaration(value: &Value) -> Result<BTreeSet<String>> {
    let mut gates = BTreeSet::new();
    match value {
        Value::String(gate) => {
            if !gate.trim().is_empty() {
                gates.insert(gate.trim().to_owned());
            } else {
                bail!("external gate declaration must not be empty");
            }
        }
        Value::Array(values) => {
            for value in values {
                if let Some(gate) = value.as_str() {
                    if !gate.trim().is_empty() {
                        gates.insert(gate.trim().to_owned());
                    } else {
                        bail!("external gate declaration must not be empty");
                    }
                } else if let Some(gate) = value.get("gate").and_then(Value::as_str) {
                    if !gate.trim().is_empty() {
                        gates.insert(gate.trim().to_owned());
                    } else {
                        bail!("external gate declaration must not be empty");
                    }
                } else {
                    bail!("external gate declaration must contain gate strings");
                }
            }
        }
        Value::Object(values) => {
            for (gate, enabled) in values {
                let enabled = enabled
                    .as_bool()
                    .context("external gate object values must be booleans")?;
                if !gate.trim().is_empty() {
                    if enabled {
                        gates.insert(gate.trim().to_owned());
                    }
                } else {
                    bail!("external gate declaration must not be empty");
                }
            }
        }
        _ => bail!("external gate declaration must be a string, array, or object"),
    }
    Ok(gates)
}

/// Extract graph-declared gates for one node. Both the current required_gates
/// field and the historical external_gates field are validated when present;
/// their union is enforced so one declaration cannot silently override the
/// other (including an explicit empty required_gates value).
pub(crate) fn required_gates(node: &Value) -> Result<Vec<String>> {
    let mut gates = BTreeSet::new();
    for key in ["required_gates", "external_gates"] {
        if let Some(value) = node.get(key) {
            gates.extend(parse_gate_declaration(value)?);
        }
    }
    Ok(gates.into_iter().collect())
}

#[allow(dead_code)]
pub(crate) fn node_has_required_gates(node: &Value) -> bool {
    required_gates(node)
        .map(|gates| !gates.is_empty())
        .unwrap_or(true)
}

/// Gate-role policy. Security review is intentionally strict; unknown local
/// gates still require an explicit non-empty role and attestation.
pub(crate) fn required_role(gate: &str) -> &'static str {
    if gate == "security_review" {
        "security_reviewer"
    } else {
        "external_reviewer"
    }
}

/// Validate a decoded ledger's append-only hash chain and record invariants.
/// This check intentionally does not inspect evidence files; callers that have
/// a repository invoke effective_for_document for fail-closed file checks too.
pub(crate) fn validate_ledger(ledger: &ExternalGateLedger) -> Result<()> {
    if ledger.schema != LEDGER_SCHEMA {
        bail!("unsupported external gate ledger schema {}", ledger.schema);
    }
    let mut previous = None::<String>;
    let mut seen = BTreeSet::new();
    let mut approvals = BTreeMap::<String, &ExternalGateRecord>::new();
    let mut revoked = BTreeSet::new();
    for (index, record) in ledger.records.iter().enumerate() {
        validate_record_shape(record)
            .with_context(|| format!("invalid external gate ledger record {index}"))?;
        if record.previous_hash != previous {
            bail!("external gate ledger hash chain mismatch at record {index}");
        }
        let computed = record_content_hash(record)?;
        if computed != record.content_hash {
            bail!(
                "external gate ledger record {} content hash mismatch: claimed {}, computed {}",
                index,
                record.content_hash,
                computed
            );
        }
        if !seen.insert(record.content_hash.clone()) {
            bail!("external gate ledger contains duplicate content hash");
        }
        match record.kind.as_str() {
            "approval" => {
                validate_attestation(
                    &record.attestation,
                    "approval",
                    &record.graph_hash,
                    &record.node_id,
                    &record.gate,
                    None,
                )?;
                approvals.insert(record.content_hash.clone(), record);
            }
            "revocation" => {
                let target = record
                    .revokes
                    .as_ref()
                    .context("revocation record is missing exact approval hash")?;
                let approval = approvals
                    .get(target)
                    .copied()
                    .with_context(|| format!("revocation targets unknown approval {target}"))?;
                if approval.graph_hash != record.graph_hash
                    || approval.node_id != record.node_id
                    || approval.gate != record.gate
                    || approval.evidence_path != record.evidence_path
                    || approval.evidence_hash != record.evidence_hash
                    || approval.evidence_length != record.evidence_length
                {
                    bail!("revocation does not exactly match its approval target");
                }
                validate_attestation(
                    &record.attestation,
                    "revocation",
                    &record.graph_hash,
                    &record.node_id,
                    &record.gate,
                    Some(target),
                )?;
                if !revoked.insert(target.clone()) {
                    bail!("approval {target} has more than one revocation");
                }
            }
            other => bail!("unsupported external gate ledger record kind {other}"),
        }
        previous = Some(record.content_hash.clone());
    }
    Ok(())
}

fn validate_record_shape(record: &ExternalGateRecord) -> Result<()> {
    if record.schema != RECORD_SCHEMA {
        bail!("unsupported external gate record schema {}", record.schema);
    }
    if !matches!(record.kind.as_str(), "approval" | "revocation") {
        bail!("external gate record kind must be approval or revocation");
    }
    validate_hash(&record.graph_hash, "graph_hash")?;
    bounded_text(&record.node_id, MAX_GATE_BYTES, "node_id")?;
    bounded_text(&record.gate, MAX_GATE_BYTES, "gate")?;
    bounded_text(&record.reviewer_id, MAX_REVIEWER_BYTES, "reviewer_id")?;
    bounded_text(&record.reviewer_label, MAX_REVIEWER_BYTES, "reviewer_label")?;
    bounded_text(&record.role, MAX_ROLE_BYTES, "role")?;
    bounded_text(&record.attestation, MAX_ATTESTATION_BYTES, "attestation")?;
    bounded_text(&record.evidence_path, MAX_PATH_BYTES, "evidence_path")?;
    bounded_text(&record.recorded_at, 128, "recorded_at")?;
    if record.authority != LOCAL_AUTHORITY || record.assurance != LOCAL_ASSURANCE {
        bail!("external gate record has unsupported local authority metadata");
    }
    if !record.project_revision.is_empty() {
        validate_hash(&record.project_revision, "project_revision")?;
    }
    if record.role != required_role(&record.gate) {
        bail!(
            "external gate {} requires role {}",
            record.gate,
            required_role(&record.gate)
        );
    }
    validate_hash(&record.evidence_hash, "evidence_hash")?;
    if record.evidence_length > MAX_EVIDENCE_BYTES {
        bail!(
            "external gate evidence length exceeds {} bytes",
            MAX_EVIDENCE_BYTES
        );
    }
    if record.kind == "approval" && record.revokes.is_some() {
        bail!("approval record cannot contain revokes");
    }
    if record.kind == "revocation" && record.revokes.is_none() {
        bail!("revocation record must contain revokes");
    }
    validate_hash(&record.content_hash, "content_hash")
}

fn validate_hash(value: &str, name: &str) -> Result<()> {
    let digest = value
        .strip_prefix("sha256:")
        .with_context(|| format!("{name} must start with sha256:"))?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{name} must contain 64 hexadecimal characters");
    }
    Ok(())
}

fn bounded_text(value: &str, max_bytes: usize, name: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("external gate {name} must not be empty");
    }
    if value.len() > max_bytes || value.chars().any(char::is_control) {
        bail!("external gate {name} is malformed or exceeds {max_bytes} bytes");
    }
    Ok(())
}

fn record_content_hash(record: &ExternalGateRecord) -> Result<String> {
    let mut value = serde_json::to_value(record)?;
    value
        .as_object_mut()
        .context("external gate record must encode as object")?
        .remove("content_hash");
    let hash = fractal_contracts::canonical_sha256(&value)
        .map_err(|error| anyhow::anyhow!("external gate record canonical hash failed: {error}"))?;
    Ok(hash)
}

fn evidence_hash(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    let digest = digest.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

fn project_revision(document: &crate::project_file::FractalProject) -> Result<String> {
    let mut value = serde_json::to_value(document)?;
    let object = value
        .as_object_mut()
        .context("project document must encode as object")?;
    object.remove("external_gate_ledger");
    normalize_revision_numbers(&mut value);
    fractal_contracts::canonical_sha256(&value)
        .map_err(|error| anyhow::anyhow!("project revision hash failed: {error}"))
}

/// The project schema contains a few telemetry fields represented as JSON
/// floating-point numbers. The graph canonicalizer intentionally rejects
/// floats, so map them to a tagged deterministic string for this snapshot
/// token (the token is an equality check, not a semantic project hash).
fn normalize_revision_numbers(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(normalize_revision_numbers),
        Value::Object(values) => values.values_mut().for_each(normalize_revision_numbers),
        Value::Number(number) if number.is_f64() => {
            *value = Value::String(format!("__fractal_f64__{}", number));
        }
        _ => {}
    }
}

#[cfg(unix)]
fn read_evidence_openat(
    canonical_workspace: &Path,
    relative: &Path,
    rendered: &str,
) -> Result<Vec<u8>> {
    let mut directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(canonical_workspace)
        .with_context(|| format!("open repository for evidence {rendered}"))?;
    let components = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect::<Vec<_>>();
    let final_index = components
        .len()
        .checked_sub(1)
        .context("external gate evidence path is empty")?;
    for (index, part) in components.iter().enumerate() {
        let name = CString::new(part.as_bytes())
            .with_context(|| format!("external gate evidence path contains NUL: {rendered}"))?;
        let flags = if index == final_index {
            // O_NONBLOCK keeps a swapped-in FIFO/device from blocking before
            // the regular-file check below can fail closed.
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open evidence component {rendered}"));
        }
        if index == final_index {
            let mut file = unsafe { fs::File::from_raw_fd(fd) };
            let metadata = file
                .metadata()
                .with_context(|| format!("stat external gate evidence {rendered}"))?;
            if !metadata.is_file() {
                bail!("external gate evidence must be a regular file: {rendered}");
            }
            if metadata.len() > MAX_EVIDENCE_BYTES {
                bail!(
                    "external gate evidence exceeds {} bytes: {rendered}",
                    MAX_EVIDENCE_BYTES
                );
            }
            let before = metadata.len();
            let mut bytes = Vec::with_capacity(before as usize);
            file.read_to_end(&mut bytes)
                .with_context(|| format!("read external gate evidence {rendered}"))?;
            let after = file
                .metadata()
                .with_context(|| format!("restat external gate evidence {rendered}"))?
                .len();
            if after != before || bytes.len() as u64 > MAX_EVIDENCE_BYTES {
                bail!("external gate evidence changed while being read: {rendered}");
            }
            return Ok(bytes);
        }
        directory = unsafe { fs::File::from_raw_fd(fd) };
    }
    bail!("external gate evidence path is empty")
}

/// Reject absolute/traversal paths and every symlink component, then read the
/// evidence bytes. The returned path is normalized and repo-relative.
pub(crate) fn read_safe_evidence(workspace: &Path, input: &Path) -> Result<(String, Vec<u8>)> {
    if input.as_os_str().is_empty() || input.is_absolute() {
        bail!("external gate evidence path must be repo-relative");
    }
    let mut relative = PathBuf::new();
    for component in input.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir => bail!("external gate evidence path may not traverse parent"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("external gate evidence path must be repo-relative")
            }
        }
    }
    let rendered = relative.to_string_lossy().to_string();
    bounded_text(&rendered, MAX_PATH_BYTES, "evidence_path")?;
    let canonical_workspace = workspace
        .canonicalize()
        .with_context(|| format!("resolve repository {}", workspace.display()))?;
    let candidate = canonical_workspace.join(&relative);
    let mut current = canonical_workspace.clone();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("inspect evidence path {rendered}"))?;
        if metadata.file_type().is_symlink() {
            bail!("external gate evidence path may not contain symlinks: {rendered}");
        }
    }
    let metadata = fs::metadata(&candidate)
        .with_context(|| format!("read external gate evidence {rendered}"))?;
    if !metadata.is_file() {
        bail!("external gate evidence must be a regular file: {rendered}");
    }
    if metadata.len() > MAX_EVIDENCE_BYTES {
        bail!(
            "external gate evidence exceeds {} bytes: {rendered}",
            MAX_EVIDENCE_BYTES
        );
    }
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolve external gate evidence {rendered}"))?;
    if !canonical.starts_with(&canonical_workspace) {
        bail!("external gate evidence escapes the repository: {rendered}");
    }
    #[cfg(unix)]
    let bytes = read_evidence_openat(&canonical_workspace, &relative, &rendered)?;
    #[cfg(not(unix))]
    let bytes = {
        let bytes = fs::read(&candidate)
            .with_context(|| format!("read external gate evidence {rendered}"))?;
        let after = fs::metadata(&candidate)
            .with_context(|| format!("restat external gate evidence {rendered}"))?
            .len();
        if after != bytes.len() as u64 || after > MAX_EVIDENCE_BYTES {
            bail!("external gate evidence changed while being read: {rendered}");
        }
        bytes
    };
    Ok((rendered, bytes))
}

fn node_by_id<'a>(graph: &'a Value, node_id: &str) -> Option<&'a Value> {
    graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(node_id))
}

fn active_approval<'a>(
    ledger: &'a ExternalGateLedger,
    graph_hash: &str,
    node_id: &str,
    gate: &str,
) -> Option<&'a ExternalGateRecord> {
    let revoked = ledger
        .records
        .iter()
        .filter(|record| record.kind == "revocation")
        .filter_map(|record| record.revokes.as_deref())
        .collect::<BTreeSet<_>>();
    ledger.records.iter().rev().find(|record| {
        record.kind == "approval"
            && record.graph_hash == graph_hash
            && record.node_id == node_id
            && record.gate == gate
            && !revoked.contains(record.content_hash.as_str())
    })
}

/// Whether a node is effective at a scheduler frontier. This checks all ledger
/// and evidence facts but deliberately leaves reviewer-vs-checkout separation
/// to final canonical checkout (where the worker identity is known).
pub(crate) fn effective_for_scheduler(
    workspace: &Path,
    graph_hash: &str,
    node: &Value,
    ledger: Option<&ExternalGateLedger>,
) -> bool {
    let Ok(gates) = required_gates(node) else {
        return false;
    };
    if gates.is_empty() {
        return true;
    }
    let Some(ledger) = ledger else {
        return false;
    };
    if validate_ledger(ledger).is_err() {
        return false;
    }
    gates.iter().all(|gate| {
        let Some(approval) = active_approval(ledger, graph_hash, node_id(node), gate) else {
            return false;
        };
        if approval.role != required_role(gate) {
            return false;
        }
        read_safe_evidence(workspace, Path::new(&approval.evidence_path))
            .ok()
            .is_some_and(|(_, bytes)| {
                bytes.len() as u64 == approval.evidence_length
                    && evidence_hash(&bytes) == approval.evidence_hash
            })
    })
}

fn node_id(node: &Value) -> &str {
    node.get("id").and_then(Value::as_str).unwrap_or("")
}

/// Final TOCTOU authority for checkout. Missing/malformed/stale/revoked or
/// tampered approvals fail closed. A reviewer may never checkout their own node.
pub(crate) fn enforce_checkout(
    workspace: &Path,
    document: &crate::project_file::FractalProject,
    node_id: &str,
    checkout_agent_id: &str,
) -> Result<()> {
    let node = node_by_id(&document.graph, node_id)
        .with_context(|| format!("unknown graph node {node_id}"))?;
    let gates = required_gates(node)?;
    if gates.is_empty() {
        return Ok(());
    }
    let ledger = document
        .external_gate_ledger
        .as_ref()
        .context("external gate ledger is missing; gated checkout is denied")?;
    validate_ledger(ledger).context("external gate ledger is invalid; gated checkout is denied")?;
    let revoked = ledger
        .records
        .iter()
        .filter(|record| record.kind == "revocation")
        .filter_map(|record| record.revokes.as_deref())
        .collect::<BTreeSet<_>>();
    for gate in gates {
        let approval = active_approval(ledger, &document.graph_hash, node_id, &gate)
            .with_context(|| format!("external gate {gate} has no effective approval"))?;
        if revoked.contains(approval.content_hash.as_str()) {
            bail!("external gate {gate} approval is revoked");
        }
        if approval.role != required_role(&gate) {
            bail!(
                "external gate {gate} requires role {}",
                required_role(&gate)
            );
        }
        if approval.reviewer_id == checkout_agent_id {
            bail!("external gate {gate} reviewer and checkout agent must be different");
        }
        let (path, bytes) = read_safe_evidence(workspace, Path::new(&approval.evidence_path))
            .with_context(|| format!("external gate {gate} evidence is stale or unsafe"))?;
        if path != approval.evidence_path
            || bytes.len() as u64 != approval.evidence_length
            || evidence_hash(&bytes) != approval.evidence_hash
        {
            bail!("external gate {gate} evidence drift/tamper detected");
        }
    }
    Ok(())
}

pub(crate) fn scheduler_admitted(
    workspace: &Path,
    graph_hash: &str,
    node: &Value,
    ledger: Option<&ExternalGateLedger>,
) -> bool {
    effective_for_scheduler(workspace, graph_hash, node, ledger)
}

pub(crate) fn filter_frontier(
    workspace: &Path,
    graph_hash: &str,
    frontier: Vec<Value>,
    ledger: Option<&ExternalGateLedger>,
) -> Vec<Value> {
    frontier
        .into_iter()
        .filter(|node| scheduler_admitted(workspace, graph_hash, node, ledger))
        .collect()
}

fn ensure_not_worker() -> Result<()> {
    if std::env::var_os("FRACTAL_WORKER").is_some() {
        bail!("external gate record/revoke is refused under FRACTAL_WORKER");
    }
    Ok(())
}

fn ensure_node_gate(
    document: &crate::project_file::FractalProject,
    node_id: &str,
    gate: &str,
) -> Result<()> {
    let node = node_by_id(&document.graph, node_id)
        .with_context(|| format!("unknown graph node {node_id}"))?;
    if !required_gates(node)?.iter().any(|value| value == gate) {
        bail!("node {node_id} does not declare external gate {gate}");
    }
    Ok(())
}

fn validate_attestation(
    attestation: &str,
    kind: &str,
    graph_hash: &str,
    node_id: &str,
    gate: &str,
    target: Option<&str>,
) -> Result<()> {
    let expected = match target {
        Some(target) => format!("revoke:{graph_hash}:{node_id}:{gate}:{target}"),
        None => format!("approve:{graph_hash}:{node_id}:{gate}"),
    };
    if attestation != expected {
        bail!(
            "external gate attestation must exactly bind decision, graph, node, and gate: expected {expected}"
        );
    }
    let _ = kind;
    Ok(())
}

fn validate_identity(
    role: &str,
    attestation: &str,
    gate: &str,
    kind: &str,
    graph_hash: &str,
    node_id: &str,
    target: Option<&str>,
) -> Result<()> {
    bounded_text(role, MAX_ROLE_BYTES, "role")?;
    bounded_text(attestation, MAX_ATTESTATION_BYTES, "attestation")?;
    if role != required_role(gate) {
        bail!("gate {gate} requires role {}", required_role(gate));
    }
    validate_attestation(attestation, kind, graph_hash, node_id, gate, target)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn make_base_record(
    kind: &str,
    graph_hash: &str,
    node_id: &str,
    gate: &str,
    reviewer_id: &str,
    reviewer_label: &str,
    role: &str,
    attestation: &str,
    evidence_path: &str,
    evidence_hash: &str,
    evidence_length: u64,
    project_revision: &str,
    recorded_at: &str,
) -> ExternalGateRecord {
    ExternalGateRecord {
        schema: RECORD_SCHEMA.to_owned(),
        kind: kind.to_owned(),
        graph_hash: graph_hash.to_owned(),
        node_id: node_id.to_owned(),
        gate: gate.to_owned(),
        reviewer_id: reviewer_id.to_owned(),
        reviewer_label: reviewer_label.to_owned(),
        role: role.to_owned(),
        attestation: attestation.to_owned(),
        authority: LOCAL_AUTHORITY.to_owned(),
        assurance: LOCAL_ASSURANCE.to_owned(),
        evidence_path: evidence_path.to_owned(),
        evidence_hash: evidence_hash.to_owned(),
        evidence_length,
        project_revision: project_revision.to_owned(),
        recorded_at: recorded_at.to_owned(),
        previous_hash: None,
        revokes: None,
        content_hash: String::new(),
        extra: BTreeMap::new(),
    }
}

fn append_record(
    ledger: &mut ExternalGateLedger,
    mut record: ExternalGateRecord,
) -> Result<ExternalGateRecord> {
    validate_ledger(ledger)?;
    record.previous_hash = ledger
        .records
        .last()
        .map(|value| value.content_hash.clone());
    record.content_hash = record_content_hash(&record)?;
    validate_record_shape(&record)?;
    ledger.records.push(record.clone());
    validate_ledger(ledger)?;
    Ok(record)
}

fn record_approval_for_document(
    document: &crate::project_file::FractalProject,
    workspace: &Path,
    input: &RecordApprovalInput,
) -> Result<ExternalGateRecord> {
    ensure_node_gate(document, &input.node_id, &input.gate)?;
    bounded_text(&input.reviewer_id, MAX_REVIEWER_BYTES, "reviewer_id")?;
    bounded_text(&input.reviewer_label, MAX_REVIEWER_BYTES, "reviewer_label")?;
    validate_identity(
        &input.role,
        &input.attestation,
        &input.gate,
        "approval",
        &document.graph_hash,
        &input.node_id,
        None,
    )?;
    let (evidence_path, bytes) = read_safe_evidence(workspace, &input.evidence_path)?;
    let ledger = document
        .external_gate_ledger
        .clone()
        .unwrap_or_else(ExternalGateLedger::empty);
    if let Some(existing) =
        active_approval(&ledger, &document.graph_hash, &input.node_id, &input.gate)
    {
        bail!(
            "external gate already has effective approval {}",
            existing.content_hash
        );
    }
    let revision = project_revision(document)?;
    Ok(make_base_record(
        "approval",
        &document.graph_hash,
        &input.node_id,
        &input.gate,
        &input.reviewer_id,
        &input.reviewer_label,
        &input.role,
        &input.attestation,
        &evidence_path,
        &evidence_hash(&bytes),
        bytes.len() as u64,
        &revision,
        &document.updated_at,
    ))
}

fn record_approval_checked(
    workspace: &Path,
    input: RecordApprovalInput,
    expected_content_hash: Option<&str>,
) -> Result<ExternalGateRecord> {
    ensure_not_worker()?;
    let workspace = workspace
        .canonicalize()
        .context("resolve gate repository")?;
    let mut created = None;
    crate::project_file::mutate_document(&workspace, |document| {
        let record = record_approval_for_document(document, &workspace, &input)?;
        let ledger = document
            .external_gate_ledger
            .clone()
            .unwrap_or_else(ExternalGateLedger::empty);
        let mut candidate = ledger;
        let appended = append_record(&mut candidate, record)?;
        if expected_content_hash.is_some_and(|expected| expected != appended.content_hash) {
            bail!("external gate preview token no longer matches current document/evidence");
        }
        created = Some(appended);
        document.external_gate_ledger = Some(candidate);
        Ok(())
    })?;
    created.context("external gate approval was not recorded")
}

#[allow(dead_code)]
pub(crate) fn record_approval(
    workspace: &Path,
    input: RecordApprovalInput,
) -> Result<ExternalGateRecord> {
    record_approval_checked(workspace, input, None)
}

pub(crate) fn record_approval_with_expected(
    workspace: &Path,
    input: RecordApprovalInput,
    expected_content_hash: &str,
) -> Result<ExternalGateRecord> {
    record_approval_checked(workspace, input, Some(expected_content_hash))
}

pub(crate) fn preview_approval(
    workspace: &Path,
    input: &RecordApprovalInput,
) -> Result<ExternalGateRecord> {
    ensure_not_worker()?;
    let workspace = workspace
        .canonicalize()
        .context("resolve gate repository")?;
    let document = crate::project_file::load(&workspace)?;
    let record = record_approval_for_document(&document, &workspace, input)?;
    let mut ledger = document
        .external_gate_ledger
        .clone()
        .unwrap_or_else(ExternalGateLedger::empty);
    append_record(&mut ledger, record)
}

fn make_revocation(
    document: &crate::project_file::FractalProject,
    ledger: &ExternalGateLedger,
    input: &RevokeApprovalInput,
) -> Result<ExternalGateRecord> {
    validate_ledger(ledger)?;
    let target = ledger
        .records
        .iter()
        .find(|record| record.kind == "approval" && record.content_hash == input.approval_hash)
        .cloned()
        .context("exact approval content hash was not found")?;
    if target.graph_hash != document.graph_hash {
        bail!("approval is stale for the current graph hash");
    }
    if active_approval(ledger, &target.graph_hash, &target.node_id, &target.gate)
        .is_none_or(|record| record.content_hash != target.content_hash)
    {
        bail!("approval is already revoked or superseded");
    }
    bounded_text(&input.reviewer_id, MAX_REVIEWER_BYTES, "reviewer_id")?;
    bounded_text(&input.reviewer_label, MAX_REVIEWER_BYTES, "reviewer_label")?;
    validate_identity(
        &input.role,
        &input.attestation,
        &target.gate,
        "revocation",
        &target.graph_hash,
        &target.node_id,
        Some(&target.content_hash),
    )?;
    let revision = project_revision(document)?;
    let mut record = make_base_record(
        "revocation",
        &target.graph_hash,
        &target.node_id,
        &target.gate,
        &input.reviewer_id,
        &input.reviewer_label,
        &input.role,
        &input.attestation,
        &target.evidence_path,
        &target.evidence_hash,
        target.evidence_length,
        &revision,
        &document.updated_at,
    );
    record.revokes = Some(target.content_hash);
    Ok(record)
}

fn revoke_approval_checked(
    workspace: &Path,
    input: RevokeApprovalInput,
    expected_content_hash: Option<&str>,
) -> Result<ExternalGateRecord> {
    ensure_not_worker()?;
    let workspace = workspace
        .canonicalize()
        .context("resolve gate repository")?;
    let mut created = None;
    crate::project_file::mutate_document(&workspace, |document| {
        let current = document
            .external_gate_ledger
            .clone()
            .context("external gate ledger is missing; cannot revoke approval")?;
        let record = make_revocation(document, &current, &input)?;
        let mut ledger = current;
        let appended = append_record(&mut ledger, record)?;
        if expected_content_hash.is_some_and(|expected| expected != appended.content_hash) {
            bail!("external gate preview token no longer matches current document/evidence");
        }
        created = Some(appended);
        document.external_gate_ledger = Some(ledger);
        Ok(())
    })?;
    created.context("external gate revocation was not recorded")
}

#[allow(dead_code)]
pub(crate) fn revoke_approval(
    workspace: &Path,
    input: RevokeApprovalInput,
) -> Result<ExternalGateRecord> {
    revoke_approval_checked(workspace, input, None)
}

pub(crate) fn revoke_approval_with_expected(
    workspace: &Path,
    input: RevokeApprovalInput,
    expected_content_hash: &str,
) -> Result<ExternalGateRecord> {
    revoke_approval_checked(workspace, input, Some(expected_content_hash))
}

pub(crate) fn preview_revoke(
    workspace: &Path,
    input: &RevokeApprovalInput,
) -> Result<ExternalGateRecord> {
    ensure_not_worker()?;
    let workspace = workspace
        .canonicalize()
        .context("resolve gate repository")?;
    let document = crate::project_file::load(&workspace)?;
    let ledger = document
        .external_gate_ledger
        .as_ref()
        .context("external gate ledger is missing; cannot revoke approval")?;
    let record = make_revocation(&document, ledger, input)?;
    let mut clone = ledger.clone();
    append_record(&mut clone, record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn workspace(name: &str) -> PathBuf {
        let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fractal-external-gate-{name}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(path.join(".fractal")).unwrap();
        path
    }

    fn project_with_gate(name: &str) -> PathBuf {
        let path = workspace(name);
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "gate-test",
            "nodes": [
                {"id": "secure", "title": "secure", "capability": "code.generate",
                 "external_gates": ["security_review"]},
                {"id": "plain", "title": "plain", "capability": "code.generate"}
            ],
            "edges": []
        });
        crate::graph_store::rehash_graph(&mut graph).unwrap();
        crate::project_file::persist(&path, &graph, "gate-test").unwrap();
        fs::write(path.join("review.txt"), b"review evidence").unwrap();
        path
    }

    fn approval_input(path: &Path, reviewer: &str) -> RecordApprovalInput {
        let document = crate::project_file::load(path).unwrap();
        RecordApprovalInput {
            node_id: "secure".to_owned(),
            gate: "security_review".to_owned(),
            evidence_path: PathBuf::from("review.txt"),
            reviewer_id: reviewer.to_owned(),
            reviewer_label: reviewer.to_owned(),
            role: "security_reviewer".to_owned(),
            attestation: format!("approve:{}:secure:security_review", document.graph_hash),
        }
    }

    #[test]
    fn malformed_gate_declarations_fail_closed() {
        assert!(required_gates(&json!({"external_gates": 3})).is_err());
        assert!(required_gates(&json!({"external_gates": [null]})).is_err());
        assert!(required_gates(&json!({
            "required_gates": [],
            "external_gates": ["security_review"]
        }))
        .is_ok_and(|gates| gates == vec!["security_review"]));
        assert!(required_gates(&json!({
            "required_gates": ["security_review"],
            "external_gates": 3
        }))
        .is_err());
        assert_eq!(required_gates(&json!({})).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn preview_apply_hashes_match_and_checkout_enforces_sod_and_drift() {
        let path = project_with_gate("preview");
        let input = approval_input(&path, "reviewer");
        let before_hash = crate::project_file::load(&path).unwrap().graph_hash;
        let preview = preview_approval(&path, &input).unwrap();
        let applied = record_approval(&path, input).unwrap();
        assert_eq!(preview.content_hash, applied.content_hash);
        assert_eq!(
            before_hash,
            crate::project_file::load(&path).unwrap().graph_hash
        );
        let denied =
            crate::project_file::checkout_start_node(&path, "secure", "reviewer", "Reviewer");
        assert!(denied.is_err());
        crate::project_file::checkout_start_node(&path, "secure", "worker", "Worker").unwrap();
        let document = crate::project_file::load(&path).unwrap();
        let secure_node = document.graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node.get("id").and_then(Value::as_str) == Some("secure"))
            .unwrap();
        assert!(scheduler_admitted(
            &path,
            &document.graph_hash,
            secure_node,
            document.external_gate_ledger.as_ref()
        ));
        fs::write(path.join("review.txt"), b"tampered").unwrap();
        assert!(enforce_checkout(&path, &document, "secure", "another-worker").is_err());
        fs::remove_file(path.join("review.txt")).unwrap();
        let current = crate::project_file::load(&path).unwrap();
        assert!(enforce_checkout(&path, &current, "secure", "another-worker").is_err());
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn revoke_is_exact_and_reapproval_is_allowed() {
        let path = project_with_gate("revoke");
        let input = approval_input(&path, "reviewer");
        let approval = record_approval(&path, input).unwrap();
        let document = crate::project_file::load(&path).unwrap();
        let revoke_input = RevokeApprovalInput {
            approval_hash: approval.content_hash.clone(),
            reviewer_id: "revoker".to_owned(),
            reviewer_label: "revoker".to_owned(),
            role: "security_reviewer".to_owned(),
            attestation: format!(
                "revoke:{}:secure:security_review:{}",
                document.graph_hash, approval.content_hash
            ),
        };
        let revocation = revoke_approval(&path, revoke_input).unwrap();
        assert_eq!(
            revocation.revokes.as_deref(),
            Some(approval.content_hash.as_str())
        );
        let current = crate::project_file::load(&path).unwrap();
        let secure_node = current.graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node.get("id").and_then(Value::as_str) == Some("secure"))
            .unwrap();
        assert!(!scheduler_admitted(
            &path,
            &current.graph_hash,
            secure_node,
            current.external_gate_ledger.as_ref()
        ));
        let denied = crate::project_file::checkout_start_node(&path, "secure", "worker", "Worker");
        assert!(denied.is_err());
        let reapproved = record_approval(&path, approval_input(&path, "reviewer-2")).unwrap();
        assert_ne!(approval.content_hash, reapproved.content_hash);
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn graph_evolution_invalidates_prior_gate_approval() {
        let path = project_with_gate("evolved");
        let input = approval_input(&path, "reviewer");
        let approval = record_approval(&path, input).unwrap();
        let before = crate::project_file::load(&path).unwrap();
        let mut child = before.graph.clone();
        child["parent_graph"] = Value::String(before.graph_hash.clone());
        child["nodes"][0]["instruction"] = Value::String("changed after review".to_owned());
        child.as_object_mut().unwrap().remove("graph_hash");
        crate::graph_store::rehash_graph(&mut child).unwrap();
        crate::project_file::persist_evolved_if_parent(&path, &child, &before.graph_hash).unwrap();

        let after = crate::project_file::load(&path).unwrap();
        assert_ne!(after.graph_hash, before.graph_hash);
        assert_eq!(
            after.external_gate_ledger.as_ref().unwrap().records[0].content_hash,
            approval.content_hash
        );
        let secure_node = after.graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node.get("id").and_then(Value::as_str) == Some("secure"))
            .unwrap();
        assert!(!scheduler_admitted(
            &path,
            &after.graph_hash,
            secure_node,
            after.external_gate_ledger.as_ref()
        ));
        assert!(
            crate::project_file::checkout_start_node(&path, "secure", "worker", "Worker").is_err()
        );
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn preview_token_rejects_worker_transition_ledger_append_and_evidence_drift() {
        let path = project_with_gate("preview-drift");
        let input = approval_input(&path, "reviewer");
        let preview = preview_approval(&path, &input).unwrap();

        // A real worker checkout mutates the canonical project document and
        // therefore invalidates the preview hash before the gated append.
        crate::project_file::checkout_start_node(&path, "plain", "worker", "Worker").unwrap();
        assert!(
            record_approval_with_expected(&path, input.clone(), &preview.content_hash).is_err()
        );
        assert!(crate::project_file::load(&path)
            .unwrap()
            .external_gate_ledger
            .is_none());

        // A competing ledger append is rejected rather than silently creating
        // a second record from a stale preview.
        let approval = record_approval(&path, input.clone()).unwrap();
        assert!(
            record_approval_with_expected(&path, input.clone(), &preview.content_hash).is_err()
        );
        assert_eq!(
            crate::project_file::load(&path)
                .unwrap()
                .external_gate_ledger
                .unwrap()
                .records
                .iter()
                .filter(|record| record.kind == "approval")
                .count(),
            1
        );

        // Revocation previews bind to the exact target. Revocation does not
        // reread the old evidence, so an evidence edit remains auditable but
        // does not prevent removing that stale approval.
        let document = crate::project_file::load(&path).unwrap();
        let revoke = RevokeApprovalInput {
            approval_hash: approval.content_hash.clone(),
            reviewer_id: "revoker".to_owned(),
            reviewer_label: "Revoker".to_owned(),
            role: "security_reviewer".to_owned(),
            attestation: format!(
                "revoke:{}:secure:security_review:{}",
                document.graph_hash, approval.content_hash
            ),
        };
        let revoke_preview = preview_revoke(&path, &revoke).unwrap();
        fs::write(path.join("review.txt"), b"evidence drift").unwrap();
        assert!(revoke_approval_with_expected(&path, revoke, &revoke_preview.content_hash).is_ok());
        // Revocation itself is allowed even when old evidence is now stale;
        // the stale approval is no longer effective and remains auditable.
        let _ = fs::remove_dir_all(path);

        // Approval previews must fail closed when the evidence bytes drift
        // before the worker supplies the expected preview token.
        let path = project_with_gate("preview-evidence-drift");
        let input = approval_input(&path, "reviewer");
        let preview = preview_approval(&path, &input).unwrap();
        fs::write(path.join("review.txt"), b"evidence drift").unwrap();
        assert!(record_approval_with_expected(&path, input, &preview.content_hash).is_err());
        assert!(crate::project_file::load(&path)
            .unwrap()
            .external_gate_ledger
            .is_none());
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn evidence_traversal_symlink_and_size_are_refused() {
        let path = project_with_gate("paths");
        assert!(read_safe_evidence(&path, Path::new("../outside")).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(path.join("review.txt"), path.join("link.txt")).unwrap();
            assert!(read_safe_evidence(&path, Path::new("link.txt")).is_err());
            fs::create_dir_all(path.join("nested")).unwrap();
            fs::write(path.join("nested/review.txt"), b"nested").unwrap();
            std::os::unix::fs::symlink(path.join("review.txt"), path.join("nested/link.txt"))
                .unwrap();
            assert!(read_safe_evidence(&path, Path::new("nested/link.txt")).is_err());

            // A FIFO must fail promptly rather than blocking the evidence
            // reader before it can reject the non-regular file.
            use std::os::unix::ffi::OsStrExt;
            let fifo = path.join("review.fifo");
            let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
            let started = std::time::Instant::now();
            assert!(read_safe_evidence(&path, Path::new("review.fifo")).is_err());
            assert!(started.elapsed() < std::time::Duration::from_secs(1));
        }
        let giant = path.join("giant.bin");
        let file = fs::File::create(&giant).unwrap();
        file.set_len(MAX_EVIDENCE_BYTES + 1).unwrap();
        assert!(read_safe_evidence(&path, Path::new("giant.bin")).is_err());
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn ledger_rejects_oversized_evidence_length_even_with_rehashed_record() {
        let path = project_with_gate("ledger-size");
        let approval = record_approval(&path, approval_input(&path, "reviewer")).unwrap();
        let mut ledger = crate::project_file::load(&path)
            .unwrap()
            .external_gate_ledger
            .unwrap();
        let record = ledger.records.first_mut().unwrap();
        assert_eq!(record.content_hash, approval.content_hash);
        record.evidence_length = MAX_EVIDENCE_BYTES + 1;
        record.content_hash = record_content_hash(record).unwrap();
        assert!(validate_ledger(&ledger).is_err());
        let _ = fs::remove_dir_all(path);
    }
}
