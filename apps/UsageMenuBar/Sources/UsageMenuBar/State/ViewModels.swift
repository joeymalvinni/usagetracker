import SwiftUI

enum DisplayStatus {
    case normal, warning, critical, stale, error, disabled, offline

    /// Semantic status color. Only surfaced when `needsAttention` (status
    /// chip, rail badge, progress-bar fill) — color always means "look here".
    var tint: Color {
        switch self {
        case .normal: .green
        case .warning: .orange
        case .critical, .error, .offline: .red
        case .stale, .disabled: .secondary
        }
    }
    var menuColor: NSColor? { switch self { case .warning: .systemOrange; case .critical, .error: .systemRed; default: nil } }

    /// Anything worth surfacing to the user. `.normal` stays quiet.
    var needsAttention: Bool {
        switch self {
        case .normal: false
        default: true
        }
    }

    var label: String {
        switch self {
        case .normal: "all good"
        case .warning: "running low"
        case .critical: "almost out"
        case .stale: "stale data"
        case .error: "error"
        case .disabled: "off"
        case .offline: "offline"
        }
    }
}

extension ProviderHealthStatus {
    /// Friendly, human-readable phrase used in rows & detail views.
    var friendly: String {
        switch self {
        case .ok: "all good"
        case .credentialsMissing: "needs login"
        case .authFailed: "auth failed"
        case .rateLimited: "rate limited"
        case .providerError: "provider error"
        case .parseError: "parse error"
        case .backingOff: "backing off"
        case .disabled: "disabled"
        case .other(let s): s
        }
    }
}

struct ProviderVM: Identifiable, Equatable {
    let id, name, short, symbol, primary, detail: String
    let percent: Double?
    let status: DisplayStatus
    let spend, windows, credits: [WindowVM]
    let resetCredits: [ResetCreditVM]
    let account: String?
    let healthText: String
    let visibleInMenu: Bool
    let enabled: Bool
    let secondary: String
    let sparkline: [Double]
}

struct MenuBarProviderVM: Identifiable, Equatable {
    var id: String { providerId }
    let providerId: String
    let short: String
    let percent: Double?
    let status: DisplayStatus
}

struct WindowVM: Identifiable, Equatable {
    let id, label, value, reset: String
    let providerId, providerName: String
    let absolute: String?
    let percent: Double?
    let status: DisplayStatus
}

struct ResetCreditVM: Identifiable, Equatable {
    let id: String
    let title: String
    let status: String
    let expiresAt: Date?
    let expiresText: String
}

struct CostDashboardVM: Equatable {
    static let empty = CostDashboardVM(days: [], providers: [])
    let days: [CostDayVM]
    let providers: [CostProviderVM]

    var hasData: Bool { days.contains { $0.totalCost > 0 || $0.totalTokens > 0 } }
    var todayCost: Double { days.last?.totalCost ?? 0 }
    var todayTokens: UInt64 { days.last?.totalTokens ?? 0 }
    var cost30d: Double { days.reduce(0) { $0 + $1.totalCost } }
    var tokens30d: UInt64 { days.reduce(0) { $0.saturatingAdd($1.totalTokens) } }
}

struct CostProviderVM: Identifiable, Equatable { let id, name, symbol: String }

struct CostDayVM: Identifiable, Equatable {
    let id: String
    let date: Date
    let providers: [CostProviderDayVM]

    var totalCost: Double { providers.reduce(0) { $0 + $1.cost } }
    var totalTokens: UInt64 { providers.reduce(0) { $0.saturatingAdd($1.tokens) } }
}

struct CostProviderDayVM: Identifiable, Equatable {
    var id: String { providerId }
    let providerId, providerName, symbol: String
    let date: Date
    let dateKey: String
    let cost: Double
    let tokens: UInt64
}

extension UInt64 {
    func saturatingAdd(_ other: UInt64) -> UInt64 {
        let result = addingReportingOverflow(other)
        return result.overflow ? UInt64.max : result.partialValue
    }
}
