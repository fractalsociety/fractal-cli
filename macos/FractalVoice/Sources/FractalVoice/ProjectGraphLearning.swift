import Foundation

/// Tolerant projection of the additive learning section in project.fractal.
/// Raw strings intentionally preserve future controlled labels without making
/// an older app treat a valid newer graph as corrupt.
struct ProjectGraphLearning: Decodable, Equatable {
    struct Envelope: Decodable {
        let learning: ProjectGraphLearning?
    }

    struct Node: Decodable, Equatable {
        struct Executor: Decodable, Equatable {
            let agent: String?
            let model: String?
            let version: String?
        }

        struct Verification: Decodable, Equatable {
            let type: String?
            let passed: Bool?
            let evidenceRefs: [String]

            enum CodingKeys: String, CodingKey {
                case type, passed
                case evidenceRefs = "evidence_refs"
            }

            init(from decoder: Decoder) throws {
                let values = try decoder.container(keyedBy: CodingKeys.self)
                type = try values.decodeIfPresent(String.self, forKey: .type)
                passed = try values.decodeIfPresent(Bool.self, forKey: .passed)
                evidenceRefs = try values.decodeIfPresent([String].self, forKey: .evidenceRefs) ?? []
            }
        }

        let nodeID: String
        let nodeType: String
        let objective: String
        let dependsOn: [String]
        let executor: Executor?
        let attemptCount: Int
        let outcome: String?
        let failureCode: String?
        let verification: Verification?
        let artifactsProduced: [String]
        let consumedBy: [String]
        let humanIntervention: Bool

        enum CodingKeys: String, CodingKey {
            case nodeID = "node_id"
            case nodeType = "node_type"
            case objective
            case dependsOn = "depends_on"
            case executor
            case attemptCount = "attempt_count"
            case outcome
            case failureCode = "failure_code"
            case verification
            case artifactsProduced = "artifacts_produced"
            case consumedBy = "consumed_by"
            case humanIntervention = "human_intervention"
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            nodeID = try values.decodeIfPresent(String.self, forKey: .nodeID) ?? ""
            nodeType = try values.decodeIfPresent(String.self, forKey: .nodeType) ?? "task"
            objective = try values.decodeIfPresent(String.self, forKey: .objective) ?? nodeID
            dependsOn = try values.decodeIfPresent([String].self, forKey: .dependsOn) ?? []
            executor = try values.decodeIfPresent(Executor.self, forKey: .executor)
            attemptCount = try values.decodeIfPresent(Int.self, forKey: .attemptCount) ?? 0
            outcome = try values.decodeIfPresent(String.self, forKey: .outcome)
            failureCode = try values.decodeIfPresent(String.self, forKey: .failureCode)
            verification = try values.decodeIfPresent(Verification.self, forKey: .verification)
            artifactsProduced = try values.decodeIfPresent([String].self, forKey: .artifactsProduced) ?? []
            consumedBy = try values.decodeIfPresent([String].self, forKey: .consumedBy) ?? []
            humanIntervention = try values.decodeIfPresent(Bool.self, forKey: .humanIntervention) ?? false
        }

        var compactSummary: String {
            var parts = [outcome?.replacingOccurrences(of: "_", with: " ") ?? "not finished"]
            if attemptCount > 0 {
                parts.append("\(attemptCount) attempt\(attemptCount == 1 ? "" : "s")")
            }
            if verification?.passed == true {
                parts.append("verified")
            } else if verification?.passed == false {
                parts.append("verification failed")
            }
            if humanIntervention {
                parts.append("human assisted")
            }
            return parts.joined(separator: " · ")
        }
    }

    struct Outcome: Decodable, Equatable {
        let finalVerifiedSuccess: Bool?
        let totalCost: Double
        let retryCount: Int
        let reopenedNodeCount: Int
        let humanInterventionCount: Int
        let verificationCoverage: Double

        enum CodingKeys: String, CodingKey {
            case finalVerifiedSuccess = "final_verified_success"
            case totalCost = "total_cost"
            case retryCount = "retry_count"
            case reopenedNodeCount = "reopened_node_count"
            case humanInterventionCount = "human_intervention_count"
            case verificationCoverage = "verification_coverage"
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            finalVerifiedSuccess = try values.decodeIfPresent(Bool.self, forKey: .finalVerifiedSuccess)
            totalCost = try values.decodeIfPresent(Double.self, forKey: .totalCost) ?? 0
            retryCount = try values.decodeIfPresent(Int.self, forKey: .retryCount) ?? 0
            reopenedNodeCount = try values.decodeIfPresent(Int.self, forKey: .reopenedNodeCount) ?? 0
            humanInterventionCount = try values.decodeIfPresent(Int.self, forKey: .humanInterventionCount) ?? 0
            verificationCoverage = try values.decodeIfPresent(Double.self, forKey: .verificationCoverage) ?? 0
        }

        var compactSummary: String {
            let result = finalVerifiedSuccess.map { $0 ? "verified success" : "not fully verified" } ?? "completed"
            return "\(result) · \(Int(verificationCoverage * 100))% verified · \(retryCount) retries"
        }
    }

    let schema: String
    let nodes: [String: Node]
    let outcome: Outcome?

    private enum CodingKeys: String, CodingKey {
        case schema, nodes, outcome
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        schema = try values.decodeIfPresent(String.self, forKey: .schema) ?? "fractal.learning.v1"
        nodes = try values.decodeIfPresent([String: Node].self, forKey: .nodes) ?? [:]
        outcome = try values.decodeIfPresent(Outcome.self, forKey: .outcome)
    }

    static func load(from projectURL: URL) -> ProjectGraphLearning? {
        guard let data = try? Data(contentsOf: projectURL),
              let envelope = try? JSONDecoder().decode(Envelope.self, from: data)
        else { return nil }
        return envelope.learning
    }
}
