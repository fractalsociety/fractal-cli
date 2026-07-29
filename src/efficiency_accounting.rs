//! Append-safe, idempotent persistence for `fractal.efficiency.v1` episodes.
//!
//! Episode identity is deterministic so retries under the project-file lock
//! deduplicate. Build and lifetime aggregates fold over deduplicated
//! `episode_id` values and keep estimates strictly separate from realized
//! savings (realized counts only with baseline/comparison evidence).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::efficiency::{
    has_comparison_evidence, validate, EfficiencyAggregate, EfficiencyData, EfficiencyEpisode,
    EfficiencyMode, RepairAction, WasteType, AGGREGATION_VERSION, EFFICIENCY_SCHEMA,
    MAX_AFFECTED_NODES, MAX_BASIS_BYTES, MAX_EVIDENCE_REFS, MAX_REFERENCE_BYTES,
};
use crate::project_file;

/// Hard cap on retained episodes in one portable envelope.
pub(crate) const MAX_EPISODES: usize = 4_096;

/// Maximum UTF-8 bytes for a compact actor label.
pub(crate) const MAX_ACTOR_BYTES: usize = MAX_REFERENCE_BYTES;

/// Outcome of an idempotent episode upsert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpsertOutcome {
    Inserted,
    IdempotentReplay,
    Updated,
}

/// Draft used to construct or update a compact efficiency episode.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EpisodeDraft {
    pub(crate) waste_type: WasteType,
    pub(crate) detected_node: String,
    pub(crate) affected_node_ids: Vec<String>,
    pub(crate) proposed_action: RepairAction,
    pub(crate) accepted: bool,
    pub(crate) mode: EfficiencyMode,
    pub(crate) estimated_tokens_avoided: u64,
    pub(crate) estimation_basis: String,
    pub(crate) confidence: f64,
    pub(crate) realized_tokens_saved: Option<u64>,
    pub(crate) realization_basis: Option<String>,
    pub(crate) actual_followup_result: Option<String>,
    pub(crate) human_override: bool,
    pub(crate) actor: String,
    pub(crate) evidence_refs: Vec<String>,
    pub(crate) config_hash: String,
    /// Optional fixed timestamp for deterministic tests; otherwise wall clock.
    pub(crate) detected_at: Option<String>,
    pub(crate) resolved_at: Option<String>,
}

/// Deterministic episode id from waste, detected node, affected set, and action.
pub(crate) fn derive_episode_id(
    waste_type: WasteType,
    detected_node: &str,
    affected_node_ids: &[String],
    proposed_action: RepairAction,
) -> String {
    let mut affected = affected_node_ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();
    affected.sort_unstable();
    affected.dedup();
    let material = format!(
        "fractal.efficiency.episode.v1|{}|{}|{}|{}",
        waste_type.as_str(),
        detected_node.trim(),
        affected.join(","),
        proposed_action.as_str()
    );
    let digest = Sha256::digest(material.as_bytes());
    let mut hex = String::with_capacity(24);
    for byte in digest.iter().take(12) {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("ep_{hex}")
}

/// Build a validated episode from a draft, capturing aggregation metadata.
pub(crate) fn build_episode(draft: &EpisodeDraft) -> Result<EfficiencyEpisode, String> {
    validate_draft_bounds(draft)?;
    let mut affected = draft
        .affected_node_ids
        .iter()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();
    affected.sort();
    affected.dedup();
    if affected.is_empty() {
        return Err("affected_node_ids must include at least one node".to_owned());
    }
    if !affected.iter().any(|id| id == draft.detected_node.trim()) {
        affected.insert(0, draft.detected_node.trim().to_owned());
        affected.sort();
        affected.dedup();
    }
    if affected.len() > MAX_AFFECTED_NODES {
        return Err("affected_node_ids exceeds count bound".to_owned());
    }

    let confidence_adjusted =
        ((draft.estimated_tokens_avoided as f64) * draft.confidence).floor() as u64;
    let detected_at = draft
        .detected_at
        .clone()
        .unwrap_or_else(project_file::project_timestamp);
    let episode = EfficiencyEpisode {
        episode_id: derive_episode_id(
            draft.waste_type,
            draft.detected_node.trim(),
            &affected,
            draft.proposed_action,
        ),
        waste_type: draft.waste_type,
        detected_node: draft.detected_node.trim().to_owned(),
        affected_count: affected.len() as u32,
        affected_node_ids: affected,
        proposed_action: draft.proposed_action,
        accepted: draft.accepted,
        mode: draft.mode,
        estimated_tokens_avoided: draft.estimated_tokens_avoided,
        estimation_basis: draft.estimation_basis.trim().to_owned(),
        confidence: draft.confidence,
        confidence_adjusted_tokens_avoided: confidence_adjusted,
        realized_tokens_saved: draft.realized_tokens_saved,
        realization_basis: draft
            .realization_basis
            .as_ref()
            .map(|value| value.trim().to_owned()),
        actual_followup_result: draft
            .actual_followup_result
            .as_ref()
            .map(|value| value.trim().to_owned()),
        human_override: draft.human_override,
        actor: draft.actor.trim().to_owned(),
        detected_at,
        resolved_at: draft
            .resolved_at
            .as_ref()
            .map(|value| value.trim().to_owned()),
        evidence_refs: draft
            .evidence_refs
            .iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect(),
        aggregation_version: AGGREGATION_VERSION,
        config_hash: draft.config_hash.trim().to_owned(),
    };
    // Reuse envelope validation by wrapping a one-episode document.
    let mut probe = empty_envelope(draft.mode, &episode.config_hash);
    probe.episodes.push(episode.clone());
    refresh_aggregates(&mut probe);
    validate(&probe)?;
    Ok(episode)
}

/// Upsert an episode into an in-memory envelope with idempotent deduplication.
pub(crate) fn upsert_episode(
    data: &mut EfficiencyData,
    episode: EfficiencyEpisode,
) -> Result<UpsertOutcome, String> {
    if data.schema != EFFICIENCY_SCHEMA {
        data.schema = EFFICIENCY_SCHEMA.to_owned();
    }
    if data.aggregation_version == 0 {
        data.aggregation_version = AGGREGATION_VERSION;
    }
    if data.config_hash.trim().is_empty() {
        data.config_hash = episode.config_hash.clone();
    }
    data.mode = episode.mode;
    data.config_hash = episode.config_hash.clone();
    data.aggregation_version = AGGREGATION_VERSION;

    if let Some(existing) = data
        .episodes
        .iter_mut()
        .find(|candidate| candidate.episode_id == episode.episode_id)
    {
        if core_identity_matches(existing, &episode) {
            if episode_state_equivalent(existing, &episode)
                || is_detection_only_replay(existing, &episode)
            {
                return Ok(UpsertOutcome::IdempotentReplay);
            }
            merge_episode_update(existing, &episode)?;
            refresh_aggregates(data);
            validate(data)?;
            return Ok(UpsertOutcome::Updated);
        }
        return Err(format!(
            "episode_id `{}` collides with a different detection identity",
            episode.episode_id
        ));
    }

    if data.episodes.len() >= MAX_EPISODES {
        return Err(format!(
            "efficiency episodes exceed count bound ({MAX_EPISODES})"
        ));
    }
    data.episodes.push(episode);
    refresh_aggregates(data);
    validate(data)?;
    Ok(UpsertOutcome::Inserted)
}

/// Atomically record an episode under the shared project-file lock.
pub(crate) fn record_episode(workspace: &Path, draft: EpisodeDraft) -> Result<UpsertOutcome> {
    let episode = build_episode(&draft).map_err(|error| anyhow::anyhow!(error))?;
    let mut outcome = UpsertOutcome::Inserted;
    project_file::mutate_document(workspace, |document| {
        let envelope = document
            .efficiency
            .get_or_insert_with(|| empty_envelope(episode.mode, &episode.config_hash));
        outcome =
            upsert_episode(envelope, episode.clone()).map_err(|error| anyhow::anyhow!(error))?;
        reject_efficiency_secrets(envelope)?;
        Ok(())
    })?;
    Ok(outcome)
}

/// Apply follow-up / override / evidence updates to an existing episode by id.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn update_episode(
    workspace: &Path,
    episode_id: &str,
    accepted: Option<bool>,
    human_override: Option<bool>,
    actual_followup_result: Option<String>,
    evidence_refs: Option<Vec<String>>,
    realized_tokens_saved: Option<Option<u64>>,
    realization_basis: Option<Option<String>>,
    resolved_at: Option<Option<String>>,
    actor: Option<String>,
) -> Result<UpsertOutcome> {
    let mut outcome = UpsertOutcome::IdempotentReplay;
    project_file::mutate_document(workspace, |document| {
        let Some(envelope) = document.efficiency.as_mut() else {
            bail!("efficiency envelope is absent");
        };
        let Some(existing) = envelope
            .episodes
            .iter_mut()
            .find(|episode| episode.episode_id == episode_id)
        else {
            bail!("unknown efficiency episode `{episode_id}`");
        };
        let before = existing.clone();
        if let Some(value) = accepted {
            existing.accepted = value;
        }
        if let Some(value) = human_override {
            existing.human_override = value;
        }
        if let Some(value) = actual_followup_result {
            existing.actual_followup_result = Some(value);
        }
        if let Some(refs) = evidence_refs {
            existing.evidence_refs = merge_evidence(&existing.evidence_refs, &refs)
                .map_err(|error| anyhow::anyhow!(error))?;
        }
        if let Some(value) = realized_tokens_saved {
            existing.realized_tokens_saved = value;
        }
        if let Some(value) = realization_basis {
            existing.realization_basis = value;
        }
        if let Some(value) = resolved_at {
            existing.resolved_at = value;
        }
        if let Some(value) = actor {
            existing.actor = value;
        }
        if episode_state_equivalent(&before, existing) {
            outcome = UpsertOutcome::IdempotentReplay;
        } else {
            outcome = UpsertOutcome::Updated;
        }
        refresh_aggregates(envelope);
        validate(envelope).map_err(|error| anyhow::anyhow!(error))?;
        reject_efficiency_secrets(envelope)?;
        Ok(())
    })?;
    Ok(outcome)
}

#[allow(dead_code)]
pub(crate) fn load_efficiency(workspace: &Path) -> Result<Option<EfficiencyData>> {
    Ok(project_file::load(workspace)?.efficiency)
}

/// Stable USD-per-token rate used by aggregate cost folds.
pub(crate) const TOKENS_TO_USD: f64 = 0.000_03;

/// Stable tokens-per-agent-hour rate used by aggregate hour folds.
pub(crate) const TOKENS_PER_AGENT_HOUR: f64 = 30_000.0;

/// Recompute build and lifetime aggregates from deduplicated episodes.
///
/// - **Build** folds episodes whose `config_hash` matches the envelope
///   (current build / config generation).
/// - **Lifetime** folds every retained episode across configs and builds.
///
/// Both folds are order-independent over `episode_id` and never treat estimates
/// as realized savings.
pub(crate) fn refresh_aggregates(data: &mut EfficiencyData) {
    data.build = fold_build_aggregate(&data.episodes, &data.config_hash);
    data.lifetime = fold_lifetime_aggregate(&data.episodes, &data.config_hash);
    data.aggregation_version = AGGREGATION_VERSION;
}

/// Fold the current-build aggregate: episodes matching `config_hash` only.
pub(crate) fn fold_build_aggregate(
    episodes: &[EfficiencyEpisode],
    config_hash: &str,
) -> EfficiencyAggregate {
    fold_episodes(episodes, config_hash, AggregateScope::Build)
}

/// Fold the lifetime aggregate: every deduplicated episode across builds.
pub(crate) fn fold_lifetime_aggregate(
    episodes: &[EfficiencyEpisode],
    config_hash: &str,
) -> EfficiencyAggregate {
    fold_episodes(episodes, config_hash, AggregateScope::Lifetime)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AggregateScope {
    Build,
    Lifetime,
}

fn fold_episodes(
    episodes: &[EfficiencyEpisode],
    config_hash: &str,
    scope: AggregateScope,
) -> EfficiencyAggregate {
    let deduped = dedupe_episodes(episodes);
    let mut waste_breakdown = BTreeMap::new();
    let mut highest_intervention = None;
    let mut episode_count = 0u32;
    let mut gross = 0u64;
    let mut adjusted = 0u64;
    let mut realized = 0u64;
    let mut rework_prevented = 0u32;

    for episode in &deduped {
        if scope == AggregateScope::Build && episode.config_hash != config_hash {
            continue;
        }
        episode_count = episode_count.saturating_add(1);
        gross = gross.saturating_add(episode.estimated_tokens_avoided);
        adjusted = adjusted.saturating_add(episode.confidence_adjusted_tokens_avoided);
        if let Some(saved) = realized_tokens_for_aggregate(episode) {
            realized = realized.saturating_add(saved);
        }
        if episode.accepted {
            rework_prevented = rework_prevented.saturating_add(1);
        }
        *waste_breakdown
            .entry(episode.waste_type.as_str().to_owned())
            .or_insert(0) += 1;
        highest_intervention = Some(rank_intervention(
            highest_intervention,
            episode.proposed_action,
        ));
    }

    EfficiencyAggregate {
        episode_count,
        gross_estimated_tokens_avoided: gross,
        confidence_adjusted_tokens_avoided: adjusted,
        // Estimates never flow into realized fields — only evidence-backed
        // realized_tokens_for_aggregate values are summed above.
        realized_tokens_saved: realized,
        estimated_cost_avoided: tokens_to_cost(gross),
        realized_cost_avoided: tokens_to_cost(realized),
        estimated_agent_hours_avoided: tokens_to_hours(gross),
        realized_agent_hours_avoided: tokens_to_hours(realized),
        rework_prevented,
        waste_breakdown,
        highest_intervention,
        aggregation_version: AGGREGATION_VERSION,
        config_hash: config_hash.to_owned(),
    }
}

/// Deduplicate by `episode_id` with deterministic preference for the richest
/// follow-up state, independent of ingestion order.
fn dedupe_episodes(episodes: &[EfficiencyEpisode]) -> Vec<EfficiencyEpisode> {
    let mut by_id: BTreeMap<&str, &EfficiencyEpisode> = BTreeMap::new();
    for episode in episodes {
        match by_id.get(episode.episode_id.as_str()) {
            Some(existing) if episode_richness(existing) >= episode_richness(episode) => {}
            _ => {
                by_id.insert(episode.episode_id.as_str(), episode);
            }
        }
    }
    by_id.into_values().cloned().collect()
}

fn episode_richness(episode: &EfficiencyEpisode) -> u32 {
    let mut score = 0u32;
    if episode.accepted {
        score += 4;
    }
    if episode.human_override {
        score += 2;
    }
    if episode.actual_followup_result.is_some() {
        score += 2;
    }
    if episode.resolved_at.is_some() {
        score += 1;
    }
    if realized_tokens_for_aggregate(episode).is_some() {
        score += 8;
    }
    score += episode.evidence_refs.len() as u32;
    score
}

/// Realized tokens count only with an explicit realization basis and
/// baseline/comparison evidence. Estimates alone never qualify.
fn realized_tokens_for_aggregate(episode: &EfficiencyEpisode) -> Option<u64> {
    let saved = episode.realized_tokens_saved?;
    let basis = episode.realization_basis.as_deref()?;
    if has_comparison_evidence(basis, &episode.evidence_refs) {
        Some(saved)
    } else {
        None
    }
}

fn tokens_to_cost(tokens: u64) -> f64 {
    (tokens as f64) * TOKENS_TO_USD
}

fn tokens_to_hours(tokens: u64) -> f64 {
    (tokens as f64) / TOKENS_PER_AGENT_HOUR
}

fn rank_intervention(current: Option<RepairAction>, candidate: RepairAction) -> RepairAction {
    let score = |action: RepairAction| match action {
        RepairAction::Cancel => 1,
        RepairAction::Merge => 2,
        RepairAction::ConsolidateVerifiers => 3,
        RepairAction::DelayVerification => 4,
        RepairAction::Reassign => 5,
        RepairAction::StopDownstream => 6,
        RepairAction::SplitDrift => 7,
    };
    match current {
        Some(existing) if score(existing) >= score(candidate) => existing,
        _ => candidate,
    }
}

fn empty_envelope(mode: EfficiencyMode, config_hash: &str) -> EfficiencyData {
    let aggregate = EfficiencyAggregate {
        aggregation_version: AGGREGATION_VERSION,
        config_hash: config_hash.to_owned(),
        ..EfficiencyAggregate::default()
    };
    EfficiencyData {
        schema: EFFICIENCY_SCHEMA.to_owned(),
        mode,
        aggregation_version: AGGREGATION_VERSION,
        config_hash: config_hash.to_owned(),
        episodes: Vec::new(),
        build: aggregate.clone(),
        lifetime: aggregate,
    }
}

fn validate_draft_bounds(draft: &EpisodeDraft) -> Result<(), String> {
    if draft.detected_node.trim().is_empty() {
        return Err("detected_node must be non-empty".to_owned());
    }
    if draft.detected_node.len() > MAX_REFERENCE_BYTES {
        return Err("detected_node exceeds size bound".to_owned());
    }
    if draft.affected_node_ids.len() > MAX_AFFECTED_NODES {
        return Err("affected_node_ids exceeds count bound".to_owned());
    }
    if draft.estimation_basis.len() > MAX_BASIS_BYTES {
        return Err("estimation_basis exceeds size bound".to_owned());
    }
    if draft.evidence_refs.len() > MAX_EVIDENCE_REFS {
        return Err("evidence_refs exceeds count bound".to_owned());
    }
    if draft.actor.trim().is_empty() || draft.actor.len() > MAX_ACTOR_BYTES {
        return Err("actor must be a compact non-empty identifier".to_owned());
    }
    if draft.config_hash.trim().is_empty() || draft.config_hash.len() > MAX_REFERENCE_BYTES {
        return Err("config_hash must be a compact non-empty identifier".to_owned());
    }
    if draft.config_hash.chars().any(char::is_whitespace) {
        return Err("config_hash must not contain whitespace".to_owned());
    }
    reject_credential_text(&draft.estimation_basis, "estimation_basis")?;
    reject_credential_text(&draft.actor, "actor")?;
    reject_credential_text(&draft.config_hash, "config_hash")?;
    if let Some(basis) = &draft.realization_basis {
        if basis.len() > MAX_BASIS_BYTES {
            return Err("realization_basis exceeds size bound".to_owned());
        }
        reject_credential_text(basis, "realization_basis")?;
    }
    if let Some(follow_up) = &draft.actual_followup_result {
        if follow_up.len() > MAX_BASIS_BYTES {
            return Err("actual_followup_result exceeds size bound".to_owned());
        }
        reject_credential_text(follow_up, "actual_followup_result")?;
    }
    for reference in &draft.evidence_refs {
        if reference.len() > MAX_REFERENCE_BYTES {
            return Err("evidence_refs entry exceeds size bound".to_owned());
        }
        reject_credential_text(reference, "evidence_refs")?;
    }
    Ok(())
}

fn reject_credential_text(value: &str, field: &str) -> Result<(), String> {
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

fn reject_efficiency_secrets(data: &EfficiencyData) -> Result<()> {
    let encoded = serde_json::to_value(data).context("encode efficiency envelope")?;
    reject_secret_shaped_keys(&encoded)?;
    Ok(())
}

fn reject_secret_shaped_keys(value: &serde_json::Value) -> Result<()> {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                let normalized = key.to_ascii_lowercase().replace('-', "_");
                if matches!(
                    normalized.as_str(),
                    "access_token"
                        | "api_key"
                        | "authorization"
                        | "credentials"
                        | "password"
                        | "private_key"
                        | "refresh_token"
                        | "secret"
                        | "secrets"
                        | "token"
                ) {
                    bail!("efficiency envelope contains forbidden credential field `{key}`");
                }
                reject_secret_shaped_keys(child)?;
            }
        }
        serde_json::Value::Array(array) => {
            for child in array {
                reject_secret_shaped_keys(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn core_identity_matches(left: &EfficiencyEpisode, right: &EfficiencyEpisode) -> bool {
    left.waste_type == right.waste_type
        && left.detected_node == right.detected_node
        && left.proposed_action == right.proposed_action
        && left.affected_node_ids == right.affected_node_ids
}

fn episode_state_equivalent(left: &EfficiencyEpisode, right: &EfficiencyEpisode) -> bool {
    left.accepted == right.accepted
        && left.mode == right.mode
        && left.estimated_tokens_avoided == right.estimated_tokens_avoided
        && left.estimation_basis == right.estimation_basis
        && (left.confidence - right.confidence).abs() < f64::EPSILON
        && left.confidence_adjusted_tokens_avoided == right.confidence_adjusted_tokens_avoided
        && left.realized_tokens_saved == right.realized_tokens_saved
        && left.realization_basis == right.realization_basis
        && left.actual_followup_result == right.actual_followup_result
        && left.human_override == right.human_override
        && left.actor == right.actor
        && left.evidence_refs == right.evidence_refs
        && left.resolved_at == right.resolved_at
        && left.config_hash == right.config_hash
        && left.aggregation_version == right.aggregation_version
}

/// A bare detection replay must not clobber richer follow-up / override state.
fn is_detection_only_replay(existing: &EfficiencyEpisode, incoming: &EfficiencyEpisode) -> bool {
    existing.estimated_tokens_avoided == incoming.estimated_tokens_avoided
        && existing.estimation_basis == incoming.estimation_basis
        && (existing.confidence - incoming.confidence).abs() < f64::EPSILON
        && existing.confidence_adjusted_tokens_avoided
            == incoming.confidence_adjusted_tokens_avoided
        && incoming.actual_followup_result.is_none()
        && incoming.realized_tokens_saved.is_none()
        && incoming.realization_basis.is_none()
        && incoming.resolved_at.is_none()
        && !incoming.accepted
        && !incoming.human_override
        && incoming.evidence_refs.iter().all(|reference| {
            existing
                .evidence_refs
                .iter()
                .any(|value| value == reference)
        })
}

fn merge_episode_update(
    existing: &mut EfficiencyEpisode,
    incoming: &EfficiencyEpisode,
) -> Result<(), String> {
    existing.accepted = incoming.accepted;
    existing.mode = incoming.mode;
    existing.human_override = incoming.human_override;
    existing.actor = incoming.actor.clone();
    existing.config_hash = incoming.config_hash.clone();
    existing.aggregation_version = AGGREGATION_VERSION;
    if incoming.actual_followup_result.is_some() {
        existing.actual_followup_result = incoming.actual_followup_result.clone();
    }
    if incoming.resolved_at.is_some() {
        existing.resolved_at = incoming.resolved_at.clone();
    }
    if incoming.realized_tokens_saved.is_some() {
        existing.realized_tokens_saved = incoming.realized_tokens_saved;
        existing.realization_basis = incoming.realization_basis.clone();
    }
    existing.evidence_refs = merge_evidence(&existing.evidence_refs, &incoming.evidence_refs)?;
    // Estimates are immutable after first insert; keep the original detection numbers.
    Ok(())
}

fn merge_evidence(existing: &[String], incoming: &[String]) -> Result<Vec<String>, String> {
    let mut merged = existing.to_vec();
    for reference in incoming {
        let trimmed = reference.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > MAX_REFERENCE_BYTES {
            return Err("evidence_refs entry exceeds size bound".to_owned());
        }
        reject_credential_text(trimmed, "evidence_refs")?;
        if !merged.iter().any(|value| value == trimmed) {
            merged.push(trimmed.to_owned());
        }
    }
    if merged.len() > MAX_EVIDENCE_REFS {
        return Err("evidence_refs exceeds count bound".to_owned());
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::efficiency::{validate_node_metadata, NodeEfficiencyMetadata};
    use serde_json::{json, Value};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "fractal-efficiency-accounting-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn seed_project(workspace: &Path) -> Result<String> {
        fs::create_dir_all(workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [
                {"id": "node_a", "capability": "code.generate", "instruction": "Build A."},
                {"id": "node_b", "capability": "code.generate", "instruction": "Build B."}
            ],
            "edges": []
        });
        graph["graph_hash"] = Value::String(
            fractal_contracts::canonical_sha256(&graph)
                .map_err(|error| anyhow::anyhow!("hash fixture: {error}"))?,
        );
        project_file::persist(workspace, &graph, "Efficiency Accounting")?;
        Ok(graph["graph_hash"].as_str().unwrap().to_owned())
    }

    fn sample_draft() -> EpisodeDraft {
        EpisodeDraft {
            waste_type: WasteType::DuplicateTask,
            detected_node: "node_a".to_owned(),
            affected_node_ids: vec!["node_a".to_owned(), "node_b".to_owned()],
            proposed_action: RepairAction::Cancel,
            accepted: false,
            mode: EfficiencyMode::Suggest,
            estimated_tokens_avoided: 12_000,
            estimation_basis: "exact title and artifact match against node_b".to_owned(),
            confidence: 0.85,
            realized_tokens_saved: None,
            realization_basis: None,
            actual_followup_result: None,
            human_override: false,
            actor: "fractal-efficiency".to_owned(),
            evidence_refs: vec!["sim:node_a:node_b".to_owned()],
            config_hash: "cfg_suggest_v1".to_owned(),
            detected_at: Some("2026-07-29T13:00:00Z".to_owned()),
            resolved_at: None,
        }
    }

    #[test]
    fn episode_id_is_deterministic_and_order_invariant() {
        let left = derive_episode_id(
            WasteType::DuplicateTask,
            "node_a",
            &["node_b".to_owned(), "node_a".to_owned()],
            RepairAction::Cancel,
        );
        let right = derive_episode_id(
            WasteType::DuplicateTask,
            "node_a",
            &["node_a".to_owned(), "node_b".to_owned()],
            RepairAction::Cancel,
        );
        assert_eq!(left, right);
        assert!(left.starts_with("ep_"));
        assert_eq!(left.len(), 3 + 24);
        let other = derive_episode_id(
            WasteType::DuplicateTest,
            "node_a",
            &["node_a".to_owned(), "node_b".to_owned()],
            RepairAction::Cancel,
        );
        assert_ne!(left, other);
    }

    #[test]
    fn persistence_appends_episode_beside_learning() -> Result<()> {
        let workspace = temp_workspace();
        let graph_hash = seed_project(&workspace)?;
        let learning_before = project_file::load(&workspace)?.learning.clone();

        assert_eq!(
            record_episode(&workspace, sample_draft())?,
            UpsertOutcome::Inserted
        );
        let document = project_file::load(&workspace)?;
        assert_eq!(document.graph_hash, graph_hash);
        assert_eq!(document.learning, learning_before);
        let efficiency = document.efficiency.expect("efficiency envelope");
        assert_eq!(efficiency.schema, EFFICIENCY_SCHEMA);
        assert_eq!(efficiency.episodes.len(), 1);
        assert_eq!(efficiency.episodes[0].actor, "fractal-efficiency");
        assert_eq!(
            efficiency.episodes[0].aggregation_version,
            AGGREGATION_VERSION
        );
        assert_eq!(efficiency.episodes[0].config_hash, "cfg_suggest_v1");
        assert_eq!(efficiency.build.episode_count, 1);
        assert_eq!(efficiency.build.gross_estimated_tokens_avoided, 12_000);
        assert_eq!(efficiency.build.realized_tokens_saved, 0);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn retry_is_idempotent_and_followup_updates_in_place() -> Result<()> {
        let workspace = temp_workspace();
        seed_project(&workspace)?;
        let draft = sample_draft();
        assert_eq!(
            record_episode(&workspace, draft.clone())?,
            UpsertOutcome::Inserted
        );
        assert_eq!(
            record_episode(&workspace, draft.clone())?,
            UpsertOutcome::IdempotentReplay
        );
        let episode_id = derive_episode_id(
            draft.waste_type,
            &draft.detected_node,
            &draft.affected_node_ids,
            draft.proposed_action,
        );
        assert_eq!(
            update_episode(
                &workspace,
                &episode_id,
                Some(true),
                Some(true),
                Some("cancelled duplicate after approval".to_owned()),
                Some(vec![
                    "decision:approved".to_owned(),
                    "baseline:run_42".to_owned(),
                ]),
                Some(Some(9_000)),
                Some(Some("baseline comparison against prior run".to_owned())),
                Some(Some("2026-07-29T14:00:00Z".to_owned())),
                Some("operator".to_owned()),
            )?,
            UpsertOutcome::Updated
        );
        let efficiency = load_efficiency(&workspace)?.expect("efficiency");
        assert_eq!(efficiency.episodes.len(), 1);
        assert!(efficiency.episodes[0].accepted);
        assert!(efficiency.episodes[0].human_override);
        assert_eq!(
            efficiency.episodes[0].actual_followup_result.as_deref(),
            Some("cancelled duplicate after approval")
        );
        assert_eq!(efficiency.episodes[0].realized_tokens_saved, Some(9_000));
        assert_eq!(efficiency.build.episode_count, 1);
        assert_eq!(efficiency.build.realized_tokens_saved, 9_000);
        assert_eq!(
            record_episode(&workspace, draft)?,
            UpsertOutcome::IdempotentReplay
        );
        assert_eq!(
            load_efficiency(&workspace)?.unwrap().episodes.len(),
            1,
            "retry after follow-up must not duplicate"
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn malformed_confidence_and_oversized_fields_are_rejected() -> Result<()> {
        let workspace = temp_workspace();
        seed_project(&workspace)?;
        let mut draft = sample_draft();
        draft.confidence = 1.5;
        let error = record_episode(&workspace, draft.clone()).expect_err("confidence");
        assert!(error.to_string().contains("confidence"));

        draft.confidence = 0.85;
        draft.estimation_basis = "x".repeat(MAX_BASIS_BYTES + 1);
        let error = record_episode(&workspace, draft.clone()).expect_err("basis size");
        assert!(error.to_string().contains("size bound"));

        draft.estimation_basis = "ok".to_owned();
        draft.evidence_refs = (0..=MAX_EVIDENCE_REFS)
            .map(|index| format!("ref:{index}"))
            .collect();
        let error = record_episode(&workspace, draft).expect_err("evidence count");
        assert!(error.to_string().contains("count bound"));
        assert!(load_efficiency(&workspace)?.is_none());
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn legacy_documents_without_efficiency_remain_loadable() -> Result<()> {
        let workspace = temp_workspace();
        let graph_hash = seed_project(&workspace)?;
        let path = project_file::path(&workspace);
        let mut raw: Value = serde_json::from_slice(&fs::read(&path)?)?;
        raw.as_object_mut().unwrap().remove("efficiency");
        fs::write(&path, serde_json::to_vec_pretty(&raw)?)?;

        let document = project_file::load(&workspace)?;
        assert!(document.efficiency.is_none());
        assert_eq!(document.graph_hash, graph_hash);
        assert_eq!(document.learning.schema, "fractal.learning.v1");
        assert!(document.learning.nodes.contains_key("node_a"));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn secret_shaped_values_and_fields_are_rejected() -> Result<()> {
        let workspace = temp_workspace();
        seed_project(&workspace)?;
        let mut draft = sample_draft();
        draft.estimation_basis = "matched using api_key=abcd".to_owned();
        let error = record_episode(&workspace, draft.clone()).expect_err("secret basis");
        assert!(error.to_string().contains("credential-shaped"));

        draft.estimation_basis = "safe structural match".to_owned();
        draft.evidence_refs = vec!["password:hunter2".to_owned()];
        let error = record_episode(&workspace, draft).expect_err("secret evidence");
        assert!(error.to_string().contains("credential-shaped"));

        let envelope = empty_envelope(EfficiencyMode::Suggest, "cfg_suggest_v1");
        let mut smuggled = serde_json::to_value(&envelope)?;
        smuggled
            .as_object_mut()
            .unwrap()
            .insert("api_key".to_owned(), json!("must-not-persist"));
        let error = reject_secret_shaped_keys(&smuggled).expect_err("secret field");
        assert!(error.to_string().contains("forbidden credential field"));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn recording_efficiency_preserves_graph_hash_and_unrelated_state() -> Result<()> {
        let workspace = temp_workspace();
        let graph_hash = seed_project(&workspace)?;
        project_file::transition(&workspace, "node_a", "checkout", "cursor", "Cursor")?;
        let before = project_file::load(&workspace)?;
        let learning_bytes = serde_json::to_vec(&before.learning)?;
        let execution_before = before.execution.clone();

        record_episode(&workspace, sample_draft())?;
        let after = project_file::load(&workspace)?;
        assert_eq!(after.graph_hash, graph_hash);
        assert_eq!(after.graph, before.graph);
        assert_eq!(serde_json::to_vec(&after.learning)?, learning_bytes);
        assert_eq!(
            serde_json::to_value(&after.execution)?,
            serde_json::to_value(&execution_before)?
        );
        assert_eq!(after.project.visibility, before.project.visibility);
        assert!(after.efficiency.is_some());
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn persist_graph_preserves_existing_efficiency_envelope() -> Result<()> {
        let workspace = temp_workspace();
        let graph_hash = seed_project(&workspace)?;
        record_episode(&workspace, sample_draft())?;
        let efficiency_before = load_efficiency(&workspace)?.expect("efficiency");

        let document = project_file::load(&workspace)?;
        project_file::persist(&workspace, &document.graph, "Efficiency Accounting")?;
        let after = project_file::load(&workspace)?;
        assert_eq!(after.graph_hash, graph_hash);
        assert_eq!(after.efficiency.as_ref(), Some(&efficiency_before));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn node_metadata_bounds_still_enforced_for_callers() {
        let meta = NodeEfficiencyMetadata {
            estimated_remaining_tokens: 1,
            dependencies: vec![],
            expected_artifact: "a".to_owned(),
            files_or_systems_affected: vec![],
            verification_plan: "test".to_owned(),
            current_assumptions: vec!["password=no".to_owned()],
            similarity_to_other_active_nodes: BTreeMap::new(),
            confidence_still_useful: 1.0,
        };
        assert!(validate_node_metadata(&meta).is_err());
    }

    #[test]
    fn malformed_on_disk_efficiency_fails_load() -> Result<()> {
        let workspace = temp_workspace();
        seed_project(&workspace)?;
        let path = project_file::path(&workspace);
        let mut raw: Value = serde_json::from_slice(&fs::read(&path)?)?;
        raw["efficiency"] = json!({
            "schema": "fractal.efficiency.v1",
            "mode": "suggest",
            "aggregation_version": 1,
            "config_hash": "cfg",
            "episodes": [{
                "episode_id": "ep_bad",
                "waste_type": "duplicate_task",
                "detected_node": "node_a",
                "affected_node_ids": ["node_a"],
                "affected_count": 1,
                "proposed_action": "cancel",
                "accepted": false,
                "mode": "suggest",
                "estimated_tokens_avoided": 10,
                "estimation_basis": "x",
                "confidence": 2.0,
                "confidence_adjusted_tokens_avoided": 10,
                "human_override": false,
                "actor": "tester",
                "detected_at": "2026-07-29T13:00:00Z",
                "evidence_refs": [],
                "aggregation_version": 1,
                "config_hash": "cfg"
            }],
            "build": {
                "episode_count": 0,
                "gross_estimated_tokens_avoided": 0,
                "confidence_adjusted_tokens_avoided": 0,
                "realized_tokens_saved": 0,
                "estimated_cost_avoided": 0.0,
                "realized_cost_avoided": 0.0,
                "estimated_agent_hours_avoided": 0.0,
                "realized_agent_hours_avoided": 0.0,
                "rework_prevented": 0,
                "waste_breakdown": {},
                "aggregation_version": 1,
                "config_hash": "cfg"
            },
            "lifetime": {
                "episode_count": 0,
                "gross_estimated_tokens_avoided": 0,
                "confidence_adjusted_tokens_avoided": 0,
                "realized_tokens_saved": 0,
                "estimated_cost_avoided": 0.0,
                "realized_cost_avoided": 0.0,
                "estimated_agent_hours_avoided": 0.0,
                "realized_agent_hours_avoided": 0.0,
                "rework_prevented": 0,
                "waste_breakdown": {},
                "aggregation_version": 1,
                "config_hash": "cfg"
            }
        });
        fs::write(&path, serde_json::to_vec_pretty(&raw)?)?;
        let error = project_file::load(&workspace).expect_err("malformed");
        assert!(error.to_string().contains("fractal.efficiency.v1"));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn estimate_only_episode(
        episode_id: &str,
        waste: WasteType,
        node: &str,
        action: RepairAction,
        estimated: u64,
        confidence: f64,
        config_hash: &str,
        accepted: bool,
    ) -> EfficiencyEpisode {
        let adjusted = ((estimated as f64) * confidence).floor() as u64;
        EfficiencyEpisode {
            episode_id: episode_id.to_owned(),
            waste_type: waste,
            detected_node: node.to_owned(),
            affected_node_ids: vec![node.to_owned()],
            affected_count: 1,
            proposed_action: action,
            accepted,
            mode: EfficiencyMode::Suggest,
            estimated_tokens_avoided: estimated,
            estimation_basis: "structural detector match".to_owned(),
            confidence,
            confidence_adjusted_tokens_avoided: adjusted,
            realized_tokens_saved: None,
            realization_basis: None,
            actual_followup_result: None,
            human_override: false,
            actor: "fractal-efficiency".to_owned(),
            detected_at: "2026-07-29T13:00:00Z".to_owned(),
            resolved_at: None,
            evidence_refs: vec![format!("sim:{node}")],
            aggregation_version: AGGREGATION_VERSION,
            config_hash: config_hash.to_owned(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn realized_episode(
        episode_id: &str,
        waste: WasteType,
        node: &str,
        action: RepairAction,
        estimated: u64,
        confidence: f64,
        realized: u64,
        config_hash: &str,
    ) -> EfficiencyEpisode {
        let mut episode = estimate_only_episode(
            episode_id,
            waste,
            node,
            action,
            estimated,
            confidence,
            config_hash,
            true,
        );
        episode.realized_tokens_saved = Some(realized);
        episode.realization_basis = Some("baseline comparison against prior run".to_owned());
        episode.evidence_refs = vec![format!("sim:{node}"), "baseline:run_42".to_owned()];
        episode.actual_followup_result = Some("repair applied and measured".to_owned());
        episode.resolved_at = Some("2026-07-29T14:00:00Z".to_owned());
        episode
    }

    #[test]
    fn golden_estimate_only_fold_never_populates_realized_fields() {
        let episodes = vec![
            estimate_only_episode(
                "ep_a",
                WasteType::DuplicateTask,
                "node_a",
                RepairAction::Cancel,
                12_000,
                0.85,
                "cfg_build_a",
                false,
            ),
            estimate_only_episode(
                "ep_b",
                WasteType::DuplicateTest,
                "node_b",
                RepairAction::ConsolidateVerifiers,
                8_000,
                0.75,
                "cfg_build_a",
                true,
            ),
        ];
        // Out-of-order duplicate of ep_a must not change totals.
        let mut shuffled = episodes.clone();
        shuffled.push(episodes[0].clone());
        shuffled.reverse();

        let build = fold_build_aggregate(&shuffled, "cfg_build_a");
        let lifetime = fold_lifetime_aggregate(&shuffled, "cfg_build_a");

        let mut expected_breakdown = BTreeMap::new();
        expected_breakdown.insert("duplicate_task".to_owned(), 1);
        expected_breakdown.insert("duplicate_test".to_owned(), 1);
        let expected = EfficiencyAggregate {
            episode_count: 2,
            gross_estimated_tokens_avoided: 20_000,
            confidence_adjusted_tokens_avoided: 16_200, // floor(12000*0.85)+floor(8000*0.75)
            realized_tokens_saved: 0,
            estimated_cost_avoided: 0.6,
            realized_cost_avoided: 0.0,
            estimated_agent_hours_avoided: 20_000.0 / 30_000.0,
            realized_agent_hours_avoided: 0.0,
            rework_prevented: 1,
            waste_breakdown: expected_breakdown,
            highest_intervention: Some(RepairAction::ConsolidateVerifiers),
            aggregation_version: AGGREGATION_VERSION,
            config_hash: "cfg_build_a".to_owned(),
        };
        assert_eq!(build, expected);
        assert_eq!(lifetime, expected);
        assert_eq!(build.realized_tokens_saved, 0);
        assert_eq!(build.realized_cost_avoided, 0.0);
        assert_eq!(build.realized_agent_hours_avoided, 0.0);
        assert_ne!(
            build.gross_estimated_tokens_avoided,
            build.realized_tokens_saved
        );
        assert_ne!(
            build.confidence_adjusted_tokens_avoided,
            build.realized_tokens_saved
        );
    }

    #[test]
    fn golden_realized_requires_baseline_evidence_and_exact_totals() {
        let estimate_only = estimate_only_episode(
            "ep_est",
            WasteType::SpecDrift,
            "node_drift",
            RepairAction::SplitDrift,
            30_000,
            0.9,
            "cfg_build_a",
            false,
        );
        // Spoofed realized without comparison evidence must not enter aggregates.
        let mut spoofed = estimate_only.clone();
        spoofed.episode_id = "ep_spoof".to_owned();
        spoofed.detected_node = "node_spoof".to_owned();
        spoofed.affected_node_ids = vec!["node_spoof".to_owned()];
        spoofed.estimated_tokens_avoided = 12_000;
        spoofed.confidence = 0.85;
        spoofed.confidence_adjusted_tokens_avoided = 10_200;
        spoofed.realized_tokens_saved = Some(99_999);
        spoofed.realization_basis = None;

        let evidenced = realized_episode(
            "ep_real",
            WasteType::DuplicateTask,
            "node_real",
            RepairAction::Cancel,
            12_000,
            0.85,
            9_000,
            "cfg_build_a",
        );

        let episodes = vec![evidenced, estimate_only, spoofed];
        let build = fold_build_aggregate(&episodes, "cfg_build_a");

        assert_eq!(build.episode_count, 3);
        assert_eq!(build.gross_estimated_tokens_avoided, 54_000);
        assert_eq!(
            build.confidence_adjusted_tokens_avoided,
            10_200 + 27_000 + 10_200
        );
        // Only the evidenced 9_000 counts; spoofed 99_999 and estimates are ignored.
        assert_eq!(build.realized_tokens_saved, 9_000);
        assert_eq!(build.realized_cost_avoided, 0.27);
        assert_eq!(build.realized_agent_hours_avoided, 0.3);
        assert_eq!(build.estimated_cost_avoided, 1.62);
        assert_eq!(build.estimated_agent_hours_avoided, 1.8);
        assert_eq!(build.rework_prevented, 1);
        assert_eq!(build.highest_intervention, Some(RepairAction::SplitDrift));
        assert_eq!(build.waste_breakdown.get("duplicate_task"), Some(&1));
        assert_eq!(build.waste_breakdown.get("spec_drift"), Some(&2));
    }

    #[test]
    fn golden_multiple_builds_and_config_versions_split_build_from_lifetime() {
        let build_a = estimate_only_episode(
            "ep_build_a",
            WasteType::OverlappingFiles,
            "node_a",
            RepairAction::Merge,
            10_000,
            1.0,
            "cfg_v1",
            true,
        );
        let build_b = realized_episode(
            "ep_build_b",
            WasteType::ExcessiveRetries,
            "node_b",
            RepairAction::Reassign,
            20_000,
            0.5,
            7_500,
            "cfg_v2",
        );
        // Same episode_id arriving again with richer follow-up (idempotent preference).
        let mut build_b_retry = build_b.clone();
        build_b_retry
            .evidence_refs
            .push("comparison:wave_3".to_owned());

        let mut older_algo = build_a.clone();
        older_algo.episode_id = "ep_legacy_algo".to_owned();
        older_algo.detected_node = "node_legacy".to_owned();
        older_algo.affected_node_ids = vec!["node_legacy".to_owned()];
        older_algo.aggregation_version = 1;
        older_algo.config_hash = "cfg_v1".to_owned();
        older_algo.estimated_tokens_avoided = 5_000;
        older_algo.confidence_adjusted_tokens_avoided = 5_000;

        // Out-of-order: current build first, then retries, then prior build.
        let episodes = vec![build_b.clone(), build_b_retry, build_a, older_algo, build_b];

        let build = fold_build_aggregate(&episodes, "cfg_v2");
        let lifetime = fold_lifetime_aggregate(&episodes, "cfg_v2");

        assert_eq!(build.episode_count, 1);
        assert_eq!(build.gross_estimated_tokens_avoided, 20_000);
        assert_eq!(build.confidence_adjusted_tokens_avoided, 10_000);
        assert_eq!(build.realized_tokens_saved, 7_500);
        assert_eq!(build.realized_cost_avoided, 0.225);
        assert_eq!(build.realized_agent_hours_avoided, 0.25);
        assert_eq!(build.estimated_cost_avoided, 0.6);
        assert_eq!(build.rework_prevented, 1);
        assert_eq!(build.highest_intervention, Some(RepairAction::Reassign));
        assert_eq!(build.config_hash, "cfg_v2");
        assert_eq!(build.aggregation_version, AGGREGATION_VERSION);

        assert_eq!(lifetime.episode_count, 3);
        assert_eq!(lifetime.gross_estimated_tokens_avoided, 35_000);
        assert_eq!(lifetime.confidence_adjusted_tokens_avoided, 25_000);
        assert_eq!(lifetime.realized_tokens_saved, 7_500);
        assert_eq!(lifetime.realized_cost_avoided, 0.225);
        assert_eq!(lifetime.estimated_cost_avoided, 1.05);
        assert_eq!(lifetime.estimated_agent_hours_avoided, 35_000.0 / 30_000.0);
        assert_eq!(lifetime.rework_prevented, 3);
        assert_eq!(lifetime.highest_intervention, Some(RepairAction::Reassign));
        assert_eq!(lifetime.waste_breakdown.get("overlapping_files"), Some(&2));
        assert_eq!(lifetime.waste_breakdown.get("excessive_retries"), Some(&1));
        // Lifetime still stamps the active envelope config / aggregation version.
        assert_eq!(lifetime.config_hash, "cfg_v2");
        assert_eq!(lifetime.aggregation_version, AGGREGATION_VERSION);
    }

    #[test]
    fn golden_followup_update_is_idempotent_and_keeps_estimates_separate() -> Result<()> {
        let workspace = temp_workspace();
        seed_project(&workspace)?;
        let draft = sample_draft();
        assert_eq!(
            record_episode(&workspace, draft.clone())?,
            UpsertOutcome::Inserted
        );
        let before = load_efficiency(&workspace)?.expect("efficiency");
        assert_eq!(before.build.gross_estimated_tokens_avoided, 12_000);
        assert_eq!(before.build.confidence_adjusted_tokens_avoided, 10_200);
        assert_eq!(before.build.realized_tokens_saved, 0);
        assert_eq!(before.build.estimated_cost_avoided, 0.36);
        assert_eq!(before.build.realized_cost_avoided, 0.0);
        assert_eq!(before.lifetime.realized_tokens_saved, 0);

        let episode_id = before.episodes[0].episode_id.clone();
        assert_eq!(
            update_episode(
                &workspace,
                &episode_id,
                Some(true),
                None,
                Some("cancelled duplicate after approval".to_owned()),
                Some(vec!["baseline:run_42".to_owned()]),
                Some(Some(9_000)),
                Some(Some("baseline comparison against prior run".to_owned())),
                Some(Some("2026-07-29T14:00:00Z".to_owned())),
                None,
            )?,
            UpsertOutcome::Updated
        );
        let after = load_efficiency(&workspace)?.expect("efficiency");
        assert_eq!(after.build.gross_estimated_tokens_avoided, 12_000);
        assert_eq!(after.build.confidence_adjusted_tokens_avoided, 10_200);
        assert_eq!(after.build.realized_tokens_saved, 9_000);
        assert_eq!(after.build.estimated_cost_avoided, 0.36);
        assert_eq!(after.build.realized_cost_avoided, 0.27);
        assert_eq!(after.build.estimated_agent_hours_avoided, 0.4);
        assert_eq!(after.build.realized_agent_hours_avoided, 0.3);
        assert_eq!(after.build.rework_prevented, 1);
        assert_eq!(after.lifetime.realized_tokens_saved, 9_000);

        // Identical follow-up is an idempotent replay; aggregates stay golden.
        assert_eq!(
            update_episode(
                &workspace,
                &episode_id,
                Some(true),
                None,
                Some("cancelled duplicate after approval".to_owned()),
                Some(vec!["baseline:run_42".to_owned()]),
                Some(Some(9_000)),
                Some(Some("baseline comparison against prior run".to_owned())),
                Some(Some("2026-07-29T14:00:00Z".to_owned())),
                None,
            )?,
            UpsertOutcome::IdempotentReplay
        );
        let again = load_efficiency(&workspace)?.expect("efficiency");
        assert_eq!(again.build, after.build);
        assert_eq!(again.lifetime, after.lifetime);
        assert_eq!(again.episodes.len(), 1);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }
}
