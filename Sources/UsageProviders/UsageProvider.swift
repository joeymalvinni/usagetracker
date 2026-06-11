import Foundation
import UsageCore

public struct ProviderContext: Sendable {
    public var homeDirectory: URL
    public var now: Date

    public init(homeDirectory: URL, now: Date) {
        self.homeDirectory = homeDirectory
        self.now = now
    }
}

public protocol UsageProvider: Sendable {
    var id: String { get }
    var displayName: String { get }
    func collect(context: ProviderContext) async -> ProviderCollection
}

public enum ProviderRegistry {
    public static func defaultProviders(environment: [String: String] = ProcessInfo.processInfo.environment) -> [UsageProvider] {
        [
            CodexUsageProvider(authPath: environment["CODEX_AUTH_FILE"]),
            OpenAIAdminProvider(apiKey: environment["OPENAI_ADMIN_KEY"])
        ]
    }
}
