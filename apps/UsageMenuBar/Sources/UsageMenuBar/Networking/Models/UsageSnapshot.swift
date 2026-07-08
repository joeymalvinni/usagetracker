import Foundation
struct UsageSnapshot: Decodable, Identifiable, Equatable {
    var id: String { "\(providerId):\(accountId)" }
    let providerId, accountId: String
    let collectedAt: Date
    let windows: [UsageWindow]
    let metadata: JSONValue
}

struct UsageWindow: Decodable, Identifiable, Equatable {
    var id: String { windowId }
    let windowId, label: String
    let kind: UsageWindowKind
    let used, limit, remaining: UsageAmount?
    let percentUsed, percentRemaining: Double?
    let resetAt: Date?
}

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