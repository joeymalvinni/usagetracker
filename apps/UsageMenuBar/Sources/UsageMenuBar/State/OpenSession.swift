import Foundation

/// Wire values for `--effort`; `systemDefault` sends nothing. Kept in sync
/// with the daemon's closed `LaunchEffort` enum.
enum EffortChoice: String, CaseIterable, Identifiable, Sendable {
    case systemDefault = "default"
    case low, medium, high, xhigh, max

    var id: String { rawValue }
    var label: String { self == .systemDefault ? "Default" : rawValue }
    var wireValue: String? { self == .systemDefault ? nil : rawValue }
}

/// Editable state behind the confirm-on-open sheet. Pure value type so the
/// prefill/trim/wire-mapping rules are unit-testable without UI.
struct OpenSessionModel: Equatable, Sendable {
    let accountId: String
    let accountTitle: String
    let providerId: String
    let hasManagedConfigDir: Bool
    var workingDirectory: String
    var model: String
    var effort: EffortChoice
    var dangerouslySkipPermissions: Bool
    var rememberDangerous: Bool

    init(
        accountId: String,
        accountTitle: String,
        providerId: String,
        settings: AccountLaunchSettingsResponse
    ) {
        self.accountId = accountId
        self.accountTitle = accountTitle
        self.providerId = providerId
        hasManagedConfigDir = settings.hasManagedConfigDir
        workingDirectory = settings.workingDirectory ?? ""
        model = settings.launch?.model ?? ""
        effort = (settings.launch?.effort).flatMap(EffortChoice.init(rawValue:)) ?? .systemDefault
        dangerouslySkipPermissions = settings.launch?.dangerouslySkipPermissions ?? false
        rememberDangerous = false
    }

    var trimmedWorkingDirectory: String {
        workingDirectory.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// The sheet is authoritative: it always sends a full flag object, so
    /// clearing a field here clears the saved preference on a successful open.
    var wireFlags: LaunchFlags {
        let trimmedModel = model.trimmingCharacters(in: .whitespacesAndNewlines)
        return LaunchFlags(
            model: trimmedModel.isEmpty ? nil : trimmedModel,
            effort: effort.wireValue,
            dangerouslySkipPermissions: dangerouslySkipPermissions
        )
    }
}
