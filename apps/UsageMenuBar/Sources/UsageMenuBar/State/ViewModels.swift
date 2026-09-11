import SwiftUI

enum DisplayStatus: Equatable, Sendable {
    case normal, warning, critical, stale, refreshing, error, disabled, offline

    var tint: Color {
        switch self {
        case .normal: .green
        case .warning: .orange
        case .critical, .error, .offline: .red
        case .stale, .refreshing, .disabled: .secondary
        }
    }
    var menuColor: NSColor? { switch self { case .warning: .systemOrange; case .critical, .error: .systemRed; default: nil } }

    var needsAttention: Bool {
        switch self {
        case .normal: false
        default: true
        }
    }

    var severity: Int {
        switch self {
        case .normal: 0
        case .disabled: 1
        case .stale, .refreshing: 2
        case .warning: 3
        case .critical: 4
        case .error, .offline: 5
        }
    }

    /// An actionable alert the user should acknowledge (as opposed to stale/disabled/off).
    var isAlert: Bool {
        switch self {
        case .warning, .critical, .error, .offline: true
        default: false
        }
    }

    /// Stable identifier for the status level, used in alert signatures.
    var code: String {
        switch self {
        case .normal: "normal"
        case .warning: "warning"
        case .critical: "critical"
        case .stale: "stale"
        case .refreshing: "refreshing"
        case .error: "error"
        case .disabled: "disabled"
        case .offline: "offline"
        }
    }

    func label(percentRemaining: Double?) -> String {
        guard let percent = percentRemaining, percent.isFinite else { return label }
        if self == .normal { return "within limit" }
        guard self == .warning || self == .critical else { return label }
        if percent <= 0 { return "limit reached" }
        return "\(Int(min(100, percent).rounded()))% remaining"
    }

    var label: String {
        switch self {
        case .normal: "usage update"
        case .warning: "usage update"
        case .critical: "usage update"
        case .stale: "stale data"
        case .refreshing: "refreshing…"
        case .error: "error"
        case .disabled: "off"
        case .offline: "offline"
        }
    }
}

extension ProviderHealthStatus {
    var friendly: String {
        switch self {
        case .ok: "up to date"
        case .credentialsMissing: "needs login"
        case .authFailed: "auth failed"
        case .keychainAccessFailed: "keychain access failed"
        case .rateLimited: "rate limited"
        case .providerError: "provider error"
        case .parseError: "parse error"
        case .backingOff: "backing off"
        case .disabled: "disabled"
        case .other(let s): s
        }
    }
}

struct ProviderVM: Identifiable, Equatable, Sendable {
    let id, providerId: String
    let accountId: String?
    let name, short, symbol, primary, detail: String
    let percent: Double?
    let status: DisplayStatus
    let spend, windows, credits: [WindowVM]
    let resetCreditSummary: ResetCreditSummaryVM?
    let account: String?
    let healthText: String
    let visibleInMenu: Bool
    let enabled: Bool
    let secondary: String
    let sparkline: [Double]
    let costDashboard: CostDashboardVM
    let subAccounts: [ProviderVM]?
    var modelCosts: [ModelCostSummary] = []
    /// Non-nil when this provider/account is in an actionable alert state.
    /// Format: "provider|account|statusCode". Acknowledgements key off this exact value.
    var alertSignature: String? = nil
    /// True when there is an active alert that the user has not yet viewed.
    var hasUnseenAlert: Bool = false
    var lastSuccessAt: Date? = nil
    var errorDetail: String? = nil
    var collectionIssue: ProviderCollectionIssue? = nil
    var accountEmail: String? = nil
    var planLabel: String? = nil
    var activitySourceLabel: String? = nil
    var hasCostData: Bool = true
    var unpricedModelNames: [String] = []

    var headlineWindow: WindowVM? { Self.headlineWindow(in: windows) }

    /// Keep the number, bar, label, and reset tied to one limit. Equal values
    /// retain provider order so a refresh does not arbitrarily switch windows.
    static func headlineWindow(in windows: [WindowVM]) -> WindowVM? {
        windows.filter { $0.percent != nil }.min {
            ($0.percent ?? 100) < ($1.percent ?? 100)
        } ?? windows.first
    }
}

struct MenuBarProviderVM: Identifiable, Equatable, Sendable {
    let id: String
    let providerId: String
    let short: String
    let percent: Double?
    let status: DisplayStatus
    var isMuted: Bool = false
}

struct WindowVM: Identifiable, Equatable, Sendable {
    let id, label, value, reset: String
    let providerId, providerName: String
    let absolute: String?
    let percent: Double?
    let status: DisplayStatus
    /// Raw reset/expiry instant, when known. Powers relative countdowns and
    /// the explicit-date disclosure; `reset` is its pre-rendered short form.
    var resetAt: Date? = nil
    var forecast: WindowForecastVM? = nil
    var isMuted: Bool = false
}

struct WindowForecastVM: Equatable, Sendable {
    let summary: String
    let detail: String
    let projectedPercentRemaining: Double?
}

struct ResetCreditVM: Identifiable, Equatable, Sendable {
    let id: String
    let title: String
    let status: String
    let expiresAt: Date?
    let expiresText: String
}

struct ResetCreditSummaryVM: Equatable, Sendable {
    let availableCount: Int
    let nextExpiresAt: Date?
    let credits: [ResetCreditVM]
}

struct CostDashboardVM: Equatable, Sendable {
    static let empty = CostDashboardVM(
        days: [],
        providers: [],
        allTimeCost: 0,
        allTimeTokens: 0
    )
    let days: [CostDayVM]
    let providers: [CostProviderVM]
    let allTimeCost: Double
    let allTimeTokens: UInt64

    var hasData: Bool { days.contains { $0.totalCost > 0 || $0.totalTokens > 0 } }
    var todayCost: Double { days.last?.totalCost ?? 0 }
    var todayTokens: UInt64 { days.last?.totalTokens ?? 0 }
    var cost30d: Double { days.suffix(30).reduce(0) { $0 + $1.totalCost } }
    var tokens30d: UInt64 {
        days.suffix(30).reduce(0) { $0.saturatingAdd($1.totalTokens) }
    }
}

struct CostProviderVM: Identifiable, Equatable, Sendable { let id, name, symbol: String }

struct CostDayVM: Identifiable, Equatable, Sendable {
    let id: String
    let date: Date
    let providers: [CostProviderDayVM]

    var totalCost: Double { providers.reduce(0) { $0 + $1.cost } }
    var totalTokens: UInt64 { providers.reduce(0) { $0.saturatingAdd($1.tokens) } }
}

struct CostProviderDayVM: Identifiable, Equatable, Sendable {
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

    func saturatingSubtract(_ other: UInt64) -> UInt64 {
        subtractingReportingOverflow(other).overflow ? 0 : self - other
    }
}
