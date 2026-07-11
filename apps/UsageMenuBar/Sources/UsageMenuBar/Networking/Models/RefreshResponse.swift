import Foundation

struct RefreshResponse: Decodable, Equatable {
    let startedAt, finishedAt: Date
    let providerResults: [ProviderRefreshResult]

    init(job: RefreshJob) {
        startedAt = job.startedAt ?? job.createdAt
        finishedAt = job.finishedAt ?? job.startedAt ?? job.createdAt
        providerResults = job.providerResults
    }
}

struct RefreshJob: Decodable, Equatable, Sendable {
    let id: String
    let scope: RefreshScope
    let trigger: RefreshTrigger
    let status: RefreshJobStatus
    let createdAt: Date
    let startedAt, finishedAt: Date?
    let providerResults: [ProviderRefreshResult]
    let failureMessage: String?

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        scope = try container.decode(RefreshScope.self, forKey: .scope)
        trigger = try container.decode(RefreshTrigger.self, forKey: .trigger)
        status = try container.decode(RefreshJobStatus.self, forKey: .status)
        createdAt = try container.decode(Date.self, forKey: .createdAt)
        startedAt = try container.decodeIfPresent(Date.self, forKey: .startedAt)
        finishedAt = try container.decodeIfPresent(Date.self, forKey: .finishedAt)
        providerResults = try container.decodeIfPresent(
            [ProviderRefreshResult].self,
            forKey: .providerResults
        ) ?? []
        failureMessage = try container.decodeIfPresent(String.self, forKey: .failureMessage)
    }

    private enum CodingKeys: String, CodingKey {
        case id, scope, trigger, status, createdAt, startedAt, finishedAt
        case providerResults, failureMessage
    }
}

struct RefreshScope: Decodable, Equatable, Sendable {
    let providers: [String]?
}

enum RefreshTrigger: String, Decodable, Equatable, Sendable {
    case manual, system
}

enum RefreshJobStatus: String, Decodable, Equatable, Sendable {
    case queued, running, completed, failed

    var isTerminal: Bool {
        self == .completed || self == .failed
    }
}

struct ProviderRefreshResult: Decodable, Equatable, Sendable {
    let providerId: String
    let accountId: String?
    let status: ProviderRefreshStatus
    let collectionMode: String?
    let collectedAt: Date?
    let message: String?
}

enum ProviderRefreshStatus: Equatable, Decodable, Sendable {
    case ok, credentialsMissing, credentialsInvalid, unauthorized, rateLimited, network, parse, providerUnavailable, storageError, disabled, other(String)
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
        case "disabled": self = .disabled
        case let s: self = .other(s)
        }
    }
}
