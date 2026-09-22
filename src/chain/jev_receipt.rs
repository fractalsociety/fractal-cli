//! Host-anchored JEV route outcome receipts.
//!
//! These receipts are deliberately an audit boundary, not a provider billing
//! or verification authority.  A command's selected model is observable
//! configuration; it is not proof of the backend that served a request.  The
//! host context passed to [`validate_host_anchored_receipt`] is intentionally
//! not serialized into the receipt: a worker can reproduce a public ledger
//! signature, but cannot manufacture host observations supplied out of band.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const ROUTE_RECEIPT_SCHEMA: &str = "fractal.jev.route_receipt.v1";
pub(crate) const UNKNOWN: &str = "unknown";

/// A numeric usage field, or an explicit absence of a provider usage receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsageValue {
    Unknown,
    Known(u64),
}

impl Serialize for UsageValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Unknown => serializer.serialize_str(UNKNOWN),
            Self::Known(value) => serializer.serialize_u64(*value),
        }
    }
}

impl<'de> Deserialize<'de> for UsageValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct UsageVisitor;

        impl<'de> serde::de::Visitor<'de> for UsageVisitor {
            type Value = UsageValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a non-negative integer or the string unknown")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UsageValue::Known(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value >= 0 {
                    Ok(UsageValue::Known(value as u64))
                } else {
                    Err(E::custom("usage counts cannot be negative"))
                }
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == UNKNOWN {
                    Ok(UsageValue::Unknown)
                } else {
                    Err(E::custom("usage strings must be unknown"))
                }
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_str(&value)
            }
        }

        deserializer.deserialize_any(UsageVisitor)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RouteUsageEstimate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cost_micros: Option<u64>,
}

/// Actual usage is never inferred from estimates or from command selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RouteUsage {
    pub(crate) input_tokens: UsageValue,
    pub(crate) output_tokens: UsageValue,
    pub(crate) cached_input_tokens: UsageValue,
    pub(crate) cost_micros: UsageValue,
    pub(crate) usage_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider_receipt_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) estimate: Option<RouteUsageEstimate>,
}

impl RouteUsage {
    pub(crate) fn unknown() -> Self {
        Self {
            input_tokens: UsageValue::Unknown,
            output_tokens: UsageValue::Unknown,
            cached_input_tokens: UsageValue::Unknown,
            cost_micros: UsageValue::Unknown,
            usage_source: "unavailable".to_owned(),
            provider_receipt_ref: None,
            estimate: None,
        }
    }

    fn all_unknown(&self) -> bool {
        matches!(
            (
                self.input_tokens,
                self.output_tokens,
                self.cached_input_tokens,
                self.cost_micros
            ),
            (
                UsageValue::Unknown,
                UsageValue::Unknown,
                UsageValue::Unknown,
                UsageValue::Unknown
            )
        )
    }

    fn all_known(&self) -> bool {
        matches!(
            (
                self.input_tokens,
                self.output_tokens,
                self.cached_input_tokens,
                self.cost_micros
            ),
            (
                UsageValue::Known(_),
                UsageValue::Known(_),
                UsageValue::Known(_),
                UsageValue::Known(_)
            )
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RouteProvenance {
    pub(crate) cli_family: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) revision: String,
    pub(crate) source: String,
}

impl RouteProvenance {
    pub(crate) fn unknown(cli_family: impl Into<String>) -> Self {
        Self {
            cli_family: cli_family.into(),
            provider: UNKNOWN.to_owned(),
            model: UNKNOWN.to_owned(),
            revision: UNKNOWN.to_owned(),
            source: "host-observed-command-config;backend-unverified".to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AuthorizationEvidenceRef {
    pub(crate) reference: String,
}

impl AuthorizationEvidenceRef {
    #[allow(dead_code)]
    pub(crate) fn new(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RouteVerification {
    pub(crate) verification_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verifier_identity: Option<String>,
    pub(crate) identity_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) independent_verifier: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) evidence_refs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RouteInvocation {
    pub(crate) cli_family: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) selected_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) selected_effort: Option<String>,
    pub(crate) configuration_source: String,
}

impl RouteInvocation {
    pub(crate) fn unknown(agent: impl Into<String>) -> Self {
        Self {
            cli_family: agent.into(),
            selected_model: None,
            selected_effort: None,
            configuration_source: "unavailable".to_owned(),
        }
    }
}

/// The serializable receipt. Public fields are status and provenance metadata;
/// no prompt, response, credential, or private trace is carried here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RouteReceiptV1 {
    pub(crate) schema: String,
    pub(crate) receipt_id: String,
    pub(crate) project_id: String,
    pub(crate) graph_id: String,
    pub(crate) graph_hash: String,
    pub(crate) node_id: String,
    pub(crate) attempt: u32,
    pub(crate) ledger_subject: String,
    pub(crate) node_objective_hash: String,
    pub(crate) request_hash: String,
    pub(crate) response_hash: String,
    pub(crate) policy_hash: String,
    pub(crate) roster_hash: String,
    pub(crate) eligibility_hash: String,
    pub(crate) model_hash: String,
    pub(crate) decision_envelope_hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) eligibility_mask: Vec<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) authorization_evidence_refs: Vec<AuthorizationEvidenceRef>,
    pub(crate) invocation: RouteInvocation,
    pub(crate) proposed_route_id: String,
    pub(crate) observed_baseline_route_id: String,
    pub(crate) actual_route_id: String,
    pub(crate) actual_route_provenance: RouteProvenance,
    pub(crate) decision_kind: String,
    pub(crate) fallback_used: bool,
    pub(crate) shadow: bool,
    pub(crate) canary: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) reason_codes: Vec<String>,
    pub(crate) decision_timestamp_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) queue_started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) process_started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) process_ended_at_ms: Option<u64>,
    pub(crate) process_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) exit_status: Option<i32>,
    pub(crate) retries: u32,
    pub(crate) usage: RouteUsage,
    pub(crate) process_success: bool,
    pub(crate) task_correctness: String,
    pub(crate) verification: RouteVerification,
    pub(crate) evidence_root_hash: String,
    pub(crate) receipt_hash: String,
}

/// Usage observations captured by the trusted host/provider boundary. This is
/// deliberately not serializable: a transported receipt cannot turn a public
/// source/ref string into an authoritative usage record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostObservedUsage {
    pub(crate) input_tokens: UsageValue,
    pub(crate) output_tokens: UsageValue,
    pub(crate) cached_input_tokens: UsageValue,
    pub(crate) cost_micros: UsageValue,
    pub(crate) usage_source: String,
    pub(crate) provider_receipt_ref: Option<String>,
}

impl HostObservedUsage {
    pub(crate) fn unknown() -> Self {
        Self {
            input_tokens: UsageValue::Unknown,
            output_tokens: UsageValue::Unknown,
            cached_input_tokens: UsageValue::Unknown,
            cost_micros: UsageValue::Unknown,
            usage_source: "unavailable".to_owned(),
            provider_receipt_ref: None,
        }
    }
}

/// Decision identity observed by the trusted host. It includes the complete
/// public decision envelope relevant to routing, including roster/mask
/// alignment. It cannot be reconstructed from a worker-authored receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostObservedDecision {
    pub(crate) request_hash: String,
    pub(crate) response_hash: String,
    pub(crate) policy_hash: String,
    pub(crate) roster_hash: String,
    pub(crate) eligibility_hash: String,
    pub(crate) model_hash: String,
    pub(crate) decision_envelope_hash: String,
    pub(crate) eligibility_mask: Vec<bool>,
    pub(crate) authorization_evidence_refs: Vec<AuthorizationEvidenceRef>,
    pub(crate) proposed_route_id: String,
    pub(crate) observed_baseline_route_id: String,
    pub(crate) decision_kind: String,
    pub(crate) fallback_used: bool,
    pub(crate) shadow: bool,
    pub(crate) canary: bool,
    pub(crate) reason_codes: Vec<String>,
}

impl HostObservedDecision {
    pub(crate) fn unknown() -> Self {
        Self {
            request_hash: UNKNOWN.to_owned(),
            response_hash: UNKNOWN.to_owned(),
            policy_hash: UNKNOWN.to_owned(),
            roster_hash: UNKNOWN.to_owned(),
            eligibility_hash: UNKNOWN.to_owned(),
            model_hash: UNKNOWN.to_owned(),
            decision_envelope_hash: UNKNOWN.to_owned(),
            eligibility_mask: Vec::new(),
            authorization_evidence_refs: Vec::new(),
            proposed_route_id: UNKNOWN.to_owned(),
            observed_baseline_route_id: UNKNOWN.to_owned(),
            decision_kind: "deterministic_fallback".to_owned(),
            fallback_used: true,
            shadow: true,
            canary: false,
            reason_codes: vec!["JEV_RECEIPT_NO_SCORER_AUTHORITY".to_owned()],
        }
    }
}

/// Host observations used to validate an untrusted/transported receipt.
/// This type is not serializable on purpose and has no self-asserted
/// authorization flag that could be copied into an audit payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostObservedContext {
    pub(crate) project_id: String,
    pub(crate) graph_id: String,
    pub(crate) graph_hash: String,
    pub(crate) node_id: String,
    pub(crate) attempt: u32,
    pub(crate) worker_identity: String,
    pub(crate) ledger_subject: String,
    pub(crate) actual_route_id: String,
    pub(crate) actual_route_provenance: RouteProvenance,
    pub(crate) invocation: RouteInvocation,
    pub(crate) process_status: String,
    pub(crate) process_started_at_ms: Option<u64>,
    pub(crate) process_ended_at_ms: Option<u64>,
    pub(crate) exit_status: Option<i32>,
    pub(crate) process_success: bool,
    pub(crate) usage: HostObservedUsage,
    pub(crate) decision: HostObservedDecision,
    pub(crate) verifier_identity: Option<String>,
    pub(crate) verification_evidence_ref: Option<String>,
    pub(crate) independent_verifier: Option<bool>,
    pub(crate) task_correctness: String,
    pub(crate) verification_status: String,
    pub(crate) evidence_root_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RouteReceiptValidation {
    pub(crate) valid: bool,
    pub(crate) training_ready: bool,
    pub(crate) reasons: Vec<String>,
}

impl RouteReceiptValidation {
    pub(crate) fn is_valid(&self) -> bool {
        self.valid
    }

    pub(crate) fn is_training_ready(&self) -> bool {
        self.training_ready
    }
}

/// Hash a JSON route/receipt payload using the shared canonical JSON contract.
pub(crate) fn canonical_route_hash(
    value: &Value,
) -> Result<String, fractal_contracts::CanonicalJsonError> {
    fractal_contracts::canonical_sha256(value)
}

fn receipt_hash(receipt: &RouteReceiptV1) -> Result<String, String> {
    let mut value = serde_json::to_value(receipt).map_err(|error| error.to_string())?;
    if let Value::Object(object) = &mut value {
        object.remove("receipt_hash");
    }
    canonical_route_hash(&value).map_err(|error| error.to_string())
}

fn expected_ledger_subject(graph_id: &str, node_id: &str, attempt: u32) -> String {
    format!("{graph_id}#{node_id}#{attempt}")
}

/// Validate a receipt against observations made by the trusted host.
///
/// `valid` means the receipt is structurally consistent with the host and can
/// be retained for audit.  `training_ready` is stricter: unknown provider
/// provenance, usage, or independent verification remains explicitly unsafe
/// for labels and promotion.
pub(crate) fn validate_host_anchored_receipt(
    receipt: &RouteReceiptV1,
    host: &HostObservedContext,
) -> RouteReceiptValidation {
    let mut reasons = Vec::new();
    let mut training_blockers = Vec::new();
    if receipt.schema != ROUTE_RECEIPT_SCHEMA {
        reasons.push("schema_mismatch".to_owned());
    }
    match receipt_hash(receipt) {
        Ok(expected) if expected == receipt.receipt_hash => {}
        Ok(_) => reasons.push("receipt_hash_mismatch".to_owned()),
        Err(_) => reasons.push("receipt_hash_uncomputable".to_owned()),
    }
    for (label, actual, expected) in [
        ("project", &receipt.project_id, &host.project_id),
        ("graph_id", &receipt.graph_id, &host.graph_id),
        ("graph_hash", &receipt.graph_hash, &host.graph_hash),
        ("node", &receipt.node_id, &host.node_id),
        (
            "ledger_subject",
            &receipt.ledger_subject,
            &host.ledger_subject,
        ),
        (
            "actual_route_id",
            &receipt.actual_route_id,
            &host.actual_route_id,
        ),
        (
            "evidence_root_hash",
            &receipt.evidence_root_hash,
            &host.evidence_root_hash,
        ),
    ] {
        if actual != expected {
            reasons.push(format!("{label}_mismatch"));
        }
    }
    if receipt.attempt != host.attempt {
        reasons.push("attempt_mismatch".to_owned());
    }
    if receipt.ledger_subject
        != expected_ledger_subject(&host.graph_id, &host.node_id, host.attempt)
    {
        reasons.push("invalid_ledger_subject".to_owned());
    }
    if receipt.actual_route_provenance != host.actual_route_provenance {
        reasons.push("route_provenance_mismatch".to_owned());
    }
    if receipt.invocation != host.invocation {
        reasons.push("invocation_observation_mismatch".to_owned());
    }
    if receipt.process_status != host.process_status
        || receipt.process_started_at_ms != host.process_started_at_ms
        || receipt.process_ended_at_ms != host.process_ended_at_ms
        || receipt.exit_status != host.exit_status
        || receipt.process_success != host.process_success
    {
        reasons.push("process_observation_mismatch".to_owned());
    }
    if receipt.process_success && receipt.process_status != "exited_success" {
        reasons.push("success_without_exited_process".to_owned());
    }
    if receipt.process_status == "not_executed"
        && (receipt.process_success
            || receipt.process_started_at_ms.is_some()
            || receipt.process_ended_at_ms.is_some())
    {
        reasons.push("nonexistent_process_claimed_executed".to_owned());
    }
    if let (Some(start), Some(end)) = (receipt.process_started_at_ms, receipt.process_ended_at_ms) {
        if start > end {
            reasons.push("process_timestamps_not_monotonic".to_owned());
        }
        if let Some(queue) = receipt.queue_started_at_ms {
            if queue > start {
                reasons.push("queue_timestamp_after_process_start".to_owned());
            }
        }
    }

    let receipt_usage = HostObservedUsage {
        input_tokens: receipt.usage.input_tokens,
        output_tokens: receipt.usage.output_tokens,
        cached_input_tokens: receipt.usage.cached_input_tokens,
        cost_micros: receipt.usage.cost_micros,
        usage_source: receipt.usage.usage_source.clone(),
        provider_receipt_ref: receipt.usage.provider_receipt_ref.clone(),
    };
    if receipt_usage != host.usage {
        reasons.push("usage_observation_mismatch".to_owned());
    }

    let decision_mismatch = receipt.request_hash != host.decision.request_hash
        || receipt.response_hash != host.decision.response_hash
        || receipt.policy_hash != host.decision.policy_hash
        || receipt.roster_hash != host.decision.roster_hash
        || receipt.eligibility_hash != host.decision.eligibility_hash
        || receipt.model_hash != host.decision.model_hash
        || receipt.decision_envelope_hash != host.decision.decision_envelope_hash
        || receipt.eligibility_mask != host.decision.eligibility_mask
        || receipt.authorization_evidence_refs != host.decision.authorization_evidence_refs
        || receipt.proposed_route_id != host.decision.proposed_route_id
        || receipt.observed_baseline_route_id != host.decision.observed_baseline_route_id
        || receipt.decision_kind != host.decision.decision_kind
        || receipt.fallback_used != host.decision.fallback_used
        || receipt.shadow != host.decision.shadow
        || receipt.canary != host.decision.canary
        || receipt.reason_codes != host.decision.reason_codes;
    if decision_mismatch {
        reasons.push("decision_observation_mismatch".to_owned());
    }

    if receipt.task_correctness != host.task_correctness {
        reasons.push("task_correctness_observation_mismatch".to_owned());
    }
    if receipt.verification.verification_status != host.verification_status {
        reasons.push("verification_status_observation_mismatch".to_owned());
    }

    if let (UsageValue::Known(input), UsageValue::Known(cached)) = (
        receipt.usage.input_tokens.clone(),
        receipt.usage.cached_input_tokens.clone(),
    ) {
        if cached > input {
            reasons.push("cached_input_tokens_exceed_input_tokens".to_owned());
        }
    }
    let usage_known = !receipt.usage.all_unknown();
    if usage_known
        && (receipt.usage.usage_source == "unavailable"
            || receipt
                .usage
                .provider_receipt_ref
                .as_deref()
                .is_none_or(str::is_empty))
    {
        reasons.push("actual_usage_missing_receipt_source".to_owned());
    }
    if !receipt.usage.all_known()
        || receipt.usage.usage_source == "unavailable"
        || receipt.usage.provider_receipt_ref.is_none()
    {
        training_blockers.push("usage_unavailable_or_incomplete".to_owned());
    }
    if receipt.actual_route_provenance.provider == UNKNOWN
        || receipt.actual_route_provenance.model == UNKNOWN
        || receipt.actual_route_provenance.revision == UNKNOWN
    {
        training_blockers.push("backend_provenance_unknown".to_owned());
    }
    if matches!(
        receipt.verification.verification_status.as_str(),
        "passed" | "failed"
    ) && (receipt.verification.verifier_identity.is_none()
        || receipt.verification.identity_source != "trusted-host-ledger"
        || host
            .verification_evidence_ref
            .as_ref()
            .is_none_or(|reference| !receipt.verification.evidence_refs.contains(reference)))
    {
        reasons.push("verification_authority_missing".to_owned());
    }
    if receipt.verification.verifier_identity != host.verifier_identity
        || receipt.verification.independent_verifier != host.independent_verifier
    {
        reasons.push("verification_observation_mismatch".to_owned());
    }
    if receipt.verification.independent_verifier == Some(true)
        && receipt.verification.verifier_identity.as_deref() == Some(host.worker_identity.as_str())
    {
        reasons.push("same_worker_claimed_independent".to_owned());
    }
    if receipt.verification.independent_verifier != Some(true) {
        training_blockers.push("independent_verifier_unknown".to_owned());
    }
    if receipt.task_correctness == UNKNOWN {
        training_blockers.push("task_correctness_unknown".to_owned());
    }
    if matches!(
        receipt.verification.verification_status.as_str(),
        UNKNOWN | "pending" | "not_applicable"
    ) {
        training_blockers.push("verification_outcome_unknown".to_owned());
    }
    if !matches!(
        (
            receipt.task_correctness.as_str(),
            receipt.verification.verification_status.as_str(),
        ),
        ("true", "passed") | ("false", "failed")
    ) {
        training_blockers.push("training_outcome_pair_not_allowed".to_owned());
    }
    if [
        &receipt.request_hash,
        &receipt.response_hash,
        &receipt.policy_hash,
        &receipt.roster_hash,
        &receipt.eligibility_hash,
        &receipt.model_hash,
        &receipt.decision_envelope_hash,
    ]
    .iter()
    .any(|value| value.as_str() == UNKNOWN)
    {
        training_blockers.push("decision_authority_hashes_unknown".to_owned());
    }

    RouteReceiptValidation {
        valid: reasons.is_empty(),
        training_ready: reasons.is_empty() && training_blockers.is_empty(),
        reasons: reasons.into_iter().chain(training_blockers).collect(),
    }
}

impl RouteReceiptV1 {
    /// Build a receipt from host observations and command-selected config.
    /// Unknowns are retained instead of being filled with route/cost guesses.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_host_observation(
        project_id: String,
        graph_id: String,
        graph_hash: String,
        node_id: String,
        attempt: u32,
        node_objective_hash: String,
        invocation: RouteInvocation,
        actual_route_id: String,
        actual_route_provenance: RouteProvenance,
        process_status: String,
        process_started_at_ms: Option<u64>,
        process_ended_at_ms: Option<u64>,
        exit_status: Option<i32>,
        process_success: bool,
        verifier_identity: Option<String>,
        verification_status: String,
        independent_verifier: Option<bool>,
        verification_evidence_ref: Option<String>,
        evidence_root_hash: String,
        retries: u32,
    ) -> Self {
        let ledger_subject = expected_ledger_subject(&graph_id, &node_id, attempt);
        let receipt_id = format!("{graph_id}/{node_id}/attempt-{attempt}");
        let mut receipt = Self {
            schema: ROUTE_RECEIPT_SCHEMA.to_owned(),
            receipt_id,
            project_id,
            graph_id,
            graph_hash,
            node_id,
            attempt,
            ledger_subject,
            node_objective_hash,
            request_hash: UNKNOWN.to_owned(),
            response_hash: UNKNOWN.to_owned(),
            policy_hash: UNKNOWN.to_owned(),
            roster_hash: UNKNOWN.to_owned(),
            eligibility_hash: UNKNOWN.to_owned(),
            model_hash: UNKNOWN.to_owned(),
            decision_envelope_hash: UNKNOWN.to_owned(),
            eligibility_mask: Vec::new(),
            authorization_evidence_refs: Vec::new(),
            invocation,
            proposed_route_id: UNKNOWN.to_owned(),
            observed_baseline_route_id: UNKNOWN.to_owned(),
            actual_route_id,
            actual_route_provenance,
            decision_kind: "deterministic_fallback".to_owned(),
            fallback_used: true,
            shadow: true,
            canary: false,
            reason_codes: vec!["JEV_RECEIPT_NO_SCORER_AUTHORITY".to_owned()],
            decision_timestamp_ms: process_started_at_ms.unwrap_or(0),
            queue_started_at_ms: process_started_at_ms,
            process_started_at_ms,
            process_ended_at_ms,
            process_status,
            exit_status,
            retries,
            usage: RouteUsage::unknown(),
            process_success,
            task_correctness: UNKNOWN.to_owned(),
            verification: RouteVerification {
                verification_status,
                verifier_identity,
                identity_source: "trusted-host-ledger".to_owned(),
                independent_verifier,
                evidence_refs: verification_evidence_ref.into_iter().collect(),
            },
            evidence_root_hash,
            receipt_hash: String::new(),
        };
        receipt.receipt_hash = receipt_hash(&receipt).unwrap_or_else(|_| UNKNOWN.to_owned());
        receipt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostObservedContext {
        HostObservedContext {
            project_id: "project".to_owned(),
            graph_id: "graph".to_owned(),
            graph_hash: "sha256:graph".to_owned(),
            node_id: "node".to_owned(),
            attempt: 2,
            worker_identity: "worker".to_owned(),
            ledger_subject: "graph#node#2".to_owned(),
            actual_route_id: "legacy:codex-cli:backend-unknown:model-unknown:high".to_owned(),
            actual_route_provenance: RouteProvenance::unknown("codex-cli"),
            invocation: RouteInvocation::unknown("codex-cli"),
            process_status: "exited_success".to_owned(),
            process_started_at_ms: Some(10),
            process_ended_at_ms: Some(20),
            exit_status: Some(0),
            process_success: true,
            usage: HostObservedUsage::unknown(),
            decision: HostObservedDecision::unknown(),
            verifier_identity: Some("verifier".to_owned()),
            verification_evidence_ref: Some("ledger:verification/node/2".to_owned()),
            independent_verifier: Some(true),
            task_correctness: UNKNOWN.to_owned(),
            verification_status: "passed".to_owned(),
            evidence_root_hash: "sha256:evidence".to_owned(),
        }
    }

    fn receipt() -> RouteReceiptV1 {
        RouteReceiptV1::from_host_observation(
            "project".to_owned(),
            "graph".to_owned(),
            "sha256:graph".to_owned(),
            "node".to_owned(),
            2,
            "sha256:objective".to_owned(),
            RouteInvocation::unknown("codex-cli"),
            "legacy:codex-cli:backend-unknown:model-unknown:high".to_owned(),
            RouteProvenance::unknown("codex-cli"),
            "exited_success".to_owned(),
            Some(10),
            Some(20),
            Some(0),
            true,
            Some("verifier".to_owned()),
            "passed".to_owned(),
            Some(true),
            Some("ledger:verification/node/2".to_owned()),
            "sha256:evidence".to_owned(),
            1,
        )
    }

    fn trusted_fixture() -> (RouteReceiptV1, HostObservedContext) {
        let mut receipt = receipt();
        let invocation = RouteInvocation {
            cli_family: "codex-cli".to_owned(),
            selected_model: Some("gpt-test".to_owned()),
            selected_effort: Some("high".to_owned()),
            configuration_source: "trusted-host-command".to_owned(),
        };
        let provenance = RouteProvenance {
            cli_family: "codex-cli".to_owned(),
            provider: "test-provider".to_owned(),
            model: "gpt-test".to_owned(),
            revision: "test-revision".to_owned(),
            source: "trusted-host-attestation".to_owned(),
        };
        let usage = HostObservedUsage {
            input_tokens: UsageValue::Known(10),
            output_tokens: UsageValue::Known(4),
            cached_input_tokens: UsageValue::Known(3),
            cost_micros: UsageValue::Known(8),
            usage_source: "trusted-provider-receipt".to_owned(),
            provider_receipt_ref: Some("provider:receipt/1".to_owned()),
        };
        let evidence_ref = "ledger:verification/node/2".to_owned();
        let decision = HostObservedDecision {
            request_hash: "sha256:request".to_owned(),
            response_hash: "sha256:response".to_owned(),
            policy_hash: "sha256:policy".to_owned(),
            roster_hash: "sha256:roster".to_owned(),
            eligibility_hash: "sha256:eligibility".to_owned(),
            model_hash: "sha256:model".to_owned(),
            decision_envelope_hash: "sha256:decision-envelope".to_owned(),
            eligibility_mask: vec![true, false],
            authorization_evidence_refs: vec![AuthorizationEvidenceRef::new(
                "ledger:decision/node/2",
            )],
            proposed_route_id: "test-provider:gpt-test:high".to_owned(),
            observed_baseline_route_id: "legacy:codex-cli:backend-unknown:model-unknown:high"
                .to_owned(),
            decision_kind: "scored_shadow".to_owned(),
            fallback_used: false,
            shadow: true,
            canary: false,
            reason_codes: vec!["JEV_TEST_FIXTURE".to_owned()],
        };
        receipt.invocation = invocation.clone();
        receipt.actual_route_id = decision.proposed_route_id.clone();
        receipt.actual_route_provenance = provenance.clone();
        receipt.usage = RouteUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cost_micros: usage.cost_micros,
            usage_source: usage.usage_source.clone(),
            provider_receipt_ref: usage.provider_receipt_ref.clone(),
            estimate: None,
        };
        receipt.request_hash = decision.request_hash.clone();
        receipt.response_hash = decision.response_hash.clone();
        receipt.policy_hash = decision.policy_hash.clone();
        receipt.roster_hash = decision.roster_hash.clone();
        receipt.eligibility_hash = decision.eligibility_hash.clone();
        receipt.model_hash = decision.model_hash.clone();
        receipt.decision_envelope_hash = decision.decision_envelope_hash.clone();
        receipt.eligibility_mask = decision.eligibility_mask.clone();
        receipt.authorization_evidence_refs = decision.authorization_evidence_refs.clone();
        receipt.proposed_route_id = decision.proposed_route_id.clone();
        receipt.observed_baseline_route_id = decision.observed_baseline_route_id.clone();
        receipt.decision_kind = decision.decision_kind.clone();
        receipt.fallback_used = decision.fallback_used;
        receipt.shadow = decision.shadow;
        receipt.canary = decision.canary;
        receipt.reason_codes = decision.reason_codes.clone();
        receipt.task_correctness = "true".to_owned();
        receipt.verification.verification_status = "passed".to_owned();
        receipt.verification.evidence_refs = vec![evidence_ref.clone()];
        receipt.receipt_hash = receipt_hash(&receipt).unwrap();

        let mut host = host();
        host.actual_route_id = receipt.actual_route_id.clone();
        host.actual_route_provenance = provenance;
        host.invocation = invocation;
        host.usage = usage;
        host.decision = decision;
        host.task_correctness = "true".to_owned();
        host.verification_status = "passed".to_owned();
        host.verification_evidence_ref = Some(evidence_ref);
        (receipt, host)
    }

    #[test]
    fn trusted_host_owned_positive_fixture_is_training_ready() {
        let (receipt, host) = trusted_fixture();
        let result = validate_host_anchored_receipt(&receipt, &host);
        assert!(result.is_valid(), "reasons: {:?}", result.reasons);
        assert!(result.is_training_ready(), "reasons: {:?}", result.reasons);
    }

    #[test]
    fn trusted_verified_failure_remains_a_valid_training_label() {
        let (mut receipt, mut host) = trusted_fixture();
        receipt.task_correctness = "false".to_owned();
        receipt.verification.verification_status = "failed".to_owned();
        receipt.receipt_hash = receipt_hash(&receipt).unwrap();
        host.task_correctness = "false".to_owned();
        host.verification_status = "failed".to_owned();
        let result = validate_host_anchored_receipt(&receipt, &host);
        assert!(result.is_valid(), "reasons: {:?}", result.reasons);
        assert!(result.is_training_ready(), "reasons: {:?}", result.reasons);
    }

    #[test]
    fn training_requires_a_closed_known_outcome_pair() {
        let cases = [
            ("", "", false),
            ("arbitrary", "arbitrary", false),
            ("true", "failed", false),
            ("false", "passed", false),
            ("unknown", "passed", false),
            ("true", "unknown", false),
            ("true", "passed", true),
            ("false", "failed", true),
        ];

        for (task_correctness, verification_status, expected_training_ready) in cases {
            let (mut receipt, mut host) = trusted_fixture();
            receipt.task_correctness = task_correctness.to_owned();
            receipt.verification.verification_status = verification_status.to_owned();
            receipt.receipt_hash = receipt_hash(&receipt).unwrap();
            host.task_correctness = task_correctness.to_owned();
            host.verification_status = verification_status.to_owned();

            let result = validate_host_anchored_receipt(&receipt, &host);
            assert!(result.is_valid(), "reasons: {:?}", result.reasons);
            assert_eq!(
                result.is_training_ready(),
                expected_training_ready,
                "outcome pair ({task_correctness:?}, {verification_status:?}) reasons: {:?}",
                result.reasons
            );
            if !expected_training_ready {
                assert!(result
                    .reasons
                    .contains(&"training_outcome_pair_not_allowed".to_owned()));
            }
        }
    }

    #[test]
    fn usage_amount_tamper_is_rejected_after_digest_recomputation() {
        let (mut receipt, host) = trusted_fixture();
        receipt.usage.input_tokens = UsageValue::Known(11);
        receipt.receipt_hash = receipt_hash(&receipt).unwrap();
        let result = validate_host_anchored_receipt(&receipt, &host);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"usage_observation_mismatch".to_owned()));
    }

    #[test]
    fn copied_usage_source_identity_is_rejected_by_host_binding() {
        let (mut receipt, host) = trusted_fixture();
        receipt.usage.provider_receipt_ref = Some("provider:receipt/copied".to_owned());
        receipt.receipt_hash = receipt_hash(&receipt).unwrap();
        let result = validate_host_anchored_receipt(&receipt, &host);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"usage_observation_mismatch".to_owned()));
    }

    #[test]
    fn wrong_decision_hash_is_rejected_after_digest_recomputation() {
        let (mut receipt, host) = trusted_fixture();
        receipt.decision_envelope_hash = "sha256:wrong-decision".to_owned();
        receipt.receipt_hash = receipt_hash(&receipt).unwrap();
        let result = validate_host_anchored_receipt(&receipt, &host);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"decision_observation_mismatch".to_owned()));
    }

    #[test]
    fn changed_mask_and_roster_identity_are_rejected_after_digest_recomputation() {
        let (mut mask, host) = trusted_fixture();
        mask.eligibility_mask = vec![false, true];
        mask.receipt_hash = receipt_hash(&mask).unwrap();
        let result = validate_host_anchored_receipt(&mask, &host);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"decision_observation_mismatch".to_owned()));

        let (mut roster, host) = trusted_fixture();
        roster.roster_hash = "sha256:wrong-roster".to_owned();
        roster.receipt_hash = receipt_hash(&roster).unwrap();
        let result = validate_host_anchored_receipt(&roster, &host);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"decision_observation_mismatch".to_owned()));
    }

    #[test]
    fn changed_verifier_verdict_and_correctness_are_rejected() {
        let (mut verdict, host) = trusted_fixture();
        verdict.verification.verification_status = "failed".to_owned();
        verdict.receipt_hash = receipt_hash(&verdict).unwrap();
        let result = validate_host_anchored_receipt(&verdict, &host);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"verification_status_observation_mismatch".to_owned()));

        let (mut correctness, host) = trusted_fixture();
        correctness.task_correctness = "false".to_owned();
        correctness.receipt_hash = receipt_hash(&correctness).unwrap();
        let result = validate_host_anchored_receipt(&correctness, &host);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"task_correctness_observation_mismatch".to_owned()));
    }

    #[test]
    fn changed_invocation_and_exit_are_rejected_after_digest_recomputation() {
        let (mut invocation, host) = trusted_fixture();
        invocation.invocation.selected_model = Some("copied-model".to_owned());
        invocation.receipt_hash = receipt_hash(&invocation).unwrap();
        let result = validate_host_anchored_receipt(&invocation, &host);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"invocation_observation_mismatch".to_owned()));

        let (mut exit, host) = trusted_fixture();
        exit.exit_status = Some(23);
        exit.receipt_hash = receipt_hash(&exit).unwrap();
        let result = validate_host_anchored_receipt(&exit, &host);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"process_observation_mismatch".to_owned()));
    }

    #[test]
    fn unknown_runtime_observations_are_audit_valid_but_not_training_ready() {
        let result = validate_host_anchored_receipt(&receipt(), &host());
        assert!(result.is_valid(), "reasons: {:?}", result.reasons);
        assert!(!result.is_training_ready());
        assert!(result
            .reasons
            .contains(&"usage_unavailable_or_incomplete".to_owned()));
        assert!(result
            .reasons
            .contains(&"decision_authority_hashes_unknown".to_owned()));
        assert!(result
            .reasons
            .contains(&"task_correctness_unknown".to_owned()));
    }

    #[test]
    fn unknown_verdict_and_correctness_are_not_training_ready() {
        let mut receipt = receipt();
        receipt.verification.verification_status = UNKNOWN.to_owned();
        receipt.verification.verifier_identity = None;
        receipt.verification.independent_verifier = None;
        receipt.verification.evidence_refs.clear();
        receipt.task_correctness = UNKNOWN.to_owned();
        receipt.receipt_hash = receipt_hash(&receipt).unwrap();
        let mut host = host();
        host.verification_status = UNKNOWN.to_owned();
        host.verifier_identity = None;
        host.verification_evidence_ref = None;
        host.independent_verifier = None;
        let result = validate_host_anchored_receipt(&receipt, &host);
        assert!(result.is_valid(), "reasons: {:?}", result.reasons);
        assert!(!result.is_training_ready());
        assert!(result
            .reasons
            .contains(&"verification_outcome_unknown".to_owned()));
    }

    #[test]
    fn unknown_decision_authority_cannot_upgrade_a_trusted_fixture() {
        let (mut receipt, host) = trusted_fixture();
        receipt.request_hash = UNKNOWN.to_owned();
        receipt.receipt_hash = receipt_hash(&receipt).unwrap();
        let result = validate_host_anchored_receipt(&receipt, &host);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"decision_observation_mismatch".to_owned()));
    }

    #[test]
    fn forged_claim_and_mismatched_subject_fail_host_validation() {
        let mut forged = receipt();
        forged.process_success = false;
        let result = validate_host_anchored_receipt(&forged, &host());
        assert!(!result.is_valid());
        assert!(result.reasons.contains(&"receipt_hash_mismatch".to_owned()));

        let mut mismatched = receipt();
        mismatched.node_id = "other".to_owned();
        let result = validate_host_anchored_receipt(&mismatched, &host());
        assert!(!result.is_valid());
        assert!(result.reasons.contains(&"node_mismatch".to_owned()));
    }

    #[test]
    fn missing_usage_is_unknown_and_estimates_do_not_become_actuals() {
        let value = serde_json::to_value(RouteUsage::unknown()).unwrap();
        assert_eq!(value["input_tokens"], "unknown");
        assert_eq!(value["cost_micros"], "unknown");
        let mut usage = RouteUsage::unknown();
        usage.estimate = Some(RouteUsageEstimate {
            input_tokens: Some(99),
            output_tokens: Some(2),
            cost_micros: Some(7),
        });
        assert!(matches!(usage.input_tokens, UsageValue::Unknown));
    }

    #[test]
    fn invalid_counts_and_cached_subset_are_rejected() {
        for raw in [
            r#"{"input_tokens":-1}"#,
            r#"{"input_tokens":18446744073709551616}"#,
            r#"{"input_tokens":"not-a-count"}"#,
        ] {
            assert!(serde_json::from_str::<UsageValue>(
                serde_json::from_str::<Value>(raw)
                    .unwrap()
                    .get("input_tokens")
                    .unwrap()
                    .to_string()
                    .as_str()
            )
            .is_err());
        }
        let mut invalid = receipt();
        invalid.usage.input_tokens = UsageValue::Known(3);
        invalid.usage.cached_input_tokens = UsageValue::Known(4);
        invalid.receipt_hash = receipt_hash(&invalid).unwrap();
        let result = validate_host_anchored_receipt(&invalid, &host());
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"cached_input_tokens_exceed_input_tokens".to_owned()));
    }

    #[test]
    fn known_usage_requires_source_and_provider_receipt() {
        let mut invalid = receipt();
        invalid.usage.input_tokens = UsageValue::Known(1);
        invalid.receipt_hash = receipt_hash(&invalid).unwrap();
        let result = validate_host_anchored_receipt(&invalid, &host());
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"actual_usage_missing_receipt_source".to_owned()));
    }

    #[test]
    fn sourced_usage_accepts_cached_subset_but_unknown_authority_stays_unready() {
        let mut sourced = receipt();
        sourced.usage = RouteUsage {
            input_tokens: UsageValue::Known(10),
            output_tokens: UsageValue::Known(4),
            cached_input_tokens: UsageValue::Known(3),
            cost_micros: UsageValue::Known(8),
            usage_source: "provider-receipt".to_owned(),
            provider_receipt_ref: Some("provider:receipt/1".to_owned()),
            estimate: Some(RouteUsageEstimate {
                input_tokens: Some(12),
                output_tokens: Some(5),
                cost_micros: Some(9),
            }),
        };
        sourced.receipt_hash = receipt_hash(&sourced).unwrap();
        let mut context = host();
        context.usage = HostObservedUsage {
            input_tokens: UsageValue::Known(10),
            output_tokens: UsageValue::Known(4),
            cached_input_tokens: UsageValue::Known(3),
            cost_micros: UsageValue::Known(8),
            usage_source: "provider-receipt".to_owned(),
            provider_receipt_ref: Some("provider:receipt/1".to_owned()),
        };
        let result = validate_host_anchored_receipt(&sourced, &context);
        assert!(result.is_valid());
        assert!(!result.is_training_ready());
        assert!(!result
            .reasons
            .contains(&"cached_input_tokens_exceed_input_tokens".to_owned()));
        assert!(result
            .reasons
            .contains(&"backend_provenance_unknown".to_owned()));
    }

    #[test]
    fn missing_verifier_authority_is_rejected_even_with_a_matching_hash() {
        let mut missing = receipt();
        missing.verification.verifier_identity = None;
        missing.verification.evidence_refs.clear();
        missing.receipt_hash = receipt_hash(&missing).unwrap();
        let result = validate_host_anchored_receipt(&missing, &host());
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"verification_authority_missing".to_owned()));
    }

    #[test]
    fn same_worker_cannot_claim_independent_verification() {
        let mut context = host();
        context.verifier_identity = Some("worker".to_owned());
        context.independent_verifier = Some(true);
        let mut claimed = receipt();
        claimed.verification.verifier_identity = Some("worker".to_owned());
        claimed.verification.independent_verifier = Some(true);
        claimed.receipt_hash = receipt_hash(&claimed).unwrap();
        let result = validate_host_anchored_receipt(&claimed, &context);
        assert!(!result.is_valid());
        assert!(result
            .reasons
            .contains(&"same_worker_claimed_independent".to_owned()));
    }
}
