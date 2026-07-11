import Foundation
struct UsageSnapshot: Decodable, Identifiable, Equatable {
    var id: String { "\(providerId):\(accountId)" }
    let providerId, accountId: String
    let collectedAt: Date
    let windows: [UsageWindow]
}

struct UsageWindow: Decodable, Identifiable, Equatable {
    var id: String { windowId }
    let windowId, label: String
    let kind: UsageWindowKind
    let used, limit, remaining: UsageAmount?
    let percentUsed, percentRemaining: Double?
    let resetAt: Date?
}

struct UsageForecast: Decodable, Equatable {
    let providerId, accountId, windowId: String
    let generatedAt: Date
    let resetAt: Date?
    let currentPercentUsed: Double
    let expectedPercentUsed, paceDeltaPercent: Double?
    let ratePercentPerHour, projectedPercentAtReset: Double?
    let projectedPercentRemainingAtReset: Double?
    let predictedExhaustionAt: Date?
    let status: ForecastStatus
    let sampleCount: Int
    let confidence: ForecastConfidence
}

enum ForecastStatus: String, Decodable {
    case insufficientData = "insufficient_data"
    case safe
    case onPace = "on_pace"
    case atRisk = "at_risk"
    case exhausted

    var conclusion: String {
        switch self {
        case .safe, .onPace: "Lasts until reset"
        case .atRisk: "Tight before reset"
        case .exhausted: "Exhausted until reset"
        case .insufficientData: "Building pace forecast"
        }
    }
}

enum ForecastConfidence: String, Decodable { case low, medium, high }

struct UsageAmount: Decodable, Equatable { let value: Double; let unit: UsageUnit }

enum UsageWindowKind: Equatable, Decodable {
    case session, daily, weekly, monthly, credits, tokens, other(String)
    init(from decoder: Decoder) throws {
        if let s = try? decoder.singleValueContainer().decode(String.self) { self = Self.named(s); return }
        let o = try decoder.singleValueContainer().decode([String: String].self)
        self = .other(o["other"] ?? o.first?.value ?? "other")
    }
    private static func named(_ s: String) -> Self {
        switch s { case "session": .session; case "daily": .daily; case "weekly": .weekly; case "monthly": .monthly; case "credits": .credits; case "tokens": .tokens; default: .other(s) }
    }
}

enum UsageUnit: Equatable, Decodable {
    case tokens, requests, credits, usd, percent, unknown, other(String)
    init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "tokens": self = .tokens
        case "requests": self = .requests
        case "credits": self = .credits
        case "usd": self = .usd
        case "percent": self = .percent
        case "unknown": self = .unknown
        case let s: self = .other(s)
        }
    }
}

extension UsageUnit {
    var label: String {
        switch self {
        case .tokens: "tokens"; case .requests: "requests"; case .credits: "credits"
        case .usd: "USD"; case .percent: "%"; case .unknown: "units"; case .other(let s): s
        }
    }
}
