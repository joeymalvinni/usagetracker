import Foundation

struct DaemonClient {
    let socketPath: String
    private let decoder = JSONDecoder.usage
    private let encoder = JSONEncoder()

    func config() async throws -> ConfigResponse { guard case let .config(v) = try await send(.getConfig, 3) else { throw DaemonError.badResponse }; return v }
    func accounts() async throws -> [Account] { guard case let .accounts(v) = try await send(.getAccounts, 3) else { throw DaemonError.badResponse }; return v }
    func health() async throws -> [ProviderHealth] { guard case let .providerHealth(v) = try await send(.getProviderHealth, 3) else { throw DaemonError.badResponse }; return v }
    func usage() async throws -> [UsageSnapshot] { guard case let .usage(v) = try await send(.getUsage, 3) else { throw DaemonError.badResponse }; return v }
    func refresh(_ providers: [String]?) async throws -> RefreshResponse { guard case let .refresh(v) = try await send(.refresh(providers), 30) else { throw DaemonError.badResponse }; return v }
    func updateConfig(pollIntervalSeconds: UInt64?, providers: [String: Bool]?) async throws -> ConfigResponse {
        guard case let .config(v) = try await send(.updateConfig(pollIntervalSeconds: pollIntervalSeconds, providers: providers), 5) else { throw DaemonError.badResponse }
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
