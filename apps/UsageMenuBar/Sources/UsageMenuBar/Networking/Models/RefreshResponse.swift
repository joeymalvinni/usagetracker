import Foundation
struct RefreshResponse: Decodable, Equatable {
    let startedAt, finishedAt: Date
    let providerResults: [ProviderRefreshResult]
}

struct ProviderRefreshResult: Decodable, Equatable {
    let providerId: String
    let accountId: String?
    let status: ProviderRefreshStatus
    let collectionMode: String?
    let collectedAt: Date?
    let message: String?
}

enum ProviderRefreshStatus: Equatable, Decodable {
    case ok, credentialsMissing, credentialsInvalid, unauthorized, rateLimited, network, parse, providerUnavailable, storageError, other(String)
    init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "ok": self = .ok
        case "credentials_missing": self = .credentialsMissing
        case "credentials_invalid": self = .credentialsInvalid
        case "unauthorized": self = .unauthorized
        case "rate_limited": self = .rateLimited
        case "network": self = .network
        case "parse": self = .parse
        case "provider_unavailable": self = .providerUnavailable
        case "storage_error": self = .storageError
        case let s: self = .other(s)
        }
    }
}