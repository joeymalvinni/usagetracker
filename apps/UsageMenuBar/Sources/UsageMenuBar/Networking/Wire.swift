import Foundation

enum DaemonWireProtocol {
    static let currentVersion = 3
}

enum ProviderSignInAction: String, Encodable {
    case open
    case copyLink = "copy_link"
}

enum DaemonRequest: Encodable {
    case getServerInfo, getState, getUsage, refresh([String]?), getRefreshJob(String)
    case getUsageEvents(accountId: String, offset: UInt32, limit: UInt16?)
    case getProviderHealth, getAccounts, getConfig, getPendingNotifications
    case acknowledgeNotifications([Int64])
    case updateConfig(pollIntervalSeconds: UInt64?, providers: [String: Bool]?, notifications: NotificationConfig?)
    case addProviderAccount(providerId: String, displayName: String?, signInAction: ProviderSignInAction)
    case updateAccount(accountId: String, displayName: String?, hidden: Bool?, collectionEnabled: Bool?)
    case removeAccount(accountId: String)
    case deleteAccount(accountId: String)
    case getProviderSetup(providerId: String)
    case updateProviderSetup(providerId: String, settings: [String: String?])
    case repairProvider(providerId: String, accountId: String?, signInAction: ProviderSignInAction)
    case launchProviderAccount(accountId: String)
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: K.self)
        try c.encode(DaemonWireProtocol.currentVersion, forKey: .apiVersion)
        switch self {
        case .getServerInfo: try c.encode("get_server_info", forKey: .method)
        case .getState: try c.encode("get_state", forKey: .method)
        case .getUsage: try c.encode("get_usage", forKey: .method)
        case .getUsageEvents(let accountId, let offset, let limit):
            try c.encode("get_usage_events", forKey: .method)
            try c.encode(accountId, forKey: .accountId)
            try c.encode(offset, forKey: .offset)
            try c.encodeIfPresent(limit, forKey: .limit)
        case .getProviderHealth: try c.encode("get_provider_health", forKey: .method)
        case .getAccounts: try c.encode("get_accounts", forKey: .method)
        case .getConfig: try c.encode("get_config", forKey: .method)
        case .getPendingNotifications: try c.encode("get_pending_notifications", forKey: .method)
        case .acknowledgeNotifications(let ids):
            try c.encode("acknowledge_notifications", forKey: .method)
            try c.encode(ids, forKey: .ids)
        case .refresh(let ids): try c.encode("refresh", forKey: .method); try c.encode(ids, forKey: .providers)
        case .getRefreshJob(let jobId):
            try c.encode("get_refresh_job", forKey: .method)
            try c.encode(jobId, forKey: .jobId)
        case .updateConfig(let interval, let providers, let notifications):
            try c.encode("update_config", forKey: .method)
            try c.encodeIfPresent(interval, forKey: .pollIntervalSeconds)
            try c.encodeIfPresent(providers?.mapValues { ProviderToggle(enabled: $0) }, forKey: .providers)
            try c.encodeIfPresent(notifications, forKey: .notifications)
        case .addProviderAccount(let providerId, let displayName, let signInAction):
            try c.encode("add_provider_account", forKey: .method)
            try c.encode(providerId, forKey: .providerId)
            try c.encodeIfPresent(displayName, forKey: .displayName)
            try c.encode(signInAction, forKey: .signInAction)
        case .updateAccount(let accountId, let displayName, let hidden, let collectionEnabled):
            try c.encode("update_account", forKey: .method)
            try c.encode(accountId, forKey: .accountId)
            try c.encodeIfPresent(displayName, forKey: .displayName)
            try c.encodeIfPresent(hidden, forKey: .hidden)
            try c.encodeIfPresent(collectionEnabled, forKey: .collectionEnabled)
        case .removeAccount(let accountId):
            try c.encode("remove_account", forKey: .method)
            try c.encode(accountId, forKey: .accountId)
        case .deleteAccount(let accountId):
            try c.encode("delete_account", forKey: .method)
            try c.encode(accountId, forKey: .accountId)
        case .getProviderSetup(let providerId):
            try c.encode("get_provider_setup", forKey: .method)
            try c.encode(providerId, forKey: .providerId)
        case .updateProviderSetup(let providerId, let settings):
            try c.encode("update_provider_setup", forKey: .method)
            try c.encode(providerId, forKey: .providerId)
            try c.encode(settings, forKey: .settings)
        case .repairProvider(let providerId, let accountId, let signInAction):
            try c.encode("repair_provider", forKey: .method)
            try c.encode(providerId, forKey: .providerId)
            try c.encodeIfPresent(accountId, forKey: .accountId)
            try c.encode(signInAction, forKey: .signInAction)
        case .launchProviderAccount(let accountId):
            try c.encode("launch_provider_account", forKey: .method)
            try c.encode(accountId, forKey: .accountId)
        }
    }
    enum K: String, CodingKey {
        case apiVersion = "api_version"
        case method, providers, notifications, hidden, ids, settings, offset, limit
        case pollIntervalSeconds = "poll_interval_seconds"
        case providerId = "provider_id"
        case accountId = "account_id"
        case jobId = "job_id"
        case displayName = "display_name"
        case signInAction = "sign_in_action"
        case collectionEnabled = "collection_enabled"
        case workspaceId = "workspace_id"
    }
}

struct UsageResponse: Decodable {
    let snapshots: [UsageSnapshot]
    let forecasts: [UsageForecast]
    let dashboard: UsageDashboardSummary
    let windowProvenance: [UsageWindowProvenance]

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        snapshots = try c.decode([UsageSnapshot].self, forKey: .snapshots)
        forecasts = try c.decodeIfPresent([UsageForecast].self, forKey: .forecasts) ?? []
        dashboard = try c.decode(UsageDashboardSummary.self, forKey: .dashboard)
        windowProvenance = try c.decodeIfPresent([UsageWindowProvenance].self, forKey: .windowProvenance) ?? []
    }

    private enum K: String, CodingKey { case snapshots, forecasts, dashboard, windowProvenance }
}

struct StateResponse: Decodable {
    let generatedAt: Date
    let server: ServerInfo
    let config: ConfigResponse
    let accounts: [Account]
    let health: [ProviderHealth]
    let snapshots: [UsageSnapshot]
    let forecasts: [UsageForecast]
    let dashboard: UsageDashboardSummary
    let windowProvenance: [UsageWindowProvenance]

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        generatedAt = try c.decode(Date.self, forKey: .generatedAt)
        server = try c.decode(ServerInfo.self, forKey: .server)
        config = try c.decode(ConfigResponse.self, forKey: .config)
        accounts = try c.decode([Account].self, forKey: .accounts)
        health = try c.decode([ProviderHealth].self, forKey: .health)
        snapshots = try c.decode([UsageSnapshot].self, forKey: .snapshots)
        forecasts = try c.decodeIfPresent([UsageForecast].self, forKey: .forecasts) ?? []
        dashboard = try c.decode(UsageDashboardSummary.self, forKey: .dashboard)
        windowProvenance = try c.decodeIfPresent(
            [UsageWindowProvenance].self,
            forKey: .windowProvenance
        ) ?? []
    }

    private enum K: String, CodingKey {
        case generatedAt, server, config, accounts, health, snapshots, forecasts, dashboard
        case windowProvenance
    }
}

struct PendingNotification: Decodable, Sendable {
    let id: Int64
    let title: String
    let body: String
    let createdAt: Date
}

struct ServerInfo: Decodable, Equatable, Sendable {
    let apiVersion: Int
    let capabilities: [String]
    let providers: [ServerProviderDescriptor]
}

struct ServerProviderDescriptor: Decodable, Equatable, Sendable {
    let id, displayName: String
    let minimumRefreshIntervalSeconds: UInt64
    let capabilities: ProviderCapabilities
}

struct ProviderCapabilities: Decodable, Equatable, Sendable {
    let multipleAccounts: Bool
    let addAccount: Bool
    let repair: Bool
    let launchAccount: Bool
    let setup: Bool
    let workspaceSetup: Bool

    init(
        multipleAccounts: Bool,
        addAccount: Bool,
        repair: Bool,
        launchAccount: Bool,
        workspaceSetup: Bool,
        setup: Bool? = nil
    ) {
        self.multipleAccounts = multipleAccounts
        self.addAccount = addAccount
        self.repair = repair
        self.launchAccount = launchAccount
        self.workspaceSetup = workspaceSetup
        self.setup = setup ?? workspaceSetup
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        multipleAccounts = try c.decode(Bool.self, forKey: .multipleAccounts)
        addAccount = try c.decode(Bool.self, forKey: .addAccount)
        repair = try c.decode(Bool.self, forKey: .repair)
        launchAccount = try c.decode(Bool.self, forKey: .launchAccount)
        workspaceSetup = try c.decode(Bool.self, forKey: .workspaceSetup)
        setup = try c.decodeIfPresent(Bool.self, forKey: .setup) ?? workspaceSetup
    }

    private enum CodingKeys: String, CodingKey {
        case multipleAccounts, addAccount, repair, launchAccount, setup, workspaceSetup
    }
}

func providerSupports(
    _ providerId: String,
    capability: KeyPath<ProviderCapabilities, Bool>,
    in providers: [String: ServerProviderDescriptor]
) -> Bool {
    providers[providerId]?.capabilities[keyPath: capability] == true
}

enum DaemonResponse: Decodable {
    case serverInfo(ServerInfo), state(StateResponse), usage(UsageResponse)
    case usageEvents(UsageEventPage)
    case refreshStarted(job: RefreshJob, coalesced: Bool), refreshJob(RefreshJob)
    case providerHealth([ProviderHealth]), accounts([Account]), config(ConfigResponse)
    case pendingNotifications([PendingNotification]), notificationsAcknowledged([Int64])
    case addProviderAccount(AddProviderAccountResponse), account(Account), accountDeleted(String)
    case providerSetup(ProviderSetupResponse), providerAction(ProviderActionResponse), error(ApiError)
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        let version = try c.decodeIfPresent(Int.self, forKey: .apiVersion)
        guard version == DaemonWireProtocol.currentVersion else {
            let received = version.map(String.init) ?? "missing"
            throw DaemonError.api(
                code: "incompatible_protocol",
                message: "Daemon protocol version \(received) is incompatible with version \(DaemonWireProtocol.currentVersion)"
            )
        }
        switch try c.decode(String.self, forKey: .type) {
        case "server_info": self = .serverInfo(try c.decode(ServerInfo.self, forKey: .server))
        case "state": self = .state(try c.decode(StateResponse.self, forKey: .state))
        case "usage": self = .usage(try UsageResponse(from: decoder))
        case "usage_events": self = .usageEvents(try c.decode(UsageEventPage.self, forKey: .page))
        case "refresh_started": self = .refreshStarted(
            job: try c.decode(RefreshJob.self, forKey: .job),
            coalesced: try c.decode(Bool.self, forKey: .coalesced)
        )
        case "refresh_job": self = .refreshJob(try c.decode(RefreshJob.self, forKey: .job))
        case "provider_health": self = .providerHealth(try c.decode([ProviderHealth].self, forKey: .health))
        case "accounts": self = .accounts(try c.decode([Account].self, forKey: .accounts))
        case "config": self = .config(try c.decode(ConfigResponse.self, forKey: .config))
        case "pending_notifications": self = .pendingNotifications(try c.decode([PendingNotification].self, forKey: .notifications))
        case "notifications_acknowledged": self = .notificationsAcknowledged(try c.decode([Int64].self, forKey: .ids))
        case "add_provider_account": self = .addProviderAccount(try c.decode(AddProviderAccountResponse.self, forKey: .account))
        case "account": self = .account(try c.decode(Account.self, forKey: .account))
        case "account_deleted": self = .accountDeleted(try c.decode(String.self, forKey: .accountId))
        case "provider_setup": self = .providerSetup(try c.decode(ProviderSetupResponse.self, forKey: .setup))
        case "provider_action": self = .providerAction(try c.decode(ProviderActionResponse.self, forKey: .action))
        case "error": self = .error(try c.decode(ApiError.self, forKey: .error))
        default: throw DecodingError.dataCorrupted(.init(codingPath: c.codingPath, debugDescription: "unknown response"))
        }
    }
    enum K: String, CodingKey {
        case apiVersion, type, snapshots, health, accounts, config, notifications, ids
        case server, state, job, coalesced, account, setup, action, error, accountId, page
    }
}

extension JSONDecoder {
    static var usage: JSONDecoder {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        d.dateDecodingStrategy = .custom { decoder in
            let s = try decoder.singleValueContainer().decode(String.self)
            for f in [DateFormats.fractional, DateFormats.whole] { if let date = f.date(from: s) { return date } }
            throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "invalid date \(s)"))
        }
        return d
    }
}

extension JSONEncoder {
    static var usage: JSONEncoder {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        return encoder
    }
}

final class LockedISO8601DateFormatter: @unchecked Sendable {
    private let formatter: ISO8601DateFormatter
    private let lock = NSLock()

    init(configure: (ISO8601DateFormatter) -> Void) {
        let formatter = ISO8601DateFormatter()
        configure(formatter)
        self.formatter = formatter
    }

    func date(from value: String) -> Date? {
        lock.withLock { formatter.date(from: value) }
    }
}

final class LockedDateFormatter: @unchecked Sendable {
    private let formatter: DateFormatter
    private let lock = NSLock()

    init(configure: (DateFormatter) -> Void) {
        let formatter = DateFormatter()
        configure(formatter)
        self.formatter = formatter
    }

    func string(from date: Date) -> String {
        lock.withLock { formatter.string(from: date) }
    }

    func date(from value: String) -> Date? {
        lock.withLock { formatter.date(from: value) }
    }
}

final class LockedRelativeDateTimeFormatter: @unchecked Sendable {
    private let formatter: RelativeDateTimeFormatter
    private let lock = NSLock()

    init(configure: (RelativeDateTimeFormatter) -> Void) {
        let formatter = RelativeDateTimeFormatter()
        configure(formatter)
        self.formatter = formatter
    }

    func localizedString(for date: Date, relativeTo referenceDate: Date) -> String {
        lock.withLock { formatter.localizedString(for: date, relativeTo: referenceDate) }
    }

    func localizedString(from components: DateComponents) -> String {
        lock.withLock { formatter.localizedString(from: components) }
    }
}

enum DateFormats {
    static let fractional = LockedISO8601DateFormatter {
        $0.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    }
    static let whole = LockedISO8601DateFormatter {
        $0.formatOptions = [.withInternetDateTime]
    }
    static let dayKey = LockedDateFormatter {
        $0.calendar = Calendar(identifier: .gregorian)
        $0.timeZone = .autoupdatingCurrent
        $0.locale = Locale(identifier: "en_US_POSIX")
        $0.dateFormat = "yyyy-MM-dd"
    }
    static let shortDay = LockedDateFormatter {
        $0.calendar = Calendar(identifier: .gregorian)
        $0.timeZone = .autoupdatingCurrent
        $0.locale = .autoupdatingCurrent
        $0.setLocalizedDateFormatFromTemplate("MMM d")
    }
    static let expiry = LockedDateFormatter {
        $0.calendar = .current
        $0.locale = Locale(identifier: "en_US_POSIX")
        $0.dateFormat = "EEE, MMM d 'at' h:mm a"
    }
    /// Fully spelled-out instant — weekday, month, day, year, time, zone — for
    /// the reset dropdown where an exact, unambiguous date is wanted.
    static let explicit = LockedDateFormatter {
        $0.calendar = .current
        $0.locale = .autoupdatingCurrent
        $0.dateFormat = "EEEE, MMMM d, yyyy 'at' h:mm a zzz"
    }
    static let relative = LockedRelativeDateTimeFormatter {
        $0.unitsStyle = .full
    }

    /// Relative wording for future reset/expiry deadlines. Foundation truncates
    /// a 45-hour interval to "in 1 day", which conflicts with an explicit date
    /// two calendar days ahead (for example, Thursday to Saturday).
    static func resetRelativeString(
        for date: Date,
        relativeTo referenceDate: Date = Date(),
        calendar: Calendar = .autoupdatingCurrent
    ) -> String {
        if date > referenceDate {
            let referenceDay = calendar.startOfDay(for: referenceDate)
            let targetDay = calendar.startOfDay(for: date)
            let calendarDays = calendar.dateComponents([.day], from: referenceDay, to: targetDay).day ?? 0
            if calendarDays >= 2 {
                return relative.localizedString(from: DateComponents(day: calendarDays))
            }
        }
        return relative.localizedString(for: date, relativeTo: referenceDate)
    }
}
