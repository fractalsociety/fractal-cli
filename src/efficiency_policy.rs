//! Pure policy decision matrix for efficiency repairs.
//!
//! Given the governed mode, the detected waste class, the proposed repair,
//! the dependency/critical-path impact, and the human's explicit approval or
//! exact scoped-autonomy grants, `decide` returns exactly one decision. The
//! function is total and side-effect free: it never touches the scheduler,
//! the graph, or any persisted state.
//!
//! Invariants encoded here:
//! - `observe` only flags — it never proposes, applies, or denies anything.
//! - `suggest` (the product default) proposes every repair and applies one
//!   only after explicit human acceptance.
//! - `auto_optimize` applies autonomously only exact duplicate
//!   cancellations/merges and provably safe verification delays. Branch
//!   pruning, spec-level repairs, stop-downstream, reassign, split-drift, and
//!   every other high-impact repair still require approval unless the exact
//!   action was separately scoped for autonomy.
//! - Critical-path impact always requires a human decision: a scoped-autonomy
//!   grant names an action, not a blast radius.
//! - An explicit human override denies the repair in every non-observe mode,
//!   regardless of scoping or safety.

#![allow(dead_code)]

use crate::efficiency::{EfficiencyMode, RepairAction, WasteType};

/// Waste classes whose repairs prune or reshape planned intent (branch and
/// spec decisions). These never apply autonomously via the safe set.
pub(crate) const APPROVAL_REQUIRED_WASTES: [WasteType; 2] =
    [WasteType::LowValueBranch, WasteType::SpecDrift];

/// Repairs that stop, move, or restructure work. They require approval in
/// every mode unless the exact action is separately scoped for autonomy.
pub(crate) const APPROVAL_REQUIRED_ACTIONS: [RepairAction; 3] = [
    RepairAction::StopDownstream,
    RepairAction::Reassign,
    RepairAction::SplitDrift,
];

/// Dependency and critical-path consequences of applying one repair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImpactAssessment {
    /// The duplicate evidence is structural (identical title and artifact),
    /// not merely a similarity score above threshold.
    pub(crate) exact_duplicate: bool,
    /// Any affected node lies on the critical path of the execution graph.
    pub(crate) on_critical_path: bool,
    /// Applying the repair would stall or orphan active dependent nodes.
    pub(crate) blocks_active_dependents: bool,
}

/// The human's explicit stance on this exact intervention, if any.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ApprovalState {
    #[default]
    NotRequested,
    /// The human explicitly accepted this intervention.
    Granted,
    /// The human explicitly overrode (declined) this intervention.
    Overridden,
}

impl ApprovalState {
    pub(crate) const ALL: [ApprovalState; 3] = [
        ApprovalState::NotRequested,
        ApprovalState::Granted,
        ApprovalState::Overridden,
    ];
}

/// One fully specified policy question.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PolicyRequest {
    pub(crate) mode: EfficiencyMode,
    pub(crate) waste: WasteType,
    pub(crate) action: RepairAction,
    pub(crate) impact: ImpactAssessment,
    pub(crate) approval: ApprovalState,
    /// Exact actions separately scoped for autonomy (auto-optimize only).
    pub(crate) scoped_autonomy: Vec<RepairAction>,
}

/// The single decision the scheduler is allowed to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyDecision {
    /// Record the signal only; never propose or apply (observe mode).
    Flag,
    /// Surface the repair and wait for explicit human acceptance.
    Propose,
    /// Apply because the human explicitly accepted this intervention.
    ApplyApproved,
    /// Apply autonomously without a human in the loop.
    AutoApply,
    /// Do nothing: the human explicitly overrode this intervention.
    Deny,
}

/// Decision plus an auditable basis suitable for episode records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PolicyOutcome {
    pub(crate) decision: PolicyDecision,
    pub(crate) reason: &'static str,
}

fn outcome(decision: PolicyDecision, reason: &'static str) -> PolicyOutcome {
    PolicyOutcome { decision, reason }
}

/// Repairs the safe autonomous set covers: exact duplicate
/// cancellations/merges and provably safe verification delays. Everything
/// else is outside autonomy regardless of mode.
pub(crate) fn is_provably_safe(
    waste: WasteType,
    action: RepairAction,
    impact: ImpactAssessment,
) -> bool {
    if impact.on_critical_path || impact.blocks_active_dependents {
        return false;
    }
    match (waste, action) {
        (
            WasteType::DuplicateTask | WasteType::DuplicateTest | WasteType::DuplicateResearch,
            RepairAction::Cancel | RepairAction::Merge,
        ) => impact.exact_duplicate,
        (
            WasteType::PrematureVerification | WasteType::ExcessiveVerification,
            RepairAction::DelayVerification,
        ) => true,
        _ => false,
    }
}

/// Resolve one policy question. Total over every input combination.
pub(crate) fn decide(request: &PolicyRequest) -> PolicyOutcome {
    if request.mode == EfficiencyMode::Observe {
        return outcome(
            PolicyDecision::Flag,
            "observe mode records efficiency signals only",
        );
    }
    match request.approval {
        ApprovalState::Overridden => {
            return outcome(
                PolicyDecision::Deny,
                "human override declines this intervention",
            );
        }
        ApprovalState::Granted => {
            return outcome(
                PolicyDecision::ApplyApproved,
                "human explicitly accepted this intervention",
            );
        }
        ApprovalState::NotRequested => {}
    }
    if request.mode == EfficiencyMode::Suggest {
        return outcome(
            PolicyDecision::Propose,
            "suggest mode requires explicit acceptance for every repair",
        );
    }

    // Auto-optimize, no explicit human stance yet.
    if request.impact.on_critical_path {
        return outcome(
            PolicyDecision::Propose,
            "critical-path impact always requires human approval",
        );
    }
    if request.scoped_autonomy.contains(&request.action) {
        return outcome(
            PolicyDecision::AutoApply,
            "exact action separately scoped for autonomy",
        );
    }
    if is_provably_safe(request.waste, request.action, request.impact) {
        return outcome(
            PolicyDecision::AutoApply,
            "exact duplicate cancellation/merge or provably safe verification delay",
        );
    }
    let reason = if APPROVAL_REQUIRED_WASTES.contains(&request.waste) {
        "branch and spec repairs require human approval"
    } else if APPROVAL_REQUIRED_ACTIONS.contains(&request.action) {
        "high-impact repair requires approval unless the exact action is scoped"
    } else if request.impact.blocks_active_dependents {
        "repair would stall active dependents and requires approval"
    } else {
        "repair is outside the provably safe set and requires acceptance"
    };
    outcome(PolicyDecision::Propose, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        mode: EfficiencyMode,
        waste: WasteType,
        action: RepairAction,
        impact: ImpactAssessment,
        approval: ApprovalState,
        scoped_autonomy: &[RepairAction],
    ) -> PolicyRequest {
        PolicyRequest {
            mode,
            waste,
            action,
            impact,
            approval,
            scoped_autonomy: scoped_autonomy.to_vec(),
        }
    }

    fn benign_exact_duplicate() -> ImpactAssessment {
        ImpactAssessment {
            exact_duplicate: true,
            on_critical_path: false,
            blocks_active_dependents: false,
        }
    }

    fn all_impacts() -> Vec<ImpactAssessment> {
        let mut impacts = Vec::new();
        for exact_duplicate in [false, true] {
            for on_critical_path in [false, true] {
                for blocks_active_dependents in [false, true] {
                    impacts.push(ImpactAssessment {
                        exact_duplicate,
                        on_critical_path,
                        blocks_active_dependents,
                    });
                }
            }
        }
        impacts
    }

    /// The complete safe autonomous set, enumerated independently of the
    /// implementation: exact duplicate cancellations/merges plus verification
    /// delays for the two verification-timing wastes.
    const SAFE_PAIRS: [(WasteType, RepairAction); 8] = [
        (WasteType::DuplicateTask, RepairAction::Cancel),
        (WasteType::DuplicateTask, RepairAction::Merge),
        (WasteType::DuplicateTest, RepairAction::Cancel),
        (WasteType::DuplicateTest, RepairAction::Merge),
        (WasteType::DuplicateResearch, RepairAction::Cancel),
        (WasteType::DuplicateResearch, RepairAction::Merge),
        (
            WasteType::PrematureVerification,
            RepairAction::DelayVerification,
        ),
        (
            WasteType::ExcessiveVerification,
            RepairAction::DelayVerification,
        ),
    ];

    #[test]
    fn suggest_is_the_default_mode() {
        assert_eq!(EfficiencyMode::default(), EfficiencyMode::Suggest);
    }

    #[test]
    fn observe_only_flags_across_the_entire_matrix() {
        for waste in WasteType::ALL {
            for action in RepairAction::ALL {
                for impact in all_impacts() {
                    for approval in ApprovalState::ALL {
                        for scope in [vec![], vec![action], RepairAction::ALL.to_vec()] {
                            let outcome = decide(&request(
                                EfficiencyMode::Observe,
                                waste,
                                action,
                                impact,
                                approval,
                                &scope,
                            ));
                            assert_eq!(
                                outcome.decision,
                                PolicyDecision::Flag,
                                "observe must only flag: {waste:?} {action:?} {impact:?} \
                                 {approval:?} scope={scope:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn suggest_proposes_everything_and_never_auto_applies() {
        for waste in WasteType::ALL {
            for action in RepairAction::ALL {
                for impact in all_impacts() {
                    // Even a full autonomy scope must not bypass acceptance
                    // outside auto-optimize.
                    let outcome = decide(&request(
                        EfficiencyMode::Suggest,
                        waste,
                        action,
                        impact,
                        ApprovalState::NotRequested,
                        &RepairAction::ALL,
                    ));
                    assert_eq!(
                        outcome.decision,
                        PolicyDecision::Propose,
                        "suggest must wait for acceptance: {waste:?} {action:?} {impact:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn suggest_applies_only_after_explicit_acceptance() {
        for waste in WasteType::ALL {
            for action in RepairAction::ALL {
                let accepted = decide(&request(
                    EfficiencyMode::Suggest,
                    waste,
                    action,
                    benign_exact_duplicate(),
                    ApprovalState::Granted,
                    &[],
                ));
                assert_eq!(accepted.decision, PolicyDecision::ApplyApproved);
                let declined = decide(&request(
                    EfficiencyMode::Suggest,
                    waste,
                    action,
                    benign_exact_duplicate(),
                    ApprovalState::Overridden,
                    &[],
                ));
                assert_eq!(declined.decision, PolicyDecision::Deny);
            }
        }
    }

    #[test]
    fn auto_optimize_autonomy_is_exactly_the_safe_set() {
        let mut autonomous = Vec::new();
        for waste in WasteType::ALL {
            for action in RepairAction::ALL {
                let outcome = decide(&request(
                    EfficiencyMode::AutoOptimize,
                    waste,
                    action,
                    benign_exact_duplicate(),
                    ApprovalState::NotRequested,
                    &[],
                ));
                match outcome.decision {
                    PolicyDecision::AutoApply => autonomous.push((waste, action)),
                    PolicyDecision::Propose => {}
                    other => panic!(
                        "unexpected decision {other:?} for {waste:?} {action:?} without \
                         approval or override"
                    ),
                }
            }
        }
        let label = |(waste, action): &(WasteType, RepairAction)| (waste.as_str(), action.as_str());
        autonomous.sort_by_key(label);
        let mut expected = SAFE_PAIRS.to_vec();
        expected.sort_by_key(label);
        assert_eq!(autonomous, expected);
    }

    #[test]
    fn similarity_based_duplicates_require_acceptance() {
        for (waste, action) in [
            (WasteType::DuplicateTask, RepairAction::Cancel),
            (WasteType::DuplicateTask, RepairAction::Merge),
            (WasteType::DuplicateTest, RepairAction::Merge),
            (WasteType::DuplicateResearch, RepairAction::Cancel),
        ] {
            let outcome = decide(&request(
                EfficiencyMode::AutoOptimize,
                waste,
                action,
                ImpactAssessment {
                    exact_duplicate: false,
                    ..benign_exact_duplicate()
                },
                ApprovalState::NotRequested,
                &[],
            ));
            assert_eq!(
                outcome.decision,
                PolicyDecision::Propose,
                "non-exact duplicate must not auto-apply: {waste:?} {action:?}"
            );
        }
    }

    #[test]
    fn critical_path_impact_always_requires_a_human() {
        let critical = ImpactAssessment {
            on_critical_path: true,
            ..benign_exact_duplicate()
        };
        for waste in WasteType::ALL {
            for action in RepairAction::ALL {
                // Neither the safe set nor an exact scoped grant covers the
                // critical path.
                for scope in [vec![], vec![action]] {
                    let outcome = decide(&request(
                        EfficiencyMode::AutoOptimize,
                        waste,
                        action,
                        critical,
                        ApprovalState::NotRequested,
                        &scope,
                    ));
                    assert_eq!(
                        outcome.decision,
                        PolicyDecision::Propose,
                        "critical path must escalate: {waste:?} {action:?} scope={scope:?}"
                    );
                }
                // Explicit acceptance is the only path to application there.
                let accepted = decide(&request(
                    EfficiencyMode::AutoOptimize,
                    waste,
                    action,
                    critical,
                    ApprovalState::Granted,
                    &[],
                ));
                assert_eq!(accepted.decision, PolicyDecision::ApplyApproved);
            }
        }
    }

    #[test]
    fn stalled_dependents_disqualify_autonomy() {
        for (waste, action) in SAFE_PAIRS {
            let outcome = decide(&request(
                EfficiencyMode::AutoOptimize,
                waste,
                action,
                ImpactAssessment {
                    blocks_active_dependents: true,
                    ..benign_exact_duplicate()
                },
                ApprovalState::NotRequested,
                &[],
            ));
            assert_eq!(
                outcome.decision,
                PolicyDecision::Propose,
                "repair stalling dependents must not auto-apply: {waste:?} {action:?}"
            );
        }
    }

    #[test]
    fn branch_spec_and_high_impact_repairs_require_approval() {
        // Branch pruning and spec repairs never auto-apply from the safe set.
        for waste in APPROVAL_REQUIRED_WASTES {
            for action in RepairAction::ALL {
                let outcome = decide(&request(
                    EfficiencyMode::AutoOptimize,
                    waste,
                    action,
                    benign_exact_duplicate(),
                    ApprovalState::NotRequested,
                    &[],
                ));
                assert_eq!(outcome.decision, PolicyDecision::Propose);
            }
        }
        // Stop-downstream, reassign, and split-drift require approval for
        // every waste class when not separately scoped.
        for action in APPROVAL_REQUIRED_ACTIONS {
            for waste in WasteType::ALL {
                let outcome = decide(&request(
                    EfficiencyMode::AutoOptimize,
                    waste,
                    action,
                    benign_exact_duplicate(),
                    ApprovalState::NotRequested,
                    &[],
                ));
                assert_eq!(
                    outcome.decision,
                    PolicyDecision::Propose,
                    "unscoped high-impact repair must escalate: {waste:?} {action:?}"
                );
            }
        }
    }

    #[test]
    fn exact_scoped_autonomy_narrowly_authorizes_one_action() {
        let scope = [RepairAction::StopDownstream];
        for waste in WasteType::ALL {
            // The scoped action applies autonomously off the critical path…
            let scoped = decide(&request(
                EfficiencyMode::AutoOptimize,
                waste,
                RepairAction::StopDownstream,
                benign_exact_duplicate(),
                ApprovalState::NotRequested,
                &scope,
            ));
            assert_eq!(scoped.decision, PolicyDecision::AutoApply);
            // …but the grant does not bleed into sibling high-impact actions.
            for action in [RepairAction::Reassign, RepairAction::SplitDrift] {
                let unscoped = decide(&request(
                    EfficiencyMode::AutoOptimize,
                    waste,
                    action,
                    benign_exact_duplicate(),
                    ApprovalState::NotRequested,
                    &scope,
                ));
                assert_eq!(
                    unscoped.decision,
                    PolicyDecision::Propose,
                    "scope for stop_downstream must not authorize {action:?}"
                );
            }
        }
        // A separately scoped split_drift lifts the spec-drift approval
        // requirement — the exact-action escape hatch the contract names.
        let drift = decide(&request(
            EfficiencyMode::AutoOptimize,
            WasteType::SpecDrift,
            RepairAction::SplitDrift,
            benign_exact_duplicate(),
            ApprovalState::NotRequested,
            &[RepairAction::SplitDrift],
        ));
        assert_eq!(drift.decision, PolicyDecision::AutoApply);
    }

    #[test]
    fn human_override_denies_in_every_non_observe_mode() {
        for mode in [EfficiencyMode::Suggest, EfficiencyMode::AutoOptimize] {
            for waste in WasteType::ALL {
                for action in RepairAction::ALL {
                    let outcome = decide(&request(
                        mode,
                        waste,
                        action,
                        benign_exact_duplicate(),
                        ApprovalState::Overridden,
                        &RepairAction::ALL,
                    ));
                    assert_eq!(
                        outcome.decision,
                        PolicyDecision::Deny,
                        "override must win: {mode:?} {waste:?} {action:?}"
                    );
                }
            }
        }
    }

    /// Exhaustive sweep over the full matrix asserting the structural
    /// invariants of the policy, independent of any specific (waste, action)
    /// table: where flags, denials, approvals, and autonomy may ever occur.
    #[test]
    fn full_matrix_upholds_structural_invariants() {
        for mode in EfficiencyMode::ALL {
            for waste in WasteType::ALL {
                for action in RepairAction::ALL {
                    for impact in all_impacts() {
                        for approval in ApprovalState::ALL {
                            let other = RepairAction::ALL
                                .into_iter()
                                .find(|candidate| *candidate != action)
                                .unwrap();
                            for scope in [vec![], vec![action], vec![other]] {
                                let outcome =
                                    decide(&request(mode, waste, action, impact, approval, &scope));
                                let context = format!(
                                    "{mode:?} {waste:?} {action:?} {impact:?} {approval:?} \
                                     scope={scope:?} -> {outcome:?}"
                                );
                                assert_eq!(
                                    outcome.decision == PolicyDecision::Flag,
                                    mode == EfficiencyMode::Observe,
                                    "flag iff observe: {context}"
                                );
                                if mode != EfficiencyMode::Observe {
                                    assert_eq!(
                                        outcome.decision == PolicyDecision::Deny,
                                        approval == ApprovalState::Overridden,
                                        "deny iff overridden: {context}"
                                    );
                                    assert_eq!(
                                        outcome.decision == PolicyDecision::ApplyApproved,
                                        approval == ApprovalState::Granted,
                                        "apply-approved iff granted: {context}"
                                    );
                                }
                                if outcome.decision == PolicyDecision::AutoApply {
                                    assert_eq!(
                                        mode,
                                        EfficiencyMode::AutoOptimize,
                                        "autonomy only in auto-optimize: {context}"
                                    );
                                    assert!(
                                        !impact.on_critical_path,
                                        "no autonomy on the critical path: {context}"
                                    );
                                    assert!(
                                        scope.contains(&action)
                                            || is_provably_safe(waste, action, impact),
                                        "autonomy needs an exact scope or the safe set: {context}"
                                    );
                                }
                                assert!(!outcome.reason.is_empty(), "reason required: {context}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn safe_set_predicate_matches_enumerated_pairs() {
        for waste in WasteType::ALL {
            for action in RepairAction::ALL {
                assert_eq!(
                    is_provably_safe(waste, action, benign_exact_duplicate()),
                    SAFE_PAIRS.contains(&(waste, action)),
                    "{waste:?} {action:?}"
                );
                // Nothing is provably safe under adverse impact.
                for impact in all_impacts() {
                    if impact.on_critical_path || impact.blocks_active_dependents {
                        assert!(!is_provably_safe(waste, action, impact));
                    }
                }
            }
        }
    }

    #[test]
    fn outcomes_carry_auditable_reasons() {
        let flagged = decide(&request(
            EfficiencyMode::Observe,
            WasteType::DuplicateTask,
            RepairAction::Cancel,
            benign_exact_duplicate(),
            ApprovalState::NotRequested,
            &[],
        ));
        assert_eq!(
            flagged.reason,
            "observe mode records efficiency signals only"
        );

        let proposed = decide(&request(
            EfficiencyMode::AutoOptimize,
            WasteType::SpecDrift,
            RepairAction::SplitDrift,
            benign_exact_duplicate(),
            ApprovalState::NotRequested,
            &[],
        ));
        assert_eq!(proposed.decision, PolicyDecision::Propose);
        assert_eq!(
            proposed.reason,
            "branch and spec repairs require human approval"
        );

        let escalated = decide(&request(
            EfficiencyMode::AutoOptimize,
            WasteType::ExcessiveRetries,
            RepairAction::StopDownstream,
            benign_exact_duplicate(),
            ApprovalState::NotRequested,
            &[],
        ));
        assert_eq!(
            escalated.reason,
            "high-impact repair requires approval unless the exact action is scoped"
        );

        let auto = decide(&request(
            EfficiencyMode::AutoOptimize,
            WasteType::DuplicateTask,
            RepairAction::Cancel,
            benign_exact_duplicate(),
            ApprovalState::NotRequested,
            &[],
        ));
        assert_eq!(auto.decision, PolicyDecision::AutoApply);
        assert_eq!(
            auto.reason,
            "exact duplicate cancellation/merge or provably safe verification delay"
        );
    }
}
