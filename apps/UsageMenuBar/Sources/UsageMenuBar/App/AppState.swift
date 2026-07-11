import Combine
import Foundation
import UserNotifications

enum DaemonState: Equatable, Sendable { case unknown, online, offline }

struct DerivedState: Equatable {
    static let empty = DerivedState(
        providers: [], settingsProviders: [], cost: .empty,
        menuPreview: "Usage", menuStatus: .stale, menuBars: []
    )

    let providers: [ProviderVM]
    let settingsProviders: [ProviderVM]
    let cost: CostDashboardVM
    let menuPreview: String
    let menuStatus: DisplayStatus
    let menuBars: [MenuBarProviderVM]
}

@MainActor final class AppState: ObservableObject {
    @Published var daemon: DaemonState = .unknown
    @Published var config: ConfigResponse?
    @Published var accounts = [Account]()
    @Published var health = [ProviderHealth]()
    @Published var snapshots = [UsageSnapshot]()
    @Published var forecasts = [UsageForecast]()
    @Published var refreshing = false
    @Published var message: String?
    @Published var actionMessage: String?
    @Published var actionError: String?
    @Published var pendingProviders = Set<String>()
    @Published var pendingAccountProviders = Set<String>()
    @Published var pendingAccounts = Set<String>()
    @Published var pendingInterval = false
    @Published var pendingNotifications = false
    @Published var notificationAuthorization: UNAuthorizationStatus = .notDetermined
    @Published var notificationAuthorizationAvailable = AppState.isRunningFromAppBundle
    @Published var providerSetups = [String: ProviderSetupResponse]()
    @Published private(set) var derived = DerivedState.empty
    var providers: [ProviderVM] { derived.providers }
    var settingsProviders: [ProviderVM] { derived.settingsProviders }
    var cost: CostDashboardVM { derived.cost }
    var menuPreview: String { derived.menuPreview }
    var menuStatus: DisplayStatus { derived.menuStatus }
    var menuBars: [MenuBarProviderVM] { derived.menuBars }
    @Published var ui = UIConfig.load() {
        didSet {
            guard ui != oldValue else { return }
            scheduleUIConfigPersistence()
            build()
        }
    }

    private var socketPath: String {
        didSet {
            if socketPath != oldValue { client = DaemonClient(socketPath: socketPath) }
        }
    }
    private var client: DaemonClient
    private let daemonSupervisor = DaemonSupervisor()
    private let uiConfigStore = UIConfigStore()
    private var uiPersistenceTask: Task<Void, Never>?
    private var buildTask: Task<Void, Never>?
    private var buildRevision = 0
    private var loadTask: Task<Void, Never>?
    private var inFlightIncludesAccounts = false
    private var lastLoadCompletedAt: Date?
    private let popoverFreshness: TimeInterval = 30

    init() {
        let socketPath = AppState.defaultSocketPath()
        self.socketPath = socketPath
        self.client = DaemonClient(socketPath: socketPath)
    }

    init(socketPath: String) {
        self.socketPath = socketPath
        self.client = DaemonClient(socketPath: socketPath)
    }

    func bootstrap() async {
        // A bundled daemon can outlive the app that launched it. Check it
        // before the first API request so replacing the app bundle also
        // replaces an older daemon instead of silently serving stale data.
        _ = await daemonSupervisor.ensureRunning(socketPath: socketPath)
        await load(all: true)
    }
    func refreshForPopoverOpen() async {
        if let lastLoadCompletedAt,
           Date().timeIntervalSince(lastLoadCompletedAt) < popoverFreshness { return }
        await load(all: false)
    }
    func pollLoop() async {
        while !Task.isCancelled {
            let seconds = max(15, Int(config?.pollIntervalSeconds ?? 60))
            try? await Task.sleep(for: .seconds(seconds))
            await load(all: false)
        }
    }
    func refreshAll() async {
        refreshing = true; defer { refreshing = false }
        do {
            let report = try await client.refresh(nil)
            applyRefreshOutcome(report)
            await load(all: true)
        } catch {
            actionError = describe(error)
        }
    }
    func refreshProvider(_ id: String) async {
        refreshing = true; defer { refreshing = false }
        do {
            let report = try await client.refresh([id])
            applyRefreshOutcome(report)
            await load(all: true)
        } catch {
            actionError = describe(error)
        }
    }
    func visible(_ id: String) -> Bool { uiVisible(id) }
    func setVisible(_ id: String, _ on: Bool) {
        if on { ui.hiddenProviders.remove(id) } else { ui.hiddenProviders.insert(id) }
    }

    func setProviderEnabled(_ id: String, _ enabled: Bool) async {
        pendingProviders.insert(id); defer { pendingProviders.remove(id) }
        do {
            config = try await client.updateConfig(pollIntervalSeconds: nil, providers: [id: enabled])
            actionError = nil
            build()
            if enabled { applyRefreshOutcome(try await client.refresh([id])) }
            await load(all: true)
        } catch {
            actionError = describe(error)
        }
    }

    func setPollInterval(_ seconds: UInt64) async {
        pendingInterval = true; defer { pendingInterval = false }
        do {
            config = try await client.updateConfig(pollIntervalSeconds: seconds, providers: nil)
            actionError = nil
            build()
            await load(all: true)
        } catch {
            actionError = describe(error)
        }
    }

    func setNotificationsEnabled(_ enabled: Bool) async {
        pendingNotifications = true; defer { pendingNotifications = false }
        if enabled && notificationAuthorization == .notDetermined {
            await requestNotificationAuthorizationIfNeeded()
        }
        do {
            config = try await client.updateConfig(
                pollIntervalSeconds: nil,
                providers: nil,
                notificationsEnabled: enabled
            )
            if enabled && notificationAuthorization == .denied {
                actionMessage = "Usage alerts are enabled, but notifications are blocked in macOS System Settings."
            } else {
                actionMessage = enabled ? "Usage alerts enabled." : "Usage alerts disabled."
            }
            actionError = nil
            build()
        } catch {
            actionError = describe(error)
        }
    }

    func refreshNotificationAuthorization() async {
        guard notificationAuthorizationAvailable else { return }
        let settings = await UNUserNotificationCenter.current().notificationSettings()
        notificationAuthorization = settings.authorizationStatus
    }

    private func requestNotificationAuthorizationIfNeeded() async {
        guard notificationAuthorizationAvailable else { return }
        guard notificationAuthorization == .notDetermined else { return }
        do {
            _ = try await UNUserNotificationCenter.current()
                .requestAuthorization(options: [.alert, .sound])
        } catch {
            actionError = "Could not request notification permission: \(describe(error))"
        }
        await refreshNotificationAuthorization()
    }

    func addProviderAccount(_ providerId: String) async {
        pendingAccountProviders.insert(providerId)
        defer { pendingAccountProviders.remove(providerId) }
        do {
            let response = try await client.addProviderAccount(providerId: providerId, displayName: nil)
            actionError = nil
            actionMessage = "Finish signing in to \(providerName(providerId)) in your browser. This account will appear automatically."
            await waitForProviderAccount(providerId: providerId, profileId: response.profileId)
        } catch {
            actionMessage = nil
            actionError = describe(error)
        }
    }

    func setAccountHidden(_ id: String, _ hidden: Bool) async {
        pendingAccounts.insert(id); defer { pendingAccounts.remove(id) }
        do {
            _ = try await client.updateAccount(accountId: id, hidden: hidden)
            actionError = nil
            await load(all: true)
        } catch {
            actionError = describe(error)
        }
    }

    func setAccountCollectionEnabled(_ id: String, _ enabled: Bool) async {
        pendingAccounts.insert(id); defer { pendingAccounts.remove(id) }
        do {
            _ = try await client.updateAccount(accountId: id, hidden: enabled ? false : nil, collectionEnabled: enabled)
            actionError = nil
            if enabled, let providerId = accounts.first(where: { $0.id == id })?.providerId {
                applyRefreshOutcome(try await client.refresh([providerId]))
            }
            await load(all: true)
        } catch {
            actionError = describe(error)
        }
    }

    func removeAccount(_ id: String) async {
        pendingAccounts.insert(id); defer { pendingAccounts.remove(id) }
        do {
            _ = try await client.removeAccount(accountId: id)
            actionError = nil
            actionMessage = "Account removed. Usage history was kept."
            await load(all: true)
        } catch {
            actionError = describe(error)
        }
    }

    func deleteAccount(_ id: String) async {
        pendingAccounts.insert(id); defer { pendingAccounts.remove(id) }
        do {
            try await deleteAccountFromDaemon(id)
        } catch {
            let message = describe(error)
            if isMissingAccount(message) {
                actionError = nil
                actionMessage = "Account was already deleted."
                await load(all: true)
            } else {
                actionError = message
                await load(all: true)
            }
            return
        }
        actionError = nil
        actionMessage = "Account and usage history deleted."
        await load(all: true)
    }

    func deleteAllAccounts() async {
        let ids = accounts.map(\.id)
        guard !ids.isEmpty else { return }
        pendingAccounts.formUnion(ids)
        defer { pendingAccounts.subtract(ids) }

        var failures = [String]()
        for id in ids {
            do {
                try await deleteAccountFromDaemon(id)
            } catch {
                let message = describe(error)
                if !isMissingAccount(message) { failures.append(message) }
            }
        }
        await load(all: true)
        if failures.isEmpty {
            actionError = nil
            actionMessage = "All accounts and usage history deleted."
        } else {
            actionMessage = nil
            actionError = "Some accounts could not be deleted: \(failures.joined(separator: "; "))"
        }
    }

    private func deleteAccountFromDaemon(_ id: String) async throws {
        do {
            try await client.deleteAccount(accountId: id)
        } catch {
            let message = describe(error)
            guard message.contains("unknown variant `delete_account`"),
                  await daemonSupervisor.restart(socketPath: socketPath) else {
                throw error
            }
            try await client.deleteAccount(accountId: id)
        }
    }

    private func isMissingAccount(_ message: String) -> Bool {
        message.localizedCaseInsensitiveContains("unknown account")
            || message.localizedCaseInsensitiveContains("account was not found")
    }

    func restoreAccount(_ id: String) async {
        await setAccountCollectionEnabled(id, true)
    }

    func renameAccount(_ id: String, displayName: String) async {
        let name = displayName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else {
            actionError = "Enter a name for this account."
            return
        }
        pendingAccounts.insert(id); defer { pendingAccounts.remove(id) }
        do {
            _ = try await client.updateAccount(accountId: id, displayName: name)
            actionError = nil
            actionMessage = "Account renamed to \(name)."
            await load(all: true)
            if let providerId = accounts.first(where: { $0.id == id })?.providerId,
               providerSetups[providerId] != nil {
                await loadProviderSetup(providerId)
            }
        } catch {
            actionError = describe(error)
        }
    }

    func loadProviderSetup(_ providerId: String) async {
        pendingAccountProviders.insert(providerId)
        defer { pendingAccountProviders.remove(providerId) }
        do {
            providerSetups[providerId] = try await client.providerSetup(providerId: providerId)
            actionError = nil
        } catch {
            actionError = describe(error)
        }
    }

    func selectWorkspace(providerId: String, workspaceId: String) async {
        pendingAccountProviders.insert(providerId)
        defer { pendingAccountProviders.remove(providerId) }
        do {
            providerSetups[providerId] = try await client.updateProviderSetup(
                providerId: providerId,
                workspaceId: workspaceId
            )
            actionError = nil
            actionMessage = "OpenCode workspace selected."
            applyRefreshOutcome(try await client.refresh([providerId]))
            await load(all: true)
        } catch {
            actionError = describe(error)
        }
    }

    func repairProvider(_ providerId: String, accountId: String? = nil) async {
        pendingAccountProviders.insert(providerId)
        defer { pendingAccountProviders.remove(providerId) }
        let startedAt = Date()
        do {
            let response = try await client.repairProvider(providerId: providerId, accountId: accountId)
            actionError = nil
            actionMessage = response.message
            if providerId == "codex" || providerId == "claude" {
                await waitForProviderRepair(
                    providerId: providerId,
                    accountId: accountId,
                    startedAt: startedAt
                )
            }
        } catch {
            actionError = describe(error)
        }
    }

    func launchProviderAccount(_ accountId: String) async {
        pendingAccounts.insert(accountId)
        defer { pendingAccounts.remove(accountId) }
        do {
            let response = try await client.launchProviderAccount(accountId: accountId)
            actionError = nil
            actionMessage = response.message
        } catch {
            actionMessage = nil
            actionError = describe(error)
        }
    }

    func completeOnboarding() {
        ui.onboardingCompleted = true
        actionError = nil
        actionMessage = "Setup complete. Usage will update automatically."
    }

    func restartOnboarding() {
        ui.onboardingCompleted = false
    }

    var lastSuccessfulRefresh: Date? {
        health.compactMap(\.lastSuccessAt).max() ?? snapshots.map(\.collectedAt).max()
    }

    /// Reorders the full provider list around two visible rail entries. Providers that
    /// are currently unavailable stay in the saved order and reappear predictably if
    /// they are enabled later.
    func moveProvider(_ providerId: String, over targetProviderId: String) {
        let order = settingsProviders.map(\.providerId)
        let reordered = ProviderOrdering.moving(
            providerId,
            over: targetProviderId,
            in: order
        )
        guard reordered != order else { return }
        ui.providerOrder = reordered
    }

    /// Reorders around a rail entry using the preview slot chosen while dragging, so the
    /// committed order exactly matches the animated rail preview.
    func moveProvider(_ providerId: String, relativeTo targetProviderId: String, after: Bool) {
        let order = settingsProviders.map(\.providerId)
        let reordered = ProviderOrdering.moving(
            providerId,
            relativeTo: targetProviderId,
            after: after,
            in: order
        )
        guard reordered != order else { return }
        ui.providerOrder = reordered
    }

    /// Moves against the entries that are actually shown in the rail, so an unavailable
    /// provider cannot make a context-menu "Move up" action appear to do nothing.
    func moveProvider(_ providerId: String, by offset: Int) {
        let visibleOrder = providers.map(\.providerId)
        guard let source = visibleOrder.firstIndex(of: providerId) else { return }
        let target = source + offset
        guard visibleOrder.indices.contains(target) else { return }
        moveProvider(providerId, over: visibleOrder[target])
    }

    func canMoveProvider(_ providerId: String, by offset: Int) -> Bool {
        let visibleOrder = providers.map(\.providerId)
        guard let source = visibleOrder.firstIndex(of: providerId) else { return false }
        return visibleOrder.indices.contains(source + offset)
    }

    func resetProviderOrder() {
        ui.providerOrder = []
    }

    private static func defaultSocketPath() -> String {
        ProcessInfo.processInfo.environment["USAGE_TRACKER_SOCKET"] ?? UIPaths.socket.path
    }
    private static var isRunningFromAppBundle: Bool {
        Bundle.main.bundleURL.pathExtension == "app" && Bundle.main.bundleIdentifier != nil
    }
    private func path(from config: ConfigResponse? = nil) -> String {
        config?.socketPath ?? socketPath
    }
    private func updateSocketPath(from config: ConfigResponse?) {
        socketPath = path(from: config)
    }
    private func uiVisible(_ id: String) -> Bool { ui.hiddenProviders.contains(id) == false }
    private func load(all: Bool) async {
        await load(all: all, allowDaemonStart: true)
    }
    private func load(all: Bool, allowDaemonStart: Bool) async {
        if let current = loadTask {
            let satisfiesRequest = !all || inFlightIncludesAccounts
            await current.value
            if !satisfiesRequest { await load(all: true, allowDaemonStart: allowDaemonStart) }
            return
        }

        inFlightIncludesAccounts = all
        let task = Task {
            await performLoad(all: all, allowDaemonStart: allowDaemonStart)
            loadTask = nil
            inFlightIncludesAccounts = false
        }
        loadTask = task
        await task.value
    }

    private func performLoad(all: Bool, allowDaemonStart: Bool) async {
        do {
            let latestConfig = try await client.config()
            if config != latestConfig { config = latestConfig }
            updateSocketPath(from: latestConfig)
            if all {
                let latestAccounts = try await client.accounts()
                if accounts != latestAccounts { accounts = latestAccounts }
            }
            async let h = client.health()
            async let u = client.usage()
            let latestHealth = try await h
            let usage = try await u
            if health != latestHealth { health = latestHealth }
            if snapshots != usage.snapshots { snapshots = usage.snapshots }
            if forecasts != usage.forecasts { forecasts = usage.forecasts }
            if !all && hasUnknownAccountReferences() {
                let latestAccounts = try await client.accounts()
                if accounts != latestAccounts { accounts = latestAccounts }
            }
            daemon = .online; message = nil; build()
            lastLoadCompletedAt = Date()
            await refreshNotificationAuthorization()
            if config?.notifications.enabled == true {
                await requestNotificationAuthorizationIfNeeded()
            }
        } catch {
            if allowDaemonStart, await daemonSupervisor.ensureRunning(socketPath: socketPath) {
                await performLoad(all: all, allowDaemonStart: false)
            } else {
                fail(error); build()
            }
        }
    }
    private func hasUnknownAccountReferences() -> Bool {
        let known = Set(accounts.map(\.id))
        let referenced = Set(snapshots.map(\.accountId) + health.compactMap(\.accountId))
        return referenced.contains { !known.contains($0) }
    }
    private func waitForProviderAccount(providerId: String, profileId: String) async {
        for _ in 0..<600 {
            guard !Task.isCancelled else { return }
            do {
                let discovered = try await client.accounts()
                if discovered.contains(where: { $0.providerId == providerId && $0.profileId == profileId }) {
                    accounts = discovered
                    actionError = nil
                    actionMessage = providerId == "claude"
                        ? "Claude account connected. Use its terminal button in Settings for profile-scoped activity."
                        : "\(providerName(providerId)) account connected."
                    await load(all: true)
                    return
                }
            } catch {
                // Login is asynchronous; transient socket failures are retried below.
            }
            try? await Task.sleep(for: .seconds(1))
        }
        actionMessage = "\(providerName(providerId)) sign-in is still pending. You can retry from Settings."
    }
    private func waitForProviderRepair(providerId: String, accountId: String?, startedAt: Date) async {
        for _ in 0..<600 {
            guard !Task.isCancelled else { return }
            do {
                let latest = try await client.health()
                let repaired = latest.first {
                    $0.providerId == providerId
                        && (accountId == nil || $0.accountId == accountId)
                        && $0.updatedAt >= startedAt
                        && $0.status != .credentialsMissing
                        && $0.status != .authFailed
                }
                if repaired != nil {
                    actionError = nil
                    actionMessage = "\(providerName(providerId)) login connected."
                    await load(all: true)
                    return
                }
            } catch {
                // Keep waiting while the browser login and daemon refresh complete.
            }
            try? await Task.sleep(for: .seconds(1))
        }
        actionMessage = "\(providerName(providerId)) sign-in is still pending. You can retry from Settings."
    }
    private func fail(_ error: Error) {
        daemon = .offline
        message = describe(error)
    }
    private func describe(_ error: Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
    }
    private func applyRefreshOutcome(_ report: RefreshResponse) {
        let failures = report.providerResults.filter {
            switch $0.status {
            case .ok, .disabled: false
            default: true
            }
        }
        if failures.isEmpty {
            actionError = nil
            actionMessage = "Usage refreshed successfully."
        } else {
            actionMessage = nil
            actionError = failures.map { result in
                let provider = providerName(result.providerId)
                return "\(provider): \(result.message ?? refreshStatusText(result.status))"
            }.joined(separator: "\n")
        }
    }

    private func providerName(_ id: String) -> String {
        switch id {
        case "codex": "Codex"
        case "claude": "Claude"
        case "opencode_go": "OpenCode Go"
        default: id
        }
    }

    private func refreshStatusText(_ status: ProviderRefreshStatus) -> String {
        switch status {
        case .ok: "refreshed"
        case .credentialsMissing: "sign-in required"
        case .credentialsInvalid, .unauthorized: "credentials need repair"
        case .rateLimited: "temporarily rate limited"
        case .network: "network request failed"
        case .parse: "provider response changed"
        case .providerUnavailable: "provider unavailable"
        case .storageError: "could not save usage"
        case .disabled: "collection disabled"
        case .other(let value): value.replacingOccurrences(of: "_", with: " ")
        }
    }
    private func build() {
        buildRevision += 1
        let revision = buildRevision
        let config = config
        let accounts = accounts
        let health = health
        let snapshots = snapshots
        let forecasts = forecasts
        let ui = ui
        let daemon = daemon

        buildTask?.cancel()
        buildTask = Task { [weak self] in
            let next = await Task.detached(priority: .userInitiated) {
                let engine = MetricEngine(
                    config: config,
                    accounts: accounts,
                    health: health,
                    snapshots: snapshots,
                    forecasts: forecasts,
                    ui: ui,
                    visible: { !ui.hiddenProviders.contains($0) }
                )
                return Self.derive(from: engine.build(), daemon: daemon, ui: ui)
            }.value
            guard let self, !Task.isCancelled, revision == self.buildRevision else { return }
            if self.derived != next { self.derived = next }
            self.pruneStaleAcknowledgements()
        }
    }

    nonisolated private static func derive(
        from output: MetricEngine.Output,
        daemon: DaemonState,
        ui: UIConfig
    ) -> DerivedState {
        let menu = menuContent(providers: output.providers, daemon: daemon, ui: ui)
        return DerivedState(
            providers: output.providers,
            settingsProviders: output.settingsProviders,
            cost: output.costDashboard,
            menuPreview: menu.preview,
            menuStatus: menu.status,
            menuBars: menu.bars
        )
    }

    private func scheduleUIConfigPersistence() {
        let config = ui
        uiPersistenceTask?.cancel()
        uiPersistenceTask = Task { [uiConfigStore] in
            try? await Task.sleep(for: .milliseconds(300))
            guard !Task.isCancelled else { return }
            await uiConfigStore.save(config)
        }
    }

    /// Drop acknowledgements whose exact alert signature is no longer active, so that a
    /// resolved-then-recurring alert (e.g. a weekly limit that resets and fills again)
    /// re-alerts, and an escalation (warning → critical) is treated as a new alert.
    private func pruneStaleAcknowledgements() {
        let liveAlerts = Set(providers.flatMap { ($0.subAccounts ?? [$0]).compactMap(\.alertSignature) })
        let dashboards = [cost] + providers.flatMap { provider in
            [provider.costDashboard] + (provider.subAccounts ?? []).map(\.costDashboard)
        }
        let livePricingNotices = Set(dashboards.compactMap(\.pricingNoticeId))
        let pruned = ui.pruningAcknowledgements(
            to: liveAlerts,
            pricingNotices: livePricingNotices
        )
        guard pruned != ui else { return }
        // UIConfig is a value type whose property observer rebuilds the view models.
        // Assign the fully-pruned value once so a partial update cannot recursively
        // re-enter this method before both acknowledgement sets have been updated.
        ui = pruned
    }

    /// Mark an account's active alert as seen, clearing its rail/chip indicator.
    func markAlertSeen(_ vm: ProviderVM) {
        guard let sig = vm.alertSignature, !ui.seenAlerts.contains(sig) else { return }
        ui.seenAlerts.insert(sig)
    }

    /// Dismiss the banner for an account's active alert (also marks it seen).
    func dismissAlert(_ vm: ProviderVM) {
        guard let sig = vm.alertSignature else { return }
        ui.dismissedAlerts.insert(sig)
        ui.seenAlerts.insert(sig)
    }

    /// Whether the banner for this account's active alert should be shown.
    func showsAlertBanner(_ vm: ProviderVM) -> Bool {
        guard let sig = vm.alertSignature else { return false }
        return !ui.dismissedAlerts.contains(sig)
    }
    func dismissPricingNotice(_ dashboard: CostDashboardVM) {
        guard let noticeId = dashboard.pricingNoticeId else { return }
        ui.dismissedPricingNotices.insert(noticeId)
    }

    func showsPricingNotice(_ dashboard: CostDashboardVM) -> Bool {
        guard let noticeId = dashboard.pricingNoticeId else { return false }
        return !ui.dismissedPricingNotices.contains(noticeId)
    }

    nonisolated private static func menuContent(
        providers: [ProviderVM],
        daemon: DaemonState,
        ui: UIConfig
    ) -> (preview: String, status: DisplayStatus, bars: [MenuBarProviderVM]) {
        guard daemon != .offline else { return ("Usage offline", .offline, []) }
        var preview = ""
        let eligible = providers.filter { $0.enabled && $0.visibleInMenu }
        let shown = Array(eligible.prefix(max(0, ui.maxMenuProviders)))
        for (index, provider) in shown.enumerated() {
            if index > 0 { preview += "  " }
            let value: String
            if let percent = provider.percent {
                let displayedValue = max(0, min(100, ui.menuMetric == .used ? 100 - percent : percent))
                value = "\(Int(displayedValue.rounded()))%"
            } else {
                value = provider.primary
            }
            let text = ui.showProviderLabels ? "\(provider.short) \(value)" : value
            preview += text
        }
        let bars = eligible.prefix(2).map { provider in
            let displayed = provider.percent.map { max(0, min(100, ui.menuMetric == .used ? 100 - $0 : $0)) }
            return MenuBarProviderVM(id: provider.id, providerId: provider.providerId, short: provider.short, percent: displayed, status: provider.status)
        }
        if preview.isEmpty { return ("Usage", .stale, []) }
        return (preview, menuStatus(for: shown), bars)
    }

    nonisolated private static func menuStatus(for providers: [ProviderVM]) -> DisplayStatus {
        providers.map(\.status).max { severity($0) < severity($1) } ?? .stale
    }

    nonisolated private static func severity(_ status: DisplayStatus) -> Int {
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
