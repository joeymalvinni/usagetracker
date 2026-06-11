import Foundation

public struct UsageCard: Codable, Equatable, Sendable {
    public var title: String
    public var accountLabel: String?
    public var rows: [UsageRow]

    public init(title: String, accountLabel: String? = nil, rows: [UsageRow]) {
        self.title = title
        self.accountLabel = accountLabel
        self.rows = rows
    }
}

public struct UsageRow: Codable, Equatable, Sendable {
    public var label: String
    public var value: String
    public var bar: String?
    public var detail: String?

    public init(label: String, value: String, bar: String? = nil, detail: String? = nil) {
        self.label = label
        self.value = value
        self.bar = bar
        self.detail = detail
    }
}

public enum UsageSnapshotBuilder {
    public static func buildCards(events: [UsageEvent], windows: [QuotaWindow], now: Date) -> [UsageCard] {
        let serviceOrder: [UsageService] = [.codex, .claude, .openAI, .anthropic]
        let hasDirectCodexStats = windows.contains(where: isDirectCodexStatWindow)
        var cards: [UsageCard] = []

        if let overview = overviewCard(from: windows) {
            cards.append(overview)
        }

        if let activity = activityCard(from: events) {
            cards.append(activity)
        }

        for service in serviceOrder {
            let serviceEvents = events.filter {
                $0.service == service
                    && !isDirectCodexDailyEvent($0)
                    && !(hasDirectCodexStats && service == .codex && $0.sourceKind == .codexLocal)
            }
            let serviceWindows = windows.filter {
                $0.service == service
                    && !isDirectCodexStatWindow($0)
                    && !(hasDirectCodexStats && service == .codex && $0.sourceKind == .configured)
            }
            if serviceEvents.isEmpty && serviceWindows.isEmpty {
                continue
            }

            let visibleWindows = preferredWindows(serviceWindows)
            let title = title(for: service, events: serviceEvents, windows: visibleWindows)
            let accountLabel = visibleWindows.first(where: { $0.accountLabel != nil })?.accountLabel
                ?? serviceEvents.first(where: { $0.accountLabel != nil })?.accountLabel

            var rows = visibleWindows
                .sorted(by: windowSort)
                .map { row(for: $0, now: now) }

            if rows.isEmpty {
                rows = observedRows(for: serviceEvents, now: now)
            }

            if let paceWindow = paceWindow(from: visibleWindows) {
                rows.append(UsageRow(label: "Pace", value: PaceEngine.project(window: paceWindow, now: now).summary))
            }

            if let accountLabel {
                rows.append(UsageRow(label: "Account", value: accountLabel))
            }

            if let plan = visibleWindows.first(where: { $0.metadata["plan"] != nil })?.metadata["plan"] {
                rows.append(UsageRow(label: "Plan", value: plan))
            }

            cards.append(UsageCard(title: title, accountLabel: accountLabel, rows: rows))
        }

        return cards
    }

    private static func overviewCard(from windows: [QuotaWindow]) -> UsageCard? {
        let stats = windows.filter(isDirectCodexStatWindow)
        guard !stats.isEmpty else {
            return nil
        }

        let lifetime = value(for: "Lifetime", in: stats)
        let peak = value(for: "Peak day", in: stats)
        let longestTask = value(for: "Longest task", in: stats)
        let streakWindow = stats.first { $0.metadata["label"] == "Streak" }
        let currentStreak = streakWindow.map { "\(Int($0.usedUnits.rounded())) days" }
        let longestStreak = streakWindow?.metadata["detail"]?
            .replacingOccurrences(of: "best ", with: "")
            .replacingOccurrences(of: "d", with: " days")

        var rows: [UsageRow] = []
        if let lifetime {
            rows.append(UsageRow(label: "Lifetime tokens", value: lifetime.replacingOccurrences(of: " tokens", with: ""), detail: pair("Peak tokens", peak)))
        }
        if let longestTask {
            rows.append(UsageRow(label: "Longest task", value: longestTask, detail: pair("Current streak", currentStreak)))
        }
        if let longestStreak {
            rows.append(UsageRow(label: "Longest streak", value: longestStreak))
        }

        return rows.isEmpty ? nil : UsageCard(title: "Overview", rows: rows)
    }

    private static func activityCard(from events: [UsageEvent]) -> UsageCard? {
        let dailyEvents = events
            .filter(isDirectCodexDailyEvent)
            .sorted { $0.startedAt < $1.startedAt }
            .suffix(7)
        guard !dailyEvents.isEmpty else {
            return nil
        }

        let maxTokens = max(Double(dailyEvents.map(\.totalTokens).max() ?? 0), 1)
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = TimeZone(secondsFromGMT: 0)
        formatter.dateFormat = "EEE"

        let rows = dailyEvents.map { event in
            let tokens = Double(event.totalTokens)
            let filled = max(tokens > 0 ? 1 : 0, min(Int((tokens / maxTokens * 12).rounded()), 12))
            let bar = String(repeating: "█", count: filled) + String(repeating: "░", count: 12 - filled)
            return UsageRow(
                label: formatter.string(from: event.startedAt),
                value: Format.activityNumber(tokens),
                bar: bar
            )
        }

        return UsageCard(title: "Activity · last 7 days", rows: rows)
    }

    private static func value(for label: String, in windows: [QuotaWindow]) -> String? {
        windows.first { $0.metadata["label"] == label }?.metadata["value"]
    }

    private static func pair(_ label: String, _ value: String?) -> String? {
        guard let value else {
            return nil
        }
        return label.padding(toLength: 16, withPad: " ", startingAt: 0)
            + value.replacingOccurrences(of: " tokens", with: "")
    }

    private static func isDirectCodexStatWindow(_ window: QuotaWindow) -> Bool {
        guard window.service == .codex,
              window.metadata["source"] == "chatgpt-wham-profile"
        else {
            return false
        }

        return switch window.metadata["label"] {
        case "Today", "Latest day", "This week", "Lifetime", "Peak day", "Streak", "Threads", "Longest task", "Skills", "Reasoning":
            true
        default:
            false
        }
    }

    private static func isDirectCodexDailyEvent(_ event: UsageEvent) -> Bool {
        event.service == .codex
            && event.metadata["source"] == "chatgpt-wham-profile"
            && event.metadata["token_kind"] == "total"
    }

    private static func row(for window: QuotaWindow, now: Date) -> UsageRow {
        let label = window.metadata["label"] ?? window.kind.displayName
        let detail = window.metadata["detail"]
            ?? Format.resetText(resetAt: window.resetAt, now: now)
            ?? window.metadata["resetDescription"]

        if let value = window.metadata["value"] {
            return UsageRow(
                label: label,
                value: value,
                bar: window.metadata["bar"],
                detail: detail
            )
        }

        let projection = PaceEngine.project(window: window, now: now)
        if let leftFraction = projection.leftFraction {
            return UsageRow(
                label: label,
                value: "\(Format.percent(leftFraction)) left",
                bar: Format.bar(leftFraction: leftFraction),
                detail: detail
            )
        }

        return UsageRow(
            label: label,
            value: "\(Format.shortNumber(window.usedUnits)) \(window.unit.displayName)",
            bar: nil,
            detail: detail
        )
    }

    private static func preferredWindows(_ windows: [QuotaWindow]) -> [QuotaWindow] {
        let hasLiveWindows = windows.contains { $0.sourceKind != .configured }
        let candidates = hasLiveWindows ? windows.filter { $0.sourceKind != .configured } : windows
        let grouped = Dictionary(grouping: candidates) { window in
            "\(window.kind.rawValue):\(window.metadata["label"] ?? "")"
        }
        return grouped.values.compactMap { group in
            group.max { lhs, rhs in
                if lhs.observedAt == rhs.observedAt {
                    return sourceRank(lhs.sourceKind) < sourceRank(rhs.sourceKind)
                }
                return lhs.observedAt < rhs.observedAt
            }
        }
    }

    private static func sourceRank(_ source: SourceKind) -> Int {
        switch source {
        case .openAIWeb, .claudeWeb:
            4
        case .openAIAdminAPI, .anthropicAdminAPI:
            3
        case .codexLocal, .claudeLocal:
            2
        case .configured:
            1
        }
    }

    private static func paceWindow(from windows: [QuotaWindow]) -> QuotaWindow? {
        let candidates = windows.filter {
            $0.limitUnits != nil
                && $0.resetAt != nil
                && $0.metadata["showPace"] != "false"
                && $0.kind != .session
                && $0.kind != .credits
        }
        return candidates.first(where: { $0.kind == .weekly })
            ?? candidates.first(where: { $0.kind == .monthly })
            ?? candidates.max { lhs, rhs in
                (lhs.resetAt ?? lhs.startedAt) < (rhs.resetAt ?? rhs.startedAt)
            }
    }

    private static func observedRows(for events: [UsageEvent], now: Date) -> [UsageRow] {
        let calendar = Calendar.current
        let todayStart = calendar.startOfDay(for: now)
        let weekStart = calendar.dateInterval(of: .weekOfYear, for: now)?.start ?? todayStart

        let todayEvents = events.filter { $0.startedAt >= todayStart }
        let weekEvents = events.filter { $0.startedAt >= weekStart }
        let todayTokens = todayEvents.reduce(0) { $0 + $1.totalTokens }
        let weekTokens = weekEvents.reduce(0) { $0 + $1.totalTokens }
        let todayRequests = todayEvents.reduce(0) { $0 + $1.requests }
        let weekRequests = weekEvents.reduce(0) { $0 + $1.requests }
        let recentTokens = events.reduce(0) { $0 + $1.totalTokens }
        let recentRequests = events.reduce(0) { $0 + $1.requests }

        let todayValue = todayTokens > 0
            ? "\(Format.shortNumber(Double(todayTokens))) tokens"
            : "\(Format.shortNumber(Double(todayRequests))) requests"
        let weekValue = weekTokens > 0
            ? "\(Format.shortNumber(Double(weekTokens))) tokens"
            : "\(Format.shortNumber(Double(weekRequests))) requests"

        var rows = [
            UsageRow(label: "Today", value: todayValue, detail: "observed"),
            UsageRow(label: "Weekly", value: weekValue, detail: "observed")
        ]
        if weekTokens == 0, weekRequests == 0, recentTokens + recentRequests > 0 {
            let recentValue = recentTokens > 0
                ? "\(Format.shortNumber(Double(recentTokens))) tokens"
                : "\(Format.shortNumber(Double(recentRequests))) requests"
            rows.append(UsageRow(label: "Recent", value: recentValue, detail: "cached"))
        }
        return rows
    }

    private static func title(for service: UsageService, events: [UsageEvent], windows: [QuotaWindow]) -> String {
        if let configuredTitle = windows.first(where: { $0.metadata["title"] != nil })?.metadata["title"] {
            return configuredTitle
        }

        let source = windows.first?.sourceKind ?? events.first?.sourceKind
        let model = events.last(where: { $0.model != nil })?.model

        switch (source, model) {
        case let (.some(source), .some(model)):
            return "\(service.displayName) · \(source.displayName) · \(model)"
        case let (.some(source), .none):
            return "\(service.displayName) · \(source.displayName)"
        default:
            return service.displayName
        }
    }

    private static func windowSort(_ lhs: QuotaWindow, _ rhs: QuotaWindow) -> Bool {
        rowOrder(lhs) < rowOrder(rhs)
    }

    private static func rowOrder(_ window: QuotaWindow) -> Int {
        switch window.metadata["label"] {
        case "Session":
            1
        case "Weekly":
            2
        case let label where label?.hasSuffix(" session") == true:
            3
        case let label where label?.hasSuffix(" weekly") == true:
            4
        case "Credits":
            5
        case "Today", "Latest day":
            10
        case "This week":
            20
        case "Lifetime":
            30
        case "Peak day":
            40
        case "Streak":
            50
        case "Threads":
            60
        case "Skills":
            70
        case "Reasoning":
            80
        default:
            order(window.kind) * 100
        }
    }

    private static func order(_ kind: QuotaWindowKind) -> Int {
        switch kind {
        case .session:
            0
        case .daily:
            1
        case .weekly:
            2
        case .monthly:
            3
        case .credits:
            4
        case .observed:
            5
        }
    }
}
