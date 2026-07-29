import Foundation
import XCTest
@testable import FractalVoice

final class ProjectGraphLearningTests: XCTestCase {
    func testLegacyProjectWithoutLearningDataRemainsReadable() throws {
        let data = #"{"schema":"fractal.project.v1"}"#.data(using: .utf8)!
        let envelope = try JSONDecoder().decode(ProjectGraphLearning.Envelope.self, from: data)
        XCTAssertNil(envelope.learning)
    }

    func testEnrichedAndFutureLabelsDecodeWithoutCrashing() throws {
        let data = """
        {
          "learning": {
            "schema": "fractal.learning.v1",
            "nodes": {
              "n_7": {
                "node_id": "n_7",
                "node_type": "implementation",
                "objective": "Implement authentication endpoint",
                "depends_on": ["n_2"],
                "attempt_count": 2,
                "outcome": "future_success_kind",
                "verification": {
                  "type": "integration_test",
                  "passed": true,
                  "evidence_refs": ["artifact:test-result-17"]
                },
                "artifacts_produced": ["artifact:commit-42"],
                "consumed_by": ["n_9"],
                "human_intervention": false
              }
            },
            "graph_edits": [],
            "outcome": {
              "final_verified_success": true,
              "total_cost": 0.11,
              "retry_count": 1,
              "reopened_node_count": 0,
              "human_intervention_count": 0,
              "verification_coverage": 1.0
            }
          }
        }
        """.data(using: .utf8)!
        let learning = try XCTUnwrap(
            JSONDecoder().decode(ProjectGraphLearning.Envelope.self, from: data).learning
        )
        XCTAssertEqual(learning.nodes["n_7"]?.outcome, "future_success_kind")
        XCTAssertEqual(
            learning.nodes["n_7"]?.compactSummary,
            "future success kind · 2 attempts · verified"
        )
        XCTAssertEqual(learning.outcome?.compactSummary, "verified success · 100% verified · 1 retries")
    }
}
