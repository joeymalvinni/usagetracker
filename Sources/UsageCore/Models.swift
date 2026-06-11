import Foundation

public enum UsageService: String, Codable, CaseIterable, Sendable {
    case codex
    case claude
    case openAI
    case anthropic

    public var displayName: String {
        switch self {
        case .codex:
            "Codex"
        case .claude:
            "Claude"
        case .openAI:
            "OpenAI"
        case .anthropic:
            "Anthropic"
        }
    }
}

public enum SourceKind: String, Codable, CaseIterable, Sendable {
    case openAIAdminAPI = "openai-admin-api"
    case anthropicAdminAPI = "anthropic-admin-api"
    case openAIWeb = "openai-web"
    case claudeWeb = "claude-web"
    case codexLocal = "codex-local"
    case claudeLocal = "claude-local"
    case configured = "configured"

    public var displayName: String {
        switch self {
        case .openAIAdminAPI:
            "openai-api"
        case .anthropicAdminAPI:
            "anthropic-api"
        case .openAIWeb:
            "openai-web"
        case .claudeWeb:
            "web"
        case .codexLocal:
            "local"
        case .claudeLocal:
            "local"
        case .configured:
            "configured"
        }
    }
}

public enum QuotaWindowKind: String, Codable, CaseIterable, Sendable {
    case session
    case daily
    case weekly
    case monthly
    case credits
    case observed

    public var displayName: String {
        switch self {
        case .session:
            "Session"
        case .daily:
            "Daily"
        case .weekly:
            "Weekly"
        case .monthly:
            "Monthly"
        case .credits:
            "Credits"
        case .observed:
            "Observed"
        }
    }
}

public enum UsageUnit: String, Codable, Sendable {
    case tokens
    case usd
    case credits
    case requests
    case sessions
    case messages
    case percent

    public var displayName: String {
        switch self {
        case .tokens:
            "tokens"
        case .usd:
            "USD"
        case .credits:
            "credits"
        case .requests:
            "requests"
        case .sessions:
            "sessions"
        case .messages:
            "messages"
        case .percent:
            "%"
        }
    }
}

public struct UsageEvent: Codable, Equatable, Sendable {
    public var id: String
    public var service: UsageService
    public var sourceKind: SourceKind
    public var accountLabel: String?
    public var model: String?
    public var startedAt: Date
    public var endedAt: Date
    public var inputTokens: Int
    public var outputTokens: Int
    public var cachedInputTokens: Int
    public var requests: Int
    public var costAmount: Decimal?
    public var costCurrency: String?
    public var metadata: [String: String]

    public init(
        id: String,
        service: UsageService,
        sourceKind: SourceKind,
        accountLabel: String? = nil,
        model: String? = nil,
        startedAt: Date,
        endedAt: Date,
        inputTokens: Int = 0,
        outputTokens: Int = 0,
        cachedInputTokens: Int = 0,
        requests: Int = 0,
        costAmount: Decimal? = nil,
        costCurrency: String? = nil,
        metadata: [String: String] = [:]
    ) {
        self.id = id
        self.service = service
        self.sourceKind = sourceKind
        self.accountLabel = accountLabel
        self.model = model
        self.startedAt = startedAt
        self.endedAt = endedAt
        self.inputTokens = inputTokens
        self.outputTokens = outputTokens
        self.cachedInputTokens = cachedInputTokens
        self.requests = requests
        self.costAmount = costAmount
        self.costCurrency = costCurrency
        self.metadata = metadata
    }

    public var totalTokens: Int {
        inputTokens + outputTokens + cachedInputTokens
    }
}

public struct QuotaWindow: Codable, Equatable, Sendable {
    public var id: String
    public var service: UsageService
    public var sourceKind: SourceKind
    public var accountLabel: String?
    public var kind: QuotaWindowKind
    public var startedAt: Date
    public var resetAt: Date?
    public var usedUnits: Double
    public var limitUnits: Double?
    public var unit: UsageUnit
    public var observedAt: Date
    public var metadata: [String: String]

    public init(
        id: String,
        service: UsageService,
        sourceKind: SourceKind,
        accountLabel: String? = nil,
        kind: QuotaWindowKind,
        startedAt: Date,
        resetAt: Date? = nil,
        usedUnits: Double,
        limitUnits: Double? = nil,
        unit: UsageUnit,
        observedAt: Date,
        metadata: [String: String] = [:]
    ) {
        self.id = id
        self.service = service
        self.sourceKind = sourceKind
        self.accountLabel = accountLabel
        self.kind = kind
        self.startedAt = startedAt
        self.resetAt = resetAt
        self.usedUnits = usedUnits
        self.limitUnits = limitUnits
        self.unit = unit
        self.observedAt = observedAt
        self.metadata = metadata
    }
}

public struct ProviderDiagnostic: Codable, Equatable, Sendable {
    public enum Severity: String, Codable, Sendable {
        case info
        case warning
        case error
    }

    public var providerID: String
    public var severity: Severity
    public var message: String

    public init(providerID: String, severity: Severity, message: String) {
        self.providerID = providerID
        self.severity = severity
        self.message = message
    }
}

public struct ProviderCollection: Codable, Equatable, Sendable {
    public var events: [UsageEvent]
    public var windows: [QuotaWindow]
    public var diagnostics: [ProviderDiagnostic]

    public init(
        events: [UsageEvent] = [],
        windows: [QuotaWindow] = [],
        diagnostics: [ProviderDiagnostic] = []
    ) {
        self.events = events
        self.windows = windows
        self.diagnostics = diagnostics
    }
}
