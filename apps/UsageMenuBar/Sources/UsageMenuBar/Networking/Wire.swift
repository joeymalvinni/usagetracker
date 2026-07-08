import Foundation

enum DaemonRequest: Encodable {
    case getUsage, refresh([String]?), getProviderHealth, getAccounts, getConfig
    case updateConfig(pollIntervalSeconds: UInt64?, providers: [String: Bool]?)
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
        }
    }
    enum K: String, CodingKey { case method, providers, pollIntervalSeconds = "poll_interval_seconds" }
}

enum DaemonResponse: Decodable {
    case usage([UsageSnapshot]), refresh(RefreshResponse), providerHealth([ProviderHealth]), accounts([Account]), config(ConfigResponse), error(ApiError)
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        switch try c.decode(String.self, forKey: .type) {
        case "usage": self = .usage(try c.decode([UsageSnapshot].self, forKey: .snapshots))
        case "refresh": self = .refresh(try RefreshResponse(from: decoder))
        case "provider_health": self = .providerHealth(try c.decode([ProviderHealth].self, forKey: .health))
        case "accounts": self = .accounts(try c.decode([Account].self, forKey: .accounts))
        case "config": self = .config(try c.decode(ConfigResponse.self, forKey: .config))
        case "error": self = .error(try c.decode(ApiError.self, forKey: .error))
        default: throw DecodingError.dataCorrupted(.init(codingPath: c.codingPath, debugDescription: "unknown response"))
        }
    }
    enum K: String, CodingKey { case type, snapshots, health, accounts, config, error }
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
    static let relative: RelativeDateTimeFormatter = { let f = RelativeDateTimeFormatter(); f.unitsStyle = .full; return f }()
}