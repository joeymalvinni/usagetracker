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
    @Published var dashboardSummary = UsageDashboardSummary.empty
    @Published var windowProvenance = [UsageWindowProvenance]()
    @Published var refreshing = false
    @Published var message: String?
    @Published var actionMessage: String?
    @Published var actionError: String?
    @Published var preferencesError: String?
    @Published var notificationError: String?
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
    var menuProviderCount: Int {
        let eligibleProviderIDs = Self.providerIDsWithDataOrConnection(
            accounts: accounts,
            snapshots: snapshots
        )
        return ui.resolvedMenuProviderCount(
            automaticCount: providers.filter {
                $0.enabled && $0.visibleInMenu && eligibleProviderIDs.contains($0.providerId)
            }.count
        )
    }
    @Published var ui: UIConfig {
        didSet {
            guard ui != oldValue else { return }
            scheduleUIConfigPersistence()
            if dashboardConfigurationChanged(from: oldValue) {
                build()
            } else if menuBarConfigurationChanged(from: oldValue) {
                updateMenuBarPresentation()
            }
        }
    }

    private var socketPath: String {
        didSet {
            if socketPath != oldValue { client = DaemonClient(socketPath: socketPath) }
        }
    }
    private var client: DaemonClient
    private let daemonSupervisor = DaemonSupervisor()
    private let notificationDelivery = NotificationDelivery()
    private let uiConfigStore = UIConfigStore()
    private var uiPersistenceTask: Task<Void, Never>?
    private var buildTask: Task<Void, Never>?
    private var buildRevision = 0
    private var loadTask: Task<Void, Never>?
    private var inFlightIncludesAccounts = false
    private var automaticRecoveryTask: Task<Void, Never>?
    private var lastAutomaticRecoveryAt: Date?
    private var refreshActivityCount = 0
    private var refreshingProviderCounts = [String: Int]()
    private let automaticRecoveryCooldown: TimeInterval = 30
    private let wakeRecoveryDelay: Duration = .seconds(2)

    init() {
        let socketPath = AppState.defaultSocketPath()
        do {
            self.ui = try UIConfig.load()
        } catch {
            self.ui = UIConfig()
            self.preferencesError = "UI preferences could not be loaded: \(error.localizedDescription)"
        }
        self.socketPath = socketPath
        self.client = DaemonClient(socketPath: socketPath)
    }

    init(socketPath: String) {
        do {
            self.ui = try UIConfig.load()
        } catch {
            self.ui = UIConfig()
            self.preferencesError = "UI preferences could not be loaded: \(error.localizedDescription)"
        }
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
        await load(all: false)
        await requestStaleDataRecovery(delay: .zero, reloadBeforeChecking: false)
    }
    func refreshAfterWake() async {
        await requestStaleDataRecovery(delay: wakeRecoveryDelay, reloadBeforeChecking: true)
    }
    func pollLoop() async {
        while !Task.isCancelled {
            let seconds = max(15, Int(config?.pollIntervalSeconds ?? 60))
            try? await Task.sleep(for: .seconds(seconds))
            await load(all: false)
        }
    }
    func refreshAll() async {
        let providerIDs = enabledProviderIDs()
        beginRefreshing(providerIDs)
        defer { endRefreshing(providerIDs) }
        do {
            let report = try await client.refresh(nil)
            applyRefreshOutcome(report)
            await load(all: true)
        } catch {
            actionError = describe(error)
        }
    }
    func refreshProvider(_ id: String) async {
        let providerIDs = Set([id])
        beginRefreshing(providerIDs)
        defer { endRefreshing(providerIDs) }
        do {
            let report = try await client.refresh([id])
            applyRefreshOutcome(report)
            await load(all: true)
        } catch {
            actionError = describe(error)
        }
    }
    func visible(_ id: String) -> Bool { uiVisible(id) }
    func setVisible(_ id: String, _ on: Bool) async {
        pendingProviders.insert(id); defer { pendingProviders.remove(id) }
        do {
            config = try await client.updateConfig(pollIntervalSeconds: nil, providers: [id: on])
            if on { ui.hiddenProviders.remove(id) } else { ui.hiddenProviders.insert(id) }
            actionError = nil
        } catch {
            actionError = describe(error)
        }
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
                notifications: (config?.notifications ?? NotificationConfig(enabled: enabled))
                    .withEnabled(enabled)
            )
            if enabled && notificationAuthorization == .denied {
                actionMessage = "Usage alerts are enabled, but notifications are blocked in macOS System Settings."
            } else {
                actionMessage = enabled ? "Usage alerts enabled." : "Usage alerts disabled."
            }
            actionError = nil
            build()
            if enabled {
                await deliverPendingNotifications()
            }
        } catch {
            actionError = describe(error)
        }
    }

    func refreshNotificationAuthorization() async {
        guard notificationAuthorizationAvailable else { return }
        notificationAuthorization = await notificationDelivery.authorizationStatus()
    }

    private func requestNotificationAuthorizationIfNeeded() async {
        guard notificationAuthorizationAvailable else { return }
        guard notificationAuthorization == .notDetermined else { return }
        do {
            _ = try await notificationDelivery.requestAuthorization()
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
            if isMissingAccount(error) {
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
                if !isMissingAccount(error) { failures.append(describe(error)) }
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
        } catch let error as DaemonError {
            guard case let .api(code, _) = error,
                  code == "unsupported_method",
                  await daemonSupervisor.restart(socketPath: socketPath) else {
                throw error
            }
            try await client.deleteAccount(accountId: id)
        }
    }

    private func isMissingAccount(_ error: Error) -> Bool {
        guard case let DaemonError.api(code, _) = error else { return false }
        return code == "unknown_account"
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
            if ProviderCatalog.supportsMultipleAccounts(providerId) {
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
    /// Developer-facing surfaces (internal paths, debug affordances) are hidden
    /// from users running the shipped `.app` and shown only in dev contexts:
    /// running via `swift run`, a fixture session, or the popover-debug flag.
    var isDeveloperMode: Bool {
        let env = ProcessInfo.processInfo.environment
        return !AppState.isRunningFromAppBundle
            || env["USAGE_TRACKER_FIXTURE"]?.isEmpty == false
            || env["USAGE_POPOVER_DEBUG"] == "1"
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
            if dashboardSummary != usage.dashboard { dashboardSummary = usage.dashboard }
            if windowProvenance != usage.windowProvenance { windowProvenance = usage.windowProvenance }
            if !all && hasUnknownAccountReferences() {
                let latestAccounts = try await client.accounts()
                if accounts != latestAccounts { accounts = latestAccounts }
            }
            daemon = .online; message = nil; build()
            await refreshNotificationAuthorization()
            if config?.notifications.enabled == true {
                await deliverPendingNotifications()
            }
        } catch {
            if allowDaemonStart, await daemonSupervisor.ensureRunning(socketPath: socketPath) {
                await performLoad(all: all, allowDaemonStart: false)
            } else {
                fail(error); build()
            }
        }
    }

    private func requestStaleDataRecovery(
        delay: Duration,
        reloadBeforeChecking: Bool
    ) async {
        if let automaticRecoveryTask {
            await automaticRecoveryTask.value
            return
        }

        let task = Task { [weak self] in
            guard let self else { return }
            if delay > .zero {
                try? await Task.sleep(for: delay)
                guard !Task.isCancelled else { return }
            }
            if reloadBeforeChecking { await load(all: false) }

            let now = Date()
            if let lastAutomaticRecoveryAt,
               now.timeIntervalSince(lastAutomaticRecoveryAt) < automaticRecoveryCooldown {
                return
            }
            let providerIDs = Set(Self.staleProviderIDs(
                config: config,
                accounts: accounts,
                snapshots: snapshots,
                now: now
            ))
            guard !providerIDs.isEmpty else { return }

            lastAutomaticRecoveryAt = now
            beginRefreshing(providerIDs)
            defer { endRefreshing(providerIDs) }
            do {
                let report = try await client.refresh(Array(providerIDs).sorted())
                applyRefreshOutcome(report)
                await load(all: true)
            } catch {
                actionError = describe(error)
            }
        }
        automaticRecoveryTask = task
        await task.value
        if automaticRecoveryTask != nil { automaticRecoveryTask = nil }
    }

    nonisolated static func staleProviderIDs(
        config: ConfigResponse?,
        accounts: [Account],
        snapshots: [UsageSnapshot],
        now: Date
    ) -> [String] {
        guard let config else { return [] }
        let staleAfter = TimeInterval(config.pollIntervalSeconds * 2)
        return config.providers.compactMap { providerID, toggle in
            guard toggle.enabled else { return nil }
            let allProviderAccounts = accounts.filter { $0.providerId == providerID }
            let providerAccounts = allProviderAccounts.filter { !$0.hidden }
            if !allProviderAccounts.isEmpty && providerAccounts.isEmpty { return nil }
            let enabledAccounts = providerAccounts.filter(\.collectionEnabled)
            if !providerAccounts.isEmpty && enabledAccounts.isEmpty { return nil }

            let accountIDs = Set(enabledAccounts.map(\.id))
            let relevantSnapshots = snapshots.filter {
                $0.providerId == providerID
                    && (accountIDs.isEmpty || accountIDs.contains($0.accountId))
            }
            if !enabledAccounts.isEmpty {
                let latestByAccount = Dictionary(grouping: relevantSnapshots, by: \.accountId)
                    .mapValues { values in values.map(\.collectedAt).max() }
                let hasStaleAccount = enabledAccounts.contains { account in
                    guard let latest = latestByAccount[account.id].flatMap({ $0 }) else { return true }
                    return now.timeIntervalSince(latest) > staleAfter
                }
                return hasStaleAccount ? providerID : nil
            }

            guard !relevantSnapshots.isEmpty else { return providerID }
            let latestByAccount = Dictionary(grouping: relevantSnapshots, by: \.accountId)
            let hasStaleAccount = latestByAccount.values.contains { values in
                guard let latest = values.map(\.collectedAt).max() else { return true }
                return now.timeIntervalSince(latest) > staleAfter
            }
            return hasStaleAccount ? providerID : nil
        }.sorted()
    }

    private func enabledProviderIDs() -> Set<String> {
        Set(config?.providers.compactMap { $0.value.enabled ? $0.key : nil } ?? [])
    }

    private func beginRefreshing(_ providerIDs: Set<String>) {
        refreshActivityCount += 1
        for providerID in providerIDs {
            refreshingProviderCounts[providerID, default: 0] += 1
        }
        refreshing = true
        build()
    }

    private func endRefreshing(_ providerIDs: Set<String>) {
        refreshActivityCount = max(0, refreshActivityCount - 1)
        for providerID in providerIDs {
            let remaining = (refreshingProviderCounts[providerID] ?? 1) - 1
            if remaining > 0 {
                refreshingProviderCounts[providerID] = remaining
            } else {
                refreshingProviderCounts.removeValue(forKey: providerID)
            }
        }
        refreshing = refreshActivityCount > 0
        build()
    }
    private func deliverPendingNotifications() async {
        guard notificationAuthorization == .authorized || notificationAuthorization == .provisional else { return }
        do {
            let pending = try await client.pendingNotifications()
            let delivery = await notificationDelivery.deliver(pending)
            notificationError = delivery.errors.isEmpty
                ? nil
                : "Could not deliver \(delivery.errors.count) usage alert(s): \(delivery.errors.joined(separator: "; "))"
            if !delivery.deliveredIDs.isEmpty {
                try await client.acknowledgeNotifications(delivery.deliveredIDs)
            }
        } catch {
            notificationError = "Could not load usage alerts: \(describe(error))"
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
        ProviderCatalog.name(for: id)
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
        let dashboardSummary = dashboardSummary
        let windowProvenance = windowProvenance
        let ui = ui
        let refreshingProviderIDs = Set(refreshingProviderCounts.keys)
        let daemon = daemon
        let menuEligibleProviderIDs = Self.providerIDsWithDataOrConnection(
            accounts: accounts,
            snapshots: snapshots
        )
        let visibleProviderIds = Set(
            config?.providers.compactMap { $0.value.enabled ? $0.key : nil } ?? []
        )

        buildTask?.cancel()
        buildTask = Task { [weak self] in
            let next = await Task.detached(priority: .userInitiated) {
                let engine = DashboardBuilder(
                    config: config,
                    accounts: accounts,
                    health: health,
                    snapshots: snapshots,
                    forecasts: forecasts,
                    dashboard: dashboardSummary,
                    windowProvenance: windowProvenance,
                    ui: ui,
                    refreshingProviderIDs: refreshingProviderIDs,
                    visible: {
                        !ui.hiddenProviders.contains($0)
                            && (config == nil || visibleProviderIds.contains($0))
                    }
                )
                return Self.derive(
                    from: engine.build(),
                    daemon: daemon,
                    ui: ui,
                    menuEligibleProviderIDs: menuEligibleProviderIDs
                )
            }.value
            guard let self, !Task.isCancelled, revision == self.buildRevision else { return }
            // A menu-only preference may have changed while this detached build
            // was running. Apply the latest menu configuration so an older
            // captured metric or row count cannot flash back into the status item.
            let current = self.updatingMenuBarPresentation(in: next)
            if self.derived != current { self.derived = current }
            self.pruneStaleAcknowledgements()
        }
    }

    nonisolated private static func derive(
        from output: DashboardBuilder.Output,
        daemon: DaemonState,
        ui: UIConfig,
        menuEligibleProviderIDs: Set<String>
    ) -> DerivedState {
        let visibleProviders = output.providers.filter(\.visibleInMenu)
        let menu = menuContent(
            providers: visibleProviders,
            daemon: daemon,
            ui: ui,
            eligibleProviderIDs: menuEligibleProviderIDs
        )
        return DerivedState(
            providers: visibleProviders,
            settingsProviders: output.settingsProviders,
            cost: output.costDashboard,
            menuPreview: menu.preview,
            menuStatus: menu.status,
            menuBars: menu.bars
        )
    }

    /// Menu-bar preferences only transform already-built provider view models.
    /// Updating these synchronously avoids launching and awaiting a full detached
    /// dashboard build for every right-click menu selection.
    private func updateMenuBarPresentation() {
        let next = updatingMenuBarPresentation(in: derived)
        if derived != next { derived = next }
    }

    private func updatingMenuBarPresentation(in state: DerivedState) -> DerivedState {
        let eligibleProviderIDs = Self.providerIDsWithDataOrConnection(
            accounts: accounts,
            snapshots: snapshots
        )
        let menu = Self.menuContent(
            providers: state.providers,
            daemon: daemon,
            ui: ui,
            eligibleProviderIDs: eligibleProviderIDs
        )
        let next = DerivedState(
            providers: state.providers,
            settingsProviders: state.settingsProviders,
            cost: state.cost,
            menuPreview: menu.preview,
            menuStatus: menu.status,
            menuBars: menu.bars
        )
        return next
    }

    private func dashboardConfigurationChanged(from oldValue: UIConfig) -> Bool {
        ui.hiddenProviders != oldValue.hiddenProviders
            || ui.hiddenWindows != oldValue.hiddenWindows
            || ui.providerOrder != oldValue.providerOrder
            || ui.seenAlerts != oldValue.seenAlerts
    }

    private func menuBarConfigurationChanged(from oldValue: UIConfig) -> Bool {
        ui.menuMetric != oldValue.menuMetric
            || ui.showProviderLabels != oldValue.showProviderLabels
            || ui.maxMenuProviders != oldValue.maxMenuProviders
    }

    private func scheduleUIConfigPersistence() {
        let config = ui
        uiPersistenceTask?.cancel()
        uiPersistenceTask = Task { [weak self, uiConfigStore] in
            try? await Task.sleep(for: .milliseconds(300))
            guard !Task.isCancelled else { return }
            do {
                try await uiConfigStore.save(config)
                self?.preferencesError = nil
            } catch {
                self?.preferencesError = "UI preferences could not be saved: \(error.localizedDescription)"
            }
        }
    }

    /// Drop acknowledgements whose exact alert signature is no longer active, so that a
    /// resolved-then-recurring alert (e.g. a weekly limit that resets and fills again)
    /// re-alerts, and an escalation (warning → critical) is treated as a new alert.
    private func pruneStaleAcknowledgements() {
        let liveAlerts = Set(providers.flatMap { ($0.subAccounts ?? [$0]).compactMap(\.alertSignature) })
        let pruned = ui.pruningAcknowledgements(to: liveAlerts)
        guard pruned != ui else { return }
        // UIConfig is a value type whose property observer rebuilds the view models.
        // Assign the fully-pruned value once so a partial update cannot recursively
        // re-enter this method before both acknowledgement sets have been updated.
        ui = pruned
    }

    /// Stable identifier for a single progress bar (window), composed of its
    /// provider and window ids. Hiding is provider-wide, not per-account.
    nonisolated static func windowKey(_ providerId: String, _ windowId: String) -> String {
        "\(providerId)|\(windowId)"
    }

    /// Hide a single progress bar. Captures the label so Settings can name it
    /// once it's been filtered out of the live view models.
    func hideWindow(_ window: WindowVM) {
        ui.hiddenWindows[Self.windowKey(window.providerId, window.id)] = window.label
    }

    /// Restore a hidden progress bar by its composite key.
    func showWindow(_ key: String) {
        ui.hiddenWindows.removeValue(forKey: key)
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

    nonisolated static func menuContent(
        providers: [ProviderVM],
        daemon: DaemonState,
        ui: UIConfig,
        eligibleProviderIDs: Set<String>
    ) -> (preview: String, status: DisplayStatus, bars: [MenuBarProviderVM]) {
        guard daemon != .offline else { return ("Usage offline", .offline, []) }
        var preview = ""
        let eligible = providers.filter {
            $0.enabled && $0.visibleInMenu && eligibleProviderIDs.contains($0.providerId)
        }
        let providerCount = ui.resolvedMenuProviderCount(automaticCount: eligible.count)
        let shown = Array(eligible.prefix(providerCount))
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
        let bars = shown.map { provider in
            let displayed = provider.percent.map { max(0, min(100, ui.menuMetric == .used ? 100 - $0 : $0)) }
            return MenuBarProviderVM(id: provider.id, providerId: provider.providerId, short: provider.short, percent: displayed, status: provider.status)
        }
        if preview.isEmpty { return ("Usage", .stale, []) }
        return (preview, menuStatus(for: shown), bars)
    }

    nonisolated static func providerIDsWithDataOrConnection(
        accounts: [Account],
        snapshots: [UsageSnapshot]
    ) -> Set<String> {
        var providerIDs = Set(snapshots.map(\.providerId))
        providerIDs.formUnion(accounts.lazy.filter { !$0.hidden }.map(\.providerId))
        return providerIDs
    }

    nonisolated private static func menuStatus(for providers: [ProviderVM]) -> DisplayStatus {
        providers.map(\.status).max { severity($0) < severity($1) } ?? .stale
    }

    nonisolated private static func severity(_ status: DisplayStatus) -> Int {
        switch status {
        case .normal: 0
        case .disabled: 1
        case .stale, .refreshing: 2
        case .warning: 3
        case .critical: 4
        case .error, .offline: 5
        }
    }
}
