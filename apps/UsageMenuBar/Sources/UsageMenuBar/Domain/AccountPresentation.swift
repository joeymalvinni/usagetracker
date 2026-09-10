import Foundation

extension Account {
    /// Use the same recognizable identity in setup, settings, and usage views.
    var displayLabel: String {
        for value in [displayName, email] {
            if let value = value?.trimmingCharacters(in: .whitespacesAndNewlines), !value.isEmpty {
                return value
            }
        }
        let identifier = externalAccountId.trimmingCharacters(in: .whitespacesAndNewlines)
        if identifier.isEmpty { return "Account" }
        if identifier.contains("@") || identifier.count <= 16 { return identifier }
        return "\(identifier.prefix(8))…\(identifier.suffix(4))"
    }
}
