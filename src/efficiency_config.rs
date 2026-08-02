//! Resolved, validated efficiency governance configuration.
//!
//! Built once from CLI input in `main` and passed by reference into the run
//! paths — there is no global mutable state and no environment side channel.
//! This module only resolves, validates, hashes, and reports; scheduler
//! mutation is deliberately not wired yet.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::cli::{EfficiencyArgs, EfficiencyModeArg, EfficiencyOpts, InterventionArg};
use crate::efficiency::{EfficiencyAggregate, EfficiencyData, EfficiencyMode, RepairAction};

/// Version tag folded into the canonical hash so future layouts rehash.
const CONFIG_HASH_SCHEMA: &str = "fractal.efficiency.config.v1";

/// Repair actions considered high impact: they stop, discard, or move work.
/// Autonomy for these is never implied by `auto_optimize`; each must be
/// granted by name with `--allow-high-impact`.
pub(crate) const HIGH_IMPACT_ACTIONS: [RepairAction; 3] = [
    RepairAction::Cancel,
    RepairAction::StopDownstream,
    RepairAction::Reassign,
];

/// Immutable efficiency governance for one invocation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EfficiencyConfig {
    pub(crate) mode: EfficiencyMode,
    /// Interventions the human explicitly approved for this invocation.
    pub(crate) approved: Vec<RepairAction>,
    /// Interventions the human explicitly overrode (declined).
    pub(crate) overridden: Vec<RepairAction>,
    /// High-impact actions granted autonomy by name (auto-optimize only).
    pub(crate) high_impact_autonomy: Vec<RepairAction>,
}

impl Default for EfficiencyConfig {
    fn default() -> Self {
        Self {
            mode: EfficiencyMode::Suggest,
            approved: Vec::new(),
            overridden: Vec::new(),
            high_impact_autonomy: Vec::new(),
        }
    }
}

impl EfficiencyConfig {
    /// Canonical sha256 over a stable, sorted text form of the configuration.
    pub(crate) fn config_hash(&self) -> String {
        let digest = Sha256::digest(self.canonical_form().as_bytes());
        let mut hex = String::from("sha256:");
        for byte in digest {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    }

    fn canonical_form(&self) -> String {
        format!(
            "{CONFIG_HASH_SCHEMA}\nmode={}\napproved={}\noverridden={}\nhigh_impact_autonomy={}\n",
            self.mode.as_str(),
            join(&self.approved),
            join(&self.overridden),
            join(&self.high_impact_autonomy),
        )
    }

    /// Whether this configuration lets `action` be applied without a human in
    /// the loop. Overrides always win; high-impact actions need a named grant.
    pub(crate) fn autonomy_granted(&self, action: RepairAction) -> bool {
        if self.overridden.contains(&action) {
            return false;
        }
        match self.mode {
            EfficiencyMode::Observe | EfficiencyMode::Suggest => false,
            EfficiencyMode::AutoOptimize => {
                !is_high_impact(action) || self.high_impact_autonomy.contains(&action)
            }
        }
    }
}

pub(crate) fn is_high_impact(action: RepairAction) -> bool {
    HIGH_IMPACT_ACTIONS.contains(&action)
}

/// Resolve CLI controls into a validated configuration, rejecting unsafe or
/// contradictory combinations.
pub(crate) fn resolve(opts: &EfficiencyOpts) -> Result<EfficiencyConfig> {
    let mode = mode_of(opts.efficiency_mode);
    let approved = normalize(&opts.approve_intervention);
    let overridden = normalize(&opts.override_intervention);
    let high_impact_autonomy = normalize(&opts.allow_high_impact);

    if mode == EfficiencyMode::Observe
        && (!approved.is_empty() || !overridden.is_empty() || !high_impact_autonomy.is_empty())
    {
        bail!(
            "contradictory efficiency configuration: observe mode records signals only, \
             so intervention approvals, overrides, and autonomy grants are not allowed"
        );
    }
    if let Some(action) = approved.iter().find(|action| overridden.contains(action)) {
        bail!(
            "contradictory efficiency configuration: `{}` is both approved and overridden",
            action.as_str()
        );
    }
    if !high_impact_autonomy.is_empty() && mode != EfficiencyMode::AutoOptimize {
        bail!(
            "unsafe efficiency configuration: --allow-high-impact requires \
             --efficiency-mode auto-optimize; {} mode never applies interventions autonomously",
            mode.as_str()
        );
    }
    if let Some(action) = high_impact_autonomy
        .iter()
        .find(|action| !is_high_impact(**action))
    {
        bail!(
            "`{}` is not a named high-impact action; autonomy grants apply only to: {}",
            action.as_str(),
            join(&HIGH_IMPACT_ACTIONS)
        );
    }
    if let Some(action) = high_impact_autonomy
        .iter()
        .find(|action| overridden.contains(action))
    {
        bail!(
            "contradictory efficiency configuration: `{}` is granted autonomy but also overridden",
            action.as_str()
        );
    }

    Ok(EfficiencyConfig {
        mode,
        approved,
        overridden,
        high_impact_autonomy,
    })
}

/// Handle `fractal efficiency`: resolve the configuration and summarize any
/// recorded efficiency data for the workspace.
pub(crate) fn run(args: &EfficiencyArgs) -> Result<()> {
    let config = resolve(&args.controls)?;
    let workspace = match &args.repo {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    crate::efficiency_accounting::ensure_envelope(&workspace, config.mode, &config.config_hash())?;
    let data = load_project_efficiency(&workspace)?.unwrap_or_default();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status_json(&config, &data))?
        );
    } else {
        print!("{}", render_status(&config, &data));
    }
    Ok(())
}

/// One-line governance banner for run entry points.
pub(crate) fn banner(config: &EfficiencyConfig) -> String {
    format!(
        "Efficiency: {} mode · config {}",
        config.mode.as_str(),
        config.config_hash()
    )
}

/// Human status report. Estimated and Realized figures are always labeled
/// distinctly and never combined.
pub(crate) fn render_status(config: &EfficiencyConfig, data: &EfficiencyData) -> String {
    let mode_note = match config.mode {
        EfficiencyMode::Observe => {
            "records efficiency signals only; never proposes or applies interventions"
        }
        EfficiencyMode::Suggest => "proposes interventions; each waits for explicit approval",
        EfficiencyMode::AutoOptimize => {
            "applies low-impact interventions autonomously; high-impact actions need named grants"
        }
    };
    let autonomous: Vec<RepairAction> = RepairAction::ALL
        .iter()
        .copied()
        .filter(|action| config.autonomy_granted(*action))
        .collect();
    let mut out = String::new();
    out.push_str("Efficiency governance\n");
    out.push_str(&format!("  Mode: {} — {mode_note}\n", config.mode.as_str()));
    out.push_str(&format!("  Config hash: {}\n", config.config_hash()));
    out.push_str(&format!(
        "  Approved interventions: {}\n",
        list_or_none(&config.approved)
    ));
    out.push_str(&format!(
        "  Overridden interventions: {}\n",
        list_or_none(&config.overridden)
    ));
    out.push_str(&format!(
        "  High-impact autonomy: {} (high-impact actions: {})\n",
        list_or_none(&config.high_impact_autonomy),
        join(&HIGH_IMPACT_ACTIONS)
    ));
    out.push_str(&format!(
        "  Applied without approval: {}\n",
        list_or_none(&autonomous)
    ));
    if !data.config_hash.is_empty() && data.config_hash != config.config_hash() {
        out.push_str(&format!(
            "  Note: recorded data below was produced under config {}\n",
            data.config_hash
        ));
    }
    out.push('\n');
    out.push_str(&render_aggregate("This build", &data.build));
    out.push('\n');
    out.push_str(&render_aggregate("Lifetime", &data.lifetime));
    out
}

fn render_aggregate(title: &str, aggregate: &EfficiencyAggregate) -> String {
    format!(
        "{title}\n\
         \x20 Estimated tokens avoided: {} (confidence-adjusted {})\n\
         \x20 Realized tokens saved: {}\n\
         \x20 Estimated cost avoided: ${:.2}\n\
         \x20 Realized cost avoided: ${:.2}\n",
        aggregate.gross_estimated_tokens_avoided,
        aggregate.confidence_adjusted_tokens_avoided,
        aggregate.realized_tokens_saved,
        aggregate.estimated_cost_avoided,
        aggregate.realized_cost_avoided,
    )
}

fn status_json(config: &EfficiencyConfig, data: &EfficiencyData) -> serde_json::Value {
    let autonomous: Vec<RepairAction> = RepairAction::ALL
        .iter()
        .copied()
        .filter(|action| config.autonomy_granted(*action))
        .collect();
    serde_json::json!({
        "schema": "fractal.efficiency.status.v1",
        "config": {
            "mode": config.mode,
            "config_hash": config.config_hash(),
            "approved_interventions": config.approved,
            "overridden_interventions": config.overridden,
            "high_impact_autonomy": config.high_impact_autonomy,
            "applied_without_approval": autonomous,
        },
        "build": aggregate_json(&data.build),
        "lifetime": aggregate_json(&data.lifetime),
    })
}

fn aggregate_json(aggregate: &EfficiencyAggregate) -> serde_json::Value {
    serde_json::json!({
        "estimated_tokens_avoided": aggregate.gross_estimated_tokens_avoided,
        "confidence_adjusted_tokens_avoided": aggregate.confidence_adjusted_tokens_avoided,
        "realized_tokens_saved": aggregate.realized_tokens_saved,
        "estimated_cost_avoided": aggregate.estimated_cost_avoided,
        "realized_cost_avoided": aggregate.realized_cost_avoided,
    })
}

/// Load the optional `efficiency` envelope from `.fractal/project.fractal`.
fn load_project_efficiency(workspace: &Path) -> Result<Option<EfficiencyData>> {
    let path = workspace.join(".fractal/project.fractal");
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("could not parse {}", path.display()))?;
    let Some(section) = value.get("efficiency") else {
        return Ok(None);
    };
    let data: EfficiencyData = serde_json::from_value(section.clone())
        .with_context(|| format!("invalid efficiency section in {}", path.display()))?;
    crate::efficiency::validate(&data)
        .map_err(|reason| anyhow!("invalid efficiency data in {}: {reason}", path.display()))?;
    Ok(Some(data))
}

fn mode_of(arg: EfficiencyModeArg) -> EfficiencyMode {
    match arg {
        EfficiencyModeArg::Observe => EfficiencyMode::Observe,
        EfficiencyModeArg::Suggest => EfficiencyMode::Suggest,
        EfficiencyModeArg::AutoOptimize => EfficiencyMode::AutoOptimize,
    }
}

fn action_of(arg: InterventionArg) -> RepairAction {
    match arg {
        InterventionArg::Merge => RepairAction::Merge,
        InterventionArg::Cancel => RepairAction::Cancel,
        InterventionArg::DelayVerification => RepairAction::DelayVerification,
        InterventionArg::StopDownstream => RepairAction::StopDownstream,
        InterventionArg::Reassign => RepairAction::Reassign,
        InterventionArg::ConsolidateVerifiers => RepairAction::ConsolidateVerifiers,
        InterventionArg::SplitDrift => RepairAction::SplitDrift,
    }
}

/// Sort by contract label and drop duplicates so hashing is canonical.
fn normalize(args: &[InterventionArg]) -> Vec<RepairAction> {
    let mut actions: Vec<RepairAction> = args.iter().copied().map(action_of).collect();
    actions.sort_by_key(|action| action.as_str());
    actions.dedup();
    actions
}

fn join(actions: &[RepairAction]) -> String {
    actions
        .iter()
        .map(|action| action.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn list_or_none(actions: &[RepairAction]) -> String {
    if actions.is_empty() {
        "none".to_owned()
    } else {
        actions
            .iter()
            .map(|action| action.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(
        mode: EfficiencyModeArg,
        approve: &[InterventionArg],
        overrides: &[InterventionArg],
        allow: &[InterventionArg],
    ) -> EfficiencyOpts {
        EfficiencyOpts {
            efficiency_mode: mode,
            approve_intervention: approve.to_vec(),
            override_intervention: overrides.to_vec(),
            allow_high_impact: allow.to_vec(),
        }
    }

    #[test]
    fn defaults_to_suggest_with_no_grants() {
        let config = resolve(&opts(EfficiencyModeArg::Suggest, &[], &[], &[])).unwrap();
        assert_eq!(config.mode, EfficiencyMode::Suggest);
        assert!(config.approved.is_empty());
        assert!(config.overridden.is_empty());
        assert!(config.high_impact_autonomy.is_empty());
        for action in RepairAction::ALL {
            assert!(!config.autonomy_granted(action));
        }
    }

    #[test]
    fn config_hash_is_canonical_and_mode_sensitive() {
        let base = resolve(&opts(
            EfficiencyModeArg::Suggest,
            &[InterventionArg::Merge, InterventionArg::Cancel],
            &[],
            &[],
        ))
        .unwrap();
        // Order and duplicates of the same approvals must not change the hash.
        let shuffled = resolve(&opts(
            EfficiencyModeArg::Suggest,
            &[
                InterventionArg::Cancel,
                InterventionArg::Merge,
                InterventionArg::Cancel,
            ],
            &[],
            &[],
        ))
        .unwrap();
        assert_eq!(base.config_hash(), shuffled.config_hash());

        let observed = resolve(&opts(EfficiencyModeArg::Observe, &[], &[], &[])).unwrap();
        assert_ne!(base.config_hash(), observed.config_hash());

        let hash = base.config_hash();
        assert!(hash.starts_with("sha256:"));
        assert_eq!(hash.len(), "sha256:".len() + 64);
        assert!(!hash.chars().any(char::is_whitespace));
    }

    #[test]
    fn observe_mode_rejects_intervention_input() {
        let error = resolve(&opts(
            EfficiencyModeArg::Observe,
            &[InterventionArg::Merge],
            &[],
            &[],
        ))
        .unwrap_err();
        assert!(error.to_string().contains("contradictory"));
        assert!(error.to_string().contains("observe"));
    }

    #[test]
    fn approving_and_overriding_the_same_action_is_contradictory() {
        let error = resolve(&opts(
            EfficiencyModeArg::Suggest,
            &[InterventionArg::Merge],
            &[InterventionArg::Merge],
            &[],
        ))
        .unwrap_err();
        assert!(error.to_string().contains("both approved and overridden"));
    }

    #[test]
    fn high_impact_autonomy_requires_auto_optimize() {
        let error = resolve(&opts(
            EfficiencyModeArg::Suggest,
            &[],
            &[],
            &[InterventionArg::Cancel],
        ))
        .unwrap_err();
        assert!(error.to_string().contains("auto-optimize"));
    }

    #[test]
    fn autonomy_grants_are_limited_to_named_high_impact_actions() {
        let error = resolve(&opts(
            EfficiencyModeArg::AutoOptimize,
            &[],
            &[],
            &[InterventionArg::Merge],
        ))
        .unwrap_err();
        assert!(error.to_string().contains("not a named high-impact action"));

        let error = resolve(&opts(
            EfficiencyModeArg::AutoOptimize,
            &[],
            &[InterventionArg::Cancel],
            &[InterventionArg::Cancel],
        ))
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("granted autonomy but also overridden"));
    }

    #[test]
    fn auto_optimize_scopes_autonomy_per_action() {
        let config = resolve(&opts(
            EfficiencyModeArg::AutoOptimize,
            &[],
            &[InterventionArg::SplitDrift],
            &[InterventionArg::Cancel],
        ))
        .unwrap();
        // Low-impact actions are autonomous in auto-optimize…
        assert!(config.autonomy_granted(RepairAction::Merge));
        // …explicit overrides always win…
        assert!(!config.autonomy_granted(RepairAction::SplitDrift));
        // …and high-impact actions need their own named grant.
        assert!(config.autonomy_granted(RepairAction::Cancel));
        assert!(!config.autonomy_granted(RepairAction::StopDownstream));
        assert!(!config.autonomy_granted(RepairAction::Reassign));
    }

    #[test]
    fn status_output_keeps_estimated_and_realized_distinct() {
        let config = resolve(&opts(EfficiencyModeArg::Suggest, &[], &[], &[])).unwrap();
        let mut data = EfficiencyData::default();
        data.build.gross_estimated_tokens_avoided = 12_000;
        data.build.confidence_adjusted_tokens_avoided = 10_200;
        data.build.realized_tokens_saved = 9_000;
        data.build.estimated_cost_avoided = 0.36;
        data.build.realized_cost_avoided = 0.27;

        let rendered = render_status(&config, &data);
        assert!(rendered.contains("Mode: suggest"));
        assert!(rendered.contains(&config.config_hash()));
        assert!(rendered.contains("Estimated tokens avoided: 12000 (confidence-adjusted 10200)"));
        assert!(rendered.contains("Realized tokens saved: 9000"));
        assert!(rendered.contains("Estimated cost avoided: $0.36"));
        assert!(rendered.contains("Realized cost avoided: $0.27"));
        // Build and lifetime sections each label both figures distinctly.
        assert_eq!(rendered.matches("Estimated tokens avoided:").count(), 2);
        assert_eq!(rendered.matches("Realized tokens saved:").count(), 2);

        let encoded = status_json(&config, &data);
        assert_eq!(encoded["config"]["mode"], "suggest");
        assert_eq!(encoded["build"]["estimated_tokens_avoided"], 12_000);
        assert_eq!(encoded["build"]["realized_tokens_saved"], 9_000);
        assert_eq!(encoded["lifetime"]["estimated_tokens_avoided"], 0);
    }

    #[test]
    fn banner_names_mode_and_config_hash() {
        let config = resolve(&opts(EfficiencyModeArg::Observe, &[], &[], &[])).unwrap();
        let banner = banner(&config);
        assert!(banner.contains("observe mode"));
        assert!(banner.contains(&config.config_hash()));
    }
}
