import Foundation
struct ProviderHealth: Decodable, Identifiable, Equatable {
    var id: String { providerId }
    let providerId: String
    let accountId: String?
    let status: ProviderHealthStatus
    let collectionMode: String?
    let lastSuccessAt, lastFailureAt: Date?
    let lastErrorCode, lastErrorMessage: String?
    let updatedAt: Date
}

enum ProviderHealthStatus: Equatable, Decodable {
    case ok, credentialsMissing, authFailed, rateLimited, providerError, parseError, backingOff, disabled, other(String)
    init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "ok": self = .ok
        case "credentials_missing": self = .credentialsMissing
        case "auth_failed": self = .authFailed
        case "rate_limited": self = .rateLimited
        case "provider_error": self = .providerError
        case "parse_error": self = .parseError
        case "backing_off": self = .backingOff
        case "disabled": self = .disabled
        case let s: self = .other(s)
        }
    }
}