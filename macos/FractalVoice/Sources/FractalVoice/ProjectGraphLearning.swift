import Foundation

/// Tolerant projection of the additive learning section in `project.fractal`.
///
/// The Fractal app reads these projections for display only. Every field is
/// optional or safely defaulted so older `fractal.project.v1` files, partially
/// written enrichment payloads, and future controlled labels remain readable.
/// Controlled strings are intentionally retained as raw strings instead of
/// throwing on unknown values.
struct ProjectGraphLearning: Codable, Equatable {
    static let schemaID = "fractal.learning.v1"
    static let projectSchemaID = "fractal.project.v1"

    struct Envelope: Codable, Equatable {
        let schema: String?
        let learning: ProjectGraphLearning?

        enum CodingKeys: String, CodingKey {
            case schema, learning
        }

        init(schema: String? = nil, learning: ProjectGraphLearning? = nil) {
            self.schema = schema
            self.learning = learning
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            schema = try values.decodeIfPresent(String.self, forKey: .schema)
            learning = try? values.decodeIfPresent(ProjectGraphLearning.self, forKey: .learning) ?? nil
        }
    }

    /// Forward-compatible controlled label. Known labels receive nicer titles;
    /// unknown labels keep their exact raw value for display and re-encoding.
    struct ControlledString: Codable, Equatable, Hashable, ExpressibleByStringLiteral {
        let rawValue: String

        init(_ rawValue: String) {
            self.rawValue = rawValue
        }

        init(stringLiteral value: String) {
            self.rawValue = value
        }

        init(from decoder: Decoder) throws {
            let container = try decoder.singleValueContainer()
            rawValue = (try? container.decode(String.self)) ?? ""
        }

        func encode(to encoder: Encoder) throws {
            var container = encoder.singleValueContainer()
            try container.encode(rawValue)
        }

        var displayName: String {
            Self.displayName(for: rawValue)
        }

        static func displayName(for rawValue: String?) -> String {
            guard let rawValue, !rawValue.isEmpty else { return "unknown" }
            return rawValue.replacingOccurrences(of: "_", with: " ")
        }
    }

    enum OutcomeKind: String, Codable, Equatable, CaseIterable {
        case success
        case verifiedSuccess = "verified_success"
        case partialSuccess = "partial_success"
        case failed
        case blocked
        case skipped
        case cancelled
        case superseded
        case reopened
        case inProgress = "in_progress"
        case notStarted = "not_started"

        var displayName: String { rawValue.replacingOccurrences(of: "_", with: " ") }
    }

    enum FailureCode: String, Codable, Equatable, CaseIterable {
        case dependencyFailed = "dependency_failed"
        case verificationFailed = "verification_failed"
        case testFailed = "test_failed"
        case buildFailed = "build_failed"
        case timeout
        case cancelled
        case conflict
        case missingEvidence = "missing_evidence"
        case humanRejected = "human_rejected"
        case toolUnavailable = "tool_unavailable"
        case unknown

        var displayName: String { rawValue.replacingOccurrences(of: "_", with: " ") }
    }

    enum VerificationStatus: String, Codable, Equatable, CaseIterable {
        case passed
        case failed
        case skipped
        case pending
        case unknown

        var displayName: String { rawValue.replacingOccurrences(of: "_", with: " ") }
    }

    struct ArtifactReference: Codable, Equatable, Hashable {
        let ref: String
        let kind: String?
        let path: String?
        let url: String?
        let title: String?
        let sha256: String?

        enum CodingKeys: String, CodingKey {
            case ref, kind, path, url, title, sha256
        }

        init(
            ref: String = "",
            kind: String? = nil,
            path: String? = nil,
            url: String? = nil,
            title: String? = nil,
            sha256: String? = nil
        ) {
            self.ref = ref
            self.kind = kind
            self.path = path
            self.url = url
            self.title = title
            self.sha256 = sha256
        }

        init(from decoder: Decoder) throws {
            if let raw = try? decoder.singleValueContainer().decode(String.self) {
                self.init(ref: raw)
                return
            }
            let values = try decoder.container(keyedBy: CodingKeys.self)
            let decodedRef = try values.decodeIfPresent(String.self, forKey: .ref)
            let decodedPath = try values.decodeIfPresent(String.self, forKey: .path)
            let decodedURL = try values.decodeIfPresent(String.self, forKey: .url)
            self.init(
                ref: decodedRef ?? decodedPath ?? decodedURL ?? "",
                kind: try values.decodeIfPresent(String.self, forKey: .kind),
                path: decodedPath,
                url: decodedURL,
                title: try values.decodeIfPresent(String.self, forKey: .title),
                sha256: try values.decodeIfPresent(String.self, forKey: .sha256)
            )
        }

        var displayName: String {
            if let title, !title.isEmpty { return title }
            if !ref.isEmpty { return ref }
            if let path, !path.isEmpty { return path }
            if let url, !url.isEmpty { return url }
            return "artifact"
        }
    }

    struct HumanIntervention: Codable, Equatable {
        let required: Bool
        let requestedBy: String?
        let actor: String?
        let reason: String?
        let outcome: String?
        let occurredAt: String?

        enum CodingKeys: String, CodingKey {
            case required
            case requestedBy = "requested_by"
            case actor, reason, outcome
            case occurredAt = "occurred_at"
        }

        init(
            required: Bool = false,
            requestedBy: String? = nil,
            actor: String? = nil,
            reason: String? = nil,
            outcome: String? = nil,
            occurredAt: String? = nil
        ) {
            self.required = required
            self.requestedBy = requestedBy
            self.actor = actor
            self.reason = reason
            self.outcome = outcome
            self.occurredAt = occurredAt
        }

        init(from decoder: Decoder) throws {
            if let bool = try? decoder.singleValueContainer().decode(Bool.self) {
                self.init(required: bool)
                return
            }
            let values = try decoder.container(keyedBy: CodingKeys.self)
            self.init(
                required: try values.decodeIfPresent(Bool.self, forKey: .required) ?? false,
                requestedBy: try values.decodeIfPresent(String.self, forKey: .requestedBy),
                actor: try values.decodeIfPresent(String.self, forKey: .actor),
                reason: try values.decodeIfPresent(String.self, forKey: .reason),
                outcome: try values.decodeIfPresent(String.self, forKey: .outcome),
                occurredAt: try values.decodeIfPresent(String.self, forKey: .occurredAt)
            )
        }
    }

    struct Node: Codable, Equatable {
        struct Executor: Codable, Equatable {
            let agent: String?
            let model: String?
            let version: String?
            let label: String?
            let startedAt: String?
            let completedAt: String?

            enum CodingKeys: String, CodingKey {
                case agent, model, version, label
                case startedAt = "started_at"
                case completedAt = "completed_at"
            }

            init(
                agent: String? = nil,
                model: String? = nil,
                version: String? = nil,
                label: String? = nil,
                startedAt: String? = nil,
                completedAt: String? = nil
            ) {
                self.agent = agent
                self.model = model
                self.version = version
                self.label = label
                self.startedAt = startedAt
                self.completedAt = completedAt
            }
        }

        struct Verification: Codable, Equatable {
            let type: String?
            let status: String?
            let passed: Bool?
            let command: String?
            let summary: String?
            let evidenceRefs: [String]
            let artifacts: [ArtifactReference]
            let verifiedAt: String?

            enum CodingKeys: String, CodingKey {
                case type, status, passed, command, summary, artifacts
                case evidenceRefs = "evidence_refs"
                case verifiedAt = "verified_at"
            }

            init(
                type: String? = nil,
                status: String? = nil,
                passed: Bool? = nil,
                command: String? = nil,
                summary: String? = nil,
                evidenceRefs: [String] = [],
                artifacts: [ArtifactReference] = [],
                verifiedAt: String? = nil
            ) {
                self.type = type
                self.status = status
                self.passed = passed
                self.command = command
                self.summary = summary
                self.evidenceRefs = evidenceRefs
                self.artifacts = artifacts
                self.verifiedAt = verifiedAt
            }

            init(from decoder: Decoder) throws {
                let values = try decoder.container(keyedBy: CodingKeys.self)
                type = try values.decodeIfPresent(String.self, forKey: .type)
                status = try values.decodeIfPresent(String.self, forKey: .status)
                passed = try values.decodeIfPresent(Bool.self, forKey: .passed)
                command = try values.decodeIfPresent(String.self, forKey: .command)
                summary = try values.decodeIfPresent(String.self, forKey: .summary)
                evidenceRefs = Self.decodeStringRefs(values, forKey: .evidenceRefs)
                artifacts = Self.decodeArtifacts(values, forKey: .artifacts)
                verifiedAt = try values.decodeIfPresent(String.self, forKey: .verifiedAt)
            }

            var compactSummary: String {
                ProjectGraphLearning.formatVerificationText(self)
            }

            private static func decodeStringRefs<K: CodingKey>(
                _ values: KeyedDecodingContainer<K>,
                forKey key: K
            ) -> [String] {
                if let strings = try? values.decodeIfPresent([String].self, forKey: key) {
                    return strings
                }
                if let refs = try? values.decodeIfPresent([ArtifactReference].self, forKey: key) {
                    return refs.map(\.displayName)
                }
                return []
            }

            private static func decodeArtifacts<K: CodingKey>(
                _ values: KeyedDecodingContainer<K>,
                forKey key: K
            ) -> [ArtifactReference] {
                (try? values.decodeIfPresent([ArtifactReference].self, forKey: key)) ?? []
            }
        }

        let nodeID: String
        let nodeType: String
        let objective: String
        let title: String?
        let dependsOn: [String]
        let executor: Executor?
        let attemptCount: Int
        /// Raw controlled outcome string. Unknown future values are preserved.
        let outcome: String?
        /// Raw controlled failure code. Unknown future values are preserved.
        let failureCode: String?
        let verification: Verification?
        /// Legacy string projection retained for existing views and tests.
        let artifactsProduced: [String]
        let artifactRefs: [ArtifactReference]
        let consumedBy: [String]
        let humanIntervention: Bool
        let intervention: HumanIntervention?
        let startedAt: String?
        let completedAt: String?

        enum CodingKeys: String, CodingKey {
            case nodeID = "node_id"
            case nodeType = "node_type"
            case objective, title
            case dependsOn = "depends_on"
            case executor
            case attemptCount = "attempt_count"
            case outcome
            case failureCode = "failure_code"
            case verification
            case artifactsProduced = "artifacts_produced"
            case artifactRefs = "artifact_refs"
            case consumedBy = "consumed_by"
            case humanIntervention = "human_intervention"
            case intervention
            case startedAt = "started_at"
            case completedAt = "completed_at"
        }

        init(
            nodeID: String = "",
            nodeType: String = "task",
            objective: String = "",
            title: String? = nil,
            dependsOn: [String] = [],
            executor: Executor? = nil,
            attemptCount: Int = 0,
            outcome: String? = nil,
            failureCode: String? = nil,
            verification: Verification? = nil,
            artifactsProduced: [String] = [],
            artifactRefs: [ArtifactReference] = [],
            consumedBy: [String] = [],
            humanIntervention: Bool = false,
            intervention: HumanIntervention? = nil,
            startedAt: String? = nil,
            completedAt: String? = nil
        ) {
            self.nodeID = nodeID
            self.nodeType = nodeType
            self.objective = objective.isEmpty ? nodeID : objective
            self.title = title
            self.dependsOn = dependsOn
            self.executor = executor
            self.attemptCount = max(0, attemptCount)
            self.outcome = outcome
            self.failureCode = failureCode
            self.verification = verification
            self.artifactsProduced = artifactsProduced
            self.artifactRefs = artifactRefs
            self.consumedBy = consumedBy
            self.humanIntervention = humanIntervention
            self.intervention = intervention
            self.startedAt = startedAt
            self.completedAt = completedAt
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            nodeID = try values.decodeIfPresent(String.self, forKey: .nodeID) ?? ""
            nodeType = try values.decodeIfPresent(String.self, forKey: .nodeType) ?? "task"
            let decodedObjective = try values.decodeIfPresent(String.self, forKey: .objective)
            title = try values.decodeIfPresent(String.self, forKey: .title)
            objective = decodedObjective ?? title ?? nodeID
            dependsOn = try values.decodeIfPresent([String].self, forKey: .dependsOn) ?? []
            executor = try? values.decodeIfPresent(Executor.self, forKey: .executor) ?? nil
            attemptCount = max(0, try values.decodeIfPresent(Int.self, forKey: .attemptCount) ?? 0)
            outcome = try values.decodeIfPresent(String.self, forKey: .outcome)
            failureCode = try values.decodeIfPresent(String.self, forKey: .failureCode)
            verification = try? values.decodeIfPresent(Verification.self, forKey: .verification) ?? nil
            let refs = Self.decodeArtifacts(values, forKey: .artifactRefs)
            let producedRefs = Self.decodeArtifacts(values, forKey: .artifactsProduced)
            artifactRefs = refs + producedRefs
            artifactsProduced = Self.decodeStringRefs(values, forKey: .artifactsProduced)
            consumedBy = try values.decodeIfPresent([String].self, forKey: .consumedBy) ?? []
            intervention = try? values.decodeIfPresent(HumanIntervention.self, forKey: .intervention) ?? nil
            let legacyHuman = try values.decodeIfPresent(Bool.self, forKey: .humanIntervention) ?? false
            humanIntervention = legacyHuman || (intervention?.required ?? false)
            startedAt = try values.decodeIfPresent(String.self, forKey: .startedAt)
            completedAt = try values.decodeIfPresent(String.self, forKey: .completedAt)
        }

        var knownOutcome: OutcomeKind? {
            outcome.flatMap(OutcomeKind.init(rawValue:))
        }

        var knownFailureCode: FailureCode? {
            failureCode.flatMap(FailureCode.init(rawValue:))
        }

        var outcomeText: String {
            ProjectGraphLearning.formatNodeOutcomeText(self)
        }

        var attemptText: String {
            ProjectGraphLearning.formatAttemptText(attemptCount)
        }

        var verificationText: String {
            verification.map(ProjectGraphLearning.formatVerificationText) ?? "not verified"
        }

        var compactSummary: String {
            ProjectGraphLearning.formatNodeSummary(self)
        }

        private static func decodeStringRefs<K: CodingKey>(
            _ values: KeyedDecodingContainer<K>,
            forKey key: K
        ) -> [String] {
            if let strings = try? values.decodeIfPresent([String].self, forKey: key) {
                return strings
            }
            if let refs = try? values.decodeIfPresent([ArtifactReference].self, forKey: key) {
                return refs.map(\.displayName)
            }
            return []
        }

        private static func decodeArtifacts<K: CodingKey>(
            _ values: KeyedDecodingContainer<K>,
            forKey key: K
        ) -> [ArtifactReference] {
            if let refs = try? values.decodeIfPresent([ArtifactReference].self, forKey: key) {
                return refs
            }
            if let strings = try? values.decodeIfPresent([String].self, forKey: key) {
                return strings.map { ArtifactReference(ref: $0) }
            }
            return []
        }
    }

    struct GraphEditEvent: Codable, Equatable {
        let eventID: String
        let kind: String
        let actor: String?
        let reason: String?
        let affectedNodeIDs: [String]
        let createdNodeIDs: [String]
        let removedNodeIDs: [String]
        let occurredAt: String?
        let artifactRefs: [ArtifactReference]

        enum CodingKeys: String, CodingKey {
            case eventID = "event_id"
            case kind, actor, reason
            case affectedNodeIDs = "affected_node_ids"
            case createdNodeIDs = "created_node_ids"
            case removedNodeIDs = "removed_node_ids"
            case occurredAt = "occurred_at"
            case artifactRefs = "artifact_refs"
        }

        init(
            eventID: String = "",
            kind: String = "unknown",
            actor: String? = nil,
            reason: String? = nil,
            affectedNodeIDs: [String] = [],
            createdNodeIDs: [String] = [],
            removedNodeIDs: [String] = [],
            occurredAt: String? = nil,
            artifactRefs: [ArtifactReference] = []
        ) {
            self.eventID = eventID
            self.kind = kind
            self.actor = actor
            self.reason = reason
            self.affectedNodeIDs = affectedNodeIDs
            self.createdNodeIDs = createdNodeIDs
            self.removedNodeIDs = removedNodeIDs
            self.occurredAt = occurredAt
            self.artifactRefs = artifactRefs
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            eventID = try values.decodeIfPresent(String.self, forKey: .eventID) ?? ""
            kind = try values.decodeIfPresent(String.self, forKey: .kind) ?? "unknown"
            actor = try values.decodeIfPresent(String.self, forKey: .actor)
            reason = try values.decodeIfPresent(String.self, forKey: .reason)
            affectedNodeIDs = try values.decodeIfPresent([String].self, forKey: .affectedNodeIDs) ?? []
            createdNodeIDs = try values.decodeIfPresent([String].self, forKey: .createdNodeIDs) ?? []
            removedNodeIDs = try values.decodeIfPresent([String].self, forKey: .removedNodeIDs) ?? []
            occurredAt = try values.decodeIfPresent(String.self, forKey: .occurredAt)
            artifactRefs = (try? values.decodeIfPresent([ArtifactReference].self, forKey: .artifactRefs)) ?? []
        }

        var compactSummary: String {
            let label = ControlledString.displayName(for: kind)
            let count = affectedNodeIDs.count + createdNodeIDs.count + removedNodeIDs.count
            return count > 0 ? "\(label) · \(count) node\(count == 1 ? "" : "s")" : label
        }
    }

    struct Outcome: Codable, Equatable {
        let finalVerifiedSuccess: Bool?
        let totalCost: Double
        let retryCount: Int
        let reopenedNodeCount: Int
        let humanInterventionCount: Int
        let verificationCoverage: Double
        let completedNodeCount: Int
        let failedNodeCount: Int
        let blockedNodeCount: Int
        let artifactCount: Int
        let summary: String?
        let failureCode: String?

        enum CodingKeys: String, CodingKey {
            case finalVerifiedSuccess = "final_verified_success"
            case totalCost = "total_cost"
            case retryCount = "retry_count"
            case reopenedNodeCount = "reopened_node_count"
            case humanInterventionCount = "human_intervention_count"
            case verificationCoverage = "verification_coverage"
            case completedNodeCount = "completed_node_count"
            case failedNodeCount = "failed_node_count"
            case blockedNodeCount = "blocked_node_count"
            case artifactCount = "artifact_count"
            case summary
            case failureCode = "failure_code"
        }

        init(
            finalVerifiedSuccess: Bool? = nil,
            totalCost: Double = 0,
            retryCount: Int = 0,
            reopenedNodeCount: Int = 0,
            humanInterventionCount: Int = 0,
            verificationCoverage: Double = 0,
            completedNodeCount: Int = 0,
            failedNodeCount: Int = 0,
            blockedNodeCount: Int = 0,
            artifactCount: Int = 0,
            summary: String? = nil,
            failureCode: String? = nil
        ) {
            self.finalVerifiedSuccess = finalVerifiedSuccess
            self.totalCost = totalCost
            self.retryCount = max(0, retryCount)
            self.reopenedNodeCount = max(0, reopenedNodeCount)
            self.humanInterventionCount = max(0, humanInterventionCount)
            self.verificationCoverage = min(max(verificationCoverage.isFinite ? verificationCoverage : 0, 0), 1)
            self.completedNodeCount = max(0, completedNodeCount)
            self.failedNodeCount = max(0, failedNodeCount)
            self.blockedNodeCount = max(0, blockedNodeCount)
            self.artifactCount = max(0, artifactCount)
            self.summary = summary
            self.failureCode = failureCode
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            finalVerifiedSuccess = try values.decodeIfPresent(Bool.self, forKey: .finalVerifiedSuccess)
            totalCost = try values.decodeIfPresent(Double.self, forKey: .totalCost) ?? 0
            retryCount = max(0, try values.decodeIfPresent(Int.self, forKey: .retryCount) ?? 0)
            reopenedNodeCount = max(0, try values.decodeIfPresent(Int.self, forKey: .reopenedNodeCount) ?? 0)
            humanInterventionCount = max(0, try values.decodeIfPresent(Int.self, forKey: .humanInterventionCount) ?? 0)
            let coverage = try values.decodeIfPresent(Double.self, forKey: .verificationCoverage) ?? 0
            verificationCoverage = min(max(coverage.isFinite ? coverage : 0, 0), 1)
            completedNodeCount = max(0, try values.decodeIfPresent(Int.self, forKey: .completedNodeCount) ?? 0)
            failedNodeCount = max(0, try values.decodeIfPresent(Int.self, forKey: .failedNodeCount) ?? 0)
            blockedNodeCount = max(0, try values.decodeIfPresent(Int.self, forKey: .blockedNodeCount) ?? 0)
            artifactCount = max(0, try values.decodeIfPresent(Int.self, forKey: .artifactCount) ?? 0)
            summary = try values.decodeIfPresent(String.self, forKey: .summary)
            failureCode = try values.decodeIfPresent(String.self, forKey: .failureCode)
        }

        var compactSummary: String {
            ProjectGraphLearning.formatGraphCompletionSummary(self)
        }
    }

    let schema: String
    let nodes: [String: Node]
    let graphEdits: [GraphEditEvent]
    let outcome: Outcome?
    let artifactRefs: [ArtifactReference]

    private enum CodingKeys: String, CodingKey {
        case schema, nodes, outcome
        case graphEdits = "graph_edits"
        case artifactRefs = "artifact_refs"
    }

    init(
        schema: String = ProjectGraphLearning.schemaID,
        nodes: [String: Node] = [:],
        graphEdits: [GraphEditEvent] = [],
        outcome: Outcome? = nil,
        artifactRefs: [ArtifactReference] = []
    ) {
        self.schema = schema
        self.nodes = nodes
        self.graphEdits = graphEdits
        self.outcome = outcome
        self.artifactRefs = artifactRefs
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        schema = try values.decodeIfPresent(String.self, forKey: .schema) ?? Self.schemaID
        nodes = Self.decodeNodes(values, forKey: .nodes)
        graphEdits = (try? values.decodeIfPresent([GraphEditEvent].self, forKey: .graphEdits)) ?? []
        outcome = try? values.decodeIfPresent(Outcome.self, forKey: .outcome) ?? nil
        artifactRefs = (try? values.decodeIfPresent([ArtifactReference].self, forKey: .artifactRefs)) ?? []
    }

    var compactCompletionSummary: String {
        ProjectGraphLearning.formatCompletionSummary(for: self)
    }

    static func load(from projectURL: URL) -> ProjectGraphLearning? {
        guard let data = try? Data(contentsOf: projectURL) else { return nil }
        return decode(from: data)
    }

    static func decode(from data: Data) -> ProjectGraphLearning? {
        guard let envelope = try? JSONDecoder().decode(Envelope.self, from: data) else {
            return nil
        }
        return envelope.learning
    }

    static func formatNodeOutcomeText(_ node: Node) -> String {
        var base = ControlledString.displayName(for: node.outcome)
        if node.outcome == nil || node.outcome?.isEmpty == true {
            base = "not finished"
        }
        if let failure = node.failureCode, !failure.isEmpty {
            base += " (\(ControlledString.displayName(for: failure)))"
        }
        return base
    }

    static func formatAttemptText(_ attemptCount: Int) -> String {
        guard attemptCount > 0 else { return "no attempts" }
        return "\(attemptCount) attempt\(attemptCount == 1 ? "" : "s")"
    }

    static func formatVerificationText(_ verification: Node.Verification) -> String {
        if let summary = verification.summary, !summary.isEmpty {
            return summary
        }
        if verification.passed == true { return "verified" }
        if verification.passed == false { return "verification failed" }
        if let status = verification.status, !status.isEmpty {
            return ControlledString.displayName(for: status)
        }
        if let type = verification.type, !type.isEmpty {
            return "\(ControlledString.displayName(for: type)) pending"
        }
        return "not verified"
    }

    static func formatNodeSummary(_ node: Node) -> String {
        var parts = [formatNodeOutcomeText(node)]
        if node.attemptCount > 0 {
            parts.append(formatAttemptText(node.attemptCount))
        }
        if let verification = node.verification {
            let verificationText = formatVerificationText(verification)
            if verificationText != "not verified" {
                parts.append(verificationText)
            }
        }
        if node.humanIntervention {
            parts.append("human assisted")
        }
        return parts.joined(separator: " · ")
    }

    static func formatGraphCompletionSummary(_ outcome: Outcome) -> String {
        if let summary = outcome.summary, !summary.isEmpty {
            return summary
        }
        var parts: [String] = [
            outcome.finalVerifiedSuccess.map { $0 ? "verified success" : "not fully verified" } ?? "completed",
            "\(Int((outcome.verificationCoverage * 100).rounded()))% verified",
            "\(outcome.retryCount) retries",
        ]
        if outcome.reopenedNodeCount > 0 {
            parts.append("\(outcome.reopenedNodeCount) reopened")
        }
        if outcome.humanInterventionCount > 0 {
            parts.append("\(outcome.humanInterventionCount) human intervention\(outcome.humanInterventionCount == 1 ? "" : "s")")
        }
        if let failureCode = outcome.failureCode, !failureCode.isEmpty {
            parts.append(ControlledString.displayName(for: failureCode))
        }
        return parts.joined(separator: " · ")
    }

    static func formatCompletionSummary(for learning: ProjectGraphLearning) -> String {
        if let outcome = learning.outcome {
            return formatGraphCompletionSummary(outcome)
        }
        guard !learning.nodes.isEmpty else { return "no learning data" }
        let verified = learning.nodes.values.filter { $0.verification?.passed == true }.count
        let attempts = learning.nodes.values.reduce(0) { $0 + $1.attemptCount }
        return "\(verified)/\(learning.nodes.count) verified · \(attempts) attempt\(attempts == 1 ? "" : "s")"
    }

    private static func decodeNodes<K: CodingKey>(
        _ values: KeyedDecodingContainer<K>,
        forKey key: K
    ) -> [String: Node] {
        if let dictionary = try? values.decodeIfPresent([String: Node].self, forKey: key) {
            return dictionary
        }
        if let list = try? values.decodeIfPresent([Node].self, forKey: key) {
            return Dictionary(uniqueKeysWithValues: list.map { ($0.nodeID, $0) })
        }
        return [:]
    }
}
