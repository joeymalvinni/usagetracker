import Foundation

struct DaemonClient: Sendable {
    let socketPath: String
    private let transport: any DaemonTransport
    private let refreshPollInterval: Duration
    private let refreshWaitTimeout: Duration

    init(
        socketPath: String,
        transport: any DaemonTransport = POSIXDaemonTransport(),
        refreshPollInterval: Duration = .milliseconds(500),
        refreshWaitTimeout: Duration = .seconds(300)
    ) {
        self.socketPath = socketPath
        self.transport = transport
        self.refreshPollInterval = refreshPollInterval
        self.refreshWaitTimeout = refreshWaitTimeout
    }

    func serverInfo() async throws -> ServerInfo { guard case let .serverInfo(v) = try await send(.getServerInfo) else { throw DaemonError.badResponse }; return v }
    func state() async throws -> StateResponse { guard case let .state(v) = try await send(.getState) else { throw DaemonError.badResponse }; return v }
    func config() async throws -> ConfigResponse { guard case let .config(v) = try await send(.getConfig) else { throw DaemonError.badResponse }; return v }
    func accounts() async throws -> [Account] { guard case let .accounts(v) = try await send(.getAccounts) else { throw DaemonError.badResponse }; return v }
    func health() async throws -> [ProviderHealth] { guard case let .providerHealth(v) = try await send(.getProviderHealth) else { throw DaemonError.badResponse }; return v }
    func usage() async throws -> UsageResponse { guard case let .usage(v) = try await send(.getUsage) else { throw DaemonError.badResponse }; return v }
    func pendingNotifications() async throws -> [PendingNotification] { guard case let .pendingNotifications(v) = try await send(.getPendingNotifications) else { throw DaemonError.badResponse }; return v }
    func acknowledgeNotifications(_ ids: [Int64]) async throws {
        guard case let .notificationsAcknowledged(acknowledged) = try await send(.acknowledgeNotifications(ids)), acknowledged == ids else { throw DaemonError.badResponse }
    }
    func refresh(_ providers: [String]?) async throws -> RefreshResponse {
        guard case let .refreshStarted(job, _) = try await send(.refresh(providers)) else {
            throw DaemonError.badResponse
        }
        let completed = try await waitForRefresh(job)
        guard completed.status != .failed else {
            throw DaemonError.refreshFailed(
                jobId: completed.id,
                message: completed.failureMessage ?? "Refresh job \(completed.id) failed"
            )
        }
        return RefreshResponse(job: completed)
    }
    func updateConfig(pollIntervalSeconds: UInt64?, providers: [String: Bool]?, notifications: NotificationConfig? = nil) async throws -> ConfigResponse {
        guard case let .config(v) = try await send(.updateConfig(pollIntervalSeconds: pollIntervalSeconds, providers: providers, notifications: notifications)) else { throw DaemonError.badResponse }
        return v
    }
    func addProviderAccount(
        providerId: String,
        displayName: String?,
        signInAction: ProviderSignInAction = .open
    ) async throws -> AddProviderAccountResponse {
        guard case let .addProviderAccount(v) = try await send(.addProviderAccount(
            providerId: providerId,
            displayName: displayName,
            signInAction: signInAction
        )) else { throw DaemonError.badResponse }
        return v
    }
    func updateAccount(accountId: String, displayName: String? = nil, hidden: Bool? = nil, collectionEnabled: Bool? = nil) async throws -> Account {
        guard case let .account(v) = try await send(.updateAccount(accountId: accountId, displayName: displayName, hidden: hidden, collectionEnabled: collectionEnabled)) else { throw DaemonError.badResponse }
        return v
    }
    func removeAccount(accountId: String) async throws -> Account {
        guard case let .account(v) = try await send(.removeAccount(accountId: accountId)) else { throw DaemonError.badResponse }
        return v
    }
    func deleteAccount(accountId: String) async throws {
        guard case let .accountDeleted(id) = try await send(.deleteAccount(accountId: accountId)), id == accountId else { throw DaemonError.badResponse }
    }
    func providerSetup(providerId: String) async throws -> ProviderSetupResponse {
        guard case let .providerSetup(v) = try await send(.getProviderSetup(providerId: providerId)) else { throw DaemonError.badResponse }
        return v
    }
    func updateProviderSetup(providerId: String, settings: [String: String?]) async throws -> ProviderSetupResponse {
        guard case let .providerSetup(v) = try await send(.updateProviderSetup(providerId: providerId, settings: settings)) else { throw DaemonError.badResponse }
        return v
    }
    func repairProvider(
        providerId: String,
        accountId: String?,
        signInAction: ProviderSignInAction = .open
    ) async throws -> ProviderActionResponse {
        guard case let .providerAction(v) = try await send(.repairProvider(
            providerId: providerId,
            accountId: accountId,
            signInAction: signInAction
        )) else { throw DaemonError.badResponse }
        return v
    }
    func launchProviderAccount(accountId: String) async throws -> ProviderActionResponse {
        guard case let .providerAction(v) = try await send(.launchProviderAccount(accountId: accountId)) else { throw DaemonError.badResponse }
        return v
    }

    private func send(_ request: DaemonRequest) async throws -> DaemonResponse {
        try Task.checkCancellation()
        let encoder = JSONEncoder.usage
        let line = try String(decoding: encoder.encode(request) + [10], as: UTF8.self)
        let response = try await transport.line(
            path: socketPath,
            request: line,
            timeout: DaemonRequestTimeout.seconds(for: request)
        )
        try Task.checkCancellation()
        let decoder = JSONDecoder.usage
        let decoded = try decoder.decode(DaemonResponse.self, from: Data(response.utf8))
        if case let .error(error) = decoded {
            throw DaemonError.api(code: error.code, message: error.message)
        }
        return decoded
    }

    private func waitForRefresh(_ initialJob: RefreshJob) async throws -> RefreshJob {
        var job = initialJob
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: refreshWaitTimeout)
        while !job.status.isTerminal {
            guard clock.now < deadline else { throw DaemonError.timeout }
            try await Task.sleep(for: refreshPollInterval)
            guard case let .refreshJob(latest) = try await send(.getRefreshJob(job.id)) else {
                throw DaemonError.badResponse
            }
            job = latest
        }
        return job
    }
}

private enum DaemonRequestTimeout {
    static func seconds(for request: DaemonRequest) -> TimeInterval {
        switch request {
        case .getServerInfo, .getState, .getUsage, .getRefreshJob, .getProviderHealth,
             .getAccounts, .getConfig, .getPendingNotifications,
             .acknowledgeNotifications:
            3
        case .updateConfig, .updateAccount, .removeAccount:
            5
        case .addProviderAccount, .deleteAccount, .repairProvider,
             .launchProviderAccount:
            10
        case .getProviderSetup, .updateProviderSetup:
            20
        case .refresh:
            // Starting/coalescing a job is fast; provider work is polled separately.
            10
        }
    }
}
