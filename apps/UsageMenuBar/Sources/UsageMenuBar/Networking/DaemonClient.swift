import Foundation

struct DaemonClient {
    let socketPath: String
    private let decoder = JSONDecoder.usage
    private let encoder = JSONEncoder()

    func config() async throws -> ConfigResponse { guard case let .config(v) = try await send(.getConfig, 3) else { throw DaemonError.badResponse }; return v }
    func accounts() async throws -> [Account] { guard case let .accounts(v) = try await send(.getAccounts, 3) else { throw DaemonError.badResponse }; return v }
    func health() async throws -> [ProviderHealth] { guard case let .providerHealth(v) = try await send(.getProviderHealth, 3) else { throw DaemonError.badResponse }; return v }
    func usage() async throws -> UsageResponse { guard case let .usage(v) = try await send(.getUsage, 3) else { throw DaemonError.badResponse }; return v }
    func pendingNotifications() async throws -> [PendingNotification] { guard case let .pendingNotifications(v) = try await send(.getPendingNotifications, 3) else { throw DaemonError.badResponse }; return v }
    func acknowledgeNotifications(_ ids: [Int64]) async throws {
        guard case let .notificationsAcknowledged(acknowledged) = try await send(.acknowledgeNotifications(ids), 3), acknowledged == ids else { throw DaemonError.badResponse }
    }
    func refresh(_ providers: [String]?) async throws -> RefreshResponse { guard case let .refresh(v) = try await send(.refresh(providers), 30) else { throw DaemonError.badResponse }; return v }
    func updateConfig(pollIntervalSeconds: UInt64?, providers: [String: Bool]?, notificationsEnabled: Bool? = nil) async throws -> ConfigResponse {
        guard case let .config(v) = try await send(.updateConfig(pollIntervalSeconds: pollIntervalSeconds, providers: providers, notificationsEnabled: notificationsEnabled), 5) else { throw DaemonError.badResponse }
        return v
    }
    func addProviderAccount(providerId: String, displayName: String?) async throws -> AddProviderAccountResponse {
        guard case let .addProviderAccount(v) = try await send(.addProviderAccount(providerId: providerId, displayName: displayName), 10) else { throw DaemonError.badResponse }
        return v
    }
    func updateAccount(accountId: String, displayName: String? = nil, hidden: Bool? = nil, collectionEnabled: Bool? = nil) async throws -> Account {
        guard case let .account(v) = try await send(.updateAccount(accountId: accountId, displayName: displayName, hidden: hidden, collectionEnabled: collectionEnabled), 5) else { throw DaemonError.badResponse }
        return v
    }
    func removeAccount(accountId: String) async throws -> Account {
        guard case let .account(v) = try await send(.removeAccount(accountId: accountId), 5) else { throw DaemonError.badResponse }
        return v
    }
    func deleteAccount(accountId: String) async throws {
        guard case let .accountDeleted(id) = try await send(.deleteAccount(accountId: accountId), 10), id == accountId else { throw DaemonError.badResponse }
    }
    func providerSetup(providerId: String) async throws -> ProviderSetupResponse {
        guard case let .providerSetup(v) = try await send(.getProviderSetup(providerId: providerId), 20) else { throw DaemonError.badResponse }
        return v
    }
    func updateProviderSetup(providerId: String, workspaceId: String?) async throws -> ProviderSetupResponse {
        guard case let .providerSetup(v) = try await send(.updateProviderSetup(providerId: providerId, workspaceId: workspaceId), 20) else { throw DaemonError.badResponse }
        return v
    }
    func repairProvider(providerId: String, accountId: String?) async throws -> ProviderActionResponse {
        guard case let .providerAction(v) = try await send(.repairProvider(providerId: providerId, accountId: accountId), 10) else { throw DaemonError.badResponse }
        return v
    }
    func launchProviderAccount(accountId: String) async throws -> ProviderActionResponse {
        guard case let .providerAction(v) = try await send(.launchProviderAccount(accountId: accountId), 10) else { throw DaemonError.badResponse }
        return v
    }

    private func send(_ request: DaemonRequest, _ seconds: Double) async throws -> DaemonResponse {
        try Task.checkCancellation()
        let line = try String(decoding: encoder.encode(request) + [10], as: UTF8.self)
        let response = try Socket.line(path: socketPath, request: line, timeout: seconds)
        try Task.checkCancellation()
        let decoded = try decoder.decode(DaemonResponse.self, from: Data(response.utf8))
        if case let .error(error) = decoded { throw DaemonError.api(error.message) }
        return decoded
    }
}
