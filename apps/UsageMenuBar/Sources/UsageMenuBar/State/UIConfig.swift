import Foundation

enum UIPaths {
    static let root = FileManager.default.homeDirectoryForCurrentUser.appending(path: ".usagetracker")
    static let ui = root.appending(path: "ui")
    static let socket = root.appending(path: "usage.sock")
    static let config = ui.appending(path: "config.json")
}

struct UIConfig: Codable, Equatable {
    enum MenuMetric: String, Codable, CaseIterable {
        case remaining, used
        var label: String { self == .remaining ? "% left" : "% used" }
    }

    var hiddenProviders = Set<String>()
    var providerOrder = [String]()
    var menuMetric = MenuMetric.remaining
    var showProviderLabels = true
    var maxMenuProviders = 2
    var colorByStatus = true

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        hiddenProviders = try c.decodeIfPresent(Set<String>.self, forKey: .hiddenProviders) ?? []
        providerOrder = try c.decodeIfPresent([String].self, forKey: .providerOrder) ?? []
        menuMetric = try c.decodeIfPresent(MenuMetric.self, forKey: .menuMetric) ?? .remaining
        showProviderLabels = try c.decodeIfPresent(Bool.self, forKey: .showProviderLabels) ?? true
        maxMenuProviders = try c.decodeIfPresent(Int.self, forKey: .maxMenuProviders) ?? 2
        colorByStatus = try c.decodeIfPresent(Bool.self, forKey: .colorByStatus) ?? true
    }

    static func load() -> Self {
        guard let data = try? Data(contentsOf: UIPaths.config),
              let config = try? JSONDecoder().decode(Self.self, from: data)
        else { let config = Self(); config.save(); return config }
        return config
    }

    func save() {
        try? FileManager.default.createDirectory(at: UIPaths.ui, withIntermediateDirectories: true)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        if let data = try? encoder.encode(self) { try? data.write(to: UIPaths.config, options: .atomic) }
    }
}