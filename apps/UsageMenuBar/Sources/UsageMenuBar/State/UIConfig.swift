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
    var onboardingCompleted = false
    /// Alert signatures the user has seen (viewed the account). Clears the rail/chip dot.
    var seenAlerts = Set<String>()
    /// Alert signatures whose banner the user has dismissed.
    var dismissedAlerts = Set<String>()
    /// Pricing coverage notices dismissed for the current set of unknown models.
    var dismissedPricingNotices = Set<String>()

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        hiddenProviders = try c.decodeIfPresent(Set<String>.self, forKey: .hiddenProviders) ?? []
        providerOrder = try c.decodeIfPresent([String].self, forKey: .providerOrder) ?? []
        menuMetric = try c.decodeIfPresent(MenuMetric.self, forKey: .menuMetric) ?? .remaining
        showProviderLabels = try c.decodeIfPresent(Bool.self, forKey: .showProviderLabels) ?? true
        maxMenuProviders = try c.decodeIfPresent(Int.self, forKey: .maxMenuProviders) ?? 2
        colorByStatus = try c.decodeIfPresent(Bool.self, forKey: .colorByStatus) ?? true
        // Existing beta users should not be interrupted; newly created configs keep false.
        onboardingCompleted = try c.decodeIfPresent(Bool.self, forKey: .onboardingCompleted) ?? true
        seenAlerts = try c.decodeIfPresent(Set<String>.self, forKey: .seenAlerts) ?? []
        dismissedAlerts = try c.decodeIfPresent(Set<String>.self, forKey: .dismissedAlerts) ?? []
        dismissedPricingNotices = try c.decodeIfPresent(Set<String>.self, forKey: .dismissedPricingNotices) ?? []
    }

    static func load() -> Self {
        guard let data = try? Data(contentsOf: UIPaths.config),
              let config = try? JSONDecoder().decode(Self.self, from: data)
        else { return Self() }
        return config
    }

    func pruningAcknowledgements(
        to liveAlerts: Set<String>,
        pricingNotices livePricingNotices: Set<String>
    ) -> Self {
        var pruned = self
        pruned.seenAlerts.formIntersection(liveAlerts)
        pruned.dismissedAlerts.formIntersection(liveAlerts)
        pruned.dismissedPricingNotices.formIntersection(livePricingNotices)
        return pruned
    }

    func save() {
        try? FileManager.default.createDirectory(at: UIPaths.ui, withIntermediateDirectories: true)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        if let data = try? encoder.encode(self) { try? data.write(to: UIPaths.config, options: .atomic) }
    }
}

actor UIConfigStore {
    func save(_ config: UIConfig) {
        config.save()
    }
}
