import Foundation

/// Comfort-import toggles mirrored from the daemon's `ImportOptions`.
/// Decoding rides the snake_case-converting decoder; encoding writes explicit
/// snake_case keys because `JSONEncoder.usage` has no key strategy.
struct ImportOptions: Equatable, Sendable, Codable {
    var prefs: Bool
    var projectTrust: Bool
    var promptHistory: Bool
    var plugins: Bool
    var projectTranscripts: Bool
    var fileHistory: Bool
    var tasksTeams: Bool
    var sessions: Bool

    init(
        prefs: Bool = true,
        projectTrust: Bool = true,
        promptHistory: Bool = true,
        plugins: Bool = false,
        projectTranscripts: Bool = false,
        fileHistory: Bool = false,
        tasksTeams: Bool = false,
        sessions: Bool = false
    ) {
        self.prefs = prefs
        self.projectTrust = projectTrust
        self.promptHistory = promptHistory
        self.plugins = plugins
        self.projectTranscripts = projectTranscripts
        self.fileHistory = fileHistory
        self.tasksTeams = tasksTeams
        self.sessions = sessions
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: DecodeKeys.self)
        prefs = try c.decodeIfPresent(Bool.self, forKey: .prefs) ?? true
        projectTrust = try c.decodeIfPresent(Bool.self, forKey: .projectTrust) ?? true
        promptHistory = try c.decodeIfPresent(Bool.self, forKey: .promptHistory) ?? true
        plugins = try c.decodeIfPresent(Bool.self, forKey: .plugins) ?? false
        projectTranscripts = try c.decodeIfPresent(Bool.self, forKey: .projectTranscripts) ?? false
        fileHistory = try c.decodeIfPresent(Bool.self, forKey: .fileHistory) ?? false
        tasksTeams = try c.decodeIfPresent(Bool.self, forKey: .tasksTeams) ?? false
        sessions = try c.decodeIfPresent(Bool.self, forKey: .sessions) ?? false
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: EncodeKeys.self)
        try c.encode(prefs, forKey: .prefs)
        try c.encode(projectTrust, forKey: .projectTrust)
        try c.encode(promptHistory, forKey: .promptHistory)
        if plugins { try c.encode(true, forKey: .plugins) }
        if projectTranscripts { try c.encode(true, forKey: .projectTranscripts) }
        if fileHistory { try c.encode(true, forKey: .fileHistory) }
        if tasksTeams { try c.encode(true, forKey: .tasksTeams) }
        if sessions { try c.encode(true, forKey: .sessions) }
    }

    private enum DecodeKeys: String, CodingKey {
        case prefs, projectTrust, promptHistory, plugins, projectTranscripts
        case fileHistory, tasksTeams, sessions
    }

    private enum EncodeKeys: String, CodingKey {
        case prefs
        case projectTrust = "project_trust"
        case promptHistory = "prompt_history"
        case plugins
        case projectTranscripts = "project_transcripts"
        case fileHistory = "file_history"
        case tasksTeams = "tasks_teams"
        case sessions
    }
}

enum ImportMode: String, Codable, Equatable, Sendable {
    case prefsOnly = "prefs_only"
    case replace
}

struct ImportJob: Codable, Equatable, Sendable {
    let id: String
    let accountId: String
    let providerId: String
    let status: ImportJobStatus
    let mode: ImportMode
    let options: ImportOptions
    let createdAt: Date
    let startedAt, finishedAt: Date?
    let progressMessage, failureMessage: String?

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        accountId = try c.decode(String.self, forKey: .accountId)
        providerId = try c.decode(String.self, forKey: .providerId)
        status = try c.decode(ImportJobStatus.self, forKey: .status)
        mode = try c.decode(ImportMode.self, forKey: .mode)
        options = try c.decode(ImportOptions.self, forKey: .options)
        createdAt = try c.decode(Date.self, forKey: .createdAt)
        startedAt = try c.decodeIfPresent(Date.self, forKey: .startedAt)
        finishedAt = try c.decodeIfPresent(Date.self, forKey: .finishedAt)
        progressMessage = try c.decodeIfPresent(String.self, forKey: .progressMessage)
        failureMessage = try c.decodeIfPresent(String.self, forKey: .failureMessage)
    }

    private enum CodingKeys: String, CodingKey {
        case id, status, mode, options, createdAt, startedAt, finishedAt
        case accountId, providerId, progressMessage, failureMessage
    }
}

enum ImportJobStatus: String, Codable, Equatable, Sendable {
    case queued, running, completed, failed

    var isTerminal: Bool {
        self == .completed || self == .failed
    }
}

struct ImportToggleSize: Codable, Equatable, Sendable {
    let key: String
    let enabledByDefault: Bool
    let supported: Bool
    let bytes: UInt64?
    let note: String?
}

struct AccountImportPreview: Codable, Equatable, Sendable {
    let providerId: String
    let accountId: String
    let sourceHome: String
    let sourceClaudeJson: String
    let destination: String
    let hasManagedConfigDir: Bool
    let sourceIdentity: String?
    let defaultMode: ImportMode
    let defaultOptions: ImportOptions
    let toggles: [ImportToggleSize]
}
