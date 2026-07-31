import Foundation

/// Human-readable label for a comfort-import toggle key from the preview.
enum ImportToggleKey: String, Sendable {
    case prefs
    case projectTrust = "project_trust"
    case promptHistory = "prompt_history"
    case plugins
    case projectTranscripts = "project_transcripts"
    case fileHistory = "file_history"
    case tasksTeams = "tasks_teams"
    case sessions

    var label: String {
        switch self {
        case .prefs: "Preferences (settings.json)"
        case .projectTrust: "Project trust (.claude.json)"
        case .promptHistory: "Prompt history"
        case .plugins: "Plugins"
        case .projectTranscripts: "Project transcripts"
        case .fileHistory: "File history"
        case .tasksTeams: "Tasks and teams"
        case .sessions: "Sessions"
        }
    }

    init?(wireKey: String) {
        self.init(rawValue: wireKey)
    }
}

/// One row in the import sheet — supported toggles are editable; stretch toggles
/// are shown disabled with an explanatory note from the daemon preview.
struct ImportToggleRow: Equatable, Sendable, Identifiable {
    let key: String
    let label: String
    let supported: Bool
    let note: String?
    let sizeLabel: String?
    var enabled: Bool

    var id: String { key }

    init(toggle: ImportToggleSize, enabled: Bool) {
        key = toggle.key
        label = ImportToggleKey(wireKey: toggle.key)?.label ?? toggle.key
        supported = toggle.supported
        note = toggle.note
        sizeLabel = Self.sizeLabel(for: toggle.bytes)
        self.enabled = enabled
    }

    private static func sizeLabel(for bytes: UInt64?) -> String? {
        guard let bytes else { return nil }
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        return formatter.string(fromByteCount: Int64(bytes))
    }
}

/// Editable state behind the import-from-local-Claude sheet. Pure value type so
/// default toggles, disabled stretch rows, and wire mapping are unit-testable.
struct ImportLocalClaudeModel: Equatable, Sendable {
    let accountId: String
    let accountTitle: String
    let providerId: String
    let sourceHome: String
    let sourceClaudeJson: String
    let destination: String
    let hasManagedConfigDir: Bool
    let sourceIdentity: String?
    var mode: ImportMode
    var toggles: [ImportToggleRow]

    init(
        accountId: String,
        accountTitle: String,
        providerId: String,
        preview: AccountImportPreview
    ) {
        self.accountId = accountId
        self.accountTitle = accountTitle
        self.providerId = providerId
        sourceHome = preview.sourceHome
        sourceClaudeJson = preview.sourceClaudeJson
        destination = preview.destination
        hasManagedConfigDir = preview.hasManagedConfigDir
        sourceIdentity = preview.sourceIdentity
        mode = preview.defaultMode
        toggles = preview.toggles.map { toggle in
            ImportToggleRow(
                toggle: toggle,
                enabled: Self.defaultEnabled(for: toggle, options: preview.defaultOptions)
            )
        }
    }

    var managedProfileError: String? {
        hasManagedConfigDir
            ? nil
            : "This account has no managed profile, so local Claude settings cannot be imported."
    }

    var canImport: Bool { hasManagedConfigDir }

    /// Options sent on Import — only the three PR2 comfort toggles; stretch keys
    /// stay off because the sheet keeps them disabled.
    var wireOptions: ImportOptions {
        ImportOptions(
            prefs: toggleValue(for: ImportToggleKey.prefs.rawValue),
            projectTrust: toggleValue(for: ImportToggleKey.projectTrust.rawValue),
            promptHistory: toggleValue(for: ImportToggleKey.promptHistory.rawValue)
        )
    }

    mutating func setToggle(_ key: String, enabled: Bool) {
        guard let index = toggles.firstIndex(where: { $0.key == key }),
              toggles[index].supported else { return }
        toggles[index].enabled = enabled
    }

    private func toggleValue(for key: String) -> Bool {
        toggles.first(where: { $0.key == key })?.enabled ?? false
    }

    private static func defaultEnabled(for toggle: ImportToggleSize, options: ImportOptions) -> Bool {
        guard toggle.supported else { return toggle.enabledByDefault }
        switch toggle.key {
        case ImportToggleKey.prefs.rawValue: return options.prefs
        case ImportToggleKey.projectTrust.rawValue: return options.projectTrust
        case ImportToggleKey.promptHistory.rawValue: return options.promptHistory
        default: return toggle.enabledByDefault
        }
    }
}

/// Thrown when the import job finishes with `failed` instead of a transport error.
struct ImportLocalClaudeFailure: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}
