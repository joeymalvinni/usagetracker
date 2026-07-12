import Foundation

struct ProviderDescriptor: Equatable, Sendable {
    let id: String
    let name: String
    let shortName: String
    let symbol: String
    let supportsMultipleAccounts: Bool

    init(
        id: String,
        name: String,
        shortName: String,
        symbol: String,
        supportsMultipleAccounts: Bool = false
    ) {
        self.id = id
        self.name = name
        self.shortName = shortName
        self.symbol = symbol
        self.supportsMultipleAccounts = supportsMultipleAccounts
    }
}

/// The menu app has provider-specific presentation assets and must therefore
/// opt in to each provider explicitly. Unknown daemon IDs stay off every UI
/// surface until a matching descriptor is added here.
enum ProviderCatalog {
    static let providers: [ProviderDescriptor] = [
        ProviderDescriptor(id: "codex", name: "Codex", shortName: "Cdx", symbol: "terminal", supportsMultipleAccounts: true),
        ProviderDescriptor(id: "claude", name: "Claude", shortName: "Clde", symbol: "sparkles", supportsMultipleAccounts: true),
        ProviderDescriptor(id: "opencode_go", name: "OpenCode Go", shortName: "Go", symbol: "bolt.horizontal"),
        ProviderDescriptor(id: "grok", name: "Grok", shortName: "Grok", symbol: "sparkle", supportsMultipleAccounts: true),
    ]

    static let supportedIDs = providers.map(\.id)
    private static let byID = Dictionary(uniqueKeysWithValues: providers.map { ($0.id, $0) })

    static func supports(_ id: String) -> Bool {
        byID[id] != nil
    }

    static func supportsMultipleAccounts(_ id: String) -> Bool {
        byID[id]?.supportsMultipleAccounts == true
    }

    static func descriptor(for id: String) -> ProviderDescriptor? {
        byID[id]
    }

    static func name(for id: String) -> String {
        byID[id]?.name ?? id
    }

    static func shortName(for id: String) -> String {
        byID[id]?.shortName ?? String(name(for: id).prefix(4))
    }

    static func symbol(for id: String) -> String {
        byID[id]?.symbol ?? "chart.bar"
    }
}
