# Fractal Execution Efficiency Layer — Criterion Audit (AC-1…AC-14)

**Audit date:** 2026-07-29
**Primary repo:** `/workspace/fractal-efficiency.yFzdFF`
**Companion repo:** `/workspace/fractalsociety-website`
**Auditor role:** Fractal graph node `criterion_audit` (task 7.1)
**INTERFACE.md:** not present; acceptance criteria taken from `.fractal/lead-prd.json`.

**Overall verdict: PASS** — every criterion AC-1 through AC-14 has concrete passing checks below. No criterion is marked failed.

---

## Gate evidence (AC-13 re-verified during this audit)

| Check | Command | Result | Log / evidence |
| --- | --- | --- | --- |
| Rust fmt | `cargo fmt --all -- --check` | exit 0 | `/tmp/efficiency-cargo-fmt.txt` (empty = clean) |
| Rust tests | `cargo test --no-fail-fast` | **206 passed, 0 failed** | `/tmp/efficiency-cargo-test2.txt` |
| Rust clippy | `cargo clippy --all-targets -- -D warnings` | exit 0 | `/tmp/efficiency-cargo-clippy2.txt` |
| Swift package | `swift test --package-path macos/FractalVoice` | **66 passed, 0 failed** (incl. 12 `ProjectGraphEfficiencyTests` + 2 learning) | `/tmp/efficiency-swift-test.txt` |
| Website tests | `npm test` (cwd website) | **542 passed, 0 failed** | `/tmp/efficiency-npm-test.txt` |
| TS project build | `npm run build` | exit 0 (`tsc -b`) | `/tmp/efficiency-npm-build.txt` |
| Dashboard prod build | `npm run dashboard:build` | ✓ Compiled successfully; 100/100 static pages | `/tmp/efficiency-dashboard-build.txt` |

Note: an earlier full `cargo test` run once failed `project_file::tests::evolved_child_preserves_parent_execution_and_has_a_valid_hash` with a missing temp `project.fractal` (likely parallel temp-dir race). Isolated re-run and a subsequent full suite both passed (206/206). Final evidence uses the green full suite.

---

## Exact enum coverage (cross-surface)

| Surface | Waste types (13) | Repair actions (7) | Modes (3) |
| --- | --- | --- | --- |
| Rust `src/efficiency.rs` | `WasteType::ALL` length 13 | `RepairAction::ALL` length 7 | `EfficiencyMode::{Observe,Suggest,AutoOptimize}` |
| TypeScript `apps/dashboard/lib/fractal-cloud/schema.ts` | `FRACTAL_WASTE_TYPES` | `FRACTAL_REPAIR_ACTIONS` | `FRACTAL_EFFICIENCY_MODES` |
| Swift `ProjectGraphEfficiency.swift` | `WasteType: CaseIterable` 13 cases | `RepairAction` 7 cases | `Mode` observe/suggest/autoOptimize |

**Exact waste spellings:**
`duplicate_task`, `duplicate_test`, `duplicate_research`, `consolidatable_tests`, `unused_output`, `superseded_assumption`, `spec_drift`, `excessive_retries`, `overlapping_files`, `over_decomposition`, `low_value_branch`, `premature_verification`, `excessive_verification`

**Exact repair spellings:**
`merge`, `cancel`, `delay_verification`, `stop_downstream`, `reassign`, `consolidate_verifiers`, `split_drift`

**Exact mode spellings:**
`observe`, `suggest` (default), `auto_optimize`

Verified by: `efficiency::tests::waste_repair_and_mode_labels_are_exact`, Swift `testWasteRepairAndModeLabelsMatchContract`, TS `JSON schema and TypeScript enums match fractal.efficiency.v1 contract`.

---

## Changed-file inventory

### Primary repository — efficiency layer

| Path | Status |
| --- | --- |
| `src/efficiency.rs` | added |
| `src/efficiency_accounting.rs` | added |
| `src/efficiency_config.rs` | added |
| `src/efficiency_detector.rs` | added |
| `src/efficiency_policy.rs` | added |
| `src/amendments.rs` | modified |
| `src/cli.rs` | modified |
| `src/compile.rs` | modified |
| `src/decompose.rs` | modified |
| `src/execute.rs` | modified |
| `src/main.rs` | modified |
| `src/orchestrate.rs` | modified |
| `src/project_file.rs` | modified |
| `src/supervise.rs` | modified |
| `macos/FractalVoice/Sources/FractalVoice/ProjectGraphEfficiency.swift` | added |
| `macos/FractalVoice/Tests/FractalVoiceTests/ProjectGraphEfficiencyTests.swift` | added |
| `macos/FractalVoice/Sources/FractalVoice/BuildCoordinator.swift` | modified |
| `macos/FractalVoice/Sources/FractalVoice/FractalVoiceApp.swift` | modified |
| `macos/FractalVoice/Sources/FractalVoice/OnboardingView.swift` | modified |
| `EFFICIENCY_CRITERION_AUDIT.md` | added by this audit |

### Primary repository — Fractal-owned / runtime state (not product source)

| Path | Status | Notes |
| --- | --- | --- |
| `.fractal/project.fractal` | modified | Fractal execution graph state |
| `.fractal/run-state.json` | untracked | Fractal runtime |
| `.fractal/sync-state.json` | untracked | Fractal runtime |
| `fractal-plan.json` | modified | planner artifact |

### Website — efficiency layer

| Path | Status |
| --- | --- |
| `apps/dashboard/lib/fractal-cloud/schema.ts` | modified |
| `apps/dashboard/lib/fractal-cloud/project-graph-view.ts` | modified |
| `apps/dashboard/public/schemas/fractal-project-v1.schema.json` | modified |
| `apps/dashboard/app/api/cli/projects/[slug]/route.ts` | modified |
| `apps/dashboard/app/[owner]/[project]/page.tsx` | modified |
| `apps/dashboard/components/projects/ExecutionGraphPreview.tsx` | modified |
| `apps/dashboard/components/projects/ProjectSharePage.tsx` | modified |
| `apps/dashboard/components/projects/project.css` | modified |
| `apps/dashboard/components/graph/GraphApp.tsx` | modified |
| `apps/dashboard/components/graph/graph.css` | modified |
| `apps/dashboard/components/account/ProjectDashboard.tsx` | modified |
| `apps/dashboard/components/account/account.css` | modified |
| `apps/dashboard/test/fractal-cloud-schema.test.ts` | modified |
| `apps/dashboard/test/fractal-cloud-efficiency-schema.test.ts` | added |
| `apps/dashboard/test/fractal-cloud-efficiency-api.test.ts` | added |
| `apps/dashboard/test/project-efficiency-view.test.ts` | added |
| `apps/dashboard/test/project-efficiency-ui-contract.test.ts` | added |

### Website — preserved unrelated dirty-tree files (no efficiency keywords; left intact)

These were present in the companion worktree and were **not** edited by this audit task. They are **out of efficiency scope** and were preserved rather than reverted:

- `apps/dashboard/components/account/PublicAccountActions.tsx` (untracked)
- `apps/dashboard/lib/fractal-cloud/account-navigation.ts` (untracked)
- `apps/dashboard/test/account-navigation.test.tsx` (untracked)
- `apps/dashboard/test/cinematic-loader.test.ts` (untracked)
- `apps/dashboard/test/umami-tracker.test.ts` (untracked)
- Diffs without efficiency keywords: `SocietyLanding.tsx`, `society.css`, `app/page.tsx`, `app/layout.tsx` (Umami script), `app/account/login/page.tsx`

---

## AC-1 — Portable contract, legacy + enums + round-trip

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | Rust: `efficiency::tests::{waste_repair_and_mode_labels_are_exact,golden_efficiency_envelope_round_trips,legacy_project_without_efficiency_remains_valid,efficiency_envelope_does_not_mutate_learning_v1,node_efficiency_metadata_bounds,unknown_waste_in_aggregate_is_rejected}`; TS: `fractal-cloud-efficiency-schema.test.ts` enum/legacy/round-trip cases |
| **Evidence** | Full Rust suite 206/206; website suite 542/542 including schema tests; enums match 13/7/3 exactly (see above) |

---

## AC-2 — Node planning metadata on new graphs; legacy still schedules

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | `compile::tests::{compiled_nodes_expose_validated_efficiency_metadata,legacy_harness_without_efficiency_metadata_still_compiles,efficiency_dependencies_must_match_graph_edges,efficiency_similarity_peers_must_name_other_nodes,out_of_range_efficiency_metadata_fails_before_commitment}`; `decompose::tests::{planned_tasks_expose_efficiency_metadata_with_defaults,decomposed_genome_compiles_with_efficiency_metadata_on_every_node,declared_efficiency_metadata_is_validated_and_preserved}`; amendment metadata tests |
| **Evidence** | Required fields present in `NodeEfficiencyMetadata`: `estimated_remaining_tokens`, `dependencies`, `expected_artifact`, `files_or_systems_affected`, `verification_plan`, `current_assumptions`, `similarity_to_other_active_nodes`, `confidence_still_useful`. Covered by compile/decompose tests in green full suite. |

---

## AC-3 — Between-wave safe boundary; no race with checkout/hash

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | `execute::tests::{efficiency_boundary_records_one_idempotent_episode_before_checkout,concurrency_barrier_blocks_checkout_during_boundary,auto_optimize_cancels_exact_duplicate_hash_invariantly,stale_snapshot_refuses_repair_mutation,resume_boundary_is_idempotent_over_completed_seed}`; accounting `recording_efficiency_preserves_graph_hash_and_unrelated_state` |
| **Evidence** | Barrier serializes inspection before checkout; repeated boundary → one episode; `graph_hash` unchanged under auto-cancel; stale snapshot refuses mutation. All green in full suite. |

---

## AC-4 — Detector: all 13 waste types + adversarial near-match rejection

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | `efficiency_detector::tests::{detects_each_waste_type_with_required_fields,false_positive_protection_for_adversarial_near_matches,deterministic_ordering_is_stable_regardless_of_input_order}` |
| **Evidence** | Positive fixture loops `WasteType::ALL` (13); adversarial near-match loop asserts no false detection per type; ordering stable under reverse input. Green in full suite. |

---

## AC-5 — Mode/policy matrix: observe / suggest default / auto_optimize safe set

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | `efficiency_policy::tests::{suggest_is_the_default_mode,observe_only_flags_across_the_entire_matrix,suggest_proposes_everything_and_never_auto_applies,suggest_applies_only_after_explicit_acceptance,auto_optimize_autonomy_is_exactly_the_safe_set,similarity_based_duplicates_require_acceptance,critical_path_impact_always_requires_a_human,stalled_dependents_disqualify_autonomy,branch_spec_and_high_impact_repairs_require_approval,exact_scoped_autonomy_narrowly_authorizes_one_action,human_override_denies_in_every_non_observe_mode,full_matrix_upholds_structural_invariants,safe_set_predicate_matches_enumerated_pairs}`; CLI/config: `efficiency_config::tests::*`, `cli::tests::*`, `tests::{dispatches_efficiency_status_with_defaults,rejects_contradictory_efficiency_configuration}` |
| **Evidence** | Observe → Flag only across waste×action×impact×approval×scope; Suggest never AutoApply without acceptance; AutoOptimize AutoApply only exact safe pairs; high-impact/branch/spec/critical-path need approval or exact scoped autonomy. Green. |

---

## AC-6 — Compact episode fields, bounds, realized-only-with-evidence

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | Rust: `efficiency::tests::{golden_efficiency_envelope_round_trips,realized_savings_require_comparison_evidence,node_efficiency_metadata_bounds}`; accounting: `episode_id_is_deterministic_and_order_invariant`, `malformed_confidence_and_oversized_fields_are_rejected`; TS schema: confidence/realized/bounds/duplicate-id rejection tests |
| **Evidence** | Episodes carry stable id, waste, nodes, action, accepted, mode, gross estimate, basis, confidence∈[0,1], adjusted estimate, optional realized + realization_basis, follow-up, override, actor, timestamps, evidence refs, aggregation_version, config_hash. Realized rejected without comparison evidence. Green. |

---

## AC-7 — Aggregates: dedupe; estimates ≠ realized

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | `efficiency_accounting::tests::{golden_estimate_only_fold_never_populates_realized_fields,golden_realized_requires_baseline_evidence_and_exact_totals,golden_multiple_builds_and_config_versions_split_build_from_lifetime,golden_followup_update_is_idempotent_and_keeps_estimates_separate,retry_is_idempotent_and_followup_updates_in_place}`; TS view: `estimate-only efficiency never presents estimates as realized`, `lifetime totals stay separate from build counters` |
| **Evidence** | Build vs lifetime folds; duplicate ingestion idempotent; estimate-only never fills realized; realized golden totals exact. Green. |

---

## AC-8 — Persistence atomic/idempotent/bounded/secret-safe

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | Rust: `secret_shaped_values_and_fields_are_rejected`, `persistence_appends_episode_beside_learning`, `retry_is_idempotent_and_followup_updates_in_place`, `legacy_documents_without_efficiency_remain_loadable`, `malformed_*`, `project_file::tests::refuses_credential_fields`; TS: schema + API credential rejection (`CLI PUT rejects credential-shaped efficiency fields and values`) |
| **Evidence** | Credential-shaped keys/values rejected in Rust and dashboard API; secrets do not persist; size/confidence bounds enforced. Diff scan of primary `src/` + `macos/` found no leaked credential assignments. Website mentions of `api_key`/`authorization`/`private_key` are rejection-fixture names only. |

---

## AC-9 — CLI modes, approvals, Estimated vs Realized

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | `cli::tests::{efficiency_defaults_to_suggest_with_no_grants,parses_efficiency_modes_and_intervention_input,rejects_unknown_efficiency_values,run_carries_flattened_efficiency_controls}`; `efficiency_config::tests::{defaults_to_suggest_with_no_grants,status_output_keeps_estimated_and_realized_distinct,observe_mode_rejects_intervention_input,approving_and_overriding_the_same_action_is_contradictory,high_impact_autonomy_requires_auto_optimize,...}`; `tests::dispatches_efficiency_status_with_defaults` |
| **Evidence** | Default mode suggest; unsafe combinations rejected; status labels keep Estimated and Realized distinct. Green. |

---

## AC-10 — Fractal Voice decode, controls, lifetime labels, learning coexistence

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | Swift `ProjectGraphEfficiencyTests`: legacy readable; default suggest; golden decode; Estimated/Realized separation; malformed → nil without throw; efficiency decode does not interfere with learning; CLI arg forwarding; observe/suggest drop unsafe args; lifetime labels never claim estimate is realized |
| **Evidence** | `swift test --package-path macos/FractalVoice` → 66/66 including 12 efficiency + 2 learning tests. `ProjectGraphLearning.swift` unmodified (`git status` clean for learning sources). |

---

## AC-11 — Society counter, details, drift amber/pulse, no confetti

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | `project-efficiency-view.test.ts` (Estimated 200K formatting, estimate≠realized, drift sets); `project-efficiency-ui-contract.test.ts` (clickable disclosure, Realized evidence, amber/pulse CSS, no conflation) |
| **Evidence** | Counter label `Estimated 200K tokens saved`; detail exposes confidence/basis/realized evidence/waste breakdown; `.execution-node.spec-drift` amber + `@keyframes efficiency-drift-pulse`; `rg confetti apps/dashboard` → no matches. Green in `npm test`. |

---

## AC-12 — Reduced motion + lifetime Estimated/Realized on sidebar/profile

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | UI contract: `prefers-reduced-motion: reduce` disables pulse/transition; drift content still present via static opacity/max-height rules in `project.css`; GraphApp + ProjectDashboard lifetime Estimated/Realized labels |
| **Evidence** | `@media (prefers-reduced-motion: reduce)` in `project.css` / `graph.css`; tests assert reduced-motion fallback and secondary lifetime labels. Green. |

---

## AC-13 — Full native quality gates

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | See gate table at top of this document |
| **Evidence** | All seven required commands green when re-run during this audit (Rust fmt/test/clippy, Swift package, npm test/build/dashboard:build). |

---

## AC-14 — This criterion audit + scope / safety confirmations

| | |
| --- | --- |
| **Status** | **PASS** |
| **Checks** | This document maps AC-1…AC-13 to concrete tests with pass evidence; enumerates changed files; verifies enums, scheduler safety, approval boundaries, accounting honesty, UI/reduced-motion, gates, learning preservation, secret absence, and prohibited actions |
| **Evidence** | File: `EFFICIENCY_CRITERION_AUDIT.md` (this report) |

### Explicit confirmations required by AC-14 / task brief

| Confirmation | Result |
| --- | --- |
| `fractal.learning.v1` preserved (not replaced/weakened) | **PASS** — learning modules unmodified; tests `efficiency_envelope_does_not_mutate_learning_v1`, Swift learning coexistence, TS `efficiency envelope round-trips beside learning without mutating learning.v1`, API ETag test preserves learning |
| Exact waste/action/mode enum coverage | **PASS** — 13 / 7 / 3 identical across Rust, Swift, TS/JSON schema |
| Scheduler safety | **PASS** — barrier, idempotent episode, hash invariance, stale-snapshot refusal tests green |
| Approval boundaries | **PASS** — full policy matrix + CLI contradiction tests green |
| Accounting honesty (estimate ≠ realized) | **PASS** — golden folds + UI/CLI/Swift label-separation tests green |
| UI / reduced-motion | **PASS** — contract + CSS media-query tests green; no confetti |
| Full gate results | **PASS** — AC-13 table |
| No secret leakage into efficiency publish path | **PASS** — secret rejection tests green; no credential assignments in efficiency source diffs |
| No `dist-*` file created/edited by this work | **PASS** — `git status` shows no `dist*` / `dist-*` paths in either repo |
| No commit | **PASS** — both repos `main...origin/main` with **0 ahead / 0 behind**; working tree dirty only |
| No push | **PASS** — not ahead of origin; no push performed by this audit |
| No sign / package / deploy / submission | **PASS** — no `.pkg`/`.dmg`/notarize/deploy/submit actions; Swift/npm gates were test/build only |
| No unrelated change made by this audit task | **PASS** — this task created only `EFFICIENCY_CRITERION_AUDIT.md`; pre-existing unrelated website dirty files were preserved, not expanded |
| Unrelated changes preserved | **PASS** — account-navigation / umami / cinematic / SocietyLanding dirty tree left intact |

---

## Summary

All acceptance criteria **AC-1 through AC-14** are **PASS** with concrete test names and gate command evidence. Efficiency deliverables are confined to the enumerated Rust/Swift/TS files; Fractal state under `.fractal/` is controller-owned; unrelated companion-repo dirt was preserved; no dist artifact, commit, push, sign, package, deploy, or submission occurred.
