import Foundation
import SwiftUI

private struct ProviderAccountKey: Hashable {
    let providerId: String
    let accountId: String?

    var rowId: String {
        accountId.map { "\(providerId):\($0)" } ?? providerId
    }
}

private struct LocalCostCoverage {
    var cost: Double
    var tokens: UInt64
    var pricedTokens: UInt64
}

private var utcCalendar: Calendar {
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = TimeZone(secondsFromGMT: 0)!
    return calendar
}

struct MetricEngine {
    let config: ConfigResponse?
    let accounts: [Account]
    let health: [ProviderHealth]
    let snapshots: [UsageSnapshot]
    let forecasts: [UsageForecast]
    let ui: UIConfig
    let visible: (String) -> Bool

    var providers: [ProviderVM] {
        let accountVMs = buildAccountVMs()
        return buildProviderGroups(from: accountVMs)
    }

    var settingsProviders: [ProviderVM] {
        let ids = ordered(Array(providerIds(includeKnownProviders: true)))
        return ids.map { providerId in
            if let existing = providers.first(where: { $0.providerId == providerId }) {
                return existing
            }
            let enabled = isEnabledProvider(providerId)
            let h = selectedHealth(providerId: providerId, accountId: nil)
            return ProviderVM(
                id: providerId,
                providerId: providerId,
                accountId: nil,
                name: pretty(providerId),
                short: short(providerId),
                symbol: symbol(providerId),
                primary: "No data",
                detail: "waiting for data",
                percent: nil,
                status: enabled ? .stale : .disabled,
                spend: [], windows: [], credits: [], resetCredits: [],
                account: nil,
                healthText: h.map { $0.status.friendly } ?? "unknown",
                visibleInMenu: visible(providerId),
                enabled: enabled,
                secondary: "no activity",
                sparkline: [],
                costDashboard: .empty,
                subAccounts: nil
            )
        }
    }

    var costDashboard: CostDashboardVM {
        buildCostDashboard(filter: nil)
    }

    private func buildAccountVMs() -> [ProviderVM] {
        orderedAccountKeys().compactMap { key -> ProviderVM? in
            guard let accountId = key.accountId else { return providerPlaceholderVM(providerId: key.providerId) }
            return accountVM(providerId: key.providerId, accountId: accountId)
        }
    }

    private func providerPlaceholderVM(providerId: String) -> ProviderVM {
        let h = selectedHealth(providerId: providerId, accountId: nil)
        let enabled = isEnabledProvider(providerId)
        let status = statusValue(id: providerId, percent: nil, latest: nil, health: h, enabled: enabled)
        return ProviderVM(
            id: providerId,
            providerId: providerId,
            accountId: nil,
            name: pretty(providerId),
            short: short(providerId),
            symbol: symbol(providerId),
            primary: "No data",
            detail: "waiting for a successful refresh",
            percent: nil,
            status: status,
            spend: [], windows: [], credits: [], resetCredits: [],
            account: nil,
            healthText: h.map { $0.status.friendly } ?? "setup required",
            visibleInMenu: visible(providerId),
            enabled: enabled,
            secondary: "setup required",
            sparkline: [],
            costDashboard: .empty,
            subAccounts: nil,
            lastSuccessAt: h?.lastSuccessAt,
            errorDetail: h?.lastErrorMessage,
            repairRecommended: h.map(needsCredentialRepair) ?? false
        )
    }

    private func buildProviderGroups(from accountVMs: [ProviderVM]) -> [ProviderVM] {
        var groups = [String: [ProviderVM]]()
        for vm in accountVMs {
            groups[vm.providerId, default: []].append(vm)
        }

        let orderedIds = ordered(Array(groups.keys))
        return orderedIds.compactMap { providerId in
            guard var vms = groups[providerId], !vms.isEmpty else { return nil }
            vms.sort { ($0.name).localizedStandardCompare($1.name) == .orderedAscending }
            let sorted = vms
            return providerGroupVM(providerId: providerId, accounts: sorted)
        }
    }

    private func providerGroupVM(providerId: String, accounts accountVMs: [ProviderVM]) -> ProviderVM {
        let singleAccount = accountVMs.count == 1 ? accountVMs[0] : nil
        let allWindows = accountVMs.flatMap(\.windows)
        let allCredits = accountVMs.flatMap(\.credits)
        let allSpend = accountVMs.flatMap(\.spend)
        let allResetCredits = accountVMs.flatMap(\.resetCredits)
        let allSparklines = accountVMs.map(\.sparkline)
        let aggregatedSparkline = mergeSparklines(allSparklines)
        let worstPercent = accountVMs.compactMap(\.percent).min()
        let worstStatus = accountVMs.map(\.status).max { severity($0) < severity($1) } ?? .stale
        let worstAccount = accountVMs.max { severity($0.status) < severity($1.status) }
        let totalTokens = aggregatedSparkline.reduce(UInt64(0)) { $0.saturatingAdd(UInt64($1.rounded())) }
        let primary = worstPercent.map { "\(Int($0.rounded()))%" } ?? allWindows.compactMap { $0.percent != nil ? nil : ($0.value.isEmpty ? nil : $0.value) }.first ?? accountVMs.first?.primary ?? "No data"
        let latestCollected = snapshots
            .filter { $0.providerId == providerId }
            .map(\.collectedAt)
            .max()
        let detail = latestCollected.map { "updated \(relative($0))" } ?? "waiting for data"
        let enabled = isEnabledProvider(providerId)
        let healthText = worstHealthText(providerId: providerId)
        let secondary = secondaryMetric(sparklineTotal: totalTokens, windows: allWindows)

        return ProviderVM(
            id: providerId,
            providerId: providerId,
            accountId: nil,
            name: singleAccount?.name ?? pretty(providerId),
            short: short(providerId),
            symbol: symbol(providerId),
            primary: primary,
            detail: detail,
            percent: worstPercent,
            status: worstStatus,
            spend: allSpend,
            windows: allWindows,
            credits: allCredits,
            resetCredits: allResetCredits,
            account: singleAccount?.account,
            healthText: healthText,
            visibleInMenu: visible(providerId),
            enabled: enabled,
            secondary: secondary,
            sparkline: aggregatedSparkline,
            costDashboard: buildCostDashboard(filter: { snap in snap.providerId == providerId }),
            subAccounts: accountVMs.count > 1 ? accountVMs : nil,
            alertSignature: worstAccount?.alertSignature,
            hasUnseenAlert: accountVMs.contains(where: \.hasUnseenAlert),
            lastSuccessAt: accountVMs.compactMap(\.lastSuccessAt).max(),
            errorDetail: worstAccount?.errorDetail,
            isEstimate: accountVMs.contains(where: \.isEstimate),
            isPartial: accountVMs.contains(where: \.isPartial),
            repairRecommended: worstAccount?.repairRecommended ?? false,
            accountEmail: singleAccount?.accountEmail
        )
    }

    private func accountVM(providerId: String, accountId: String) -> ProviderVM? {
        let account = accounts.first { $0.id == accountId }
        if account?.hidden == true { return nil }
        let latest = snapshots
            .filter { $0.providerId == providerId && $0.accountId == accountId }
            .max { $0.collectedAt < $1.collectedAt }
        let h = selectedHealth(providerId: providerId, accountId: accountId)
        let snapshotWindows = latest?.windows ?? []
        let spend = snapshotWindows.filter(isSpendWindow).map { window($0, providerId: providerId, accountId: accountId) }
        var windows = snapshotWindows.filter { !isSpendWindow($0) && $0.kind != .credits }.map { window($0, providerId: providerId, accountId: accountId) }
        if let latest, let resetCredits = resetCreditWindow(latest, providerId: providerId) {
            windows.append(resetCredits)
        }
        let credits = snapshotWindows.filter { !isSpendWindow($0) && $0.kind == .credits }.map { window($0, providerId: providerId, accountId: accountId) }
        let resetCredits = latest.map(resetCreditDetails) ?? []
        let primary = windows.compactMap(\.percent).min()
        let enabled = isEnabledProvider(providerId) && (account?.collectionEnabled ?? true)
        let status = statusValue(id: providerId, percent: primary, latest: latest, health: h, enabled: enabled)
        let (sparkline, sparklineTotal) = dailyTokens(providerId: providerId, accountId: accountId)
        let secondary = secondaryMetric(sparklineTotal: sparklineTotal, windows: windows)
        let displayName = friendlyAccountName(displayName: account?.displayName, externalId: account?.externalAccountId)
        let rowNameValue = accountName(providerId: providerId, accountName: displayName)
        let signature = alertSignature(providerId: providerId, accountId: accountId, status: status)

        return ProviderVM(
            id: "\(providerId):\(accountId)",
            providerId: providerId,
            accountId: accountId,
            name: rowNameValue,
            short: short(providerId),
            symbol: symbol(providerId),
            primary: primary.map { "\(Int($0.rounded()))%" } ?? windows.first?.value ?? "No data",
            detail: latest.map { "updated \(relative($0.collectedAt))" } ?? "waiting for data",
            percent: primary,
            status: status,
            spend: spend,
            windows: windows,
            credits: credits,
            resetCredits: resetCredits,
            account: displayName,
            healthText: h.map { $0.status.friendly } ?? "unknown",
            visibleInMenu: visible(providerId),
            enabled: enabled,
            secondary: secondary,
            sparkline: sparkline,
            costDashboard: buildCostDashboard(filter: { snap in snap.providerId == providerId && snap.accountId == accountId }),
            subAccounts: nil,
            alertSignature: signature,
            hasUnseenAlert: signature.map { !ui.seenAlerts.contains($0) } ?? false,
            lastSuccessAt: h?.lastSuccessAt,
            errorDetail: h?.lastErrorMessage,
            isEstimate: estimateState(latest).estimated,
            isPartial: estimateState(latest).partial,
            repairRecommended: h.map(needsCredentialRepair) ?? false,
            accountEmail: account?.email
        )
    }

    private func friendlyAccountName(displayName: String?, externalId: String?) -> String {
        if let displayName, !displayName.isEmpty { return displayName }
        guard let externalId, !externalId.isEmpty else { return "Unknown" }
        if externalId.count <= 12 { return externalId }
        let prefix = String(externalId.prefix(8))
        let suffix = String(externalId.suffix(4))
        return "\(prefix)...\(suffix)"
    }

    private func accountName(providerId: String, accountName: String) -> String {
        let lowerAccount = accountName.lowercased()
        let lowerProvider = pretty(providerId).lowercased()
        if lowerAccount.contains(lowerProvider) || lowerAccount == lowerProvider {
            return accountName
        }
        return accountName
    }

    private func buildCostDashboard(filter: ((UsageSnapshot) -> Bool)?) -> CostDashboardVM {
        let calendar = utcCalendar
        let today = calendar.startOfDay(for: Date())
        let dayStarts = (0..<30).compactMap { offset in
            calendar.date(byAdding: .day, value: offset - 29, to: today)
        }
        let dayKeys = dayStarts.map { DateFormats.dayKey.string(from: $0) }
        let hiddenAccounts = hiddenAccountIds()
        let visibleSnapshots = snapshots.filter { !hiddenAccounts.contains($0.accountId) }
        let filtered = filter.map { visibleSnapshots.filter($0) } ?? visibleSnapshots
        let codexReferenceCostPerToken = codexCostPerToken(in: visibleSnapshots)
        var providerRows = [String: [String: (cost: Double, tokens: UInt64)]]()
        var localCostCoverage = [String: [String: LocalCostCoverage]]()
        var activeProviderIds = Set<String>()
        var unpricedModels = Set<String>()
        var isEstimated = false
        var isPartial = false
        var sources = Set<String>()

        let scalarKey: (UsageSnapshot) -> String = { snap in
            filter == nil ? snap.providerId : "\(snap.providerId):\(snap.accountId)"
        }

        for snapshot in filtered {
            let providerId = snapshot.providerId
            let metadata = snapshot.metadata.object
            let cost = metadata?["\(providerId)_cost"]?.object
            let activity = metadata?["\(providerId)_activity"]?.object
            guard cost != nil || activity != nil else { continue }
            if let cost {
                isEstimated = isEstimated || (cost["estimate"]?.bool ?? false)
                isPartial = isPartial || (cost["partial"]?.bool ?? false) || !(cost["complete_lookback"]?.bool ?? true)
                if let source = cost["source"]?.string { sources.insert(source) }
            }
            if let source = activity?["source"]?.string { sources.insert(source) }
            let pKey = scalarKey(snapshot)
            let coverageKey = "\(providerId):\(snapshot.accountId)"
            let costRows = cost?["by_day"]?.array
                ?? cost.map { synthesizedTodayRow(from: $0, todayKey: dayKeys.last) }
                ?? []
            for rowValue in costRows {
                guard let row = rowValue.object,
                      let dateKey = row["date"]?.string,
                      dayKeys.contains(dateKey)
                else { continue }
                let rowCost = row["cost_usd"]?.double ?? 0
                let rowTokens = row["tokens"]?.uint64 ?? 0
                let rowUnpricedTokens = row["unpriced_tokens"]?.uint64 ?? 0
                let rowPricedTokens = row["priced_tokens"]?.uint64
                    ?? rowTokens.saturatingSubtract(rowUnpricedTokens)
                localCostCoverage[coverageKey, default: [:]][dateKey] = LocalCostCoverage(
                    cost: rowCost,
                    tokens: rowTokens,
                    pricedTokens: rowPricedTokens
                )
                if rowUnpricedTokens > 0 {
                    let models = row["unpriced_models"]?.array ?? []
                    let names = models.compactMap { $0.object?["model"]?.string }
                    if names.isEmpty { unpricedModels.insert("unknown") }
                    else { unpricedModels.formUnion(names) }
                }
                guard rowCost > 0 else { continue }
                if providerId == "codex" { continue }
                let existing = providerRows[pKey]?[dateKey] ?? (0, 0)
                providerRows[pKey, default: [:]][dateKey] = (
                    existing.cost + rowCost,
                    existing.tokens
                )
                activeProviderIds.insert(pKey)
            }
            let tokenRows = activity?["by_day"]?.array ?? costRows
            for rowValue in tokenRows {
                guard let row = rowValue.object,
                      let dateKey = row["date"]?.string,
                      dayKeys.contains(dateKey)
                else { continue }
                let rowTokens = row["tokens"]?.uint64 ?? 0
                if rowTokens == 0 { continue }
                let existing = providerRows[pKey]?[dateKey] ?? (0, 0)
                let costEstimate: Double
                if providerId == "codex" {
                    if let local = localCostCoverage[coverageKey]?[dateKey],
                       local.cost > 0,
                       local.tokens > 0,
                       local.pricedTokens > 0
                    {
                        if rowTokens < local.tokens {
                            costEstimate = local.cost * Double(rowTokens) / Double(local.tokens)
                        } else {
                            let remoteTokens = rowTokens.saturatingSubtract(local.tokens)
                            costEstimate = local.cost
                                + Double(remoteTokens) * local.cost / Double(local.pricedTokens)
                        }
                        if rowTokens != local.tokens {
                            sources.insert("codex_observed_rate_estimate")
                        }
                    } else if localCostCoverage[coverageKey]?[dateKey] == nil,
                              let codexReferenceCostPerToken
                    {
                        costEstimate = Double(rowTokens) * codexReferenceCostPerToken
                        sources.insert("codex_observed_rate_estimate")
                    } else {
                        costEstimate = 0
                    }
                } else {
                    costEstimate = 0
                }
                providerRows[pKey, default: [:]][dateKey] = (
                    existing.cost + costEstimate,
                    existing.tokens.saturatingAdd(rowTokens)
                )
                activeProviderIds.insert(pKey)
            }
        }

        // The overview chart is another provider summary surface, so its stack and
        // hover order should follow the same preference as the rail and cards. A
        // provider detail dashboard uses account-qualified IDs and keeps its existing
        // stable account ordering instead.
        let activeIds = filter == nil
            ? ordered(Array(activeProviderIds))
            : Array(activeProviderIds).sorted()
        let providers = activeIds.map { pid in
            CostProviderVM(id: pid, name: filter == nil ? pretty(pid) : pid, symbol: symbol(filter == nil ? pid : String(pid.split(separator: ":").first ?? "")))
        }
        let days = zip(dayStarts, dayKeys).map { date, key in
            CostDayVM(
                id: key,
                date: date,
                providers: activeIds.map { pid in
                    let value = providerRows[pid]?[key] ?? (0, 0)
                    return CostProviderDayVM(
                        providerId: pid,
                        providerName: filter == nil ? pretty(pid) : pid,
                        symbol: symbol(filter == nil ? pid : String(pid.split(separator: ":").first ?? "")),
                        date: date,
                        dateKey: key,
                        cost: value.cost,
                        tokens: value.tokens
                    )
                }
            )
        }
        let sourceText = sources.sorted().map(sourceLabel).joined(separator: " + ")
        let pricingNoticeId = unpricedModels.isEmpty
            ? nil
            : "unpriced-models:\(unpricedModels.sorted().joined(separator: ","))"
        return CostDashboardVM(
            days: days,
            providers: providers,
            isEstimated: isEstimated,
            isPartial: isPartial,
            sourceLabel: sourceText.isEmpty ? nil : sourceText,
            pricingNoticeId: pricingNoticeId
        )
    }

    /// Codex's account-wide activity API reports aggregate tokens without a
    /// model or input/output split. Use the visible local logs only as a
    /// provider-wide pricing reference; activity dates and token counts still
    /// come exclusively from each account's own snapshot.
    private func codexCostPerToken(in snapshots: [UsageSnapshot]) -> Double? {
        var totalCost = 0.0
        var totalTokens: UInt64 = 0
        for snapshot in snapshots where snapshot.providerId == "codex" {
            guard let cost = snapshot.metadata.object?["codex_cost"]?.object,
                  let costUsd = cost["total_cost_usd"]?.double,
                  let tokens = cost["priced_tokens"]?.uint64 ?? cost["total_tokens"]?.uint64,
                  costUsd > 0,
                  tokens > 0
            else { continue }
            totalCost += costUsd
            totalTokens = totalTokens.saturatingAdd(tokens)
        }
        guard totalCost > 0, totalTokens > 0 else { return nil }
        return totalCost / Double(totalTokens)
    }

    private var knownProviderIds: [String] { ["codex", "claude", "opencode_go"] }

    private func orderedAccountKeys() -> [ProviderAccountKey] {
        var keys = Set<ProviderAccountKey>()
        let hiddenAccounts = hiddenAccountIds()
        for snapshot in snapshots where isSupportedProvider(snapshot.providerId) && !hiddenAccounts.contains(snapshot.accountId) {
            keys.insert(ProviderAccountKey(providerId: snapshot.providerId, accountId: snapshot.accountId))
        }
        for account in accounts where isSupportedProvider(account.providerId) && !account.hidden {
            keys.insert(ProviderAccountKey(providerId: account.providerId, accountId: account.id))
        }
        for row in health where isSupportedProvider(row.providerId) && row.accountId.map({ !hiddenAccounts.contains($0) }) ?? true {
            keys.insert(ProviderAccountKey(providerId: row.providerId, accountId: row.accountId))
        }
        let providersWithAccounts = Set(keys.compactMap { key in
            key.accountId == nil ? nil : key.providerId
        })
        keys = Set(keys.filter { key in
            key.accountId != nil || !providersWithAccounts.contains(key.providerId)
        })
        for providerId in providerIds() where !keys.contains(where: { $0.providerId == providerId }) {
            keys.insert(ProviderAccountKey(providerId: providerId, accountId: nil))
        }
        let providerRank = Dictionary(uniqueKeysWithValues: ordered(Array(Set(keys.map(\.providerId)))).enumerated().map { ($0.element, $0.offset) })
        return keys.sorted {
            let leftRank = providerRank[$0.providerId] ?? Int.max
            let rightRank = providerRank[$1.providerId] ?? Int.max
            if leftRank != rightRank { return leftRank < rightRank }
            return accountLabel($0) < accountLabel($1)
        }
    }

    private func providerIds(includeKnownProviders: Bool = false) -> Set<String> {
        var ids = Set((config?.enabledProviders ?? []).filter(isSupportedProvider) + health.map(\.providerId).filter(isSupportedProvider) + snapshots.map(\.providerId).filter(isSupportedProvider))
        ids.formUnion(accounts.map(\.providerId).filter(isSupportedProvider))
        if let config { ids.formUnion(config.providers.keys.filter(isSupportedProvider)) }
        if includeKnownProviders { ids.formUnion(knownProviderIds) }
        return ids
    }

    private func hiddenAccountIds() -> Set<String> {
        Set(accounts.filter(\.hidden).map(\.id))
    }

    private func ordered(_ ids: [String]) -> [String] {
        let preferred = ui.providerOrder.filter(isSupportedProvider) + knownProviderIds
        let supported = ids.filter(isSupportedProvider)
        return ProviderOrdering.resolve(supported, preferred: preferred)
    }

    private func ordered(_ keys: [ProviderAccountKey]) -> [ProviderAccountKey] {
        let providerRank = Dictionary(uniqueKeysWithValues: ordered(Array(Set(keys.map(\.providerId)))).enumerated().map { ($0.element, $0.offset) })
        return keys.sorted {
            let leftRank = providerRank[$0.providerId] ?? Int.max
            let rightRank = providerRank[$1.providerId] ?? Int.max
            if leftRank != rightRank { return leftRank < rightRank }
            return accountLabel($0) < accountLabel($1)
        }
    }

    private func alertSignature(providerId: String, accountId: String, status: DisplayStatus) -> String? {
        guard status.isAlert else { return nil }
        return "\(providerId)|\(accountId)|\(status.code)"
    }

    private func isSupportedProvider(_ id: String) -> Bool { id != "opencode" }

    private func isDefaultVisibleProvider(_ id: String) -> Bool {
        guard isEnabledProvider(id) else { return false }
        if hasProviderData(id) { return true }
        return !isUnavailableProvider(id)
    }

    private func hasProviderData(_ id: String) -> Bool {
        snapshots.contains { $0.providerId == id } || accounts.contains { $0.providerId == id }
    }

    private func isEnabledProvider(_ id: String) -> Bool {
        if let enabled = config?.providers[id]?.enabled { return enabled }
        return config?.enabledProviders.contains(id) == true
    }

    private func isUnavailableProvider(_ id: String) -> Bool {
        health.contains { $0.providerId == id && $0.lastErrorCode == "provider_unavailable" }
    }

    private func resetCreditWindow(_ snapshot: UsageSnapshot, providerId: String) -> WindowVM? {
        guard let root = snapshot.metadata.object else { return nil }
        let metadata = root["rate_limit_reset_credits"]?.object
        let available = metadata?["available_count"]?.double ?? root["rate_limit_reset_credits_available_count"]?.double
        guard let available, available > 0 else { return nil }

        let count = Int(available.rounded())
        let expiresAt = metadata?["next_expires_at"]?.double.map { Date(timeIntervalSince1970: $0) }
        let reset = expiresAt.map { "expires \(expiryTime($0))" } ?? "expiry unknown"
        let status: DisplayStatus
        if let expiresAt, expiresAt <= Date() {
            status = .critical
        } else if let expiresAt, expiresAt.timeIntervalSinceNow < 24 * 60 * 60 {
            status = .warning
        } else {
            status = .normal
        }

        return WindowVM(
            id: "\(providerId)_rate_limit_resets",
            label: "Codex resets",
            value: "\(count) available",
            reset: reset,
            providerId: providerId,
            providerName: pretty(providerId),
            absolute: nil,
            percent: nil,
            status: status,
            resetAt: expiresAt
        )
    }

    private func resetCreditDetails(_ snapshot: UsageSnapshot) -> [ResetCreditVM] {
        guard snapshot.providerId == "codex",
              let root = snapshot.metadata.object,
              let credits = root["rate_limit_reset_credits"]?.object?["credits"]?.array
        else { return [] }

        return credits.enumerated().compactMap { index, value in
            guard let credit = value.object else { return nil }
            let expiresAt = credit["expires_at"]?.double.map { Date(timeIntervalSince1970: $0) }
            let status = credit["status"]?.string ?? "unknown"
            let title = credit["title"]?.string ?? credit["reset_type"]?.string ?? "Reset credit"
            let id = credit["id"]?.string ?? "\(snapshot.id):reset_credit:\(index)"
            return ResetCreditVM(
                id: id,
                title: title,
                status: status,
                expiresAt: expiresAt,
                expiresText: expiresAt.map(expiryTime) ?? "expiry unknown"
            )
        }
        .sorted {
            switch ($0.expiresAt, $1.expiresAt) {
            case let (left?, right?): return left < right
            case (_?, nil): return true
            case (nil, _?): return false
            case (nil, nil): return $0.title < $1.title
            }
        }
    }

    private func selectedHealth(providerId: String, accountId: String?) -> ProviderHealth? {
        let providerHealth = health.filter { $0.providerId == providerId }
        if let accountId, let accountHealth = providerHealth.first(where: { $0.accountId == accountId }) {
            return accountHealth
        }
        return providerHealth.max { $0.updatedAt < $1.updatedAt }
    }

    private func estimateState(_ snapshot: UsageSnapshot?) -> (estimated: Bool, partial: Bool) {
        guard let snapshot,
              let cost = snapshot.metadata.object?["\(snapshot.providerId)_cost"]?.object
        else { return (false, false) }
        return (
            cost["estimate"]?.bool ?? false,
            (cost["partial"]?.bool ?? false) || !(cost["complete_lookback"]?.bool ?? true)
        )
    }

    private func sourceLabel(_ source: String) -> String {
        switch source {
        case "codex_account_usage": return "Codex account history"
        case "codex_observed_rate_estimate": return "observed local Codex rate"
        case "local_session_logs": return "local Codex logs"
        case "local_project_logs": return "local Claude logs"
        case "opencode_local_sqlite": return "local OpenCode history"
        case "opencode_usage_page": return "OpenCode usage history"
        default: return source.replacingOccurrences(of: "_", with: " ")
        }
    }

    private func worstHealthText(providerId: String) -> String {
        let providerHealth = health.filter { $0.providerId == providerId }
        let accountHealth = providerHealth.filter { $0.accountId != nil }
        let relevant = accountHealth.isEmpty ? providerHealth : accountHealth
        let worst = relevant.max {
            healthSeverity($0.status) < healthSeverity($1.status)
        }
        return worst.map { $0.status.friendly } ?? "unknown"
    }

    private func healthSeverity(_ s: ProviderHealthStatus) -> Int {
        switch s {
        case .ok: 0
        case .backingOff: 1
        case .rateLimited: 2
        case .providerError: 3
        case .parseError: 4
        case .authFailed: 5
        case .credentialsMissing: 6
        case .disabled: 7
        case .other: 8
        }
    }

    private func needsCredentialRepair(_ health: ProviderHealth) -> Bool {
        switch health.status {
        case .credentialsMissing, .authFailed: true
        default: false
        }
    }

    private func window(_ w: UsageWindow, providerId: String, accountId: String) -> WindowVM {
        let percent = (w.percentRemaining ?? computedPercent(w)).map { max(0, min(100, $0)) }
        let status: DisplayStatus = percent.map { $0 < 10 ? .critical : ($0 < 25 ? .warning : .normal) } ?? .normal
        let matchingForecast = forecasts.first {
            $0.providerId == providerId && $0.accountId == accountId && $0.windowId == w.windowId
        }
        return WindowVM(
            id: w.id,
            label: w.label,
            value: percent.map { "\(Int($0.rounded()))% left" } ?? amount(w.remaining ?? w.used),
            reset: w.resetAt.map { "Resets \(DateFormats.expiry.string(from: $0))" } ?? "",
            providerId: providerId,
            providerName: pretty(providerId),
            absolute: absoluteText(w),
            percent: percent,
            status: status,
            resetAt: w.resetAt,
            forecast: matchingForecast.map(windowForecast)
        )
    }

    private func windowForecast(_ forecast: UsageForecast) -> WindowForecastVM {
        let projectedRemaining = forecast.projectedPercentRemainingAtReset.map {
            max(0, min(100, $0))
        }
        let detail: String
        if let projectedRemaining {
            let rounded = Int(projectedRemaining.rounded())
            if forecast.status == .exhausted {
                detail = "This limit is exhausted until it resets."
            } else if rounded == 0 {
                detail = "At your current pace, you’re projected to have 0% remaining and may run out before this resets."
            } else {
                detail = "You’re on pace to have about \(rounded)% remaining when this resets."
            }
        } else {
            switch forecast.status {
            case .safe, .onPace:
                detail = "Your usage is on pace to last until this reset. More history is needed to estimate what will remain."
            case .atRisk:
                detail = "Your usage is running ahead of pace and may run out before this resets."
            case .exhausted:
                detail = "This limit is exhausted until it resets."
            case .insufficientData:
                detail = "More usage history is needed to estimate what will remain when this resets."
            }
        }
        return WindowForecastVM(
            summary: forecast.status.conclusion,
            detail: detail,
            projectedPercentRemaining: projectedRemaining
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

    private func statusValue(id: String, percent: Double?, latest: UsageSnapshot?, health h: ProviderHealth?, enabled: Bool) -> DisplayStatus {
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

    private func dailyTokens(providerId: String, accountId: String?) -> (sparkline: [Double], total: UInt64) {
        let calendar = utcCalendar
        let today = calendar.startOfDay(for: Date())
        let dayKeys = (0..<30).compactMap { offset in
            calendar.date(byAdding: .day, value: offset - 29, to: today)
        }.map { DateFormats.dayKey.string(from: $0) }

        var perDay = [String: UInt64]()
        let hiddenAccounts = hiddenAccountIds()
        for snapshot in snapshots where snapshot.providerId == providerId && (accountId == nil || snapshot.accountId == accountId) && !hiddenAccounts.contains(snapshot.accountId) {
            let metadata = snapshot.metadata.object
            let activity = metadata?["\(providerId)_activity"]?.object
            let cost = metadata?["\(providerId)_cost"]?.object
            guard activity != nil || cost != nil else { continue }
            let rows = activity?["by_day"]?.array
                ?? cost?["by_day"]?.array
                ?? cost.map { synthesizedTodayRow(from: $0, todayKey: dayKeys.last) }
                ?? []
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

    private func mergeSparklines(_ sparklines: [[Double]]) -> [Double] {
        guard !sparklines.isEmpty else { return [] }
        let count = sparklines[0].count
        return (0..<count).map { i in
            sparklines.reduce(0.0) { $0 + ($1.indices.contains(i) ? $1[i] : 0) }
        }
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

    private func accountLabel(_ key: ProviderAccountKey) -> String {
        guard let accountId = key.accountId else { return "" }
        return accounts.first { $0.id == accountId }.flatMap { $0.displayName ?? $0.externalAccountId } ?? accountId
    }

    private func symbol(_ id: String) -> String {
        if id == "codex" { return "terminal" }
        if id == "claude" { return "sparkles" }
        if id == "opencode_go" { return "bolt.horizontal" }
        return "chart.bar"
    }
    private func expiryTime(_ d: Date) -> String {
        DateFormats.expiry.string(from: d)
    }
    private func relative(_ d: Date) -> String { DateFormats.relative.localizedString(for: d, relativeTo: Date()) }
    private func severity(_ status: DisplayStatus) -> Int {
        switch status {
        case .normal: 0
        case .disabled: 1
        case .stale: 2
        case .warning: 3
        case .critical: 4
        case .error, .offline: 5
        }
    }
}

func formatUsd(_ value: Double) -> String {
    if value > 0 && value < 0.01 { return "<$0.01" }
    return value.formatted(.currency(code: "USD"))
}

func formatTokens(_ value: UInt64) -> String {
    Double(value).formatted(.number.notation(.compactName).precision(.fractionLength(0...1)))
}

func shortDate(_ date: Date) -> String {
    DateFormats.shortDay.string(from: date)
}
