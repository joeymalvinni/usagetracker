import Foundation

enum UIPaths {
    static let root: URL = {
        let environment = ProcessInfo.processInfo.environment
        if let override = environment["USAGE_TRACKER_HOME"], !override.isEmpty {
            return URL(fileURLWithPath: override).standardizedFileURL
        }
        let production = FileManager.default.homeDirectoryForCurrentUser.appending(path: ".usagetracker")
        if let fixture = environment["USAGE_TRACKER_FIXTURE"], !fixture.isEmpty {
            return production.appending(path: "fixtures").appending(path: fixture)
        }
        return production
    }()
    static let ui = root.appending(path: "ui")
    static let socket = root.appending(path: "usage.sock")
    static let config = ui.appending(path: "config.json")
}

struct UIConfig: Codable, Equatable {
    static let menuProviderCountRange = 1...2

    enum MenuMetric: String, Codable, CaseIterable {
        case remaining, used
        var label: String { self == .remaining ? "% left" : "% used" }
    }

    /// Individually hidden progress bars (windows), keyed by `providerId|windowId`
    /// with the value holding the window's display label so the Settings restore
    /// list can name a bar that has been filtered out of all live view models.
    var hiddenWindows = [String: String]()
    var providerOrder = [String]()
    var menuMetric = MenuMetric.remaining
    /// Controls provider names in the status item's tooltip. The icon itself is
    /// intentionally label-free so it remains compact in the menu bar.
    var showProviderLabels = true
    /// Nil means the app chooses one or two rows from the number of connected
    /// providers. Setting a value records an explicit user choice.
    var maxMenuProviders: Int?
    var colorByStatus = true
    var darkModeEnabled = true
    var onboardingCompleted = false
    /// Alert signatures the user has seen (viewed the account). Clears the rail/chip dot.
    var seenAlerts = Set<String>()
    /// Alert signatures whose banner the user has dismissed.
    var dismissedAlerts = Set<String>()

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        hiddenWindows = try c.decodeIfPresent([String: String].self, forKey: .hiddenWindows) ?? [:]
        providerOrder = try c.decodeIfPresent([String].self, forKey: .providerOrder) ?? []
        menuMetric = try c.decodeIfPresent(MenuMetric.self, forKey: .menuMetric) ?? .remaining
        showProviderLabels = try c.decodeIfPresent(Bool.self, forKey: .showProviderLabels) ?? true
        maxMenuProviders = try c.decodeIfPresent(Int.self, forKey: .maxMenuProviders).map {
            min(
                max($0, Self.menuProviderCountRange.lowerBound),
                Self.menuProviderCountRange.upperBound
            )
        }
        colorByStatus = try c.decodeIfPresent(Bool.self, forKey: .colorByStatus) ?? true
        darkModeEnabled = try c.decodeIfPresent(Bool.self, forKey: .darkModeEnabled) ?? true
        // Existing beta users should not be interrupted; newly created configs keep false.
        onboardingCompleted = try c.decodeIfPresent(Bool.self, forKey: .onboardingCompleted) ?? true
        seenAlerts = try c.decodeIfPresent(Set<String>.self, forKey: .seenAlerts) ?? []
        dismissedAlerts = try c.decodeIfPresent(Set<String>.self, forKey: .dismissedAlerts) ?? []
    }

    static func load() throws -> Self {
        if ProcessInfo.processInfo.environment["USAGE_TRACKER_FIXTURE"]?.isEmpty == false {
            var config = Self()
            config.onboardingCompleted = true
            return config
        }
        guard FileManager.default.fileExists(atPath: UIPaths.config.path) else { return Self() }
        let data = try Data(contentsOf: UIPaths.config)
        return try JSONDecoder().decode(Self.self, from: data)
    }

    func pruningAcknowledgements(to liveAlerts: Set<String>) -> Self {
        var pruned = self
        pruned.seenAlerts.formIntersection(liveAlerts)
        pruned.dismissedAlerts.formIntersection(liveAlerts)
        return pruned
    }

    func resolvedMenuProviderCount(automaticCount: Int) -> Int {
        min(
            max(maxMenuProviders ?? automaticCount, Self.menuProviderCountRange.lowerBound),
            Self.menuProviderCountRange.upperBound
        )
    }

    func save() throws {
        try FileManager.default.createDirectory(at: UIPaths.ui, withIntermediateDirectories: true)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        let data = try encoder.encode(self)
        try data.write(to: UIPaths.config, options: .atomic)
    }
}

actor UIConfigStore {
    func save(_ config: UIConfig) throws {
        try config.save()
    }
}
