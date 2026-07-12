import Foundation

struct ProviderDescriptor: Equatable, Sendable {
    let id: String
    let name: String
    let shortName: String
    let symbol: String
}

/// The menu app has provider-specific presentation assets and must therefore
/// opt in to each provider explicitly. Unknown daemon IDs stay off every UI
/// surface until a matching descriptor is added here.
enum ProviderCatalog {
    static let providers: [ProviderDescriptor] = [
        ProviderDescriptor(id: "codex", name: "Codex", shortName: "Cdx", symbol: "terminal"),
        ProviderDescriptor(id: "claude", name: "Claude", shortName: "Clde", symbol: "sparkles"),
        ProviderDescriptor(id: "opencode_go", name: "OpenCode Go", shortName: "Go", symbol: "bolt.horizontal"),
        ProviderDescriptor(id: "grok", name: "Grok", shortName: "Grok", symbol: "sparkle"),
    ]

    static let supportedIDs = providers.map(\.id)
    private static let byID = Dictionary(uniqueKeysWithValues: providers.map { ($0.id, $0) })

    static func supports(_ id: String) -> Bool {
        byID[id] != nil
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
