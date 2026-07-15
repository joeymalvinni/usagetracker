import Foundation
import SwiftUI

private struct ProviderAccountKey: Hashable {
    let providerId: String
    let accountId: String?

    var rowId: String {
        accountId.map { "\(providerId):\($0)" } ?? providerId
    }
}

private var dayCalendar: Calendar {
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = .autoupdatingCurrent
    return calendar
}

private struct ForecastKey: Hashable {
    let providerId: String
    let accountId: String
    let windowId: String
}

private struct WindowProvenanceKey: Hashable {
    let providerId: String
    let accountId: String
    let windowId: String
}

struct DashboardBuilder {
    struct Output {
        let providers: [ProviderVM]
        let settingsProviders: [ProviderVM]
        let costDashboard: CostDashboardVM
    }

    let config: ConfigResponse?
    let accounts: [Account]
    let health: [ProviderHealth]
    let snapshots: [UsageSnapshot]
    let forecasts: [UsageForecast]
    let dashboard: UsageDashboardSummary
    let windowProvenance: [UsageWindowProvenance]
    let serverProviders: [String: ServerProviderDescriptor]
    let serverProviderOrder: [String]
    let ui: UIConfig
    let refreshingProviderIDs: Set<String>
    let visible: (String) -> Bool
    private let accountsById: [String: Account]
    private let healthByProvider: [String: [ProviderHealth]]
    private let snapshotsByProvider: [String: [UsageSnapshot]]
    private let snapshotsByAccount: [ProviderAccountKey: [UsageSnapshot]]
    private let forecastsByWindow: [ForecastKey: UsageForecast]
    private let dashboardByAccount: [ProviderAccountKey: AccountUsageSummary]
    private let provenanceByWindow: [WindowProvenanceKey: UsageWindowProvenance]
    private let hiddenAccountIdSet: Set<String>

    init(
        config: ConfigResponse?,
        accounts: [Account],
        health: [ProviderHealth],
        snapshots: [UsageSnapshot],
        forecasts: [UsageForecast],
        dashboard: UsageDashboardSummary,
        windowProvenance: [UsageWindowProvenance],
        serverProviders: [String: ServerProviderDescriptor] = [:],
        serverProviderOrder: [String] = [],
        ui: UIConfig,
        refreshingProviderIDs: Set<String> = [],
        visible: @escaping (String) -> Bool
    ) {
        self.config = config
        self.accounts = accounts
        self.health = health
        self.snapshots = snapshots
        self.forecasts = forecasts
        self.dashboard = dashboard
        self.windowProvenance = windowProvenance
        self.serverProviders = serverProviders
        self.serverProviderOrder = serverProviderOrder
        self.ui = ui
        self.refreshingProviderIDs = refreshingProviderIDs
        self.visible = visible
        accountsById = Dictionary(uniqueKeysWithValues: accounts.map { ($0.id, $0) })
        hiddenAccountIdSet = Set(accounts.lazy.filter(\.hidden).map(\.id))
        healthByProvider = Dictionary(grouping: health, by: \.providerId)
        snapshotsByProvider = Dictionary(grouping: snapshots, by: \.providerId)
        snapshotsByAccount = Dictionary(grouping: snapshots) {
            ProviderAccountKey(providerId: $0.providerId, accountId: $0.accountId)
        }
        forecastsByWindow = Dictionary(
            forecasts.map {
                (ForecastKey(providerId: $0.providerId, accountId: $0.accountId, windowId: $0.windowId), $0)
            },
            uniquingKeysWith: { _, latest in latest }
        )
        dashboardByAccount = Dictionary(
            dashboard.accounts.map {
                (ProviderAccountKey(providerId: $0.providerId, accountId: $0.accountId), $0)
            },
            uniquingKeysWith: { _, latest in latest }
        )
        provenanceByWindow = Dictionary(
            windowProvenance.map {
                (WindowProvenanceKey(providerId: $0.providerId, accountId: $0.accountId, windowId: $0.windowId), $0)
            },
            uniquingKeysWith: { _, latest in latest }
        )
    }

    func build() -> Output {
        let accountVMs = buildAccountVMs()
        let providers = buildProviderGroups(from: accountVMs)
        return Output(
            providers: providers,
            settingsProviders: buildSettingsProviders(from: providers),
            costDashboard: buildCostDashboard(filter: nil)
        )
    }

    private func buildSettingsProviders(from providers: [ProviderVM]) -> [ProviderVM] {
        let ids = ordered(Array(providerIds(includeKnownProviders: true)))
        return ids.map { providerId in
            if let existing = providers.first(where: { $0.providerId == providerId }) {
                return existing
            }
            let enabled = isEnabledProvider(providerId)
            let h = selectedHealth(providerId: providerId, accountId: nil)
            let status: DisplayStatus = if enabled {
                refreshingProviderIDs.contains(providerId) ? .refreshing : .stale
            } else {
                .disabled
            }
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
                status: status,
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
        let worstStatus = accountVMs.map(\.status).max { $0.severity < $1.severity } ?? .stale
        let worstAccount = accountVMs.max { $0.status.severity < $1.status.severity }
        let totalTokens = aggregatedSparkline.reduce(UInt64(0)) { $0.saturatingAdd(UInt64($1.rounded())) }
        let primary = worstPercent.map { "\(Int($0.rounded()))%" } ?? allWindows.compactMap { $0.percent != nil ? nil : ($0.value.isEmpty ? nil : $0.value) }.first ?? accountVMs.first?.primary ?? "No data"
        let latestCollected = snapshotsByProvider[providerId]?.map(\.collectedAt).max()
        let detail = latestCollected.map { "updated \(relative($0))" } ?? "waiting for data"
        let enabled = isEnabledProvider(providerId)
        let healthText = worstHealthText(providerId: providerId)
        let secondary = secondaryMetric(sparklineTotal: totalTokens, windows: allWindows)

        return ProviderVM(
            id: providerId,
            providerId: providerId,
            accountId: nil,
            // A provider group always represents the provider, even when it has
            // only one account. Keep the account's editable label in `account`
            // so navigation and headings do not change when that label changes.
            name: pretty(providerId),
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
            repairRecommended: worstAccount?.repairRecommended ?? false,
            accountEmail: singleAccount?.accountEmail
        )
    }

    private func accountVM(providerId: String, accountId: String) -> ProviderVM? {
        let account = accountsById[accountId]
        if account?.hidden == true { return nil }
        let latest = snapshotsByAccount[ProviderAccountKey(providerId: providerId, accountId: accountId)]?
            .max { $0.collectedAt < $1.collectedAt }
        let h = selectedHealth(providerId: providerId, accountId: accountId)
        let snapshotWindows = latest?.windows.filter {
            let provenance = provenanceByWindow[
                WindowProvenanceKey(providerId: providerId, accountId: accountId, windowId: $0.windowId)
            ]
            return provenance?.quotaLike != true || provenance?.authoritative == true
        } ?? []
        let spend = snapshotWindows.filter(isSpendWindow).map { window($0, providerId: providerId, accountId: accountId) }
        var windows = snapshotWindows.filter { !isSpendWindow($0) && $0.kind != .credits }.map { window($0, providerId: providerId, accountId: accountId) }
        if let latest, let resetCredits = resetCreditWindow(latest, providerId: providerId) {
            windows.append(resetCredits)
        }
        var credits = snapshotWindows.filter { !isSpendWindow($0) && $0.kind == .credits }.map { window($0, providerId: providerId, accountId: accountId) }
        // Drop user-hidden progress bars before they reach any view model, so the
        // provider's headline percent, status, and menu-bar number recompute
        // without them.
        windows.removeAll { ui.hiddenWindows[AppState.windowKey(providerId, $0.id)] != nil }
        credits.removeAll { ui.hiddenWindows[AppState.windowKey(providerId, $0.id)] != nil }
        let resetCredits = latest.map(resetCreditDetails) ?? []
        let primary = windows.compactMap(\.percent).min()
        let enabled = isEnabledProvider(providerId) && (account?.collectionEnabled ?? true)
        let status = statusValue(id: providerId, percent: primary, latest: latest, health: h, enabled: enabled)
        let (sparkline, sparklineTotal) = dailyTokens(providerId: providerId, accountId: accountId)
        let secondary = secondaryMetric(sparklineTotal: sparklineTotal, windows: windows)
        let displayName = friendlyAccountName(displayName: account?.displayName, externalId: account?.externalAccountId)
        let signature = alertSignature(providerId: providerId, accountId: accountId, status: status)

        return ProviderVM(
            id: "\(providerId):\(accountId)",
            providerId: providerId,
            accountId: accountId,
            name: displayName,
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

    private func buildCostDashboard(filter: ((UsageSnapshot) -> Bool)?) -> CostDashboardVM {
        let calendar = dayCalendar
        let today = calendar.startOfDay(for: Date())
        let dayStarts = (0..<30).compactMap { offset in
            calendar.date(byAdding: .day, value: offset - 29, to: today)
        }
        let dayKeys = dayStarts.map { DateFormats.dayKey.string(from: $0) }
        let allowedAccounts: Set<ProviderAccountKey>? = filter.map { predicate in
            Set(snapshots.filter(predicate).map {
                ProviderAccountKey(providerId: $0.providerId, accountId: $0.accountId)
            })
        }
        let summaries = dashboard.accounts.filter { summary in
            guard !hiddenAccountIdSet.contains(summary.accountId) else { return false }
            return allowedAccounts?.contains(
                ProviderAccountKey(providerId: summary.providerId, accountId: summary.accountId)
            ) ?? true
        }

        var rows = [String: [String: (cost: Double, tokens: UInt64)]]()
        var active = Set<String>()

        for summary in summaries {
            let rowId = filter == nil
                ? summary.providerId
                : "\(summary.providerId):\(summary.accountId)"
            if let activity = summary.activity {
                for point in activity.days where dayKeys.contains(point.dateKey) {
                    let current = rows[rowId]?[point.dateKey] ?? (0, 0)
                    rows[rowId, default: [:]][point.dateKey] = (
                        current.cost,
                        current.tokens.saturatingAdd(point.tokens)
                    )
                    if point.tokens > 0 { active.insert(rowId) }
                }
            }
            if let cost = summary.cost {
                for point in cost.days where dayKeys.contains(point.dateKey) {
                    let current = rows[rowId]?[point.dateKey] ?? (0, 0)
                    rows[rowId, default: [:]][point.dateKey] = (
                        current.cost + (point.costUsd ?? 0),
                        summary.activity == nil
                            ? current.tokens.saturatingAdd(point.tokens)
                            : current.tokens
                    )
                    if point.costUsd ?? 0 > 0 { active.insert(rowId) }
                }
            }
        }

        let activeIds = filter == nil
            ? ordered(Array(active))
            : Array(active).sorted()
        let providers = activeIds.map { id in
            let providerId = String(id.split(separator: ":").first ?? "")
            return CostProviderVM(
                id: id,
                name: filter == nil ? pretty(id) : accountLabel(
                    ProviderAccountKey(
                        providerId: providerId,
                        accountId: String(id.split(separator: ":").dropFirst().joined(separator: ":"))
                    )
                ),
                symbol: symbol(providerId)
            )
        }
        let days = zip(dayStarts, dayKeys).map { date, key in
            CostDayVM(
                id: key,
                date: date,
                providers: activeIds.map { id in
                    let providerId = String(id.split(separator: ":").first ?? "")
                    let value = rows[id]?[key] ?? (0, 0)
                    return CostProviderDayVM(
                        providerId: id,
                        providerName: filter == nil ? pretty(id) : accountLabel(
                            ProviderAccountKey(
                                providerId: providerId,
                                accountId: String(id.split(separator: ":").dropFirst().joined(separator: ":"))
                            )
                        ),
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

    private var knownProviderIds: [String] {
        if serverProviders.isEmpty { return ProviderCatalog.supportedIDs }
        if !serverProviderOrder.isEmpty { return serverProviderOrder }
        return serverProviders.keys.sorted()
    }

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
        var ids = Set(health.map(\.providerId).filter(isSupportedProvider) + snapshots.map(\.providerId).filter(isSupportedProvider))
        ids.formUnion(accounts.map(\.providerId).filter(isSupportedProvider))
        if let config { ids.formUnion(config.providers.keys.filter(isSupportedProvider)) }
        if includeKnownProviders { ids.formUnion(knownProviderIds) }
        return ids
    }

    private func hiddenAccountIds() -> Set<String> {
        hiddenAccountIdSet
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

    private func isSupportedProvider(_ id: String) -> Bool {
        serverProviders.isEmpty ? ProviderCatalog.supports(id) : serverProviders[id] != nil
    }

    private func isDefaultVisibleProvider(_ id: String) -> Bool {
        guard isEnabledProvider(id) else { return false }
        if hasProviderData(id) { return true }
        return !isUnavailableProvider(id)
    }

    private func hasProviderData(_ id: String) -> Bool {
        snapshots.contains { $0.providerId == id } || accounts.contains { $0.providerId == id }
    }

    private func isEnabledProvider(_ id: String) -> Bool {
        config?.providers[id]?.enabled == true
    }

    private func isUnavailableProvider(_ id: String) -> Bool {
        health.contains { $0.providerId == id && $0.lastErrorCode == "provider_unavailable" }
    }

    private func resetCreditWindow(_ snapshot: UsageSnapshot, providerId: String) -> WindowVM? {
        guard let summary = dashboardByAccount[
            ProviderAccountKey(providerId: snapshot.providerId, accountId: snapshot.accountId)
        ]?.resetCredits, summary.availableCount > 0 else { return nil }

        let count = Int(summary.availableCount)
        let expiresAt = summary.nextExpiresAt
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
            label: "Rate-limit resets",
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
        guard let credits = dashboardByAccount[
            ProviderAccountKey(providerId: snapshot.providerId, accountId: snapshot.accountId)
        ]?.resetCredits?.credits
        else { return [] }

        return credits.map { credit in
            return ResetCreditVM(
                id: credit.id,
                title: credit.title,
                status: credit.status,
                expiresAt: credit.expiresAt,
                expiresText: credit.expiresAt.map(expiryTime) ?? "expiry unknown"
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
        let providerHealth = healthByProvider[providerId] ?? []
        if let accountId, let accountHealth = providerHealth.first(where: { $0.accountId == accountId }) {
            return accountHealth
        }
        return providerHealth.max { $0.updatedAt < $1.updatedAt }
    }

    private func worstHealthText(providerId: String) -> String {
        let providerHealth = healthByProvider[providerId] ?? []
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
        case .authFailed, .keychainAccessFailed: 5
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
        let matchingForecast = forecastsByWindow[
            ForecastKey(providerId: providerId, accountId: accountId, windowId: w.windowId)
        ]
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
        guard let used = w.used, let limit = w.limit, used.unit == limit.unit, limit.value > 0 else { return nil }
        // Percentage windows already render their remaining percentage. Claude
        // also reports these as an absolute N-of-100 pair, which would only
        // duplicate that value as a confusing "N / 100" label.
        guard used.unit != .percent else { return nil }
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
        if let latest, Date().timeIntervalSince(latest.collectedAt) > Double((config?.pollIntervalSeconds ?? 60) * 2) {
            return refreshingProviderIDs.contains(id) ? .refreshing : .stale
        }
        if let percent { return percent < 10 ? .critical : (percent < 25 ? .warning : .normal) }
        if latest == nil { return refreshingProviderIDs.contains(id) ? .refreshing : .stale }
        return .normal
    }

    private func computedPercent(_ w: UsageWindow) -> Double? {
        guard let used = w.used, let limit = w.limit, used.unit == limit.unit, limit.value > 0 else { return nil }
        return max(0, min(100, 100 - used.value / limit.value * 100))
    }

    private func amount(_ a: UsageAmount?) -> String {
        guard let a else { return "No data" }
        if a.unit == .usd { return a.value.formatted(.currency(code: "USD")) }
        if a.unit == .tokens { return "\(compact(a.value)) tokens" }
        return "\(compact(a.value)) \(a.unit.label)"
    }

    private func dailyTokens(providerId: String, accountId: String?) -> (sparkline: [Double], total: UInt64) {
        let calendar = dayCalendar
        let today = calendar.startOfDay(for: Date())
        let dayKeys = (0..<30).compactMap { offset in
            calendar.date(byAdding: .day, value: offset - 29, to: today)
        }.map { DateFormats.dayKey.string(from: $0) }

        var perDay = [String: UInt64]()
        let summaries = dashboard.accounts.filter {
            $0.providerId == providerId
                && !hiddenAccountIdSet.contains($0.accountId)
                && (accountId == nil || $0.accountId == accountId)
        }
        for summary in summaries {
            let points = summary.activity?.days ?? summary.cost?.days ?? []
            for point in points where dayKeys.contains(point.dateKey) {
                perDay[point.dateKey, default: 0] = perDay[point.dateKey, default: 0]
                    .saturatingAdd(point.tokens)
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

    private func pretty(_ id: String) -> String {
        serverProviders[id]?.displayName ?? ProviderCatalog.name(for: id)
    }

    private func short(_ id: String) -> String {
        ProviderCatalog.shortName(for: id)
    }

    private func accountLabel(_ key: ProviderAccountKey) -> String {
        guard let accountId = key.accountId else { return "" }
        return accountsById[accountId].flatMap { $0.displayName ?? $0.externalAccountId } ?? accountId
    }

    private func symbol(_ id: String) -> String {
        ProviderCatalog.symbol(for: id)
    }
    private func expiryTime(_ d: Date) -> String {
        DateFormats.expiry.string(from: d)
    }
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
    DateFormats.shortDay.string(from: date)
}
