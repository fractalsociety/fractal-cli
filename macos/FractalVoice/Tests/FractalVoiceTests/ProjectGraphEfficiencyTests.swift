import Foundation
import XCTest
@testable import FractalVoice

final class ProjectGraphEfficiencyTests: XCTestCase {
    func testLegacyProjectWithoutEfficiencyRemainsReadable() throws {
        let data = #"{"schema":"fractal.project.v1"}"#.data(using: .utf8)!
        let envelope = try JSONDecoder().decode(ProjectGraphEfficiency.Envelope.self, from: data)
        XCTAssertNil(envelope.efficiency)
        XCTAssertNil(ProjectGraphEfficiency.decode(from: data))
    }

    func testDefaultModeIsSuggestWhenModeOmitted() throws {
        let data = """
        {
          "efficiency": {
            "schema": "fractal.efficiency.v1",
            "aggregation_version": 1,
            "config_hash": "cfg_suggest_v1",
            "episodes": [],
            "build": {},
            "lifetime": {}
          }
        }
        """.data(using: .utf8)!
        let efficiency = try XCTUnwrap(
            JSONDecoder().decode(ProjectGraphEfficiency.Envelope.self, from: data).efficiency
        )
        XCTAssertEqual(efficiency.mode, .suggest)
        XCTAssertEqual(efficiency.mode.displayName, "Suggest")
    }

    func testGoldenEfficiencyEnvelopeDecodesWithViewModels() throws {
        let data = """
        {
          "efficiency": {
            "schema": "fractal.efficiency.v1",
            "mode": "suggest",
            "aggregation_version": 1,
            "config_hash": "cfg_suggest_v1",
            "episodes": [
              {
                "episode_id": "ep_duplicate_task_node_a",
                "waste_type": "duplicate_task",
                "detected_node": "node_a",
                "affected_node_ids": ["node_a", "node_b"],
                "affected_count": 2,
                "proposed_action": "cancel",
                "accepted": false,
                "mode": "suggest",
                "estimated_tokens_avoided": 12000,
                "estimation_basis": "exact title and artifact match against node_b",
                "confidence": 0.85,
                "confidence_adjusted_tokens_avoided": 10200,
                "human_override": false,
                "actor": "fractal-efficiency",
                "detected_at": "2026-07-29T13:00:00Z",
                "evidence_refs": ["sim:node_a:node_b"],
                "aggregation_version": 1,
                "config_hash": "cfg_suggest_v1"
              }
            ],
            "build": {
              "episode_count": 1,
              "gross_estimated_tokens_avoided": 12000,
              "confidence_adjusted_tokens_avoided": 10200,
              "realized_tokens_saved": 0,
              "estimated_cost_avoided": 0.36,
              "realized_cost_avoided": 0.0,
              "estimated_agent_hours_avoided": 0.4,
              "realized_agent_hours_avoided": 0.0,
              "rework_prevented": 1,
              "waste_breakdown": { "duplicate_task": 1 },
              "highest_intervention": "cancel",
              "aggregation_version": 1,
              "config_hash": "cfg_suggest_v1"
            },
            "lifetime": {
              "episode_count": 1,
              "gross_estimated_tokens_avoided": 12000,
              "confidence_adjusted_tokens_avoided": 10200,
              "realized_tokens_saved": 9000,
              "estimated_cost_avoided": 0.36,
              "realized_cost_avoided": 0.27,
              "estimated_agent_hours_avoided": 0.4,
              "realized_agent_hours_avoided": 0.3,
              "rework_prevented": 1,
              "waste_breakdown": { "duplicate_task": 1 },
              "highest_intervention": "cancel",
              "aggregation_version": 1,
              "config_hash": "cfg_suggest_v1"
            }
          }
        }
        """.data(using: .utf8)!

        let efficiency = try XCTUnwrap(ProjectGraphEfficiency.decode(from: data))
        XCTAssertEqual(efficiency.schema, ProjectGraphEfficiency.schemaID)
        XCTAssertEqual(efficiency.episodes.count, 1)
        XCTAssertEqual(efficiency.episodes[0].wasteType, .duplicateTask)
        XCTAssertEqual(efficiency.episodes[0].proposedAction, .cancel)
        XCTAssertNil(efficiency.episodes[0].realizedTokensSaved)

        let build = efficiency.buildViewModel
        XCTAssertEqual(build.grossEstimatedTokensLabel, "Estimated 12,000 tokens")
        XCTAssertEqual(
            build.confidenceAdjustedEstimatedTokensLabel,
            "Estimated 10,200 tokens (confidence-adjusted)"
        )
        XCTAssertEqual(build.realizedTokensLabel, "Realized 0 tokens")
        XCTAssertEqual(build.highestInterventionLabel, "Highest intervention: cancel")
        XCTAssertTrue(build.wasteBreakdownLabel.contains("duplicate task"))

        let episode = efficiency.episodes[0].viewModel
        XCTAssertTrue(episode.confidenceBasisLabel.hasPrefix("Estimated "))
        XCTAssertTrue(episode.confidenceBasisLabel.contains("confidence 85%"))
        XCTAssertEqual(episode.realizedLabel, "Realized unavailable")
        XCTAssertFalse(episode.confidenceBasisLabel.contains("Realized"))
    }

    func testEstimatedAndRealizedLabelsStaySeparated() throws {
        let data = """
        {
          "efficiency": {
            "schema": "fractal.efficiency.v1",
            "mode": "observe",
            "aggregation_version": 1,
            "config_hash": "cfg",
            "lifetime": {
              "gross_estimated_tokens_avoided": 200000,
              "confidence_adjusted_tokens_avoided": 150000,
              "realized_tokens_saved": 40000,
              "waste_breakdown": { "spec_drift": 2 },
              "highest_intervention": "stop_downstream"
            }
          }
        }
        """.data(using: .utf8)!
        let efficiency = try XCTUnwrap(ProjectGraphEfficiency.decode(from: data))

        XCTAssertEqual(efficiency.lifetimeEstimatedLabel, "Estimated 200,000 tokens")
        XCTAssertEqual(
            efficiency.lifetimeConfidenceAdjustedEstimatedLabel,
            "Estimated 150,000 tokens (confidence-adjusted)"
        )
        XCTAssertEqual(efficiency.lifetimeRealizedLabel, "Realized 40,000 tokens")

        XCTAssertFalse(efficiency.lifetimeEstimatedLabel.contains("Realized"))
        XCTAssertFalse(efficiency.lifetimeRealizedLabel.contains("Estimated"))
        XCTAssertNotEqual(efficiency.lifetimeEstimatedLabel, efficiency.lifetimeRealizedLabel)

        let summary = efficiency.lifetimeViewModel.compactSummary
        XCTAssertTrue(summary.contains("Estimated 200,000 tokens"))
        XCTAssertTrue(summary.contains("Estimated 150,000 tokens (confidence-adjusted)"))
        XCTAssertTrue(summary.contains("Realized 40,000 tokens"))
        XCTAssertEqual(
            efficiency.lifetimeViewModel.highestInterventionLabel,
            "Highest intervention: stop downstream"
        )
    }

    func testMalformedEfficiencyReturnsNilWithoutThrowing() {
        let truncated = #"{"efficiency":{"schema":"#.data(using: .utf8)!
        XCTAssertNil(ProjectGraphEfficiency.decode(from: truncated))

        let wrongTypes = """
        {"efficiency":{"schema":"fractal.efficiency.v1","aggregation_version":"one","episodes":{}}}
        """.data(using: .utf8)!
        XCTAssertNil(ProjectGraphEfficiency.decode(from: wrongTypes))

        // Nested aggregate type errors soft-recover to empty totals rather than crashing.
        let badBuild = #"{"efficiency":{"schema":"fractal.efficiency.v1","build":"not-an-object"}}"#
            .data(using: .utf8)!
        let recovered = ProjectGraphEfficiency.decode(from: badBuild)
        XCTAssertEqual(recovered?.build.grossEstimatedTokensAvoided, 0)
        XCTAssertEqual(recovered?.buildViewModel.realizedTokensLabel, "Realized 0 tokens")
        XCTAssertFalse(recovered?.buildViewModel.grossEstimatedTokensLabel.contains("Realized") ?? true)
    }

    func testEfficiencyDecodeDoesNotInterfereWithLearningEnvelope() throws {
        let data = """
        {
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
                "human_intervention": false
              }
            },
            "graph_edits": []
          },
          "efficiency": {
            "schema": "fractal.efficiency.v1",
            "mode": "auto_optimize",
            "aggregation_version": 1,
            "config_hash": "cfg",
            "build": {
              "gross_estimated_tokens_avoided": 100,
              "realized_tokens_saved": 0
            },
            "lifetime": {
              "gross_estimated_tokens_avoided": 100,
              "realized_tokens_saved": 0
            }
          }
        }
        """.data(using: .utf8)!

        let learning = try XCTUnwrap(
            JSONDecoder().decode(ProjectGraphLearning.Envelope.self, from: data).learning
        )
        XCTAssertEqual(learning.schema, "fractal.learning.v1")
        XCTAssertEqual(learning.nodes["n1"]?.objective, "establish contract")

        let efficiency = try XCTUnwrap(ProjectGraphEfficiency.decode(from: data))
        XCTAssertEqual(efficiency.mode, .autoOptimize)
        XCTAssertEqual(efficiency.build.grossEstimatedTokensAvoided, 100)
        XCTAssertEqual(efficiency.buildViewModel.realizedTokensLabel, "Realized 0 tokens")
    }

    func testWasteRepairAndModeLabelsMatchContract() {
        XCTAssertEqual(
            ProjectGraphEfficiency.WasteType.allCases.map(\.rawValue),
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
        )
        XCTAssertEqual(
            ProjectGraphEfficiency.RepairAction.allCases.map(\.rawValue),
            [
                "merge",
                "cancel",
                "delay_verification",
                "stop_downstream",
                "reassign",
                "consolidate_verifiers",
                "split_drift",
            ]
        )
        XCTAssertEqual(
            ProjectGraphEfficiency.Mode.allCases.map(\.rawValue),
            ["observe", "suggest", "auto_optimize"]
        )
        XCTAssertNil(ProjectGraphEfficiency.WasteType(rawValue: "duplicate_work"))
        XCTAssertNil(ProjectGraphEfficiency.RepairAction(rawValue: "rewrite"))
        XCTAssertNil(ProjectGraphEfficiency.Mode(rawValue: "autonomous"))
    }

    func testDefaultEfficiencyControlsEmitNoCLIArguments() {
        let args = BuildCoordinator.efficiencyCLIArguments(.default)
        XCTAssertTrue(args.isEmpty)
    }

    func testEfficiencyCLIArgumentsForwardModeApprovalsAndAutonomySafely() {
        let controls = EfficiencyControls(
            mode: .autoOptimize,
            approved: [.merge, .delayVerification],
            overridden: [.splitDrift],
            highImpactAutonomy: [.cancel, .stopDownstream]
        )
        let args = BuildCoordinator.efficiencyCLIArguments(controls)
        XCTAssertEqual(
            args,
            [
                "--efficiency-mode", "auto-optimize",
                "--approve-intervention", "delay_verification",
                "--approve-intervention", "merge",
                "--override-intervention", "split_drift",
                "--allow-high-impact", "cancel",
                "--allow-high-impact", "stop_downstream",
            ]
        )
    }

    func testObserveModeDropsApprovalsAndAutonomyFromCLIArguments() {
        let controls = EfficiencyControls(
            mode: .observe,
            approved: [.merge],
            overridden: [.cancel],
            highImpactAutonomy: [.reassign]
        )
        XCTAssertEqual(
            BuildCoordinator.efficiencyCLIArguments(controls),
            ["--efficiency-mode", "observe"]
        )
    }

    func testSuggestModeDropsHighImpactAutonomyFromCLIArguments() {
        let controls = EfficiencyControls(
            mode: .suggest,
            approved: [.merge],
            overridden: [],
            highImpactAutonomy: [.cancel]
        )
        XCTAssertEqual(
            BuildCoordinator.efficiencyCLIArguments(controls),
            [
                "--efficiency-mode", "suggest",
                "--approve-intervention", "merge",
            ]
        )
    }

    func testLifetimeLabelsNeverClaimEstimateIsRealized() throws {
        let data = """
        {
          "efficiency": {
            "schema": "fractal.efficiency.v1",
            "mode": "suggest",
            "lifetime": {
              "gross_estimated_tokens_avoided": 200000,
              "realized_tokens_saved": 40000
            }
          }
        }
        """.data(using: .utf8)!
        let efficiency = try XCTUnwrap(ProjectGraphEfficiency.decode(from: data))
        XCTAssertTrue(efficiency.lifetimeEstimatedLabel.hasPrefix("Estimated "))
        XCTAssertTrue(efficiency.lifetimeRealizedLabel.hasPrefix("Realized "))
        XCTAssertFalse(efficiency.lifetimeEstimatedLabel.contains("Realized"))
        XCTAssertFalse(efficiency.lifetimeRealizedLabel.contains("Estimated"))
        XCTAssertFalse(EfficiencyControls.lifetimeEstimatedHelp.lowercased().contains("proven savings that already happened"))
        XCTAssertTrue(EfficiencyControls.lifetimeRealizedHelp.contains("never counted as realized"))
    }
}
