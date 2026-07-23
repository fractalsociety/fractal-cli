//! Direct natural-language → [`WorkV1`] constructor (pipeline P0.3).
//!
//! Reuses `fractal-contracts` field layout and Rust-parity `content_hash`.
//! Optional classifier output (P0.2) can be supplied; otherwise a local rules
//! mirror of `classifier.ts` fills intent/privacy/risk for the direct path.

use std::fmt;

use fractal_contracts::{
    validate_goal_not_command, validate_success_criteria, IntentSource, NetworkPolicy,
    PrivacyClass, WorkConstraints, WorkInput, WorkRisk, WorkV1, WorkValidationError,
    FRACTAL_WORK_V1,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Capability id mapped from a classifier intent family.
const INTENT_CAPABILITY: &[(&str, &str)] = &[
    ("answer", "reason.answer"),
    ("plan", "reason.plan"),
    ("code", "code.generate"),
    ("analyze", "content.analyze"),
    ("summarize", "content.summarize"),
    ("research", "retrieval.research"),
    ("execute", "tool.execute"),
    ("verify", "result.verify"),
];

/// Classifier-shaped fields consumed by the FractalWork constructor.
///
/// Field names mirror FractalWork `TaskClassification` so P0.2 can pass JSON
/// through without remapping.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IntentClassification {
    /// Intent family (`code`, `plan`, `analyze`, …).
    pub intent: String,
    /// Topic label from the classifier.
    #[serde(default)]
    pub topic: String,
    /// Privacy level (`local-only`, `public`, `user-approved-cloud`).
    pub privacy_level: String,
    /// Difficulty (`easy`, `medium`, `hard`, `frontier-needed`).
    #[serde(default = "default_difficulty")]
    pub difficulty: String,
    /// Verification level (`none`, `basic`, `verifier`, `adversarial`, `hidden-eval`).
    #[serde(default = "default_verification")]
    pub verification_level: String,
    /// Likely tool names (become `tool.<name>` capabilities).
    #[serde(default)]
    pub likely_tools: Vec<String>,
    /// Whether external/network calls were approved.
    #[serde(default)]
    pub external_calls_allowed: bool,
}

fn default_difficulty() -> String {
    "medium".to_owned()
}

fn default_verification() -> String {
    "basic".to_owned()
}

/// Inputs for the direct NL → FractalWork constructor.
#[derive(Clone, Debug)]
pub struct NlWorkRequest {
    /// Raw natural-language request.
    pub request: String,
    /// Stable requester identity (for example `local:user`).
    pub requester: String,
    /// Creation timestamp in Unix milliseconds (fixed in tests for determinism).
    pub created_at_ms: u64,
    /// Optional explicit work id; otherwise derived from the request hash.
    pub work_id: Option<String>,
    /// Optional classifier output (P0.2). When absent, local rules classify.
    pub classification: Option<IntentClassification>,
    /// Optional repository path recorded as a memory scope.
    pub repo: Option<String>,
    /// Optional success criteria; defaults to a single machine-checkable goal.
    pub success_criteria: Option<Vec<String>>,
    /// Optional cost ceiling in microunits.
    pub max_cost_microunits: Option<u64>,
}

/// Errors from the NL → work constructor.
#[derive(Debug)]
pub enum NlWorkError {
    /// Goal failed the declarative-goal check.
    Validation(WorkValidationError),
    /// Request text was empty.
    EmptyRequest,
}

impl fmt::Display for NlWorkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(error) => write!(formatter, "{error}"),
            Self::EmptyRequest => formatter.write_str("natural-language request is empty"),
        }
    }
}

impl std::error::Error for NlWorkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Validation(error) => Some(error),
            Self::EmptyRequest => None,
        }
    }
}

impl From<WorkValidationError> for NlWorkError {
    fn from(error: WorkValidationError) -> Self {
        Self::Validation(error)
    }
}

/// Build a hashed [`WorkV1`] and the accompanying intent-source audit record.
///
/// # Errors
///
/// Returns [`NlWorkError`] when the request is empty, the goal looks executable,
/// or success criteria fail validation / hashing.
pub fn build_work_from_nl(input: &NlWorkRequest) -> Result<(WorkV1, IntentSource), NlWorkError> {
    let request = input.request.trim();
    if request.is_empty() {
        return Err(NlWorkError::EmptyRequest);
    }

    let classification = input
        .classification
        .clone()
        .unwrap_or_else(|| classify_locally(request));
    let intent_family = normalize_intent(&classification.intent);
    let intent = format!("nl.{intent_family}");
    let goal = abstract_goal(request);
    validate_goal_not_command(&goal)?;

    let success_criteria = input.success_criteria.clone().unwrap_or_else(|| {
        vec![format!(
            "the requested outcome is delivered and verified: {goal}"
        )]
    });
    validate_success_criteria(&success_criteria)?;

    let prompt_hash = sha256_hex_prefixed(request.as_bytes());
    let work_id = input
        .work_id
        .clone()
        .unwrap_or_else(|| format!("fw_{}", &prompt_hash[7..19]));

    let privacy = map_privacy(&classification.privacy_level);
    let network_policy = map_network_policy(privacy, classification.external_calls_allowed);
    let risk = map_risk(
        &classification.difficulty,
        &classification.verification_level,
    );
    let required_capabilities = required_capabilities(&intent_family, &classification.likely_tools);
    let memory_scopes = memory_scopes(input.repo.as_deref(), &work_id);

    let work = WorkV1 {
        schema: FRACTAL_WORK_V1.to_owned(),
        work_id,
        intent: intent.clone(),
        goal: goal.clone(),
        inputs: vec![WorkInput {
            kind: "nl_prompt".to_owned(),
            artifact_hash: prompt_hash,
        }],
        constraints: WorkConstraints {
            privacy,
            deadline_ms: deadline_for(&classification.difficulty),
            max_memory_mib: 20_480,
            max_tokens: 12_000,
            max_cost_microunits: input.max_cost_microunits.unwrap_or(0),
            network_policy,
        },
        required_capabilities,
        risk,
        success_criteria,
        memory_scopes,
        requester: input.requester.clone(),
        created_at_ms: input.created_at_ms,
        content_hash: String::new(),
    }
    .with_content_hash()?;

    let source = IntentSource::RawPrompt {
        raw_prompt: request.to_owned(),
        abstracted_intent: intent,
        abstracted_goal: goal,
        override_note: None,
    };
    Ok((work, source))
}

/// Local rules mirror of FractalWork `classifyWithRules` / `inferIntent`.
pub fn classify_locally(prompt: &str) -> IntentClassification {
    let text = prompt.to_ascii_lowercase();
    let intent = if regex_any(&text, &["code", "implement", "fix", "build", "refactor"]) {
        "code"
    } else if regex_any(&text, &["plan", "roadmap", "prd", "task", "tasks"]) {
        "plan"
    } else if regex_any(&text, &["analyze", "audit", "debug", "investigate"]) {
        "analyze"
    } else if regex_any(&text, &["summarize", "summary", "tl;dr"]) {
        "summarize"
    } else if regex_any(
        &text,
        &["research", "search", "find papers", "source", "sources"],
    ) {
        "research"
    } else if regex_any(&text, &["run", "execute", "deploy", "call tool"]) {
        "execute"
    } else if regex_any(&text, &["verify", "check", "validate", "test"]) {
        "verify"
    } else {
        "answer"
    }
    .to_owned();

    let sensitive = text.contains("confidential")
        || text.contains("local only")
        || text.contains("do not share")
        || text.contains("api_key")
        || text.contains("password")
        || text.contains("private key");

    IntentClassification {
        intent,
        topic: if regex_any(
            &text,
            &["repo", "typescript", "api", "code", "test", "build"],
        ) {
            "coding".to_owned()
        } else {
            "general".to_owned()
        },
        privacy_level: if sensitive {
            "local-only".to_owned()
        } else {
            "public".to_owned()
        },
        difficulty: if prompt.len() > 1200 {
            "hard".to_owned()
        } else if prompt.len() > 240 || regex_any(&text, &["implement", "debug", "analyze", "plan"])
        {
            "medium".to_owned()
        } else {
            "easy".to_owned()
        },
        verification_level: if regex_any(&text, &["verify", "validate", "test"]) {
            "verifier".to_owned()
        } else if sensitive {
            "basic".to_owned()
        } else {
            "none".to_owned()
        },
        likely_tools: if regex_any(&text, &["repo", "codebase", "test", "tests"]) {
            vec!["repo-map".to_owned()]
        } else {
            Vec::new()
        },
        external_calls_allowed: false,
    }
}

fn regex_any(text: &str, words: &[&str]) -> bool {
    // Whole-word (token) match only. A prior `|| text.contains(word)` fallback
    // defeated the tokenization and matched substrings, so "code" fired on
    // "encode"/"barcode", "test" on "latest", "api" on "capital".
    words.iter().any(|word| {
        text.split(|c: char| !c.is_ascii_alphanumeric() && c != ';' && c != '-')
            .any(|token| token == *word)
    })
}

fn abstract_goal(request: &str) -> String {
    let trimmed = request.trim();
    // Prefer a declarative phrasing without shell punctuation.
    let without_bang = trimmed.trim_end_matches('!');
    if without_bang.chars().count() > 240 {
        let mut shortened: String = without_bang.chars().take(237).collect();
        shortened.push('…');
        shortened
    } else {
        without_bang.to_owned()
    }
}

fn normalize_intent(intent: &str) -> String {
    let lower = intent.trim().to_ascii_lowercase();
    if INTENT_CAPABILITY.iter().any(|(name, _)| *name == lower) {
        lower
    } else {
        "answer".to_owned()
    }
}

fn map_privacy(level: &str) -> PrivacyClass {
    match level {
        "local-only" | "local_only" => PrivacyClass::LocalOnly,
        "public" => PrivacyClass::Public,
        "restricted" => PrivacyClass::Restricted,
        _ => PrivacyClass::Private,
    }
}

fn map_network_policy(privacy: PrivacyClass, external_calls_allowed: bool) -> NetworkPolicy {
    match privacy {
        PrivacyClass::LocalOnly => NetworkPolicy::Deny,
        PrivacyClass::Public if external_calls_allowed => NetworkPolicy::RetrievalOnly,
        _ if external_calls_allowed => NetworkPolicy::AllowScoped,
        _ => NetworkPolicy::DenyByDefault,
    }
}

fn map_risk(difficulty: &str, verification: &str) -> WorkRisk {
    if verification == "hidden-eval" || difficulty == "frontier-needed" {
        WorkRisk::Critical
    } else if verification == "adversarial" || difficulty == "hard" {
        WorkRisk::High
    } else if difficulty == "medium" {
        WorkRisk::Medium
    } else {
        WorkRisk::Low
    }
}

fn required_capabilities(intent: &str, tools: &[String]) -> Vec<String> {
    let mut caps = Vec::new();
    if let Some((_, capability)) = INTENT_CAPABILITY.iter().find(|(name, _)| *name == intent) {
        caps.push((*capability).to_owned());
    }
    for tool in tools {
        let normalized = normalize_identifier(tool);
        if !normalized.is_empty() {
            caps.push(format!("tool.{normalized}"));
        }
    }
    caps.sort();
    caps.dedup();
    caps
}

fn memory_scopes(repo: Option<&str>, work_id: &str) -> Vec<String> {
    let mut scopes = vec![format!("work:{work_id}")];
    if let Some(repo) = repo {
        let name = std::path::Path::new(repo)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("project");
        let normalized = normalize_identifier(name);
        if !normalized.is_empty() {
            scopes.push(format!("project:{normalized}"));
        }
    }
    scopes.sort();
    scopes.dedup();
    scopes
}

fn deadline_for(difficulty: &str) -> u64 {
    match difficulty {
        "easy" => 120_000,
        "hard" | "frontier-needed" => 900_000,
        _ => 300_000,
    }
}

fn normalize_identifier(value: &str) -> String {
    let mut out = String::new();
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '/' | '-') {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

fn sha256_hex_prefixed(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("sha256:{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fractal_contracts::PrivacyClass;

    fn sample_request(request: &str) -> NlWorkRequest {
        NlWorkRequest {
            request: request.to_owned(),
            requester: "local:user".to_owned(),
            created_at_ms: 1_000,
            work_id: Some("fw_test_1".to_owned()),
            classification: None,
            repo: Some("/tmp/demo-cli".to_owned()),
            success_criteria: Some(vec!["unit tests pass".to_owned()]),
            max_cost_microunits: Some(0),
        }
    }

    #[test]
    fn builds_stable_hashed_work_from_nl() {
        let input = sample_request("build a tiny CLI that reverses a string");
        let (first, source) = build_work_from_nl(&input).expect("build");
        let (second, _) = build_work_from_nl(&input).expect("rebuild");

        assert_eq!(first.schema, FRACTAL_WORK_V1);
        assert_eq!(first.intent, "nl.code");
        assert!(first
            .required_capabilities
            .contains(&"code.generate".to_owned()));
        assert_eq!(first.constraints.privacy, PrivacyClass::Public);
        first.verify_content_hash().expect("hash verifies");
        assert_eq!(first.content_hash, second.content_hash);
        assert!(matches!(
            source,
            IntentSource::RawPrompt {
                abstracted_intent: ref intent,
                ..
            } if intent == "nl.code"
        ));
        assert!(first
            .memory_scopes
            .iter()
            .any(|scope| scope == "project:demo-cli"));
    }

    #[test]
    fn respects_classifier_privacy_and_rejects_empty() {
        let mut input = sample_request("summarize this confidential design note");
        input.classification = Some(IntentClassification {
            intent: "summarize".to_owned(),
            topic: "general".to_owned(),
            privacy_level: "local-only".to_owned(),
            difficulty: "easy".to_owned(),
            verification_level: "basic".to_owned(),
            likely_tools: vec![],
            external_calls_allowed: false,
        });
        let (work, _) = build_work_from_nl(&input).expect("private work");
        assert_eq!(work.constraints.privacy, PrivacyClass::LocalOnly);
        assert_eq!(work.constraints.network_policy, NetworkPolicy::Deny);
        assert_eq!(work.intent, "nl.summarize");

        let err = build_work_from_nl(&NlWorkRequest {
            request: "   ".to_owned(),
            requester: "local:user".to_owned(),
            created_at_ms: 0,
            work_id: None,
            classification: None,
            repo: None,
            success_criteria: None,
            max_cost_microunits: None,
        })
        .expect_err("empty");
        assert!(matches!(err, NlWorkError::EmptyRequest));
    }

    #[test]
    fn rejects_executable_looking_goals() {
        let err = build_work_from_nl(&sample_request("$ rm -rf /")).expect_err("shell goal");
        assert!(matches!(
            err,
            NlWorkError::Validation(WorkValidationError::ExecutableGoal)
        ));
    }
}
