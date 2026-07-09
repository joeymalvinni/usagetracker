import Foundation
import SwiftUI

struct MetricEngine {
    let config: ConfigResponse?
    let accounts: [Account]
    let health: [ProviderHealth]
    let snapshots: [UsageSnapshot]
    let ui: UIConfig
    let visible: (String) -> Bool

    var providers: [ProviderVM] {
        var ids = Set((config?.enabledProviders ?? []).filter(isSupportedProvider) + health.map(\.providerId).filter(isSupportedProvider) + snapshots.map(\.providerId).filter(isSupportedProvider))
        if let config { ids.formUnion(config.providers.keys.filter(isSupportedProvider)) }
        return ordered(Array(ids)).map(model)
    }

    var costDashboard: CostDashboardVM {
        let calendar = Calendar.current
        let today = calendar.startOfDay(for: Date())
        let dayStarts = (0..<30).compactMap { offset in
            calendar.date(byAdding: .day, value: offset - 29, to: today)
        }
        let dayKeys = dayStarts.map { DateFormats.dayKey.string(from: $0) }
        let knownProviders = ordered(Array(Set(snapshots.map(\.providerId).filter(isSupportedProvider) + ["codex", "claude", "opencode_go"])))
        var providerRows = [String: [String: (cost: Double, tokens: UInt64)]]()
        var activeProviderIds = Set<String>()

        for snapshot in snapshots {
            let providerId = snapshot.providerId
            guard let cost = snapshot.metadata.object?["\(providerId)_cost"]?.object else { continue }
            let rows = cost["by_day"]?.array ?? synthesizedTodayRow(from: cost, todayKey: dayKeys.last)
            for rowValue in rows {
                guard let row = rowValue.object,
                      let dateKey = row["date"]?.string,
                      dayKeys.contains(dateKey)
                else { continue }
                let rowCost = row["cost_usd"]?.double ?? 0
                let rowTokens = row["tokens"]?.uint64 ?? 0
                if rowCost <= 0 && rowTokens == 0 { continue }
                let existing = providerRows[providerId]?[dateKey] ?? (0, 0)
                providerRows[providerId, default: [:]][dateKey] = (
                    existing.cost + rowCost,
                    existing.tokens.saturatingAdd(rowTokens)
                )
                activeProviderIds.insert(providerId)
            }
        }

        let providerIds = knownProviders.filter { activeProviderIds.contains($0) }
        let providers = providerIds.map { CostProviderVM(id: $0, name: pretty($0), symbol: symbol($0)) }
        let days = zip(dayStarts, dayKeys).map { date, key in
            CostDayVM(
                id: key,
                date: date,
                providers: providerIds.map { providerId in
                    let value = providerRows[providerId]?[key] ?? (0, 0)
                    return CostProviderDayVM(
                        providerId: providerId,
                        providerName: pretty(providerId),
                        symbol: symbol(providerId),
                        date: date,
                        dateKey: key,
                        cost: value.cost,
                        tokens: value.tokens
                    )
                }
            )
        }
        return CostDashboardVM(days: days, providers: providers)
    }

    private func ordered(_ ids: [String]) -> [String] {
        let preferred = ui.providerOrder.filter(isSupportedProvider) + ["codex", "claude", "opencode_go"]
        var seen = Set<String>()
        let supported = ids.filter(isSupportedProvider)
        let ranked = preferred.filter { supported.contains($0) && seen.insert($0).inserted }
        return ranked + supported.filter { !ranked.contains($0) }.sorted()
    }

    private func isSupportedProvider(_ id: String) -> Bool { id != "opencode" }

    private func model(_ id: String) -> ProviderVM {
        let latest = snapshots.filter { $0.providerId == id }.max { $0.collectedAt < $1.collectedAt }
        let h = selectedHealth(providerId: id, accountId: latest?.accountId)
        let account = accounts.first { $0.id == latest?.accountId || $0.id == h?.accountId }
        let snapshotWindows = latest?.windows ?? []
        let spend = snapshotWindows.filter(isSpendWindow).map { window($0, providerId: id) }
        let windows = snapshotWindows.filter { !isSpendWindow($0) && $0.kind != .credits }.map { window($0, providerId: id) }
        let credits = snapshotWindows.filter { !isSpendWindow($0) && $0.kind == .credits }.map { window($0, providerId: id) }
        let primary = windows.compactMap(\.percent).min()
        let enabled = config?.providers[id]?.enabled ?? (config?.enabledProviders.contains(id) ?? (h?.status != .disabled))
        let status = status(id: id, percent: primary, latest: latest, health: h, enabled: enabled)
        let (sparkline, sparklineTotal) = dailyTokens(providerId: id)
        let secondary = secondaryMetric(sparklineTotal: sparklineTotal, windows: windows)
        return ProviderVM(
            id: id, name: pretty(id), short: short(id), symbol: symbol(id),
            primary: primary.map { "\(Int($0.rounded()))%" } ?? windows.first?.value ?? "No data",
            detail: latest.map { "updated \(relative($0.collectedAt))" } ?? "waiting for data",
            percent: primary, status: status, spend: spend, windows: windows, credits: credits,
            account: account?.displayName ?? account?.externalAccountId,
            healthText: h.map { $0.status.friendly } ?? "unknown",
            visibleInMenu: visible(id),
            enabled: enabled,
            secondary: secondary,
            sparkline: sparkline
        )
    }

    private func selectedHealth(providerId: String, accountId: String?) -> ProviderHealth? {
        let providerHealth = health.filter { $0.providerId == providerId }
        if let accountId, let accountHealth = providerHealth.first(where: { $0.accountId == accountId }) {
            return accountHealth
        }
        return providerHealth.max { $0.updatedAt < $1.updatedAt }
    }

    private func window(_ w: UsageWindow, providerId: String) -> WindowVM {
        let percent = (w.percentRemaining ?? computedPercent(w)).map { max(0, min(100, $0)) }
        let status: DisplayStatus = percent.map { $0 < 10 ? .critical : ($0 < 25 ? .warning : .normal) } ?? .normal
        return WindowVM(
            id: w.id,
            label: w.label,
            value: percent.map { "\(Int($0.rounded()))% left" } ?? amount(w.remaining ?? w.used),
            reset: w.resetAt.map { "resets \(time($0))" } ?? "",
            providerId: providerId,
            providerName: pretty(providerId),
            absolute: absoluteText(w),
            percent: percent,
            status: status
        )
    }

    private func isSpendWindow(_ w: UsageWindow) -> Bool {
        guard w.limit == nil, w.percentRemaining == nil, let used = w.used else { return false }
        return used.unit == .usd || used.unit == .tokens
    }

    private func absoluteText(_ w: UsageWindow) -> String? {
        guard let used = w.used, let limit = w.limit, same(used.unit, limit.unit), limit.value > 0 else { return nil }
        return "\(compact(used.value)) / \(compact(limit.value))"
    }

    private func compact(_ value: Double) -> String {
        Double(value).formatted(.number.notation(.compactName).precision(.fractionLength(0...1)))
    }

    private func status(id: String, percent: Double?, latest: UsageSnapshot?, health h: ProviderHealth?, enabled: Bool) -> DisplayStatus {
        guard enabled else { return .disabled }
        switch h?.status {
        case .ok, .none: break
        case .disabled?: return .disabled
        case .backingOff?: return .warning
        default: return .error
        }
        if let latest, Date().timeIntervalSince(latest.collectedAt) > Double((config?.pollIntervalSeconds ?? 60) * 2) { return .stale }
        if let percent { return percent < 10 ? .critical : (percent < 25 ? .warning : .normal) }
        return latest == nil ? .stale : .normal
    }

    private func computedPercent(_ w: UsageWindow) -> Double? {
        guard let used = w.used, let limit = w.limit, same(used.unit, limit.unit), limit.value > 0 else { return nil }
        return max(0, min(100, 100 - used.value / limit.value * 100))
    }

    private func same(_ a: UsageUnit, _ b: UsageUnit) -> Bool { String(describing: a) == String(describing: b) }

    private func amount(_ a: UsageAmount?) -> String {
        guard let a else { return "No data" }
        if a.unit == .usd { return a.value.formatted(.currency(code: "USD")) }
        if a.unit == .tokens { return "\(compact(a.value)) tokens" }
        return "\(compact(a.value)) \(a.unit.label)"
    }

    private func dailyTokens(providerId: String) -> (sparkline: [Double], total: UInt64) {
        let calendar = Calendar.current
        let today = calendar.startOfDay(for: Date())
        let dayKeys = (0..<30).compactMap { offset in
            calendar.date(byAdding: .day, value: offset - 29, to: today)
        }.map { DateFormats.dayKey.string(from: $0) }

        var perDay = [String: UInt64]()
        for snapshot in snapshots where snapshot.providerId == providerId {
            guard let cost = snapshot.metadata.object?["\(providerId)_cost"]?.object else { continue }
            let rows = cost["by_day"]?.array ?? synthesizedTodayRow(from: cost, todayKey: dayKeys.last)
            for rowValue in rows {
                guard let row = rowValue.object, let dateKey = row["date"]?.string, dayKeys.contains(dateKey) else { continue }
                let tokens = row["tokens"]?.uint64 ?? 0
                perDay[dateKey, default: 0] = perDay[dateKey, default: 0].saturatingAdd(tokens)
            }
        }
        let values = dayKeys.map { Double(perDay[$0] ?? 0) }
        let total = values.reduce(UInt64(0)) { $0.saturatingAdd(UInt64($1.rounded())) }
        return (values, total)
    }

    private func secondaryMetric(sparklineTotal: UInt64, windows: [WindowVM]) -> String {
        if sparklineTotal > 0 { return "30d · \(formatTokens(sparklineTotal)) tok" }
        if let first = windows.first { return first.value }
        return "no activity"
    }

    private func synthesizedTodayRow(from cost: [String: JSONValue], todayKey: String?) -> [JSONValue] {
        guard let todayKey,
              let tokens = cost["today_tokens"]?.uint64,
              tokens > 0
        else { return [] }
        return [.object([
            "date": .string(todayKey),
            "cost_usd": .number(cost["today_cost_usd"]?.double ?? 0),
            "tokens": .number(Double(tokens)),
        ])]
    }

    private func pretty(_ id: String) -> String {
        if id == "codex" { return "Codex" }
        if id == "claude" { return "Claude" }
        if id == "opencode_go" { return "OpenCode Go" }
        return id.capitalized
    }

    private func short(_ id: String) -> String {
        if id == "codex" { return "Cdx" }
        if id == "claude" { return "Clde" }
        if id == "opencode_go" { return "Go" }
        return String(pretty(id).prefix(4))
    }

    private func symbol(_ id: String) -> String {
        if id == "codex" { return "terminal" }
        if id == "claude" { return "sparkles" }
        if id == "opencode_go" { return "bolt.horizontal" }
        return "chart.bar"
    }
    private func time(_ d: Date) -> String { d.formatted(date: .omitted, time: .shortened) }
    private func relative(_ d: Date) -> String { DateFormats.relative.localizedString(for: d, relativeTo: Date()) }
}

func formatUsd(_ value: Double) -> String {
    if value > 0 && value < 0.01 { return "<$0.01" }
    return value.formatted(.currency(code: "USD"))
}

func formatTokens(_ value: UInt64) -> String {
    Double(value).formatted(.number.notation(.compactName).precision(.fractionLength(0...1)))
}

func shortDate(_ date: Date) -> String {
    date.formatted(.dateTime.month(.abbreviated).day())
}
