import Foundation

/// Tolerant projection of the optional `fractal.efficiency.v1` envelope.
/// Absent or malformed efficiency data decodes to `nil` and must never block
/// builds or alter `ProjectGraphLearning` behavior.
struct ProjectGraphEfficiency: Decodable, Equatable {
    static let schemaID = "fractal.efficiency.v1"
    static let aggregationVersion = 1
    static let maxBasisBytes = 480
    static let maxReferenceBytes = 240
    static let maxEvidenceRefs = 32
    static let maxAffectedNodes = 64

    struct Envelope: Decodable {
        let efficiency: ProjectGraphEfficiency?
    }

    enum Mode: String, Codable, Equatable, CaseIterable {
        case observe
        case suggest
        case autoOptimize = "auto_optimize"

        var displayName: String {
            switch self {
            case .observe: return "Observe"
            case .suggest: return "Suggest"
            case .autoOptimize: return "Auto-optimize"
            }
        }
    }

    enum WasteType: String, Codable, Equatable, CaseIterable {
        case duplicateTask = "duplicate_task"
        case duplicateTest = "duplicate_test"
        case duplicateResearch = "duplicate_research"
        case consolidatableTests = "consolidatable_tests"
        case unusedOutput = "unused_output"
        case supersededAssumption = "superseded_assumption"
        case specDrift = "spec_drift"
        case excessiveRetries = "excessive_retries"
        case overlappingFiles = "overlapping_files"
        case overDecomposition = "over_decomposition"
        case lowValueBranch = "low_value_branch"
        case prematureVerification = "premature_verification"
        case excessiveVerification = "excessive_verification"

        var displayName: String {
            rawValue.replacingOccurrences(of: "_", with: " ")
        }
    }

    enum RepairAction: String, Codable, Equatable, CaseIterable {
        case merge
        case cancel
        case delayVerification = "delay_verification"
        case stopDownstream = "stop_downstream"
        case reassign
        case consolidateVerifiers = "consolidate_verifiers"
        case splitDrift = "split_drift"

        var displayName: String {
            rawValue.replacingOccurrences(of: "_", with: " ")
        }
    }

    struct Aggregate: Decodable, Equatable {
        let episodeCount: Int
        let grossEstimatedTokensAvoided: UInt64
        let confidenceAdjustedTokensAvoided: UInt64
        let realizedTokensSaved: UInt64
        let estimatedCostAvoided: Double
        let realizedCostAvoided: Double
        let estimatedAgentHoursAvoided: Double
        let realizedAgentHoursAvoided: Double
        let reworkPrevented: Int
        let wasteBreakdown: [String: Int]
        let highestIntervention: RepairAction?
        let aggregationVersion: Int
        let configHash: String

        enum CodingKeys: String, CodingKey {
            case episodeCount = "episode_count"
            case grossEstimatedTokensAvoided = "gross_estimated_tokens_avoided"
            case confidenceAdjustedTokensAvoided = "confidence_adjusted_tokens_avoided"
            case realizedTokensSaved = "realized_tokens_saved"
            case estimatedCostAvoided = "estimated_cost_avoided"
            case realizedCostAvoided = "realized_cost_avoided"
            case estimatedAgentHoursAvoided = "estimated_agent_hours_avoided"
            case realizedAgentHoursAvoided = "realized_agent_hours_avoided"
            case reworkPrevented = "rework_prevented"
            case wasteBreakdown = "waste_breakdown"
            case highestIntervention = "highest_intervention"
            case aggregationVersion = "aggregation_version"
            case configHash = "config_hash"
        }

        init(
            episodeCount: Int = 0,
            grossEstimatedTokensAvoided: UInt64 = 0,
            confidenceAdjustedTokensAvoided: UInt64 = 0,
            realizedTokensSaved: UInt64 = 0,
            estimatedCostAvoided: Double = 0,
            realizedCostAvoided: Double = 0,
            estimatedAgentHoursAvoided: Double = 0,
            realizedAgentHoursAvoided: Double = 0,
            reworkPrevented: Int = 0,
            wasteBreakdown: [String: Int] = [:],
            highestIntervention: RepairAction? = nil,
            aggregationVersion: Int = ProjectGraphEfficiency.aggregationVersion,
            configHash: String = ""
        ) {
            self.episodeCount = episodeCount
            self.grossEstimatedTokensAvoided = grossEstimatedTokensAvoided
            self.confidenceAdjustedTokensAvoided = confidenceAdjustedTokensAvoided
            self.realizedTokensSaved = realizedTokensSaved
            self.estimatedCostAvoided = estimatedCostAvoided
            self.realizedCostAvoided = realizedCostAvoided
            self.estimatedAgentHoursAvoided = estimatedAgentHoursAvoided
            self.realizedAgentHoursAvoided = realizedAgentHoursAvoided
            self.reworkPrevented = reworkPrevented
            self.wasteBreakdown = wasteBreakdown
            self.highestIntervention = highestIntervention
            self.aggregationVersion = aggregationVersion
            self.configHash = configHash
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            episodeCount = try values.decodeIfPresent(Int.self, forKey: .episodeCount) ?? 0
            grossEstimatedTokensAvoided =
                try values.decodeIfPresent(UInt64.self, forKey: .grossEstimatedTokensAvoided) ?? 0
            confidenceAdjustedTokensAvoided =
                try values.decodeIfPresent(UInt64.self, forKey: .confidenceAdjustedTokensAvoided) ?? 0
            realizedTokensSaved =
                try values.decodeIfPresent(UInt64.self, forKey: .realizedTokensSaved) ?? 0
            estimatedCostAvoided =
                try values.decodeIfPresent(Double.self, forKey: .estimatedCostAvoided) ?? 0
            realizedCostAvoided =
                try values.decodeIfPresent(Double.self, forKey: .realizedCostAvoided) ?? 0
            estimatedAgentHoursAvoided =
                try values.decodeIfPresent(Double.self, forKey: .estimatedAgentHoursAvoided) ?? 0
            realizedAgentHoursAvoided =
                try values.decodeIfPresent(Double.self, forKey: .realizedAgentHoursAvoided) ?? 0
            reworkPrevented = try values.decodeIfPresent(Int.self, forKey: .reworkPrevented) ?? 0
            wasteBreakdown = try values.decodeIfPresent([String: Int].self, forKey: .wasteBreakdown) ?? [:]
            highestIntervention =
                try? values.decodeIfPresent(RepairAction.self, forKey: .highestIntervention) ?? nil
            aggregationVersion =
                try values.decodeIfPresent(Int.self, forKey: .aggregationVersion)
                ?? ProjectGraphEfficiency.aggregationVersion
            configHash = try values.decodeIfPresent(String.self, forKey: .configHash) ?? ""
        }

        /// View-model labels keep Estimated and Realized totals distinct.
        var viewModel: AggregateViewModel {
            AggregateViewModel(
                grossEstimatedTokensLabel:
                    "Estimated \(Self.formatTokens(grossEstimatedTokensAvoided)) tokens",
                confidenceAdjustedEstimatedTokensLabel:
                    "Estimated \(Self.formatTokens(confidenceAdjustedTokensAvoided)) tokens (confidence-adjusted)",
                realizedTokensLabel:
                    "Realized \(Self.formatTokens(realizedTokensSaved)) tokens",
                wasteBreakdownLabel: Self.formatBreakdown(wasteBreakdown),
                highestInterventionLabel: highestIntervention.map {
                    "Highest intervention: \($0.displayName)"
                } ?? "Highest intervention: none",
                compactSummary: [
                    "Estimated \(Self.formatTokens(grossEstimatedTokensAvoided)) tokens",
                    "Estimated \(Self.formatTokens(confidenceAdjustedTokensAvoided)) tokens (confidence-adjusted)",
                    "Realized \(Self.formatTokens(realizedTokensSaved)) tokens",
                ].joined(separator: " · ")
            )
        }

        private static func formatTokens(_ value: UInt64) -> String {
            let formatter = NumberFormatter()
            formatter.locale = Locale(identifier: "en_US_POSIX")
            formatter.numberStyle = .decimal
            formatter.usesGroupingSeparator = true
            formatter.groupingSeparator = ","
            formatter.maximumFractionDigits = 0
            return formatter.string(from: NSNumber(value: value)) ?? "\(value)"
        }

        private static func formatBreakdown(_ breakdown: [String: Int]) -> String {
            guard !breakdown.isEmpty else { return "Breakdown: none" }
            let parts = breakdown.keys.sorted().compactMap { key -> String? in
                guard let count = breakdown[key] else { return nil }
                let label = WasteType(rawValue: key)?.displayName
                    ?? key.replacingOccurrences(of: "_", with: " ")
                return "\(label) \(count)"
            }
            return "Breakdown: " + parts.joined(separator: ", ")
        }
    }

    struct AggregateViewModel: Equatable {
        let grossEstimatedTokensLabel: String
        let confidenceAdjustedEstimatedTokensLabel: String
        let realizedTokensLabel: String
        let wasteBreakdownLabel: String
        let highestInterventionLabel: String
        let compactSummary: String
    }

    struct Episode: Decodable, Equatable {
        let episodeID: String
        let wasteType: WasteType?
        let detectedNode: String
        let affectedNodeIDs: [String]
        let affectedCount: Int
        let proposedAction: RepairAction?
        let accepted: Bool
        let mode: Mode
        let estimatedTokensAvoided: UInt64
        let estimationBasis: String
        let confidence: Double
        let confidenceAdjustedTokensAvoided: UInt64
        let realizedTokensSaved: UInt64?
        let realizationBasis: String?
        let actualFollowupResult: String?
        let humanOverride: Bool
        let actor: String
        let detectedAt: String
        let resolvedAt: String?
        let evidenceRefs: [String]
        let aggregationVersion: Int
        let configHash: String

        enum CodingKeys: String, CodingKey {
            case episodeID = "episode_id"
            case wasteType = "waste_type"
            case detectedNode = "detected_node"
            case affectedNodeIDs = "affected_node_ids"
            case affectedCount = "affected_count"
            case proposedAction = "proposed_action"
            case accepted, mode
            case estimatedTokensAvoided = "estimated_tokens_avoided"
            case estimationBasis = "estimation_basis"
            case confidence
            case confidenceAdjustedTokensAvoided = "confidence_adjusted_tokens_avoided"
            case realizedTokensSaved = "realized_tokens_saved"
            case realizationBasis = "realization_basis"
            case actualFollowupResult = "actual_followup_result"
            case humanOverride = "human_override"
            case actor
            case detectedAt = "detected_at"
            case resolvedAt = "resolved_at"
            case evidenceRefs = "evidence_refs"
            case aggregationVersion = "aggregation_version"
            case configHash = "config_hash"
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            episodeID = try values.decodeIfPresent(String.self, forKey: .episodeID) ?? ""
            wasteType = try? values.decodeIfPresent(WasteType.self, forKey: .wasteType) ?? nil
            detectedNode = try values.decodeIfPresent(String.self, forKey: .detectedNode) ?? ""
            affectedNodeIDs = try values.decodeIfPresent([String].self, forKey: .affectedNodeIDs) ?? []
            affectedCount =
                try values.decodeIfPresent(Int.self, forKey: .affectedCount) ?? affectedNodeIDs.count
            proposedAction = try? values.decodeIfPresent(RepairAction.self, forKey: .proposedAction) ?? nil
            accepted = try values.decodeIfPresent(Bool.self, forKey: .accepted) ?? false
            mode = (try? values.decodeIfPresent(Mode.self, forKey: .mode)) ?? .suggest
            estimatedTokensAvoided =
                try values.decodeIfPresent(UInt64.self, forKey: .estimatedTokensAvoided) ?? 0
            estimationBasis = Self.boundedBasis(
                try values.decodeIfPresent(String.self, forKey: .estimationBasis) ?? ""
            )
            let rawConfidence = try values.decodeIfPresent(Double.self, forKey: .confidence) ?? 0
            confidence = min(max(rawConfidence.isFinite ? rawConfidence : 0, 0), 1)
            confidenceAdjustedTokensAvoided =
                try values.decodeIfPresent(UInt64.self, forKey: .confidenceAdjustedTokensAvoided) ?? 0
            realizedTokensSaved = try values.decodeIfPresent(UInt64.self, forKey: .realizedTokensSaved)
            realizationBasis =
                try values.decodeIfPresent(String.self, forKey: .realizationBasis).map(Self.boundedBasis)
            actualFollowupResult =
                try values.decodeIfPresent(String.self, forKey: .actualFollowupResult).map(Self.boundedBasis)
            humanOverride = try values.decodeIfPresent(Bool.self, forKey: .humanOverride) ?? false
            actor = try values.decodeIfPresent(String.self, forKey: .actor) ?? ""
            detectedAt = try values.decodeIfPresent(String.self, forKey: .detectedAt) ?? ""
            resolvedAt = try values.decodeIfPresent(String.self, forKey: .resolvedAt)
            let refs = try values.decodeIfPresent([String].self, forKey: .evidenceRefs) ?? []
            evidenceRefs = Array(refs.prefix(ProjectGraphEfficiency.maxEvidenceRefs))
            aggregationVersion =
                try values.decodeIfPresent(Int.self, forKey: .aggregationVersion)
                ?? ProjectGraphEfficiency.aggregationVersion
            configHash = try values.decodeIfPresent(String.self, forKey: .configHash) ?? ""
        }

        /// Confidence and basis stay paired; Estimated never reads as Realized.
        var viewModel: EpisodeViewModel {
            let waste = wasteType?.displayName ?? "unknown"
            let confidencePercent = Int((confidence * 100).rounded())
            let estimated =
                "Estimated \(estimatedTokensAvoided) tokens · confidence \(confidencePercent)% · \(estimationBasis)"
            let realized: String
            if let realizedTokensSaved {
                let basis = realizationBasis?.isEmpty == false
                    ? realizationBasis!
                    : "comparison evidence required"
                realized = "Realized \(realizedTokensSaved) tokens · \(basis)"
            } else {
                realized = "Realized unavailable"
            }
            return EpisodeViewModel(
                confidenceBasisLabel: estimated,
                realizedLabel: realized,
                wasteActionLabel: "\(waste) → \(proposedAction?.displayName ?? "none")",
                compactSummary: "\(estimated) · \(realized)"
            )
        }

        private static func boundedBasis(_ value: String) -> String {
            guard value.utf8.count > ProjectGraphEfficiency.maxBasisBytes else { return value }
            var truncated = String()
            truncated.reserveCapacity(ProjectGraphEfficiency.maxBasisBytes)
            for scalar in value.unicodeScalars {
                let next = truncated.utf8.count + String(scalar).utf8.count
                if next > ProjectGraphEfficiency.maxBasisBytes { break }
                truncated.unicodeScalars.append(scalar)
            }
            return truncated
        }
    }

    struct EpisodeViewModel: Equatable {
        let confidenceBasisLabel: String
        let realizedLabel: String
        let wasteActionLabel: String
        let compactSummary: String
    }

    let schema: String
    let mode: Mode
    let aggregationVersion: Int
    let configHash: String
    let episodes: [Episode]
    let build: Aggregate
    let lifetime: Aggregate

    private enum CodingKeys: String, CodingKey {
        case schema, mode
        case aggregationVersion = "aggregation_version"
        case configHash = "config_hash"
        case episodes, build, lifetime
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        schema = try values.decodeIfPresent(String.self, forKey: .schema) ?? Self.schemaID
        mode = (try? values.decodeIfPresent(Mode.self, forKey: .mode)) ?? .suggest
        aggregationVersion =
            try values.decodeIfPresent(Int.self, forKey: .aggregationVersion) ?? Self.aggregationVersion
        configHash = try values.decodeIfPresent(String.self, forKey: .configHash) ?? ""
        // Soft-decode nested collections so one bad episode/aggregate cannot block the build.
        episodes = (try? values.decodeIfPresent([Episode].self, forKey: .episodes)) ?? []
        build = (try? values.decodeIfPresent(Aggregate.self, forKey: .build))
            ?? Aggregate(configHash: configHash)
        lifetime = (try? values.decodeIfPresent(Aggregate.self, forKey: .lifetime))
            ?? Aggregate(configHash: configHash)
    }

    var buildViewModel: AggregateViewModel { build.viewModel }
    var lifetimeViewModel: AggregateViewModel { lifetime.viewModel }

    /// Secondary lifetime labels keep Estimated and Realized visually separate.
    var lifetimeEstimatedLabel: String { lifetimeViewModel.grossEstimatedTokensLabel }
    var lifetimeConfidenceAdjustedEstimatedLabel: String {
        lifetimeViewModel.confidenceAdjustedEstimatedTokensLabel
    }
    var lifetimeRealizedLabel: String { lifetimeViewModel.realizedTokensLabel }

    static func load(from projectURL: URL) -> ProjectGraphEfficiency? {
        guard let data = try? Data(contentsOf: projectURL) else { return nil }
        return decode(from: data)
    }

    /// Returns `nil` when efficiency is absent or the payload is malformed.
    static func decode(from data: Data) -> ProjectGraphEfficiency? {
        guard let envelope = try? JSONDecoder().decode(Envelope.self, from: data) else {
            return nil
        }
        return envelope.efficiency
    }
}
