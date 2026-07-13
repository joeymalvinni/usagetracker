import Foundation

struct UsageDashboardSummary: Decodable, Equatable {
    static let empty = UsageDashboardSummary(
        accounts: [],
        days: [],
        pricing: .empty,
        provenance: .empty
    )

    let accounts: [AccountUsageSummary]
    let days: [DailyUsagePoint]
    let pricing: PricingCoverage
    let provenance: AggregateProvenance
}

struct AccountUsageSummary: Decodable, Equatable {
    let providerId: String
    let accountId: String
    let activity: ActivitySummary?
    let cost: CostSummary?
    let resetCredits: ResetCreditSummary?
}

struct ResetCreditSummary: Decodable, Equatable {
    let availableCount: UInt64
    let nextExpiresAt: Date?
    let credits: [ResetCredit]
}

struct ResetCredit: Decodable, Equatable {
    let id: String
    let title: String
    let status: String
    let expiresAt: Date?
}

struct ActivitySummary: Decodable, Equatable {
    let provenance: DataProvenance
    let days: [DailyUsagePoint]
    let todayTokens: UInt64
    let lookbackTokens: UInt64
    let lifetimeTokens: UInt64?
}

struct CostSummary: Decodable, Equatable {
    let provenance: DataProvenance
    let days: [DailyUsagePoint]
    let todayCostUsd: Double
    let lookbackCostUsd: Double
    let pricing: PricingCoverage
}

struct DailyUsagePoint: Decodable, Equatable {
    let dateKey: String
    let tokens: UInt64
    let costUsd: Double?
    let pricedTokens: UInt64
    let unpricedTokens: UInt64

    var date: Date? { DateFormats.dayKey.date(from: dateKey) }

    private enum CodingKeys: String, CodingKey {
        case dateKey = "date"
        case tokens, costUsd, pricedTokens, unpricedTokens
    }
}

struct PricingCoverage: Decodable, Equatable {
    static let empty = PricingCoverage(
        pricedTokens: 0,
        unpricedTokens: 0,
        coveredPercent: 0,
        unpricedModels: [],
        catalogVersion: nil,
        catalogSource: nil,
        catalogEffectiveFrom: nil
    )

    let pricedTokens: UInt64
    let unpricedTokens: UInt64
    let coveredPercent: Double
    let unpricedModels: [String]
    let catalogVersion: String?
    let catalogSource: String?
    let catalogEffectiveFrom: String?

    init(
        pricedTokens: UInt64,
        unpricedTokens: UInt64,
        coveredPercent: Double,
        unpricedModels: [String],
        catalogVersion: String?,
        catalogSource: String?,
        catalogEffectiveFrom: String?
    ) {
        self.pricedTokens = pricedTokens
        self.unpricedTokens = unpricedTokens
        self.coveredPercent = coveredPercent
        self.unpricedModels = unpricedModels
        self.catalogVersion = catalogVersion
        self.catalogSource = catalogSource
        self.catalogEffectiveFrom = catalogEffectiveFrom
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        pricedTokens = try container.decode(UInt64.self, forKey: .pricedTokens)
        unpricedTokens = try container.decode(UInt64.self, forKey: .unpricedTokens)
        coveredPercent = try container.decode(Double.self, forKey: .coveredPercent)
        unpricedModels = try container.decodeIfPresent(
            [String].self,
            forKey: .unpricedModels
        ) ?? []
        catalogVersion = try container.decodeIfPresent(String.self, forKey: .catalogVersion)
        catalogSource = try container.decodeIfPresent(String.self, forKey: .catalogSource)
        catalogEffectiveFrom = try container.decodeIfPresent(
            String.self,
            forKey: .catalogEffectiveFrom
        )
    }

    private enum CodingKeys: String, CodingKey {
        case pricedTokens, unpricedTokens, coveredPercent, unpricedModels
        case catalogVersion, catalogSource, catalogEffectiveFrom
    }

    var label: String {
        "\(Int(coveredPercent.rounded()))% priced · \(formatTokens(unpricedTokens)) unpriced"
    }
}

struct DataProvenance: Decodable, Equatable {
    let source: UsageDataSource
    let scope: UsageDataScope
    let quality: UsageDataQuality
    let completeness: UsageDataCompleteness
    let confidence: UsageDataConfidence

    var badges: [String] {
        var values = [scope.label]
        if quality == .estimated { values.append("Estimated") }
        if completeness == .partial { values.append("Partial") }
        return values
    }
}

struct AggregateProvenance: Decodable, Equatable {
    static let empty = AggregateProvenance(
        scopes: [], qualities: [], partial: false, estimated: false,
        mixedScope: false, explanation: "No activity has been collected yet."
    )

    let scopes: [UsageDataScope]
    let qualities: [UsageDataQuality]
    let partial: Bool
    let estimated: Bool
    let mixedScope: Bool
    let explanation: String

    var badges: [String] {
        var values = scopes.map(\.label)
        if estimated { values.append("Estimated") }
        if partial { values.append("Partial") }
        return values.reduce(into: []) { result, value in
            if !result.contains(value) { result.append(value) }
        }
    }
}

enum UsageDataSource: String, Decodable {
    case providerReported = "provider_reported"
    case localLogs = "local_logs"
    case localDatabase = "local_database"
    case syntheticLocalEstimate = "synthetic_local_estimate"
}
enum UsageDataScope: String, Decodable {
    case accountWide = "account_wide"
    case thisDevice = "this_device"
    case selectedLocalRoots = "selected_local_roots"
    case workspace

    var label: String {
        switch self {
        case .accountWide: "Account-wide"
        case .thisDevice, .selectedLocalRoots: "This Mac"
        case .workspace: "Workspace"
        }
    }
}
enum UsageDataQuality: String, Decodable { case authoritative, observed, estimated }
enum UsageDataCompleteness: String, Decodable { case complete, partial }
enum UsageDataConfidence: String, Decodable { case low, medium, high }

struct UsageWindowProvenance: Decodable, Equatable {
    let providerId: String
    let accountId: String
    let windowId: String
    let source: UsageDataSource
    let scope: UsageDataScope
    let quality: UsageDataQuality
    let completeness: UsageDataCompleteness
    let confidence: UsageDataConfidence
    let authoritative: Bool
    let quotaLike: Bool
}
