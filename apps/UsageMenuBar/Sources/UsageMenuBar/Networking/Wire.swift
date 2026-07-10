import Foundation

enum DaemonRequest: Encodable {
    case getUsage, refresh([String]?), getProviderHealth, getAccounts, getConfig
    case updateConfig(pollIntervalSeconds: UInt64?, providers: [String: Bool]?)
    case addProviderAccount(providerId: String, displayName: String?)
    case updateAccount(accountId: String, displayName: String?, hidden: Bool?, collectionEnabled: Bool?)
    case removeAccount(accountId: String)
    case deleteAccount(accountId: String)
    case getProviderSetup(providerId: String)
    case updateProviderSetup(providerId: String, workspaceId: String?)
    case repairProvider(providerId: String, accountId: String?)
    case launchProviderAccount(accountId: String)
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: K.self)
        switch self {
        case .getUsage: try c.encode("get_usage", forKey: .method)
        case .getProviderHealth: try c.encode("get_provider_health", forKey: .method)
        case .getAccounts: try c.encode("get_accounts", forKey: .method)
        case .getConfig: try c.encode("get_config", forKey: .method)
        case .refresh(let ids): try c.encode("refresh", forKey: .method); try c.encode(ids, forKey: .providers)
        case .updateConfig(let interval, let providers):
            try c.encode("update_config", forKey: .method)
            try c.encodeIfPresent(interval, forKey: .pollIntervalSeconds)
            try c.encodeIfPresent(providers?.mapValues { ProviderToggle(enabled: $0) }, forKey: .providers)
        case .addProviderAccount(let providerId, let displayName):
            try c.encode("add_provider_account", forKey: .method)
            try c.encode(providerId, forKey: .providerId)
            try c.encodeIfPresent(displayName, forKey: .displayName)
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
        case .updateProviderSetup(let providerId, let workspaceId):
            try c.encode("update_provider_setup", forKey: .method)
            try c.encode(providerId, forKey: .providerId)
            try c.encodeIfPresent(workspaceId, forKey: .workspaceId)
        case .repairProvider(let providerId, let accountId):
            try c.encode("repair_provider", forKey: .method)
            try c.encode(providerId, forKey: .providerId)
            try c.encodeIfPresent(accountId, forKey: .accountId)
        case .launchProviderAccount(let accountId):
            try c.encode("launch_provider_account", forKey: .method)
            try c.encode(accountId, forKey: .accountId)
        }
    }
    enum K: String, CodingKey {
        case method, providers, hidden
        case pollIntervalSeconds = "poll_interval_seconds"
        case providerId = "provider_id"
        case accountId = "account_id"
        case displayName = "display_name"
        case collectionEnabled = "collection_enabled"
        case workspaceId = "workspace_id"
    }
}

enum DaemonResponse: Decodable {
    case usage([UsageSnapshot]), refresh(RefreshResponse), providerHealth([ProviderHealth]), accounts([Account]), config(ConfigResponse), addProviderAccount(AddProviderAccountResponse), account(Account), accountDeleted(String), providerSetup(ProviderSetupResponse), providerAction(ProviderActionResponse), error(ApiError)
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        switch try c.decode(String.self, forKey: .type) {
        case "usage": self = .usage(try c.decode([UsageSnapshot].self, forKey: .snapshots))
        case "refresh": self = .refresh(try RefreshResponse(from: decoder))
        case "provider_health": self = .providerHealth(try c.decode([ProviderHealth].self, forKey: .health))
        case "accounts": self = .accounts(try c.decode([Account].self, forKey: .accounts))
        case "config": self = .config(try c.decode(ConfigResponse.self, forKey: .config))
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
        case type, snapshots, health, accounts, config, account, setup, action, error, accountId
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

enum DateFormats {
    static let fractional: ISO8601DateFormatter = { let f = ISO8601DateFormatter(); f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]; return f }()
    static let whole: ISO8601DateFormatter = { let f = ISO8601DateFormatter(); f.formatOptions = [.withInternetDateTime]; return f }()
    static let dayKey: DateFormatter = { let f = DateFormatter(); f.calendar = .current; f.locale = Locale(identifier: "en_US_POSIX"); f.dateFormat = "yyyy-MM-dd"; return f }()
    static let expiry: DateFormatter = { let f = DateFormatter(); f.calendar = .current; f.locale = Locale(identifier: "en_US_POSIX"); f.dateFormat = "EEE, MMM d 'at' h:mm a"; return f }()
    /// Fully spelled-out instant — weekday, month, day, year, time, zone — for
    /// the reset dropdown where an exact, unambiguous date is wanted.
    static let explicit: DateFormatter = { let f = DateFormatter(); f.calendar = .current; f.locale = .autoupdatingCurrent; f.dateFormat = "EEEE, MMMM d, yyyy 'at' h:mm a zzz"; return f }()
    static let relative: RelativeDateTimeFormatter = { let f = RelativeDateTimeFormatter(); f.unitsStyle = .full; return f }()

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
