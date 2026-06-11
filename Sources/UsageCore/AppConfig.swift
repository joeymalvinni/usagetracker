import Foundation

public struct AppConfig: Codable, Equatable, Sendable {
    public var accounts: [AccountConfig]

    public init(accounts: [AccountConfig] = []) {
        self.accounts = accounts
    }
}

public struct AccountConfig: Codable, Equatable, Sendable {
    public var id: String
    public var service: UsageService
    public var title: String?
    public var accountLabel: String?
    public var windows: [ConfiguredWindow]

    public init(
        id: String,
        service: UsageService,
        title: String? = nil,
        accountLabel: String? = nil,
        windows: [ConfiguredWindow] = []
    ) {
        self.id = id
        self.service = service
        self.title = title
        self.accountLabel = accountLabel
        self.windows = windows
    }
}

public struct ConfiguredWindow: Codable, Equatable, Sendable {
    public enum Period: String, Codable, Sendable {
        case day
        case week
        case month
        case rollingHours
    }

    public var kind: QuotaWindowKind
    public var limit: Double
    public var unit: UsageUnit
    public var period: Period
    public var rollingHours: Double?
    public var anchorHour: Int?
    public var startsAt: Date?
    public var resetAt: Date?

    public init(
        kind: QuotaWindowKind,
        limit: Double,
        unit: UsageUnit = .tokens,
        period: Period,
        rollingHours: Double? = nil,
        anchorHour: Int? = nil,
        startsAt: Date? = nil,
        resetAt: Date? = nil
    ) {
        self.kind = kind
        self.limit = limit
        self.unit = unit
        self.period = period
        self.rollingHours = rollingHours
        self.anchorHour = anchorHour
        self.startsAt = startsAt
        self.resetAt = resetAt
    }
}
