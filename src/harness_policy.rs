//! Versioned, fail-closed harness policy loading and resolution.
//!
//! The policy is intentionally separate from the compiled harness genome.  A
//! genome describes work; this module describes the authority under which that
//! work may run.  Policies are read-only inputs, normalised into a deterministic
//! JSON representation, and content hashed before they are attached to a graph.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub(crate) const HARNESS_POLICY_SCHEMA: &str = "fractal.harness_policy.v1";
pub(crate) const NODE_POLICY_CONTRACT_SCHEMA: &str = "fractal.node_policy_contract.v1";
const BUILTIN_PROVENANCE: &str = "builtin:safe-default.v1";
const YAML_NAME: &str = ".fractal/harness.yaml";
const JSON_NAMES: &[&str] = &[
    ".fractal/harness.json",
    ".fractal/harness_policy.json",
    ".fractal/harness-policy.json",
];

/// A policy document with immutable provenance and a canonical content hash.
#[derive(Clone, Debug)]
pub(crate) struct LoadedHarnessPolicy {
    pub(crate) policy: HarnessPolicy,
    pub(crate) policy_hash: String,
    pub(crate) provenance: PolicyProvenance,
    /// Normalised policy JSON used for hashing and diagnostics.  Keeping this
    /// value makes `show --json` a faithful, no-write inspection operation.
    pub(crate) normalized: Value,
    pub(crate) diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PolicyProvenance {
    pub(crate) kind: String,
    pub(crate) source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
}

impl PolicyProvenance {
    fn builtin() -> Self {
        Self {
            kind: "builtin_default".to_owned(),
            source: BUILTIN_PROVENANCE.to_owned(),
            path: None,
        }
    }

    fn file(path: &Path, format: &str) -> Self {
        let relative = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| format!(".fractal/{name}"))
            .unwrap_or_else(|| match format {
                "yaml" => YAML_NAME.to_owned(),
                _ => ".fractal/harness.json".to_owned(),
            });
        Self {
            kind: format.to_owned(),
            source: format!("project:{relative}"),
            path: Some(path.display().to_string()),
        }
    }
}

/// Top-level v1 policy.  The old `fractal_harness_v1 2` YAML is accepted with
/// the same names, while additional runtime fields are now typed below.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct HarnessPolicy {
    #[serde(alias = "version")]
    pub(crate) schema: String,
    #[serde(default = "default_mode")]
    pub(crate) mode: String,
    #[serde(default)]
    pub(crate) authority_order: Vec<String>,
    #[serde(default)]
    pub(crate) workspace: WorkspacePolicy,
    #[serde(default)]
    pub(crate) commands: CommandPolicy,
    #[serde(default)]
    pub(crate) network: NetworkPolicy,
    #[serde(default)]
    pub(crate) secrets: SecretPolicy,
    #[serde(default)]
    pub(crate) context: ContextPolicy,
    #[serde(default)]
    pub(crate) limits: LimitsPolicy,
    #[serde(default)]
    pub(crate) verification: VerificationPolicy,
    #[serde(default)]
    pub(crate) artifacts: ArtifactPolicy,
    #[serde(default)]
    pub(crate) termination_states: Vec<String>,
    /// Capability grants are deny-by-default.  A map entry is an explicit
    /// grant; `grants` is retained as a migration alias for early prototypes.
    #[serde(default, alias = "grants")]
    pub(crate) capabilities: BTreeMap<String, CapabilityGrant>,
    #[serde(default)]
    pub(crate) phases: Vec<PhasePolicy>,
    #[serde(default)]
    pub(crate) verifier: VerifierPlan,
    #[serde(default)]
    pub(crate) evidence: EvidencePolicy,
    #[serde(default)]
    pub(crate) learning: LearningPolicy,
    /// Unknown fields are retained for diagnostics but are not silently used
    /// by enforcement.  v1 validation rejects them after reporting their path.
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WorkspacePolicy {
    #[serde(default = "default_isolation")]
    pub(crate) isolation: String,
    #[serde(default)]
    pub(crate) clean_start_required: bool,
    #[serde(default)]
    pub(crate) writable: Vec<String>,
    #[serde(default)]
    pub(crate) readonly: Vec<String>,
    #[serde(default)]
    pub(crate) forbidden: Vec<String>,
    #[serde(default = "default_max_files")]
    pub(crate) max_files_changed: u64,
    #[serde(default = "default_max_diff")]
    pub(crate) max_diff_lines: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CommandPolicy {
    #[serde(default = "default_shell")]
    pub(crate) shell: String,
    #[serde(default)]
    pub(crate) allow: Vec<String>,
    #[serde(default)]
    pub(crate) deny_patterns: Vec<String>,
    #[serde(default)]
    pub(crate) approval_required: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct NetworkPolicy {
    #[serde(default = "default_network")]
    pub(crate) default: String,
    #[serde(default)]
    pub(crate) allowed_destinations: Vec<String>,
    #[serde(default)]
    pub(crate) record_dns_and_destinations: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SecretPolicy {
    #[serde(default = "default_secret_mode")]
    pub(crate) default: String,
    #[serde(default)]
    pub(crate) allowed_names: Vec<String>,
    #[serde(default)]
    pub(crate) redact_outputs: bool,
    #[serde(default)]
    pub(crate) never_persist: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ContextPolicy {
    #[serde(default = "default_initial_files")]
    pub(crate) initial_files_max: u64,
    #[serde(default)]
    pub(crate) progressive_disclosure: bool,
    #[serde(default)]
    pub(crate) record_every_file_open: bool,
    #[serde(default)]
    pub(crate) untrusted_content_cannot_grant_capabilities: bool,
}

/// Global bounds.  All values are integers so they remain portable under
/// `fractal-cjson-v1` (which deliberately rejects floating-point values).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LimitsPolicy {
    #[serde(default = "default_max_steps")]
    pub(crate) max_steps: u64,
    #[serde(default = "default_max_minutes")]
    pub(crate) max_minutes: u64,
    #[serde(default = "default_max_attempts")]
    pub(crate) max_attempts: u64,
    #[serde(default = "default_max_files")]
    pub(crate) max_files_changed: u64,
    #[serde(default = "default_max_diff")]
    pub(crate) max_diff_lines: u64,
    #[serde(default = "default_max_repeated_failure")]
    pub(crate) max_repeated_identical_failure: u64,
    #[serde(default = "default_max_tool_errors")]
    pub(crate) max_tool_errors: u64,
    #[serde(default = "default_max_input_tokens")]
    pub(crate) max_input_tokens: u64,
    #[serde(default = "default_max_output_tokens")]
    pub(crate) max_output_tokens: u64,
    #[serde(default = "default_max_cost_usd")]
    pub(crate) max_cost_usd: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct VerificationPolicy {
    #[serde(default)]
    pub(crate) independent_verifier_required: bool,
    #[serde(default)]
    pub(crate) protected_tests_immutable: bool,
    #[serde(default)]
    pub(crate) raw_output_required: bool,
    #[serde(default)]
    pub(crate) evidence_manifest_required: bool,
    #[serde(default)]
    pub(crate) baseline_comparison_required_for_performance_claims: bool,
    #[serde(default)]
    pub(crate) unsupported_claims_field_required: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ArtifactPolicy {
    #[serde(default = "default_artifact_root")]
    pub(crate) root: String,
    #[serde(default = "default_hash_algorithm")]
    pub(crate) hash_algorithm: String,
    #[serde(default)]
    pub(crate) capture: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CapabilityGrant {
    /// A named capability is an explicit grant by default.  Setting `enabled:
    /// false` is useful for revoking a built-in or inherited grant.
    #[serde(default = "default_grant_enabled")]
    pub(crate) enabled: bool,
    #[serde(default, alias = "allowed_writes")]
    pub(crate) writable: Vec<String>,
    #[serde(default, alias = "allowed_commands")]
    pub(crate) commands: Vec<String>,
    #[serde(default)]
    pub(crate) network: Option<NetworkGrant>,
    #[serde(default)]
    pub(crate) external_side_effects: bool,
    #[serde(default)]
    pub(crate) sandbox_profile: Option<String>,
    #[serde(default)]
    pub(crate) budgets: BudgetOverrides,
    #[serde(default)]
    pub(crate) verifier_ids: Vec<String>,
    #[serde(default)]
    pub(crate) evidence_requirements: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct NetworkGrant {
    #[serde(default = "default_network")]
    pub(crate) default: String,
    #[serde(default)]
    pub(crate) allowed_destinations: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct BudgetOverrides {
    #[serde(default)]
    pub(crate) max_steps: Option<u64>,
    #[serde(default)]
    pub(crate) max_minutes: Option<u64>,
    #[serde(default)]
    pub(crate) max_attempts: Option<u64>,
    #[serde(default)]
    pub(crate) max_files_changed: Option<u64>,
    #[serde(default)]
    pub(crate) max_diff_lines: Option<u64>,
    #[serde(default)]
    pub(crate) max_input_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) max_output_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) max_cost_usd: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PhasePolicy {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) allowed_capabilities: Vec<String>,
    #[serde(default)]
    pub(crate) terminal_statuses: Vec<String>,
    #[serde(default)]
    pub(crate) required_verifier_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct VerifierPlan {
    #[serde(default)]
    pub(crate) independent_required: bool,
    #[serde(default)]
    pub(crate) verifier_ids: Vec<String>,
    #[serde(default)]
    pub(crate) plan: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct EvidencePolicy {
    #[serde(default)]
    pub(crate) required: Vec<String>,
    #[serde(default)]
    pub(crate) artifact_requirements: Vec<String>,
    #[serde(default)]
    pub(crate) unsupported_claims_field: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LearningPolicy {
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) only_after_verification: bool,
    #[serde(
        default = "default_confidence",
        deserialize_with = "deserialize_confidence"
    )]
    pub(crate) minimum_confidence: u64,
    #[serde(default)]
    pub(crate) requires_evidence_refs: bool,
    #[serde(default)]
    pub(crate) lessons_cannot_override_policy: bool,
}

impl Default for WorkspacePolicy {
    fn default() -> Self {
        Self {
            isolation: default_isolation(),
            clean_start_required: false,
            writable: Vec::new(),
            readonly: vec!["**".to_owned()],
            forbidden: vec![
                ".env".to_owned(),
                ".env.*".to_owned(),
                "**/secrets/**".to_owned(),
            ],
            max_files_changed: default_max_files(),
            max_diff_lines: default_max_diff(),
        }
    }
}

impl Default for CommandPolicy {
    fn default() -> Self {
        Self {
            shell: default_shell(),
            allow: Vec::new(),
            deny_patterns: vec![
                "sudo *".to_owned(),
                "rm -rf *".to_owned(),
                "git push*".to_owned(),
            ],
            approval_required: vec![
                "delete".to_owned(),
                "publish".to_owned(),
                "deploy".to_owned(),
            ],
        }
    }
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            default: default_network(),
            allowed_destinations: Vec::new(),
            record_dns_and_destinations: true,
        }
    }
}

impl Default for SecretPolicy {
    fn default() -> Self {
        Self {
            default: default_secret_mode(),
            allowed_names: Vec::new(),
            redact_outputs: true,
            never_persist: true,
        }
    }
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            initial_files_max: default_initial_files(),
            progressive_disclosure: true,
            record_every_file_open: true,
            untrusted_content_cannot_grant_capabilities: true,
        }
    }
}

impl Default for LimitsPolicy {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            max_minutes: default_max_minutes(),
            max_attempts: default_max_attempts(),
            max_files_changed: default_max_files(),
            max_diff_lines: default_max_diff(),
            max_repeated_identical_failure: default_max_repeated_failure(),
            max_tool_errors: default_max_tool_errors(),
            max_input_tokens: default_max_input_tokens(),
            max_output_tokens: default_max_output_tokens(),
            max_cost_usd: default_max_cost_usd(),
        }
    }
}

impl Default for VerificationPolicy {
    fn default() -> Self {
        Self {
            independent_verifier_required: true,
            protected_tests_immutable: true,
            raw_output_required: true,
            evidence_manifest_required: true,
            baseline_comparison_required_for_performance_claims: true,
            unsupported_claims_field_required: true,
        }
    }
}

impl Default for ArtifactPolicy {
    fn default() -> Self {
        Self {
            root: default_artifact_root(),
            hash_algorithm: default_hash_algorithm(),
            capture: vec![
                "commands".to_owned(),
                "stdout".to_owned(),
                "stderr".to_owned(),
                "exit_codes".to_owned(),
                "diff".to_owned(),
            ],
        }
    }
}

impl Default for VerifierPlan {
    fn default() -> Self {
        Self {
            independent_required: true,
            verifier_ids: vec!["independent".to_owned()],
            plan: vec![
                "policy_review".to_owned(),
                "deterministic_checks".to_owned(),
                "regression_probe".to_owned(),
            ],
        }
    }
}

impl Default for EvidencePolicy {
    fn default() -> Self {
        Self {
            required: vec![
                "commands".to_owned(),
                "stdout".to_owned(),
                "stderr".to_owned(),
                "exit_codes".to_owned(),
            ],
            artifact_requirements: vec!["evidence_manifest".to_owned()],
            unsupported_claims_field: Some("unsupported_claims".to_owned()),
        }
    }
}

impl Default for LearningPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            only_after_verification: true,
            minimum_confidence: 75,
            requires_evidence_refs: true,
            lessons_cannot_override_policy: true,
        }
    }
}

impl Default for CapabilityGrant {
    fn default() -> Self {
        Self {
            enabled: true,
            writable: Vec::new(),
            commands: Vec::new(),
            network: None,
            external_side_effects: false,
            sandbox_profile: None,
            budgets: BudgetOverrides::default(),
            verifier_ids: Vec::new(),
            evidence_requirements: Vec::new(),
        }
    }
}

fn default_mode() -> String {
    "deny_by_default".to_owned()
}
fn default_isolation() -> String {
    "git_worktree".to_owned()
}
fn default_shell() -> String {
    "restricted".to_owned()
}
fn default_network() -> String {
    "deny".to_owned()
}
fn default_secret_mode() -> String {
    "deny".to_owned()
}
fn default_artifact_root() -> String {
    ".fractal/artifacts".to_owned()
}
fn default_hash_algorithm() -> String {
    "sha256".to_owned()
}
fn default_initial_files() -> u64 {
    6
}
fn default_max_steps() -> u64 {
    40
}
fn default_max_minutes() -> u64 {
    60
}
fn default_max_attempts() -> u64 {
    1
}
fn default_max_files() -> u64 {
    6
}
fn default_max_diff() -> u64 {
    500
}
fn default_max_repeated_failure() -> u64 {
    2
}
fn default_max_tool_errors() -> u64 {
    5
}
fn default_max_input_tokens() -> u64 {
    250_000
}
fn default_max_output_tokens() -> u64 {
    50_000
}
fn default_max_cost_usd() -> u64 {
    20
}
fn default_grant_enabled() -> bool {
    true
}
fn default_confidence() -> u64 {
    75
}

fn default_terminal_states() -> Vec<String> {
    [
        "PASS",
        "FAILED_CHECK",
        "BLOCKED_PERMISSION",
        "BLOCKED_AMBIGUITY",
        "BUDGET_EXHAUSTED",
        "RISK_ESCALATION",
        "ENVIRONMENT_FAILURE",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn default_phases() -> Vec<PhasePolicy> {
    [
        "orient", "localize", "plan", "execute", "prove", "report", "stop",
    ]
    .into_iter()
    .map(|id| PhasePolicy {
        id: id.to_owned(),
        allowed_capabilities: Vec::new(),
        terminal_statuses: Vec::new(),
        required_verifier_ids: if id == "prove" {
            vec!["independent".to_owned()]
        } else {
            Vec::new()
        },
    })
    .collect()
}

fn deserialize_confidence<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let parsed = match value {
        Value::Number(number) => {
            if let Some(integer) = number.as_u64() {
                integer
            } else if let Some(decimal) = number.as_f64() {
                if !(0.0..=1.0).contains(&decimal) {
                    return Err(serde::de::Error::custom(
                        "minimum_confidence decimal must be between 0 and 1",
                    ));
                }
                (decimal * 100.0).round() as u64
            } else {
                return Err(serde::de::Error::custom("minimum_confidence is not finite"));
            }
        }
        Value::String(text) => {
            let decimal: f64 = text
                .parse()
                .map_err(|_| serde::de::Error::custom("minimum_confidence must be a number"))?;
            if decimal <= 1.0 {
                (decimal * 100.0).round() as u64
            } else {
                decimal.round() as u64
            }
        }
        _ => {
            return Err(serde::de::Error::custom(
                "minimum_confidence must be a number",
            ))
        }
    };
    Ok(parsed)
}

/// Load the policy for a repository.  No file is created or modified.  If no
/// policy exists, an explicit built-in safe default is returned.
pub(crate) fn load_for_repo(repo: &Path) -> Result<LoadedHarnessPolicy> {
    let root = repo;
    let yaml = root.join(YAML_NAME);
    if yaml.is_file() {
        return load_file(&yaml, "yaml");
    }
    for name in JSON_NAMES {
        let json = root.join(name);
        if json.is_file() {
            return load_file(&json, "json");
        }
    }
    Ok(builtin_default())
}

fn load_file(path: &Path, format: &str) -> Result<LoadedHarnessPolicy> {
    let bytes =
        fs::read(path).with_context(|| format!("read harness policy {}", path.display()))?;
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("harness policy {} is not UTF-8", path.display()))?;
    let mut raw = if format == "yaml" {
        let yaml: serde_yaml::Value = serde_yaml::from_str(text)
            .with_context(|| format!("parse YAML harness policy {}", path.display()))?;
        serde_json::to_value(yaml).context("convert YAML harness policy to JSON")?
    } else {
        serde_json::from_str(text)
            .with_context(|| format!("parse JSON harness policy {}", path.display()))?
    };
    let raw_object = raw
        .as_object_mut()
        .context("harness policy root must be an object")?;
    let mut diagnostics = Vec::new();
    let used_legacy_version =
        raw_object.contains_key("version") && !raw_object.contains_key("schema");
    if used_legacy_version {
        if let Some(value) = raw_object.remove("version") {
            raw_object.insert("schema".to_owned(), value);
        }
        diagnostics.push("migrated legacy `version` field to `schema`".to_owned());
    }
    let mut schema = raw_object
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if schema == "fractal.harness.v1" {
        raw_object.insert(
            "schema".to_owned(),
            Value::String(HARNESS_POLICY_SCHEMA.to_owned()),
        );
        schema = HARNESS_POLICY_SCHEMA;
        diagnostics.push(
            "migrated legacy schema `fractal.harness.v1` to `fractal.harness_policy.v1`".to_owned(),
        );
    }
    if schema != HARNESS_POLICY_SCHEMA {
        if schema.starts_with("fractal.harness_policy.") {
            bail!("unsupported future harness policy schema `{schema}`; enforcement is fail-closed (source preserved at {})", path.display());
        }
        bail!("harness policy schema must be `{HARNESS_POLICY_SCHEMA}`, got `{schema}`");
    }
    let mut policy: HarnessPolicy = serde_json::from_value(raw.clone())
        .with_context(|| format!("decode typed harness policy {}", path.display()))?;
    diagnostics.extend(migration_diagnostics(&raw));
    normalize_validate(&mut policy, &mut diagnostics)?;
    let normalized = serde_json::to_value(&policy).context("encode normalized harness policy")?;
    let policy_hash = canonical_policy_hash(&normalized)?;
    Ok(LoadedHarnessPolicy {
        policy,
        policy_hash,
        provenance: PolicyProvenance::file(path, format),
        normalized,
        diagnostics,
    })
}

fn migration_diagnostics(raw: &Value) -> Vec<String> {
    let Some(object) = raw.as_object() else {
        return Vec::new();
    };
    let mut diagnostics = Vec::new();
    for (field, status) in [
        (
            "workspace",
            "enforced: workspace isolation, path globs, and file/diff bounds",
        ),
        (
            "commands",
            "enforced: command allow/deny and approval requirements",
        ),
        (
            "network",
            "enforced: destination allowlist and default deny",
        ),
        (
            "secrets",
            "enforced: secret deny/redaction/non-persistence policy",
        ),
        (
            "verification",
            "enforced: independent verifier and evidence requirements",
        ),
        (
            "learning",
            "enforced: post-verification, evidence-backed learning guard",
        ),
        (
            "context",
            "enforced: bounded progressive-disclosure context",
        ),
        (
            "artifacts",
            "enforced: artifact root, hash algorithm, and capture list",
        ),
    ] {
        if object.contains_key(field) {
            diagnostics.push(format!("legacy field `{field}` {status}"));
        }
    }
    for field in [
        "termination_states",
        "capabilities",
        "grants",
        "phases",
        "verifier",
        "evidence",
    ] {
        if object.contains_key(field) {
            diagnostics.push(format!("field `{field}` is enforced by the v1 runtime"));
        }
    }
    diagnostics
}

/// Return the explicit safe default used when no project policy exists.
pub(crate) fn builtin_default() -> LoadedHarnessPolicy {
    let mut policy = HarnessPolicy {
        schema: HARNESS_POLICY_SCHEMA.to_owned(),
        mode: default_mode(),
        authority_order: vec![
            "runtime_policy".to_owned(),
            "task_contract".to_owned(),
            "repository_invariants".to_owned(),
            "project_graph".to_owned(),
            "prior_lessons".to_owned(),
            "model_assumptions".to_owned(),
        ],
        workspace: WorkspacePolicy::default(),
        commands: CommandPolicy::default(),
        network: NetworkPolicy::default(),
        secrets: SecretPolicy::default(),
        context: ContextPolicy::default(),
        limits: LimitsPolicy::default(),
        verification: VerificationPolicy::default(),
        artifacts: ArtifactPolicy::default(),
        termination_states: vec![
            "PASS".to_owned(),
            "FAILED_CHECK".to_owned(),
            "BLOCKED_PERMISSION".to_owned(),
            "BLOCKED_AMBIGUITY".to_owned(),
            "BUDGET_EXHAUSTED".to_owned(),
            "RISK_ESCALATION".to_owned(),
            "ENVIRONMENT_FAILURE".to_owned(),
        ],
        capabilities: BTreeMap::new(),
        phases: vec![
            PhasePolicy {
                id: "orient".to_owned(),
                allowed_capabilities: Vec::new(),
                terminal_statuses: Vec::new(),
                required_verifier_ids: Vec::new(),
            },
            PhasePolicy {
                id: "localize".to_owned(),
                allowed_capabilities: Vec::new(),
                terminal_statuses: Vec::new(),
                required_verifier_ids: Vec::new(),
            },
            PhasePolicy {
                id: "plan".to_owned(),
                allowed_capabilities: Vec::new(),
                terminal_statuses: Vec::new(),
                required_verifier_ids: Vec::new(),
            },
            PhasePolicy {
                id: "execute".to_owned(),
                allowed_capabilities: Vec::new(),
                terminal_statuses: Vec::new(),
                required_verifier_ids: Vec::new(),
            },
            PhasePolicy {
                id: "prove".to_owned(),
                allowed_capabilities: Vec::new(),
                terminal_statuses: Vec::new(),
                required_verifier_ids: vec!["independent".to_owned()],
            },
            PhasePolicy {
                id: "report".to_owned(),
                allowed_capabilities: Vec::new(),
                terminal_statuses: Vec::new(),
                required_verifier_ids: Vec::new(),
            },
            PhasePolicy {
                id: "stop".to_owned(),
                allowed_capabilities: Vec::new(),
                terminal_statuses: Vec::new(),
                required_verifier_ids: Vec::new(),
            },
        ],
        verifier: VerifierPlan::default(),
        evidence: EvidencePolicy::default(),
        learning: LearningPolicy::default(),
        extra: BTreeMap::new(),
    };
    let mut diagnostics = vec![
        "no project policy found; using explicit built-in safe default".to_owned(),
        "external side effects, writable paths, commands, network, and secrets are denied unless explicitly granted".to_owned(),
    ];
    // Keep the default policy usable for read/analyse/verify/control nodes while
    // denying side effects.  A project policy must explicitly grant writes or
    // commands to mutate a workspace.
    for capability in [
        "content.analyze",
        "code.generate",
        "code.edit",
        "python.tests.execute",
        "result.verify",
        "reason.answer",
        "reason.plan",
        "content.summarize",
        "retrieval.research",
        "tool.execute",
        "control.plan",
        "control.complete",
        "project.tests.execute",
    ] {
        policy
            .capabilities
            .insert(capability.to_owned(), CapabilityGrant::default());
    }
    normalize_validate(&mut policy, &mut diagnostics).expect("built-in policy is valid");
    let normalized = serde_json::to_value(&policy).expect("encode built-in harness policy");
    let policy_hash = canonical_policy_hash(&normalized).expect("hash built-in harness policy");
    LoadedHarnessPolicy {
        policy,
        policy_hash,
        provenance: PolicyProvenance::builtin(),
        normalized,
        diagnostics,
    }
}

fn normalize_validate(policy: &mut HarnessPolicy, diagnostics: &mut Vec<String>) -> Result<()> {
    if policy.schema != HARNESS_POLICY_SCHEMA {
        bail!("harness policy schema must be `{HARNESS_POLICY_SCHEMA}`");
    }
    policy.mode = policy.mode.trim().to_ascii_lowercase();
    if policy.mode != "deny_by_default" {
        bail!("harness policy mode must be `deny_by_default`");
    }
    for value in &mut policy.authority_order {
        *value = value.trim().to_owned();
    }
    policy.authority_order.retain(|value| !value.is_empty());
    normalize_path_list(&mut policy.workspace.writable, "workspace.writable")?;
    normalize_path_list(&mut policy.workspace.readonly, "workspace.readonly")?;
    normalize_path_list(&mut policy.workspace.forbidden, "workspace.forbidden")?;
    validate_bound(
        "workspace.max_files_changed",
        policy.workspace.max_files_changed,
        1,
        1_000_000,
    )?;
    validate_bound(
        "workspace.max_diff_lines",
        policy.workspace.max_diff_lines,
        1,
        10_000_000,
    )?;
    normalize_strings(&mut policy.commands.allow);
    normalize_strings(&mut policy.commands.deny_patterns);
    normalize_strings(&mut policy.commands.approval_required);
    for command in policy
        .commands
        .allow
        .iter()
        .chain(policy.commands.deny_patterns.iter())
    {
        reject_unsafe_string(command, "commands")?;
    }
    policy.network.default = normalize_network(&policy.network.default)?;
    normalize_strings(&mut policy.network.allowed_destinations);
    for destination in &policy.network.allowed_destinations {
        validate_destination(destination)?;
    }
    policy.secrets.default = normalize_secret_mode(&policy.secrets.default)?;
    normalize_strings(&mut policy.secrets.allowed_names);
    validate_bounds(policy)?;
    normalize_path(&mut policy.artifacts.root, "artifacts.root")?;
    if !policy
        .artifacts
        .hash_algorithm
        .trim()
        .eq_ignore_ascii_case("sha256")
    {
        bail!("artifacts.hash_algorithm must be `sha256`");
    }
    policy.artifacts.hash_algorithm = "sha256".to_owned();
    normalize_strings(&mut policy.artifacts.capture);
    normalize_strings(&mut policy.termination_states);
    if policy.termination_states.is_empty() {
        policy.termination_states = default_terminal_states();
    }
    if policy.phases.is_empty() {
        policy.phases = default_phases();
    }
    for phase in &mut policy.phases {
        phase.id = phase.id.trim().to_owned();
        if phase.id.is_empty() {
            bail!("phase id cannot be empty");
        }
        normalize_strings(&mut phase.allowed_capabilities);
        normalize_strings(&mut phase.terminal_statuses);
        normalize_strings(&mut phase.required_verifier_ids);
    }
    normalize_strings(&mut policy.verifier.verifier_ids);
    normalize_strings(&mut policy.verifier.plan);
    normalize_strings(&mut policy.evidence.required);
    normalize_strings(&mut policy.evidence.artifact_requirements);
    if let Some(field) = policy.evidence.unsupported_claims_field.as_mut() {
        *field = field.trim().to_owned();
    }
    for (capability, grant) in &mut policy.capabilities {
        if capability.trim().is_empty() {
            bail!("capability grant name cannot be empty");
        }
        normalize_path_list(
            &mut grant.writable,
            &format!("capabilities.{capability}.writable"),
        )?;
        normalize_strings(&mut grant.commands);
        for command in &grant.commands {
            reject_unsafe_string(command, "capability command")?;
        }
        if let Some(network) = grant.network.as_mut() {
            network.default = normalize_network(&network.default)?;
            normalize_strings(&mut network.allowed_destinations);
            for destination in &network.allowed_destinations {
                validate_destination(destination)?;
            }
        }
        if let Some(profile) = grant.sandbox_profile.as_mut() {
            reject_unsafe_string(profile, "sandbox_profile")?;
            *profile = profile.trim().to_owned();
        }
        normalize_strings(&mut grant.verifier_ids);
        normalize_strings(&mut grant.evidence_requirements);
        validate_overrides(&grant.budgets, capability)?;
    }
    // Flattened unknown fields must not silently become enforcement inputs.
    if let Some((key, _)) = policy.extra.iter().next() {
        diagnostics.push(format!(
            "unknown v1 field `{key}` is preserved but rejected for enforcement"
        ));
        bail!("unknown harness policy field `{key}`; fail-closed enforcement requires an explicit migration");
    }
    reject_unsafe_tree(&serde_json::to_value(policy)?, &[])?;
    Ok(())
}

fn validate_bounds(policy: &HarnessPolicy) -> Result<()> {
    let limits = &policy.limits;
    validate_bound("limits.max_steps", limits.max_steps, 1, 1_000_000)?;
    validate_bound("limits.max_minutes", limits.max_minutes, 1, 10_080)?;
    validate_bound("limits.max_attempts", limits.max_attempts, 1, 1_000)?;
    validate_bound(
        "limits.max_files_changed",
        limits.max_files_changed,
        1,
        1_000_000,
    )?;
    validate_bound(
        "limits.max_diff_lines",
        limits.max_diff_lines,
        1,
        10_000_000,
    )?;
    validate_bound(
        "limits.max_repeated_identical_failure",
        limits.max_repeated_identical_failure,
        1,
        100,
    )?;
    validate_bound("limits.max_tool_errors", limits.max_tool_errors, 1, 1_000)?;
    validate_bound(
        "limits.max_input_tokens",
        limits.max_input_tokens,
        1,
        10_000_000_000,
    )?;
    validate_bound(
        "limits.max_output_tokens",
        limits.max_output_tokens,
        1,
        10_000_000_000,
    )?;
    validate_bound("limits.max_cost_usd", limits.max_cost_usd, 0, 1_000_000)?;
    validate_bound(
        "context.initial_files_max",
        policy.context.initial_files_max,
        1,
        100_000,
    )?;
    validate_bound(
        "learning.minimum_confidence",
        policy.learning.minimum_confidence,
        0,
        100,
    )?;
    Ok(())
}

fn validate_overrides(overrides: &BudgetOverrides, name: &str) -> Result<()> {
    for (field, value, min, max) in [
        ("max_steps", overrides.max_steps, 1, 1_000_000),
        ("max_minutes", overrides.max_minutes, 1, 10_080),
        ("max_attempts", overrides.max_attempts, 1, 1_000),
        (
            "max_files_changed",
            overrides.max_files_changed,
            1,
            1_000_000,
        ),
        ("max_diff_lines", overrides.max_diff_lines, 1, 10_000_000),
        (
            "max_input_tokens",
            overrides.max_input_tokens,
            1,
            10_000_000_000,
        ),
        (
            "max_output_tokens",
            overrides.max_output_tokens,
            1,
            10_000_000_000,
        ),
        ("max_cost_usd", overrides.max_cost_usd, 0, 1_000_000),
    ] {
        if let Some(value) = value {
            validate_bound(
                &format!("capabilities.{name}.budgets.{field}"),
                value,
                min,
                max,
            )?;
        }
    }
    Ok(())
}

fn validate_bound(name: &str, value: u64, min: u64, max: u64) -> Result<()> {
    if value < min || value > max {
        bail!("{name} must be between {min} and {max}, got {value}");
    }
    Ok(())
}

fn normalize_strings(values: &mut Vec<String>) {
    for value in values.iter_mut() {
        *value = value.trim().to_owned();
    }
    values.retain(|value| !value.is_empty());
    values.sort();
    values.dedup();
}

fn normalize_path_list(values: &mut Vec<String>, field: &str) -> Result<()> {
    for value in values.iter_mut() {
        normalize_path(value, field)?;
    }
    normalize_strings(values);
    Ok(())
}

fn normalize_path(value: &mut String, field: &str) -> Result<()> {
    *value = value.trim().replace('\\', "/");
    if value.is_empty() {
        bail!("{field} contains an empty path");
    }
    reject_absolute_path(value, field)?;
    if value.split('/').any(|part| part == "..") {
        bail!("{field} may not contain `..` path traversal");
    }
    Ok(())
}

fn reject_absolute_path(value: &str, field: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if value.starts_with('/') || value.starts_with('~') || (bytes.len() >= 2 && bytes[1] == b':') {
        bail!("{field} contains an absolute path `{value}`");
    }
    Ok(())
}

fn validate_destination(destination: &str) -> Result<()> {
    if destination.is_empty() {
        bail!("network destination cannot be empty");
    }
    // URL schemes are destinations, not filesystem paths; everything else is
    // treated as a host/glob and must remain relative.
    if !destination.contains("://") {
        reject_absolute_path(destination, "network.allowed_destinations")?;
    }
    if destination.chars().any(char::is_whitespace) {
        bail!("network destination cannot contain whitespace");
    }
    Ok(())
}

fn normalize_network(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if !matches!(
        value.as_str(),
        "deny" | "deny_by_default" | "allow" | "allow_scoped" | "retrieval_only"
    ) {
        bail!(
            "network policy must be deny, deny_by_default, allow, allow_scoped, or retrieval_only"
        );
    }
    Ok(value)
}

fn normalize_secret_mode(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if !matches!(value.as_str(), "deny" | "deny_by_default" | "allow_scoped") {
        bail!("secret policy must be deny, deny_by_default, or allow_scoped");
    }
    Ok(value)
}

fn reject_unsafe_string(value: &str, field: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        bail!("{field} may not contain newlines");
    }
    let lower = value.to_ascii_lowercase();
    for marker in [
        "chain_of_thought",
        "chain-of-thought",
        "chain of thought",
        "cot",
        "scratchpad",
        "raw_log",
        "raw-log",
    ] {
        if lower.contains(marker) {
            bail!("{field} contains forbidden reasoning/raw-log marker `{marker}`");
        }
    }
    Ok(())
}

fn reject_secret_shaped_string(value: &str, field: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    for marker in [
        "api_key=",
        "access_token=",
        "password=",
        "private_key=",
        "bearer ",
        "secret=",
    ] {
        if lower.contains(marker) {
            bail!("{field} contains secret-shaped material `{marker}`");
        }
    }
    Ok(())
}

fn reject_unsafe_tree(value: &Value, path: &[String]) -> Result<()> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let normalized = key.to_ascii_lowercase().replace('-', "_");
                let mut child_path = path.to_vec();
                child_path.push(key.clone());
                let allowed_secret_container = path.is_empty() && normalized == "secrets";
                if !allowed_secret_container
                    && matches!(
                        normalized.as_str(),
                        "api_key"
                            | "access_token"
                            | "authorization"
                            | "password"
                            | "private_key"
                            | "refresh_token"
                            | "credential"
                            | "credentials"
                            | "token"
                            | "secret"
                    )
                {
                    bail!(
                        "policy contains forbidden secret field `{}`",
                        child_path.join(".")
                    );
                }
                if matches!(
                    normalized.as_str(),
                    "chain_of_thought" | "cot" | "scratchpad" | "raw_log" | "raw_logs"
                ) {
                    bail!(
                        "policy contains forbidden reasoning/raw-log field `{}`",
                        child_path.join(".")
                    );
                }
                reject_unsafe_tree(child, &child_path)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                reject_unsafe_tree(child, path)?;
            }
        }
        Value::String(string) => {
            reject_unsafe_string(string, &path.join("."))?;
            if !path.iter().any(|part| part == "allowed_names") {
                reject_secret_shaped_string(string, &path.join("."))?;
            }
            // Secrets are forbidden as values in command/path/policy text.  A
            // URL is fine; an absolute path is never portable or safe.
            if path.iter().any(|part| {
                part.contains("path")
                    || part.contains("writable")
                    || part.contains("readonly")
                    || part.contains("forbidden")
                    || part.contains("root")
            }) {
                reject_absolute_path(string, &path.join("."))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Hash the normalised policy while excluding fields reserved for volatile
/// provenance.  v1 policy structs currently have none, but recursive removal
/// keeps future compatible records deterministic.
pub(crate) fn canonical_policy_hash(normalized: &Value) -> Result<String> {
    let mut stable = normalized.clone();
    strip_volatile(&mut stable);
    fractal_contracts::canonical_sha256(&stable)
        .map_err(|error| anyhow!("harness policy hash: {error}"))
}

fn strip_volatile(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for key in [
                "policy_hash",
                "loaded_at",
                "generated_at",
                "timestamp",
                "provenance",
            ] {
                object.remove(key);
            }
            for child in object.values_mut() {
                strip_volatile(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                strip_volatile(child);
            }
        }
        _ => {}
    }
}

/// Resolve a compact immutable contract for one execution capability.
pub(crate) fn resolve_node_contract(policy: &LoadedHarnessPolicy, capability: &str) -> Value {
    let grant = policy.policy.capabilities.get(capability);
    let enabled = grant.is_some_and(|grant| grant.enabled);
    let writable = resolved_writable(&policy.policy, grant);
    let commands = grant
        .map(|grant| {
            if grant.commands.is_empty() {
                policy.policy.commands.allow.clone()
            } else {
                grant.commands.clone()
            }
        })
        .unwrap_or_default();
    let network = grant
        .and_then(|grant| grant.network.clone())
        .map(|network| json_network(&network))
        .unwrap_or_else(|| {
            json_network(&NetworkGrant {
                default: policy.policy.network.default.clone(),
                allowed_destinations: policy.policy.network.allowed_destinations.clone(),
            })
        });
    let sandbox_profile = grant
        .and_then(|grant| grant.sandbox_profile.clone())
        .unwrap_or_else(|| {
            if enabled {
                "local-work-v1".to_owned()
            } else {
                "deny".to_owned()
            }
        });
    let verifier_ids = grant
        .map(|grant| {
            if grant.verifier_ids.is_empty() {
                policy.policy.verifier.verifier_ids.clone()
            } else {
                grant.verifier_ids.clone()
            }
        })
        .unwrap_or_else(|| policy.policy.verifier.verifier_ids.clone());
    let evidence = grant
        .map(|grant| {
            if grant.evidence_requirements.is_empty() {
                policy.policy.evidence.required.clone()
            } else {
                grant.evidence_requirements.clone()
            }
        })
        .unwrap_or_else(|| policy.policy.evidence.required.clone());
    let budgets = resolved_budgets(&policy.policy, grant);
    serde_json::json!({
        "schema": NODE_POLICY_CONTRACT_SCHEMA,
        "policy_hash": policy.policy_hash,
        "provenance": policy.provenance.source,
        "capability": capability,
        "decision": if enabled { "allow" } else { "deny" },
        "sandbox_profile": sandbox_profile,
        "allowed_writes": writable,
        "allowed_commands": commands,
        "network": network,
        "budgets": budgets,
        "verifier_ids": verifier_ids,
        "evidence_requirements": evidence,
        "external_side_effects": grant.is_some_and(|grant| grant.enabled && grant.external_side_effects),
    })
}

fn resolved_writable(policy: &HarnessPolicy, grant: Option<&CapabilityGrant>) -> Vec<String> {
    let Some(grant) = grant else {
        return Vec::new();
    };
    let mut writable = if grant.writable.is_empty() {
        policy.workspace.writable.clone()
    } else {
        grant.writable.clone()
    };
    writable.retain(|path| {
        !policy
            .workspace
            .forbidden
            .iter()
            .any(|forbidden| forbidden == path)
            && !policy
                .workspace
                .readonly
                .iter()
                .any(|readonly| readonly == path)
    });
    writable
}

fn json_network(network: &NetworkGrant) -> Value {
    serde_json::json!({ "default": network.default, "allowed_destinations": network.allowed_destinations })
}

fn resolved_budgets(policy: &HarnessPolicy, grant: Option<&CapabilityGrant>) -> Value {
    let overrides = grant.map(|grant| &grant.budgets);
    let pick = |value: u64, selected: Option<u64>| selected.unwrap_or(value);
    serde_json::json!({
        "max_steps": pick(policy.limits.max_steps, overrides.and_then(|v| v.max_steps)),
        "max_minutes": pick(policy.limits.max_minutes, overrides.and_then(|v| v.max_minutes)),
        "max_attempts": pick(policy.limits.max_attempts, overrides.and_then(|v| v.max_attempts)),
        "max_files_changed": pick(policy.limits.max_files_changed.min(policy.workspace.max_files_changed), overrides.and_then(|v| v.max_files_changed)),
        "max_diff_lines": pick(policy.limits.max_diff_lines.min(policy.workspace.max_diff_lines), overrides.and_then(|v| v.max_diff_lines)),
        "max_input_tokens": pick(policy.limits.max_input_tokens, overrides.and_then(|v| v.max_input_tokens)),
        "max_output_tokens": pick(policy.limits.max_output_tokens, overrides.and_then(|v| v.max_output_tokens)),
        "max_cost_usd": pick(policy.limits.max_cost_usd, overrides.and_then(|v| v.max_cost_usd)),
    })
}

/// Embed policy provenance in a harness source document.  This is used by the
/// compiler sidecar as well as by recompile-after-amendment paths.
pub(crate) fn attach_to_harness(harness: &mut Value, policy: &LoadedHarnessPolicy) {
    harness["harness_policy"] = serde_json::json!({
        "schema": HARNESS_POLICY_SCHEMA,
        "policy_hash": policy.policy_hash.clone(),
        "provenance": {
            "kind": policy.provenance.kind.clone(),
            "source": policy.provenance.source.clone(),
        },
        "policy": policy.normalized.clone(),
    });
}

/// Recover an embedded policy, falling back to the explicit safe default for
/// legacy harness sources that predate policy contracts.
pub(crate) fn from_harness(harness: &Value) -> Result<LoadedHarnessPolicy> {
    let Some(document) = harness.get("harness_policy") else {
        return Ok(builtin_default());
    };
    let normalized = document
        .get("policy")
        .cloned()
        .context("harness_policy.policy missing")?;
    let mut policy: HarnessPolicy =
        serde_json::from_value(normalized.clone()).context("decode embedded harness policy")?;
    let mut diagnostics = Vec::new();
    normalize_validate(&mut policy, &mut diagnostics)?;
    let normalized = serde_json::to_value(&policy)?;
    let computed = canonical_policy_hash(&normalized)?;
    let claimed = document
        .get("policy_hash")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if claimed != computed {
        bail!("embedded harness policy hash mismatch: claimed {claimed}, computed {computed}");
    }
    let provenance: PolicyProvenance =
        serde_json::from_value(document.get("provenance").cloned().unwrap_or(Value::Null))
            .unwrap_or_else(|_| PolicyProvenance::builtin());
    Ok(LoadedHarnessPolicy {
        policy,
        policy_hash: computed,
        provenance,
        normalized,
        diagnostics,
    })
}

/// Human/JSON output for `fractal harness show`.
pub(crate) fn show(repo: &Path, as_json: bool) -> Result<()> {
    let loaded = load_for_repo(repo)?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_value(&loaded, true))?
        );
    } else {
        println!("Harness policy: {}", loaded.policy.schema);
        println!("Policy hash: {}", loaded.policy_hash);
        println!(
            "Provenance: {} ({})",
            loaded.provenance.source, loaded.provenance.kind
        );
        println!("Mode: {}", loaded.policy.mode);
        println!(
            "Capabilities: {} explicit grant(s)",
            loaded.policy.capabilities.len()
        );
        println!(
            "Writes: {}  Commands: {}  Network: {}",
            loaded.policy.workspace.writable.len(),
            loaded.policy.commands.allow.len(),
            loaded.policy.network.default
        );
        for diagnostic in loaded.diagnostics {
            println!("Diagnostic: {diagnostic}");
        }
    }
    Ok(())
}

/// Human/JSON output for `fractal harness validate`.
pub(crate) fn validate(repo: &Path, as_json: bool) -> Result<()> {
    match load_for_repo(repo) {
        Ok(loaded) => {
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report_value(&loaded, false))?
                );
            } else {
                println!(
                    "Valid harness policy {} ({})",
                    loaded.policy_hash, loaded.provenance.source
                );
                for diagnostic in loaded.diagnostics {
                    println!("Diagnostic: {diagnostic}");
                }
            }
            Ok(())
        }
        Err(error) => {
            if as_json {
                println!(
                    "{}",
                    serde_json::json!({"schema":"fractal.harness_policy_validation.v1", "valid":false, "error": format!("{error:#}")})
                );
            }
            Err(error)
        }
    }
}

fn report_value(loaded: &LoadedHarnessPolicy, include_policy: bool) -> Value {
    let mut object = Map::new();
    object.insert(
        "schema".to_owned(),
        Value::String("fractal.harness_policy_report.v1".to_owned()),
    );
    object.insert("valid".to_owned(), Value::Bool(true));
    object.insert(
        "policy_hash".to_owned(),
        Value::String(loaded.policy_hash.clone()),
    );
    object.insert(
        "provenance".to_owned(),
        serde_json::to_value(&loaded.provenance).expect("provenance serializes"),
    );
    object.insert(
        "diagnostics".to_owned(),
        Value::Array(
            loaded
                .diagnostics
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    if include_policy {
        object.insert("policy".to_owned(), loaded.normalized.clone());
    }
    Value::Object(object)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn absent_policy_is_explicit_safe_default() {
        let policy = builtin_default();
        assert_eq!(policy.policy.schema, HARNESS_POLICY_SCHEMA);
        assert_eq!(policy.provenance.source, BUILTIN_PROVENANCE);
        assert!(policy.policy.capabilities["code.generate"]
            .writable
            .is_empty());
        assert_eq!(
            resolve_node_contract(&policy, "unknown.capability")["decision"],
            "deny"
        );
    }

    #[test]
    fn hash_is_stable_for_normalized_order() {
        let first = builtin_default();
        let mut second = first.normalized.clone();
        second["workspace"]["writable"] = serde_json::json!(["b/**", "a/**"]);
        let first_hash = canonical_policy_hash(&first.normalized).expect("hash");
        let second_hash = canonical_policy_hash(&second).expect("hash");
        assert_ne!(first_hash, second_hash);
        second["workspace"]["writable"] = serde_json::json!([]);
        assert_eq!(first_hash, canonical_policy_hash(&second).expect("hash"));
    }

    #[test]
    fn rejects_secrets_cot_paths_and_bounds() {
        let mut policy = builtin_default().policy;
        policy.extra.insert(
            "chain_of_thought".to_owned(),
            Value::String("no".to_owned()),
        );
        assert!(normalize_validate(&mut policy, &mut Vec::new()).is_err());
        let mut policy = builtin_default().policy;
        policy.workspace.writable.push("/tmp".to_owned());
        assert!(normalize_validate(&mut policy, &mut Vec::new()).is_err());
        let mut policy = builtin_default().policy;
        policy.limits.max_steps = 0;
        assert!(normalize_validate(&mut policy, &mut Vec::new()).is_err());
    }

    #[test]
    fn contract_contains_deny_defaults_and_grant_fields() {
        let mut loaded = builtin_default();
        loaded.policy.capabilities.insert(
            "code.generate".to_owned(),
            CapabilityGrant {
                writable: vec!["src/**".to_owned()],
                commands: vec!["cargo test".to_owned()],
                external_side_effects: true,
                network: Some(NetworkGrant {
                    default: "allow_scoped".to_owned(),
                    allowed_destinations: vec!["crates.io".to_owned()],
                }),
                ..CapabilityGrant::default()
            },
        );
        normalize_validate(&mut loaded.policy, &mut Vec::new()).expect("valid grant");
        loaded.normalized = serde_json::to_value(&loaded.policy).expect("encode");
        loaded.policy_hash = canonical_policy_hash(&loaded.normalized).expect("hash");
        let contract = resolve_node_contract(&loaded, "code.generate");
        assert_eq!(contract["decision"], "allow");
        assert_eq!(contract["allowed_writes"], serde_json::json!(["src/**"]));
        assert_eq!(contract["external_side_effects"], true);
    }

    #[test]
    fn loads_valid_yaml_and_reports_migration_without_writing() {
        let root = std::env::temp_dir().join(format!(
            "fractal-harness-policy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".fractal")).expect("directory");
        let policy_path = root.join(YAML_NAME);
        let before = std::fs::read_dir(&root).expect("read root").count();
        std::fs::write(
            &policy_path,
            r#"
version: fractal.harness.v1
mode: deny_by_default
workspace:
  writable: ["src/**"]
  max_files_changed: 4
  max_diff_lines: 120
capabilities:
  code.generate:
    writable: ["src/**"]
    commands: ["cargo test"]
    budgets:
      max_steps: 12
learning:
  minimum_confidence: 0.75
"#,
        )
        .expect("policy");
        let loaded = load_for_repo(&root).expect("valid YAML");
        assert_eq!(loaded.policy.schema, HARNESS_POLICY_SCHEMA);
        assert_eq!(loaded.policy.workspace.max_files_changed, 4);
        assert_eq!(loaded.policy.learning.minimum_confidence, 75);
        assert_eq!(loaded.provenance.source, "project:.fractal/harness.yaml");
        assert!(loaded
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("migrated")));
        assert_eq!(std::fs::read_dir(&root).expect("read root").count(), before);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rejects_unknown_future_schema_and_dangerous_paths() {
        let root = std::env::temp_dir().join(format!(
            "fractal-harness-policy-future-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".fractal")).expect("directory");
        std::fs::write(
            root.join(YAML_NAME),
            "version: fractal.harness_policy.v9\nmode: deny_by_default\n",
        )
        .expect("policy");
        let error = load_for_repo(&root).expect_err("future schema must fail closed");
        assert!(error.to_string().contains("future"));
        std::fs::write(
            root.join(YAML_NAME),
            "version: fractal.harness.v1\nworkspace:\n  writable: [/tmp]\n",
        )
        .expect("policy");
        let error = load_for_repo(&root).expect_err("absolute path must fail closed");
        assert!(error.to_string().contains("absolute path"));
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
