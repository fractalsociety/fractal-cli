//! Canonical portable contract for optional `fractal.efficiency.v1` data.
//!
//! Efficiency state lives beside the immutable execution graph and
//! `fractal.learning.v1`. This module defines the typed serde envelope only;
//! detection, policy, persistence, and scheduler integration are separate.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema identifier for the efficiency envelope.
pub(crate) const EFFICIENCY_SCHEMA: &str = "fractal.efficiency.v1";

/// Current aggregation algorithm version stamped on episodes and aggregates.
pub(crate) const AGGREGATION_VERSION: u32 = 1;

/// Maximum UTF-8 bytes for compact free-text bases and follow-up results.
pub(crate) const MAX_BASIS_BYTES: usize = 480;

/// Maximum UTF-8 bytes for a single evidence or path reference.
pub(crate) const MAX_REFERENCE_BYTES: usize = 240;

/// Maximum evidence references retained on one episode.
pub(crate) const MAX_EVIDENCE_REFS: usize = 32;

/// Maximum affected node IDs retained on one episode.
pub(crate) const MAX_AFFECTED_NODES: usize = 64;

/// Maximum assumptions retained on node planning metadata.
pub(crate) const MAX_ASSUMPTIONS: usize = 32;

/// Maximum files/systems listed on node planning metadata.
pub(crate) const MAX_AFFECTED_PATHS: usize = 64;

/// Detected avoidable-execution waste classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WasteType {
    DuplicateTask,
    DuplicateTest,
    DuplicateResearch,
    ConsolidatableTests,
    UnusedOutput,
    SupersededAssumption,
    SpecDrift,
    ExcessiveRetries,
    OverlappingFiles,
    OverDecomposition,
    LowValueBranch,
    PrematureVerification,
    ExcessiveVerification,
}

impl WasteType {
    pub(crate) const ALL: [WasteType; 13] = [
        WasteType::DuplicateTask,
        WasteType::DuplicateTest,
        WasteType::DuplicateResearch,
        WasteType::ConsolidatableTests,
        WasteType::UnusedOutput,
        WasteType::SupersededAssumption,
        WasteType::SpecDrift,
        WasteType::ExcessiveRetries,
        WasteType::OverlappingFiles,
        WasteType::OverDecomposition,
        WasteType::LowValueBranch,
        WasteType::PrematureVerification,
        WasteType::ExcessiveVerification,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateTask => "duplicate_task",
            Self::DuplicateTest => "duplicate_test",
            Self::DuplicateResearch => "duplicate_research",
            Self::ConsolidatableTests => "consolidatable_tests",
            Self::UnusedOutput => "unused_output",
            Self::SupersededAssumption => "superseded_assumption",
            Self::SpecDrift => "spec_drift",
            Self::ExcessiveRetries => "excessive_retries",
            Self::OverlappingFiles => "overlapping_files",
            Self::OverDecomposition => "over_decomposition",
            Self::LowValueBranch => "low_value_branch",
            Self::PrematureVerification => "premature_verification",
            Self::ExcessiveVerification => "excessive_verification",
        }
    }
}

/// Stable repair actions proposed or applied against detected waste.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepairAction {
    Merge,
    Cancel,
    DelayVerification,
    StopDownstream,
    Reassign,
    ConsolidateVerifiers,
    SplitDrift,
}

impl RepairAction {
    pub(crate) const ALL: [RepairAction; 7] = [
        RepairAction::Merge,
        RepairAction::Cancel,
        RepairAction::DelayVerification,
        RepairAction::StopDownstream,
        RepairAction::Reassign,
        RepairAction::ConsolidateVerifiers,
        RepairAction::SplitDrift,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Cancel => "cancel",
            Self::DelayVerification => "delay_verification",
            Self::StopDownstream => "stop_downstream",
            Self::Reassign => "reassign",
            Self::ConsolidateVerifiers => "consolidate_verifiers",
            Self::SplitDrift => "split_drift",
        }
    }
}

/// Governed efficiency operating modes. Suggest is the product default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EfficiencyMode {
    Observe,
    #[default]
    Suggest,
    AutoOptimize,
}

impl EfficiencyMode {
    #[allow(dead_code)]
    pub(crate) const ALL: [EfficiencyMode; 3] = [
        EfficiencyMode::Observe,
        EfficiencyMode::Suggest,
        EfficiencyMode::AutoOptimize,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Suggest => "suggest",
            Self::AutoOptimize => "auto_optimize",
        }
    }
}

/// Planning-time efficiency metadata required on newly planned nodes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct NodeEfficiencyMetadata {
    pub(crate) estimated_remaining_tokens: u64,
    pub(crate) dependencies: Vec<String>,
    pub(crate) expected_artifact: String,
    pub(crate) files_or_systems_affected: Vec<String>,
    pub(crate) verification_plan: String,
    pub(crate) current_assumptions: Vec<String>,
    /// Similarity scores keyed by peer node id in `[0.0, 1.0]`.
    pub(crate) similarity_to_other_active_nodes: BTreeMap<String, f64>,
    /// Confidence the node is still useful, in `[0.0, 1.0]`.
    pub(crate) confidence_still_useful: f64,
}

/// Compact, auditable efficiency episode recorded at a safe boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct EfficiencyEpisode {
    pub(crate) episode_id: String,
    pub(crate) waste_type: WasteType,
    pub(crate) detected_node: String,
    pub(crate) affected_node_ids: Vec<String>,
    pub(crate) affected_count: u32,
    pub(crate) proposed_action: RepairAction,
    pub(crate) accepted: bool,
    pub(crate) mode: EfficiencyMode,
    /// Gross estimated avoidable tokens before confidence adjustment.
    pub(crate) estimated_tokens_avoided: u64,
    pub(crate) estimation_basis: String,
    /// Detection confidence in `[0.0, 1.0]`.
    pub(crate) confidence: f64,
    /// `estimated_tokens_avoided` scaled by confidence.
    pub(crate) confidence_adjusted_tokens_avoided: u64,
    /// Present only when explicit baseline/comparison evidence exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) realized_tokens_saved: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) realization_basis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) actual_followup_result: Option<String>,
    pub(crate) human_override: bool,
    pub(crate) actor: String,
    pub(crate) detected_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resolved_at: Option<String>,
    pub(crate) evidence_refs: Vec<String>,
    pub(crate) aggregation_version: u32,
    pub(crate) config_hash: String,
}

/// Deduplicated totals for one build or across a lifetime of builds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct EfficiencyAggregate {
    pub(crate) episode_count: u32,
    pub(crate) gross_estimated_tokens_avoided: u64,
    pub(crate) confidence_adjusted_tokens_avoided: u64,
    pub(crate) realized_tokens_saved: u64,
    pub(crate) estimated_cost_avoided: f64,
    pub(crate) realized_cost_avoided: f64,
    pub(crate) estimated_agent_hours_avoided: f64,
    pub(crate) realized_agent_hours_avoided: f64,
    pub(crate) rework_prevented: u32,
    pub(crate) waste_breakdown: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) highest_intervention: Option<RepairAction>,
    pub(crate) aggregation_version: u32,
    pub(crate) config_hash: String,
}

/// Optional portable envelope stored beside learning and execution state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct EfficiencyData {
    pub(crate) schema: String,
    pub(crate) mode: EfficiencyMode,
    pub(crate) aggregation_version: u32,
    pub(crate) config_hash: String,
    #[serde(default)]
    pub(crate) episodes: Vec<EfficiencyEpisode>,
    #[serde(default)]
    pub(crate) build: EfficiencyAggregate,
    #[serde(default)]
    pub(crate) lifetime: EfficiencyAggregate,
}

impl Default for EfficiencyData {
    fn default() -> Self {
        Self {
            schema: EFFICIENCY_SCHEMA.to_owned(),
            mode: EfficiencyMode::Suggest,
            aggregation_version: AGGREGATION_VERSION,
            config_hash: String::new(),
            episodes: Vec::new(),
            build: EfficiencyAggregate {
                aggregation_version: AGGREGATION_VERSION,
                ..EfficiencyAggregate::default()
            },
            lifetime: EfficiencyAggregate {
                aggregation_version: AGGREGATION_VERSION,
                ..EfficiencyAggregate::default()
            },
        }
    }
}

/// Validate node planning metadata ranges and size bounds.
pub(crate) fn validate_node_metadata(meta: &NodeEfficiencyMetadata) -> Result<(), String> {
    if meta.expected_artifact.trim().is_empty() {
        return Err("expected_artifact must be non-empty".to_owned());
    }
    if meta.expected_artifact.len() > MAX_BASIS_BYTES {
        return Err("expected_artifact exceeds size bound".to_owned());
    }
    if meta.verification_plan.trim().is_empty() {
        return Err("verification_plan must be non-empty".to_owned());
    }
    if meta.verification_plan.len() > MAX_BASIS_BYTES {
        return Err("verification_plan exceeds size bound".to_owned());
    }
    if meta.files_or_systems_affected.len() > MAX_AFFECTED_PATHS {
        return Err("files_or_systems_affected exceeds count bound".to_owned());
    }
    if meta.current_assumptions.len() > MAX_ASSUMPTIONS {
        return Err("current_assumptions exceeds count bound".to_owned());
    }
    for assumption in &meta.current_assumptions {
        validate_secret_safe_text(assumption, "current_assumptions")?;
        if assumption.len() > MAX_BASIS_BYTES {
            return Err("assumption exceeds size bound".to_owned());
        }
    }
    for path in &meta.files_or_systems_affected {
        validate_reference(path)?;
    }
    for dependency in &meta.dependencies {
        if dependency.trim().is_empty() {
            return Err("dependencies must be non-empty ids".to_owned());
        }
    }
    validate_unit_interval(meta.confidence_still_useful, "confidence_still_useful")?;
    for (peer, score) in &meta.similarity_to_other_active_nodes {
        if peer.trim().is_empty() {
            return Err("similarity peer id must be non-empty".to_owned());
        }
        validate_unit_interval(*score, "similarity_to_other_active_nodes")?;
    }
    Ok(())
}

/// Validate a complete efficiency envelope.
pub(crate) fn validate(data: &EfficiencyData) -> Result<(), String> {
    if data.schema != EFFICIENCY_SCHEMA {
        return Err(format!("efficiency.schema must equal {EFFICIENCY_SCHEMA}"));
    }
    if data.aggregation_version == 0 {
        return Err("aggregation_version must be >= 1".to_owned());
    }
    if data.config_hash.trim().is_empty() {
        return Err("config_hash must be non-empty".to_owned());
    }
    validate_reference(&data.config_hash)?;
    validate_aggregate(&data.build, "build")?;
    validate_aggregate(&data.lifetime, "lifetime")?;
    let mut seen = std::collections::BTreeSet::new();
    for episode in &data.episodes {
        validate_episode(episode)?;
        if !seen.insert(episode.episode_id.clone()) {
            return Err(format!(
                "duplicate efficiency episode id `{}`",
                episode.episode_id
            ));
        }
    }
    Ok(())
}

fn validate_aggregate(aggregate: &EfficiencyAggregate, label: &str) -> Result<(), String> {
    if aggregate.aggregation_version == 0 {
        return Err(format!("{label}.aggregation_version must be >= 1"));
    }
    for (key, value) in [
        ("estimated_cost_avoided", aggregate.estimated_cost_avoided),
        ("realized_cost_avoided", aggregate.realized_cost_avoided),
        (
            "estimated_agent_hours_avoided",
            aggregate.estimated_agent_hours_avoided,
        ),
        (
            "realized_agent_hours_avoided",
            aggregate.realized_agent_hours_avoided,
        ),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(format!(
                "{label}.{key} must be a finite non-negative number"
            ));
        }
    }
    for waste in aggregate.waste_breakdown.keys() {
        if WasteType::ALL
            .iter()
            .all(|known| known.as_str() != waste.as_str())
        {
            return Err(format!("{label}.waste_breakdown has unknown key `{waste}`"));
        }
    }
    Ok(())
}

fn validate_episode(episode: &EfficiencyEpisode) -> Result<(), String> {
    if episode.episode_id.trim().is_empty() || episode.episode_id.len() > MAX_REFERENCE_BYTES {
        return Err("episode_id must be a compact non-empty identifier".to_owned());
    }
    if episode.detected_node.trim().is_empty() {
        return Err("detected_node must be non-empty".to_owned());
    }
    if episode.affected_node_ids.len() > MAX_AFFECTED_NODES {
        return Err("affected_node_ids exceeds count bound".to_owned());
    }
    if episode.affected_count as usize != episode.affected_node_ids.len()
        && episode.affected_count < episode.affected_node_ids.len() as u32
    {
        return Err("affected_count must cover affected_node_ids".to_owned());
    }
    if episode.affected_count == 0 {
        return Err("affected_count must be >= 1".to_owned());
    }
    validate_unit_interval(episode.confidence, "confidence")?;
    validate_basis(&episode.estimation_basis, "estimation_basis")?;
    if let Some(basis) = &episode.realization_basis {
        validate_basis(basis, "realization_basis")?;
    }
    if let Some(follow_up) = &episode.actual_followup_result {
        validate_basis(follow_up, "actual_followup_result")?;
    }
    if episode.actor.trim().is_empty() || episode.actor.len() > MAX_REFERENCE_BYTES {
        return Err("actor must be a compact non-empty identifier".to_owned());
    }
    if episode.detected_at.trim().is_empty() {
        return Err("detected_at must be non-empty".to_owned());
    }
    if episode.evidence_refs.len() > MAX_EVIDENCE_REFS {
        return Err("evidence_refs exceeds count bound".to_owned());
    }
    for reference in &episode.evidence_refs {
        validate_reference(reference)?;
        validate_secret_safe_text(reference, "evidence_refs")?;
    }
    if episode.aggregation_version == 0 {
        return Err("episode.aggregation_version must be >= 1".to_owned());
    }
    validate_reference(&episode.config_hash)?;
    if episode.config_hash.trim().is_empty() {
        return Err("episode.config_hash must be non-empty".to_owned());
    }

    let expected_adjusted =
        ((episode.estimated_tokens_avoided as f64) * episode.confidence).floor() as u64;
    if episode.confidence_adjusted_tokens_avoided > episode.estimated_tokens_avoided {
        return Err(
            "confidence_adjusted_tokens_avoided cannot exceed estimated_tokens_avoided".to_owned(),
        );
    }
    // Allow exact floor scaling; detectors may also clamp lower, but never higher than floor.
    if episode.confidence_adjusted_tokens_avoided > expected_adjusted {
        return Err(
            "confidence_adjusted_tokens_avoided exceeds confidence-scaled estimate".to_owned(),
        );
    }

    match (
        episode.realized_tokens_saved,
        episode.realization_basis.as_ref(),
    ) {
        (Some(_), None) => {
            return Err(
                "realized_tokens_saved requires realization_basis with comparison evidence"
                    .to_owned(),
            );
        }
        (Some(_), Some(basis)) if !has_comparison_evidence(basis, &episode.evidence_refs) => {
            return Err(
                "realized_tokens_saved requires explicit baseline/comparison evidence".to_owned(),
            );
        }
        (None, Some(_)) => {
            return Err("realization_basis requires realized_tokens_saved".to_owned());
        }
        _ => {}
    }
    Ok(())
}

/// True when both the realization basis and an evidence ref explicitly name a
/// baseline/comparison. Used by validation and by aggregate folds so estimates
/// never populate realized totals.
pub(crate) fn has_comparison_evidence(basis: &str, evidence_refs: &[String]) -> bool {
    let lowered = basis.to_ascii_lowercase();
    let basis_ok = lowered.contains("baseline")
        || lowered.contains("comparison")
        || lowered.contains("before_after")
        || lowered.contains("diff");
    let evidence_ok = evidence_refs.iter().any(|reference| {
        let value = reference.to_ascii_lowercase();
        value.contains("baseline")
            || value.contains("comparison")
            || value.contains("before_after")
            || value.contains("diff")
    });
    basis_ok && evidence_ok
}

fn validate_unit_interval(value: f64, field: &str) -> Result<(), String> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!("{field} must be a finite number in 0..=1"));
    }
    Ok(())
}

fn validate_basis(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must be non-empty"));
    }
    if value.len() > MAX_BASIS_BYTES {
        return Err(format!("{field} exceeds size bound"));
    }
    validate_secret_safe_text(value, field)
}

fn validate_reference(reference: &str) -> Result<(), String> {
    if reference.is_empty()
        || reference.len() > MAX_REFERENCE_BYTES
        || reference.chars().any(char::is_whitespace)
    {
        return Err("references must be compact external IDs without whitespace".to_owned());
    }
    validate_secret_safe_text(reference, "reference")
}

fn validate_secret_safe_text(value: &str, field: &str) -> Result<(), String> {
    let lowered = value.to_ascii_lowercase();
    for needle in [
        "authorization",
        "api_key",
        "apikey",
        "password",
        "private_key",
        "private-key",
        "secret",
        "cookie",
        "bearer ",
        "token=",
    ] {
        if lowered.contains(needle) {
            return Err(format!(
                "{field} must not contain credential-shaped material"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning_data::LearningData;
    use serde_json::{json, Value};

    /// Minimal project slice used only to prove optional efficiency embedding.
    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct ProjectContractSlice {
        schema: String,
        #[serde(default)]
        learning: LearningData,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        efficiency: Option<EfficiencyData>,
    }

    fn sample_episode() -> EfficiencyEpisode {
        EfficiencyEpisode {
            episode_id: "ep_duplicate_task_node_a".to_owned(),
            waste_type: WasteType::DuplicateTask,
            detected_node: "node_a".to_owned(),
            affected_node_ids: vec!["node_a".to_owned(), "node_b".to_owned()],
            affected_count: 2,
            proposed_action: RepairAction::Cancel,
            accepted: false,
            mode: EfficiencyMode::Suggest,
            estimated_tokens_avoided: 12_000,
            estimation_basis: "exact title and artifact match against node_b".to_owned(),
            confidence: 0.85,
            confidence_adjusted_tokens_avoided: 10_200,
            realized_tokens_saved: None,
            realization_basis: None,
            actual_followup_result: None,
            human_override: false,
            actor: "fractal-efficiency".to_owned(),
            detected_at: "2026-07-29T13:00:00Z".to_owned(),
            resolved_at: None,
            evidence_refs: vec!["sim:node_a:node_b".to_owned()],
            aggregation_version: AGGREGATION_VERSION,
            config_hash: "cfg_suggest_v1".to_owned(),
        }
    }

    fn sample_efficiency() -> EfficiencyData {
        let mut waste_breakdown = BTreeMap::new();
        waste_breakdown.insert(WasteType::DuplicateTask.as_str().to_owned(), 1);
        let aggregate = EfficiencyAggregate {
            episode_count: 1,
            gross_estimated_tokens_avoided: 12_000,
            confidence_adjusted_tokens_avoided: 10_200,
            realized_tokens_saved: 0,
            estimated_cost_avoided: 0.36,
            realized_cost_avoided: 0.0,
            estimated_agent_hours_avoided: 0.4,
            realized_agent_hours_avoided: 0.0,
            rework_prevented: 1,
            waste_breakdown,
            highest_intervention: Some(RepairAction::Cancel),
            aggregation_version: AGGREGATION_VERSION,
            config_hash: "cfg_suggest_v1".to_owned(),
        };
        EfficiencyData {
            schema: EFFICIENCY_SCHEMA.to_owned(),
            mode: EfficiencyMode::Suggest,
            aggregation_version: AGGREGATION_VERSION,
            config_hash: "cfg_suggest_v1".to_owned(),
            episodes: vec![sample_episode()],
            build: aggregate.clone(),
            lifetime: aggregate,
        }
    }

    #[test]
    fn waste_repair_and_mode_labels_are_exact() {
        assert_eq!(
            WasteType::ALL.map(|value| serde_json::to_value(value).unwrap()),
            [
                "duplicate_task",
                "duplicate_test",
                "duplicate_research",
                "consolidatable_tests",
                "unused_output",
                "superseded_assumption",
                "spec_drift",
                "excessive_retries",
                "overlapping_files",
                "over_decomposition",
                "low_value_branch",
                "premature_verification",
                "excessive_verification",
            ]
            .map(Value::from)
        );
        assert_eq!(
            RepairAction::ALL.map(|value| serde_json::to_value(value).unwrap()),
            [
                "merge",
                "cancel",
                "delay_verification",
                "stop_downstream",
                "reassign",
                "consolidate_verifiers",
                "split_drift",
            ]
            .map(Value::from)
        );
        assert_eq!(
            EfficiencyMode::ALL.map(|value| serde_json::to_value(value).unwrap()),
            ["observe", "suggest", "auto_optimize"].map(Value::from)
        );
        assert!(serde_json::from_str::<WasteType>("\"duplicate_work\"").is_err());
        assert!(serde_json::from_str::<RepairAction>("\"rewrite\"").is_err());
        assert!(serde_json::from_str::<EfficiencyMode>("\"autonomous\"").is_err());
        assert_eq!(EfficiencyMode::default(), EfficiencyMode::Suggest);
        for value in WasteType::ALL {
            assert_eq!(
                serde_json::to_value(value).unwrap(),
                Value::from(value.as_str())
            );
        }
        for value in RepairAction::ALL {
            assert_eq!(
                serde_json::to_value(value).unwrap(),
                Value::from(value.as_str())
            );
        }
        for value in EfficiencyMode::ALL {
            assert_eq!(
                serde_json::to_value(value).unwrap(),
                Value::from(value.as_str())
            );
        }
    }

    #[test]
    fn golden_efficiency_envelope_round_trips() {
        let data = sample_efficiency();
        validate(&data).expect("sample envelope must validate");
        let encoded = serde_json::to_value(&data).unwrap();
        assert_eq!(encoded["schema"], EFFICIENCY_SCHEMA);
        assert_eq!(encoded["mode"], "suggest");
        assert_eq!(encoded["aggregation_version"], AGGREGATION_VERSION);
        assert_eq!(encoded["config_hash"], "cfg_suggest_v1");
        assert_eq!(encoded["episodes"][0]["waste_type"], "duplicate_task");
        assert_eq!(encoded["episodes"][0]["proposed_action"], "cancel");
        assert_eq!(
            encoded["episodes"][0]["episode_id"],
            "ep_duplicate_task_node_a"
        );
        assert_eq!(encoded["episodes"][0]["human_override"], false);
        assert!(encoded["episodes"][0]
            .get("realized_tokens_saved")
            .is_none());
        let decoded: EfficiencyData = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded, data);
        let again = serde_json::to_value(&decoded).unwrap();
        assert_eq!(again, encoded);
    }

    #[test]
    fn realized_savings_require_comparison_evidence() {
        let mut episode = sample_episode();
        episode.realized_tokens_saved = Some(9_000);
        assert!(validate_episode(&episode).is_err());

        episode.realization_basis = Some("baseline comparison against prior run".to_owned());
        episode.evidence_refs = vec!["baseline:run_42".to_owned()];
        assert!(validate_episode(&episode).is_ok());

        episode.confidence = 1.5;
        assert!(validate_episode(&episode).is_err());
        episode.confidence = 0.85;
        episode.confidence_adjusted_tokens_avoided = 12_001;
        assert!(validate_episode(&episode).is_err());
    }

    #[test]
    fn node_efficiency_metadata_bounds() {
        let mut meta = NodeEfficiencyMetadata {
            estimated_remaining_tokens: 4_000,
            dependencies: vec!["node_root".to_owned()],
            expected_artifact: "src/efficiency.rs".to_owned(),
            files_or_systems_affected: vec!["src/efficiency.rs".to_owned()],
            verification_plan: "cargo test --bin fractal efficiency::".to_owned(),
            current_assumptions: vec!["contract is optional beside learning".to_owned()],
            similarity_to_other_active_nodes: BTreeMap::from([("node_other".to_owned(), 0.2)]),
            confidence_still_useful: 0.9,
        };
        assert!(validate_node_metadata(&meta).is_ok());
        meta.confidence_still_useful = -0.1;
        assert!(validate_node_metadata(&meta).is_err());
        meta.confidence_still_useful = 0.9;
        meta.current_assumptions = vec!["password=super-secret".to_owned()];
        assert!(validate_node_metadata(&meta).is_err());
    }

    #[test]
    fn legacy_project_without_efficiency_remains_valid() {
        let legacy = json!({
            "schema": "fractal.project.v1",
            "learning": {
                "schema": "fractal.learning.v1",
                "nodes": {
                    "n1": {
                        "node_id": "n1",
                        "node_type": "inference",
                        "objective": "establish contract",
                        "depends_on": [],
                        "attempt_count": 1,
                        "artifacts_produced": [],
                        "consumed_by": [],
                        "human_intervention": false,
                        "reopen_count": 0
                    }
                },
                "graph_edits": []
            }
        });
        let slice: ProjectContractSlice = serde_json::from_value(legacy.clone()).unwrap();
        assert!(slice.efficiency.is_none());
        assert_eq!(slice.learning.schema, "fractal.learning.v1");
        crate::learning_data::validate(&slice.learning).unwrap();

        let reencoded = serde_json::to_value(&slice).unwrap();
        assert!(reencoded.get("efficiency").is_none());
        assert_eq!(reencoded["learning"]["schema"], "fractal.learning.v1");
        assert_eq!(
            reencoded["learning"]["nodes"]["n1"]["objective"],
            "establish contract"
        );
    }

    #[test]
    fn efficiency_envelope_does_not_mutate_learning_v1() {
        let learning_before = LearningData {
            schema: "fractal.learning.v1".to_owned(),
            nodes: BTreeMap::new(),
            graph_edits: Vec::new(),
            outcome: None,
        };
        let learning_bytes = serde_json::to_vec(&learning_before).unwrap();

        let mut slice = ProjectContractSlice {
            schema: "fractal.project.v1".to_owned(),
            learning: learning_before.clone(),
            efficiency: Some(sample_efficiency()),
        };
        validate(slice.efficiency.as_ref().unwrap()).unwrap();
        let with_efficiency = serde_json::to_value(&slice).unwrap();
        assert_eq!(with_efficiency["efficiency"]["schema"], EFFICIENCY_SCHEMA);
        assert_eq!(with_efficiency["learning"]["schema"], "fractal.learning.v1");

        // Dropping efficiency leaves learning JSON semantically identical.
        slice.efficiency = None;
        let learning_after = serde_json::to_vec(&slice.learning).unwrap();
        assert_eq!(learning_bytes, learning_after);
        assert_eq!(slice.learning, learning_before);
        assert_eq!(
            serde_json::from_slice::<LearningData>(&learning_bytes)
                .unwrap()
                .schema,
            "fractal.learning.v1"
        );
    }

    #[test]
    fn unknown_waste_in_aggregate_is_rejected() {
        let mut data = sample_efficiency();
        data.build
            .waste_breakdown
            .insert("mystery_waste".to_owned(), 1);
        assert!(validate(&data).is_err());
    }
}
