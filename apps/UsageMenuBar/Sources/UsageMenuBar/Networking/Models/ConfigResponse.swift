import Foundation
struct ConfigResponse: Decodable, Equatable {
    let pollIntervalSeconds: UInt64
    let notifications: NotificationConfig
    let configPath, socketPath, dbPath: String
    let enabledProviders: [String]
    let providers: [String: ProviderToggle]
}

struct ProviderToggle: Codable, Equatable { let enabled: Bool }
struct NotificationConfig: Codable, Equatable {
    var enabled: Bool
    var thresholdsPercentRemaining: [UInt8]
    var resetAlerts: Bool
    var predictiveAlerts: Bool
    var cooldownMinutes: UInt32
    var quietHours: NotificationQuietHours?
    var rules: [NotificationRule]

    init(
        enabled: Bool,
        thresholdsPercentRemaining: [UInt8] = [50, 25, 10, 5, 0],
        resetAlerts: Bool = true,
        predictiveAlerts: Bool = false,
        cooldownMinutes: UInt32 = 15,
        quietHours: NotificationQuietHours? = nil,
        rules: [NotificationRule] = []
    ) {
        self.enabled = enabled
        self.thresholdsPercentRemaining = thresholdsPercentRemaining
        self.resetAlerts = resetAlerts
        self.predictiveAlerts = predictiveAlerts
        self.cooldownMinutes = cooldownMinutes
        self.quietHours = quietHours
        self.rules = rules
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        enabled = try container.decode(Bool.self, forKey: .enabled)
        thresholdsPercentRemaining = try container.decode(
            [UInt8].self,
            forKey: .thresholdsPercentRemaining
        )
        resetAlerts = try container.decode(Bool.self, forKey: .resetAlerts)
        predictiveAlerts = try container.decode(Bool.self, forKey: .predictiveAlerts)
        cooldownMinutes = try container.decode(UInt32.self, forKey: .cooldownMinutes)
        quietHours = try container.decodeIfPresent(
            NotificationQuietHours.self,
            forKey: .quietHours
        )
        rules = try container.decodeIfPresent([NotificationRule].self, forKey: .rules) ?? []
    }

    private enum CodingKeys: String, CodingKey {
        case enabled, thresholdsPercentRemaining, resetAlerts, predictiveAlerts
        case cooldownMinutes, quietHours, rules
    }

    func withEnabled(_ enabled: Bool) -> Self {
        var copy = self
        copy.enabled = enabled
        return copy
    }
}

struct NotificationQuietHours: Codable, Equatable {
    let startHourLocal: UInt8
    let endHourLocal: UInt8
}

struct NotificationRule: Codable, Equatable {
    let accountId: String?
    let windowId: String?
    let enabled: Bool?
    let thresholdsPercentRemaining: [UInt8]?
    let resetAlerts: Bool?
    let predictiveAlerts: Bool?
    let snoozedUntil: Date?
}

struct ApiError: Decodable, Equatable {
    let code, message: String
    let retryable: Bool
}
