import Foundation
import UsageCore

public struct CodexUsageProvider: UsageProvider {
    public let id = "codex-direct"
    public let displayName = "Codex direct usage API"

    private let authPathOverride: String?
    private let session: URLSession

    public init(authPath: String? = nil, session: URLSession = .shared) {
        self.authPathOverride = authPath
        self.session = session
    }

    public func collect(context: ProviderContext) async -> ProviderCollection {
        do {
            let credentials = try readCredentials(context: context)
            var events: [UsageEvent] = []
            var windows: [QuotaWindow] = []
            var diagnostics: [ProviderDiagnostic] = []

            do {
                let snapshot = try await fetchProfileSnapshot(credentials: credentials)
                events.append(contentsOf: makeDailyEvents(snapshot: snapshot))
                windows.append(contentsOf: makeProfileWindows(snapshot: snapshot, now: context.now))
            } catch {
                diagnostics.append(
                    ProviderDiagnostic(
                        providerID: id,
                        severity: .warning,
                        message: "Codex profile stats unavailable: \(error)"
                    )
                )
            }

            do {
                let snapshot = try await fetchQuotaSnapshot(credentials: credentials)
                windows.append(contentsOf: makeQuotaWindows(snapshot: snapshot, now: context.now))
            } catch {
                diagnostics.append(
                    ProviderDiagnostic(
                        providerID: id,
                        severity: .warning,
                        message: "Codex quota usage unavailable: \(error)"
                    )
                )
            }

            return ProviderCollection(events: events, windows: windows, diagnostics: diagnostics)
        } catch {
            return ProviderCollection(diagnostics: [
                ProviderDiagnostic(
                    providerID: id,
                    severity: .warning,
                    message: "Codex direct usage unavailable: \(error)"
                )
            ])
        }
    }

    private func readCredentials(context: ProviderContext) throws -> Credentials {
        let authURL = authPathOverride.map(URL.init(fileURLWithPath:))
            ?? context.homeDirectory
                .appendingPathComponent(".codex")
                .appendingPathComponent("auth.json")

        let root = try JSONHelpers.object(from: Data(contentsOf: authURL))
        guard let tokens = root["tokens"] as? [String: Any] else {
            throw CodexUsageError.missingCredentials("\(authURL.path) does not contain a tokens object")
        }
        guard let accessToken = JSONHelpers.string(tokens, "access_token"), !accessToken.isEmpty else {
            throw CodexUsageError.missingCredentials("\(authURL.path) does not contain tokens.access_token")
        }
        guard let accountID = JSONHelpers.string(tokens, "account_id"), !accountID.isEmpty else {
            throw CodexUsageError.missingCredentials("\(authURL.path) does not contain tokens.account_id")
        }
        return Credentials(accessToken: accessToken, accountID: accountID)
    }

    private func fetchProfileSnapshot(credentials: Credentials) async throws -> ProfileSnapshot {
        try await ProfileSnapshot(root: fetchObject(path: "/backend-api/wham/profiles/me", credentials: credentials))
    }

    private func fetchQuotaSnapshot(credentials: Credentials) async throws -> QuotaSnapshot {
        try await QuotaSnapshot(root: fetchObject(path: "/backend-api/wham/usage", credentials: credentials))
    }

    private func fetchObject(path: String, credentials: Credentials) async throws -> [String: Any] {
        guard let url = URL(string: "https://chatgpt.com\(path)") else {
            throw ProviderHTTPError.invalidURL(path)
        }

        var request = URLRequest(url: url, timeoutInterval: 8)
        request.setValue("Bearer \(credentials.accessToken)", forHTTPHeaderField: "Authorization")
        request.setValue(credentials.accountID, forHTTPHeaderField: "ChatGPT-Account-Id")
        request.setValue("codex-cli", forHTTPHeaderField: "User-Agent")

        let (data, response) = try await session.data(for: request)
        if let http = response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw ProviderHTTPError.status(http.statusCode, body: String(data: data, encoding: .utf8) ?? "")
        }

        return try JSONHelpers.object(from: data)
    }

    private func makeDailyEvents(snapshot: Snapshot) -> [UsageEvent] {
        snapshot.stats.dailyUsageBuckets.compactMap { bucket in
            guard let start = parseDay(bucket.startDate) else {
                return nil
            }
            let end = Calendar.utc.date(byAdding: .day, value: 1, to: start) ?? start.addingTimeInterval(86_400)
            return UsageEvent(
                id: "codex-direct:daily:\(bucket.startDate)",
                service: .codex,
                sourceKind: .openAIWeb,
                accountLabel: snapshot.accountLabel,
                startedAt: start,
                endedAt: end,
                inputTokens: Int(bucket.tokens),
                metadata: [
                    "source": "chatgpt-wham-profile",
                    "date": bucket.startDate,
                    "token_kind": "total"
                ]
            )
        }
    }

    private func makeProfileWindows(snapshot: Snapshot, now: Date) -> [QuotaWindow] {
        let observedAt = snapshot.metadata.generatedAt ?? now
        let today = snapshot.todayBucket
        let latestWeek = snapshot.stats.weeklyUsageBuckets.last
        var windows: [QuotaWindow] = []

        if let today, let start = parseDay(today.startDate) {
            windows.append(
                window(
                    id: "codex-direct:today",
                    kind: .daily,
                    label: today.startDate == snapshot.metadata.statsAsOf ? "Today" : "Latest day",
                    value: "\(Format.shortNumber(Double(today.tokens))) tokens",
                    detail: today.startDate,
                    usedUnits: Double(today.tokens),
                    startedAt: start,
                    resetAt: Calendar.utc.date(byAdding: .day, value: 1, to: start),
                    observedAt: observedAt,
                    snapshot: snapshot
                )
            )
        }

        if let latestWeek, let start = parseDay(latestWeek.startDate) {
            windows.append(
                window(
                    id: "codex-direct:this-week",
                    kind: .weekly,
                    label: "This week",
                    value: "\(Format.shortNumber(Double(latestWeek.tokens))) tokens",
                    detail: "since \(latestWeek.startDate)",
                    usedUnits: Double(latestWeek.tokens),
                    startedAt: start,
                    resetAt: Calendar.utc.date(byAdding: .day, value: 7, to: start),
                    observedAt: observedAt,
                    snapshot: snapshot
                )
            )
        }

        windows.append(
            window(
                id: "codex-direct:lifetime",
                kind: .observed,
                label: "Lifetime",
                value: "\(Format.shortNumber(Double(snapshot.stats.lifetimeTokens))) tokens",
                detail: snapshot.metadata.statsAsOf.map { "as of \($0)" },
                usedUnits: Double(snapshot.stats.lifetimeTokens),
                observedAt: observedAt,
                snapshot: snapshot
            )
        )

        if snapshot.stats.peakDailyTokens > 0 {
            windows.append(
                window(
                    id: "codex-direct:peak-day",
                    kind: .observed,
                    label: "Peak day",
                    value: "\(Format.shortNumber(Double(snapshot.stats.peakDailyTokens))) tokens",
                    usedUnits: Double(snapshot.stats.peakDailyTokens),
                    observedAt: observedAt,
                    snapshot: snapshot
                )
            )
        }

        if snapshot.stats.currentStreakDays > 0 {
            let detail = snapshot.stats.longestStreakDays > 0
                ? "best \(snapshot.stats.longestStreakDays)d"
                : nil
            windows.append(
                window(
                    id: "codex-direct:streak",
                    kind: .observed,
                    label: "Streak",
                    value: "\(snapshot.stats.currentStreakDays)d current",
                    detail: detail,
                    usedUnits: Double(snapshot.stats.currentStreakDays),
                    unit: .sessions,
                    observedAt: observedAt,
                    snapshot: snapshot
                )
            )
        }

        if snapshot.stats.totalThreads > 0 {
            windows.append(
                window(
                    id: "codex-direct:threads",
                    kind: .observed,
                    label: "Threads",
                    value: Format.shortNumber(Double(snapshot.stats.totalThreads)),
                    usedUnits: Double(snapshot.stats.totalThreads),
                    unit: .sessions,
                    observedAt: observedAt,
                    snapshot: snapshot
                )
            )
        }

        if snapshot.stats.longestRunningTurnSeconds > 0 {
            windows.append(
                window(
                    id: "codex-direct:longest-task",
                    kind: .observed,
                    label: "Longest task",
                    value: Format.duration(seconds: snapshot.stats.longestRunningTurnSeconds),
                    usedUnits: Double(snapshot.stats.longestRunningTurnSeconds),
                    unit: .sessions,
                    observedAt: observedAt,
                    snapshot: snapshot
                )
            )
        }

        if snapshot.stats.totalSkillsUsed > 0 {
            let detail = snapshot.stats.uniqueSkillsUsed > 0
                ? "\(snapshot.stats.uniqueSkillsUsed) unique"
                : nil
            windows.append(
                window(
                    id: "codex-direct:skills",
                    kind: .observed,
                    label: "Skills",
                    value: "\(Format.shortNumber(Double(snapshot.stats.totalSkillsUsed))) uses",
                    detail: detail,
                    usedUnits: Double(snapshot.stats.totalSkillsUsed),
                    unit: .requests,
                    observedAt: observedAt,
                    snapshot: snapshot
                )
            )
        }

        if let effort = snapshot.stats.mostUsedReasoningEffort, !effort.isEmpty {
            let detail = snapshot.stats.mostUsedReasoningEffortPercentage.map { "\(Format.percent($0 / 100)) of turns" }
            windows.append(
                window(
                    id: "codex-direct:reasoning-effort",
                    kind: .observed,
                    label: "Reasoning",
                    value: effort,
                    detail: detail,
                    usedUnits: snapshot.stats.mostUsedReasoningEffortPercentage ?? 0,
                    unit: .percent,
                    observedAt: observedAt,
                    snapshot: snapshot
                )
            )
        }

        return windows
    }

    private func makeQuotaWindows(snapshot: QuotaSnapshot, now: Date) -> [QuotaWindow] {
        var windows: [QuotaWindow] = []

        if let primary = snapshot.rateLimit.primaryWindow {
            windows.append(
                quotaWindow(
                    id: "codex-direct:quota:primary",
                    label: "Session",
                    kind: .session,
                    sourceName: "chatgpt-wham-usage",
                    window: primary,
                    snapshot: snapshot,
                    now: now
                )
            )
        }

        if let secondary = snapshot.rateLimit.secondaryWindow {
            windows.append(
                quotaWindow(
                    id: "codex-direct:quota:secondary",
                    label: "Weekly",
                    kind: .weekly,
                    sourceName: "chatgpt-wham-usage",
                    window: secondary,
                    snapshot: snapshot,
                    now: now
                )
            )
        }

        for additionalLimit in snapshot.additionalRateLimits {
            if let primary = additionalLimit.rateLimit.primaryWindow {
                windows.append(
                    quotaWindow(
                        id: "codex-direct:quota:\(additionalLimit.idFragment):primary",
                        label: "\(additionalLimit.displayName) session",
                        kind: .session,
                        sourceName: "chatgpt-wham-usage-additional",
                        window: primary,
                        snapshot: snapshot,
                        now: now,
                        showPace: false,
                        extraMetadata: additionalLimit.metadata
                    )
                )
            }

            if let secondary = additionalLimit.rateLimit.secondaryWindow {
                windows.append(
                    quotaWindow(
                        id: "codex-direct:quota:\(additionalLimit.idFragment):secondary",
                        label: "\(additionalLimit.displayName) weekly",
                        kind: .weekly,
                        sourceName: "chatgpt-wham-usage-additional",
                        window: secondary,
                        snapshot: snapshot,
                        now: now,
                        showPace: false,
                        extraMetadata: additionalLimit.metadata
                    )
                )
            }
        }

        if let credits = snapshot.credits, credits.shouldDisplay {
            let detail: String? = if credits.unlimited {
                "unlimited"
            } else if credits.overageLimitReached {
                "overage limit reached"
            } else if !credits.hasCredits {
                "none available"
            } else {
                nil
            }
            var metadata = quotaMetadata(
                label: "Credits",
                sourceName: "chatgpt-wham-usage",
                snapshot: snapshot,
                showPace: false
            )
            metadata["value"] = credits.displayBalance
            if let detail {
                metadata["detail"] = detail
            }
            windows.append(
                QuotaWindow(
                    id: "codex-direct:quota:credits",
                    service: .codex,
                    sourceKind: .openAIWeb,
                    accountLabel: snapshot.accountLabel,
                    kind: .credits,
                    startedAt: now,
                    usedUnits: credits.balanceValue ?? 0,
                    unit: .credits,
                    observedAt: now,
                    metadata: metadata
                )
            )
        }

        return windows
    }

    private func window(
        id: String,
        kind: QuotaWindowKind,
        label: String,
        value: String,
        detail: String? = nil,
        usedUnits: Double,
        unit: UsageUnit = .tokens,
        startedAt: Date? = nil,
        resetAt: Date? = nil,
        observedAt: Date,
        snapshot: Snapshot
    ) -> QuotaWindow {
        var metadata = [
            "title": "Codex · direct",
            "source": "chatgpt-wham-profile",
            "showPace": "false",
            "label": label,
            "value": value
        ]
        if let detail {
            metadata["detail"] = detail
        }
        if let statsError = snapshot.metadata.statsError {
            metadata["stats_error"] = statsError
        }

        return QuotaWindow(
            id: id,
            service: .codex,
            sourceKind: .openAIWeb,
            accountLabel: snapshot.accountLabel,
            kind: kind,
            startedAt: startedAt ?? observedAt,
            resetAt: resetAt,
            usedUnits: usedUnits,
            unit: unit,
            observedAt: observedAt,
            metadata: metadata
        )
    }

    private func quotaWindow(
        id: String,
        label: String,
        kind: QuotaWindowKind,
        sourceName: String,
        window: UsageLimitWindow,
        snapshot: QuotaSnapshot,
        now: Date,
        showPace: Bool = true,
        extraMetadata: [String: String] = [:]
    ) -> QuotaWindow {
        let resetAt = window.resetAt ?? window.resetAfterSeconds.map { now.addingTimeInterval($0) }
        let startedAt = startDate(for: window, resetAt: resetAt, now: now)
        var metadata = quotaMetadata(label: label, sourceName: sourceName, snapshot: snapshot, showPace: showPace)
        for (key, value) in extraMetadata {
            metadata[key] = value
        }
        if let usedPercent = window.usedPercent {
            metadata["used_percent"] = Format.shortNumber(usedPercent)
        }
        if let resetAfterSeconds = window.resetAfterSeconds {
            metadata["reset_after_seconds"] = "\(Int(resetAfterSeconds.rounded()))"
        }
        if window.limitReached {
            metadata["limit_reached"] = "true"
        }

        return QuotaWindow(
            id: id,
            service: .codex,
            sourceKind: .openAIWeb,
            accountLabel: snapshot.accountLabel,
            kind: kind,
            startedAt: startedAt,
            resetAt: resetAt,
            usedUnits: window.usedPercent ?? 0,
            limitUnits: 100,
            unit: .percent,
            observedAt: now,
            metadata: metadata
        )
    }

    private func quotaMetadata(
        label: String,
        sourceName: String,
        snapshot: QuotaSnapshot,
        showPace: Bool
    ) -> [String: String] {
        var metadata = [
            "title": "Codex · openai-web",
            "source": sourceName,
            "label": label,
            "showPace": showPace ? "true" : "false"
        ]
        if let planType = snapshot.planType {
            metadata["plan"] = planType
        }
        if let rateLimitReachedType = snapshot.rateLimitReachedType {
            metadata["rate_limit_reached_type"] = rateLimitReachedType
        }
        if !snapshot.rateLimit.allowed {
            metadata["allowed"] = "false"
        }
        return metadata
    }

    private func startDate(for window: UsageLimitWindow, resetAt: Date?, now: Date) -> Date {
        if let resetAt, let limitWindowSeconds = window.limitWindowSeconds {
            return resetAt.addingTimeInterval(-limitWindowSeconds)
        }
        if let limitWindowSeconds = window.limitWindowSeconds,
           let resetAfterSeconds = window.resetAfterSeconds {
            return now.addingTimeInterval(resetAfterSeconds - limitWindowSeconds)
        }
        return now
    }

    private func parseDay(_ value: String) -> Date? {
        JSONHelpers.parseDay(value, calendar: .utc)
    }
}

private struct Credentials {
    var accessToken: String
    var accountID: String
}

private typealias Snapshot = ProfileSnapshot

private struct ProfileSnapshot {
    var profile: Profile
    var stats: Stats
    var metadata: Metadata

    var accountLabel: String? {
        switch (profile.displayName, profile.username) {
        case let (.some(displayName), .some(username)) where !displayName.isEmpty && !username.isEmpty:
            "\(displayName) (@\(username))"
        case let (.some(displayName), _):
            displayName
        case let (_, .some(username)):
            "@\(username)"
        default:
            nil
        }
    }

    var todayBucket: UsageBucket? {
        if let statsAsOf = metadata.statsAsOf,
           let matching = stats.dailyUsageBuckets.last(where: { $0.startDate == statsAsOf }) {
            return matching
        }
        return stats.dailyUsageBuckets.max { lhs, rhs in
            lhs.startDate < rhs.startDate
        }
    }

    init(root: [String: Any]) throws {
        guard let profile = root["profile"] as? [String: Any] else {
            throw CodexUsageError.invalidResponse("missing profile")
        }
        guard let stats = root["stats"] as? [String: Any] else {
            throw CodexUsageError.invalidResponse("missing stats")
        }
        let metadata = root["metadata"] as? [String: Any] ?? [:]
        self.profile = Profile(raw: profile)
        self.stats = Stats(raw: stats)
        self.metadata = Metadata(raw: metadata)
    }
}

private struct QuotaSnapshot {
    var userID: String?
    var accountID: String?
    var email: String?
    var planType: String?
    var rateLimit: UsageRateLimit
    var additionalRateLimits: [AdditionalRateLimit]
    var credits: Credits?
    var rateLimitReachedType: String?

    var accountLabel: String? {
        email ?? accountID ?? userID
    }

    init(root: [String: Any]) {
        userID = JSONHelpers.string(root, "user_id")
        accountID = JSONHelpers.string(root, "account_id")
        email = JSONHelpers.string(root, "email")
        planType = JSONHelpers.string(root, "plan_type")
        rateLimit = UsageRateLimit(raw: root["rate_limit"] as? [String: Any] ?? [:])
        additionalRateLimits = (root["additional_rate_limits"] as? [[String: Any]] ?? []).map(AdditionalRateLimit.init(raw:))
        credits = (root["credits"] as? [String: Any]).map(Credits.init(raw:))
        rateLimitReachedType = JSONHelpers.string(root, "rate_limit_reached_type")
    }
}

private struct UsageRateLimit {
    var allowed: Bool
    var limitReached: Bool
    var primaryWindow: UsageLimitWindow?
    var secondaryWindow: UsageLimitWindow?

    init(raw: [String: Any]) {
        allowed = JSONHelpers.bool(raw, "allowed") ?? true
        limitReached = JSONHelpers.bool(raw, "limit_reached") ?? false
        primaryWindow = (raw["primary_window"] as? [String: Any]).map { UsageLimitWindow(raw: $0, parentLimitReached: limitReached) }
        secondaryWindow = (raw["secondary_window"] as? [String: Any]).map { UsageLimitWindow(raw: $0, parentLimitReached: limitReached) }
    }
}

private struct UsageLimitWindow {
    var usedPercent: Double?
    var limitWindowSeconds: Double?
    var resetAfterSeconds: Double?
    var resetAt: Date?
    var limitReached: Bool

    init(raw: [String: Any], parentLimitReached: Bool) {
        usedPercent = JSONHelpers.double(raw, "used_percent")
        limitWindowSeconds = JSONHelpers.double(raw, "limit_window_seconds")
        resetAfterSeconds = JSONHelpers.double(raw, "reset_after_seconds")
        resetAt = JSONHelpers.double(raw, "reset_at").map(Date.init(timeIntervalSince1970:))
        limitReached = JSONHelpers.bool(raw, "limit_reached") ?? parentLimitReached
    }
}

private struct AdditionalRateLimit {
    var limitName: String
    var meteredFeature: String?
    var rateLimit: UsageRateLimit

    var displayName: String {
        let base = limitName
            .replacingOccurrences(of: "GPT-5.3-Codex-", with: "")
            .replacingOccurrences(of: "Codex-", with: "")
        return base.isEmpty ? "Additional" : base
    }

    var idFragment: String {
        let raw = meteredFeature ?? limitName
        let scalars = raw.unicodeScalars.map { scalar in
            CharacterSet.alphanumerics.contains(scalar) ? Character(scalar).lowercased() : "-"
        }
        let collapsed = scalars.joined()
            .split(separator: "-")
            .joined(separator: "-")
        return collapsed.isEmpty ? "additional" : collapsed
    }

    var metadata: [String: String] {
        var metadata = ["limit_name": limitName]
        if let meteredFeature {
            metadata["metered_feature"] = meteredFeature
        }
        return metadata
    }

    init(raw: [String: Any]) {
        limitName = JSONHelpers.string(raw, "limit_name") ?? "Additional"
        meteredFeature = JSONHelpers.string(raw, "metered_feature")
        rateLimit = UsageRateLimit(raw: raw["rate_limit"] as? [String: Any] ?? [:])
    }
}

private struct Credits {
    var hasCredits: Bool
    var unlimited: Bool
    var overageLimitReached: Bool
    var balance: String?
    var balanceValue: Double?

    var shouldDisplay: Bool {
        hasCredits || unlimited || balance != nil
    }

    var displayBalance: String {
        if unlimited {
            return "unlimited"
        }
        return balance ?? "0"
    }

    init(raw: [String: Any]) {
        hasCredits = JSONHelpers.bool(raw, "has_credits") ?? false
        unlimited = JSONHelpers.bool(raw, "unlimited") ?? false
        overageLimitReached = JSONHelpers.bool(raw, "overage_limit_reached") ?? false
        balance = JSONHelpers.string(raw, "balance")
        balanceValue = JSONHelpers.double(raw, "balance")
    }
}

private struct Profile {
    var username: String?
    var displayName: String?

    init(raw: [String: Any]) {
        username = JSONHelpers.string(raw, "username")
        displayName = JSONHelpers.string(raw, "display_name")
    }
}

private struct Stats {
    var lifetimeTokens: Int64
    var peakDailyTokens: Int64
    var currentStreakDays: Int
    var longestStreakDays: Int
    var totalThreads: Int
    var longestRunningTurnSeconds: Int
    var totalSkillsUsed: Int
    var uniqueSkillsUsed: Int
    var mostUsedReasoningEffort: String?
    var mostUsedReasoningEffortPercentage: Double?
    var dailyUsageBuckets: [UsageBucket]
    var weeklyUsageBuckets: [UsageBucket]

    init(raw: [String: Any]) {
        lifetimeTokens = int64(raw, "lifetime_tokens")
        peakDailyTokens = int64(raw, "peak_daily_tokens")
        currentStreakDays = JSONHelpers.int(raw, "current_streak_days")
        longestStreakDays = JSONHelpers.int(raw, "longest_streak_days")
        totalThreads = JSONHelpers.int(raw, "total_threads")
        longestRunningTurnSeconds = JSONHelpers.int(raw, "longest_running_turn_sec")
        totalSkillsUsed = JSONHelpers.int(raw, "total_skills_used")
        uniqueSkillsUsed = JSONHelpers.int(raw, "unique_skills_used")
        mostUsedReasoningEffort = JSONHelpers.string(raw, "most_used_reasoning_effort")
        mostUsedReasoningEffortPercentage = JSONHelpers.double(raw, "most_used_reasoning_effort_percentage")
        dailyUsageBuckets = usageBuckets(raw["daily_usage_buckets"])
        weeklyUsageBuckets = usageBuckets(raw["weekly_usage_buckets"])
    }
}

private struct Metadata {
    var statsAsOf: String?
    var generatedAt: Date?
    var statsError: String?

    init(raw: [String: Any]) {
        statsAsOf = JSONHelpers.string(raw, "stats_as_of")
        generatedAt = JSONHelpers.string(raw, "generated_at").flatMap(JSONHelpers.parseISODate)
        statsError = JSONHelpers.string(raw, "stats_error")
    }
}

private struct UsageBucket {
    var startDate: String
    var tokens: Int64
}

private enum CodexUsageError: Error, CustomStringConvertible {
    case missingCredentials(String)
    case invalidResponse(String)

    var description: String {
        switch self {
        case let .missingCredentials(message):
            message
        case let .invalidResponse(message):
            "invalid response: \(message)"
        }
    }
}

private extension Calendar {
    static let utc: Calendar = {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(secondsFromGMT: 0) ?? .current
        return calendar
    }()
}

private func usageBuckets(_ value: Any?) -> [UsageBucket] {
    let rawBuckets = value as? [[String: Any]] ?? []
    return rawBuckets.compactMap { raw in
        guard let startDate = JSONHelpers.string(raw, "start_date") else {
            return nil
        }
        return UsageBucket(startDate: startDate, tokens: int64(raw, "tokens"))
    }
}

private func int64(_ dictionary: [String: Any], _ key: String) -> Int64 {
    if let int = dictionary[key] as? Int {
        return Int64(int)
    }
    if let int64 = dictionary[key] as? Int64 {
        return int64
    }
    if let double = dictionary[key] as? Double {
        return Int64(double)
    }
    if let string = dictionary[key] as? String, let int64 = Int64(string) {
        return int64
    }
    return 0
}
