import Combine
import Foundation
@preconcurrency import UserNotifications

enum DaemonState: Equatable, Sendable { case unknown, online, offline }

struct DerivedState: Equatable, Sendable {
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

private enum PendingAction {
    case provider(String), accountProvider(String), account(String), interval, notifications
}

private enum ProviderSignInFollowUp {
    case addedAccount(profileId: String)
    case repairedAccount(accountId: String?, startedAt: Date)
}

@MainActor final class AppState: ObservableObject {
    let updater = AppUpdater()
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
    @Published var serverProviders = [String: ServerProviderDescriptor]()
    @Published var serverProviderOrder = [String]()
    @Published var onboardingDiscoveryStarted = false
    @Published var onboardingDiscoveryRunning = false
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
    private var automaticRecoveryTask: Task<Void, Never>?
    private var lastAutomaticRecoveryAt: Date?
    private var onboardingProviderDefaultsPending = true
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
        self.onboardingProviderDefaultsPending = !ui.onboardingCompleted
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
        self.onboardingProviderDefaultsPending = !ui.onboardingCompleted
    }

    func bootstrap() async {
        // First-run onboarding explains Keychain access before any collector can
        // cause macOS to show its permission prompt. The user starts the daemon
        // explicitly from the onboarding screen after reading that explanation.
        guard ui.onboardingCompleted else { return }
        // A bundled daemon can outlive the app that launched it. Check it
        // before the first API request so replacing the app bundle also
        // replaces an older daemon instead of silently serving stale data.
        _ = await daemonSupervisor.ensureRunning(socketPath: socketPath)
        await load()
        // Running the notifications fixture is an explicit request to exercise
        // desktop delivery. Development builds use their own bundle identity,
        // so request its authorization on the first fixture launch and then
        // drain the alerts that the initial load left queued.
        if ProcessInfo.processInfo.environment["USAGE_TRACKER_FIXTURE"] == "notifications",
           notificationAuthorization == .notDetermined
        {
            await requestNotificationAuthorizationIfNeeded()
            await deliverPendingNotifications()
        }
    }

    func usageEvents(
        accountId: String,
        offset: UInt32 = 0,
        limit: UInt16? = nil
    ) async throws -> UsageEventPage {
        try await client.usageEvents(accountId: accountId, offset: offset, limit: limit)
    }
    func refreshForPopoverOpen() async {
        guard ui.onboardingCompleted || onboardingDiscoveryStarted else { return }
        await load()
        await requestStaleDataRecovery(delay: .zero, reloadBeforeChecking: false)
    }
    func refreshAfterWake() async {
        guard ui.onboardingCompleted || onboardingDiscoveryStarted else { return }
        await requestStaleDataRecovery(delay: wakeRecoveryDelay, reloadBeforeChecking: true)
    }
    func pollLoop() async {
        while !Task.isCancelled {
            let seconds = max(15, Int(config?.pollIntervalSeconds ?? 60))
            try? await Task.sleep(for: .seconds(seconds))
            guard ui.onboardingCompleted || onboardingDiscoveryStarted else { continue }
            await load()
        }
    }
    func refreshAll() async {
        guard ui.onboardingCompleted || onboardingDiscoveryStarted else { return }
        let providerIDs = enabledProviderIDs()
        guard !providerIDs.isEmpty else { return }
        beginRefreshing(providerIDs)
        defer { endRefreshing(providerIDs) }
        do {
            let report = try await client.refresh(Array(providerIDs).sorted())
            applyRefreshOutcome(report)
            await load()
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
            await load()
        } catch {
            actionError = describe(error)
        }
    }
    func visible(_ id: String) -> Bool { config?.providers[id]?.enabled == true }
    func setProviderEnabled(_ id: String, _ enabled: Bool) async {
        await perform(.provider(id)) {
            config = try await client.updateConfig(pollIntervalSeconds: nil, providers: [id: enabled])
            build()
            if enabled { applyRefreshOutcome(try await client.refresh([id])) }
            await load()
        }
    }

    func setPollInterval(_ seconds: UInt64) async {
        await perform(.interval) {
            config = try await client.updateConfig(pollIntervalSeconds: seconds, providers: nil)
            build()
            await load()
        }
    }

    func setNotificationsEnabled(_ enabled: Bool) async {
        if enabled && notificationAuthorization == .notDetermined {
            await requestNotificationAuthorizationIfNeeded()
        }
        await perform(.notifications) {
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
            build()
            if enabled {
                await deliverPendingNotifications()
            }
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
        guard supportsAddAccount(providerId) else {
            actionError = "\(providerName(providerId)) does not support adding accounts."
            return
        }
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

    func providerSignInLink(
        _ providerId: String,
        accountId: String? = nil,
        addAccount: Bool = false
    ) async -> String? {
        pendingAccountProviders.insert(providerId)
        defer { pendingAccountProviders.remove(providerId) }
        do {
            let url: String?
            let followUp: ProviderSignInFollowUp
            let hasAccounts = accounts.contains { $0.providerId == providerId }
            let canAddAccount = supportsAddAccount(providerId)
            let canRepair = supportsRepair(providerId)
            let shouldRepair = canRepair
                && (!canAddAccount || (hasAccounts && !addAccount))
            if shouldRepair {
                let repairAccountId = accountId
                    ?? accounts.first { $0.providerId == providerId }?.id
                let startedAt = Date()
                let response = try await client.repairProvider(
                    providerId: providerId,
                    accountId: repairAccountId,
                    signInAction: .copyLink
                )
                url = response.authenticationUrl
                followUp = .repairedAccount(
                    accountId: repairAccountId,
                    startedAt: startedAt
                )
            } else if canAddAccount {
                let response = try await client.addProviderAccount(
                    providerId: providerId,
                    displayName: nil,
                    signInAction: .copyLink
                )
                url = response.authenticationUrl
                followUp = .addedAccount(profileId: response.profileId)
            } else {
                actionError = "\(providerName(providerId)) does not support sign-in."
                return nil
            }

            guard let url, !url.isEmpty else {
                actionError = "\(providerName(providerId)) did not provide a sign-in link."
                return nil
            }
            actionError = nil
            monitorProviderSignIn(providerId: providerId, followUp: followUp)
            return url
        } catch {
            actionMessage = nil
            actionError = describe(error)
            return nil
        }
    }

    private func monitorProviderSignIn(
        providerId: String,
        followUp: ProviderSignInFollowUp
    ) {
        Task { [weak self] in
            guard let self else { return }
            switch followUp {
            case .addedAccount(let profileId):
                await waitForProviderAccount(providerId: providerId, profileId: profileId)
            case .repairedAccount(let accountId, let startedAt):
                await waitForProviderRepair(
                    providerId: providerId,
                    accountId: accountId,
                    startedAt: startedAt
                )
            }
        }
    }

    func setAccountHidden(_ id: String, _ hidden: Bool) async {
        await perform(.account(id)) {
            _ = try await client.updateAccount(accountId: id, hidden: hidden)
            await load()
        }
    }

    func setAccountCollectionEnabled(_ id: String, _ enabled: Bool) async {
        await perform(.account(id)) {
            _ = try await client.updateAccount(accountId: id, hidden: enabled ? false : nil, collectionEnabled: enabled)
            if enabled, let providerId = accounts.first(where: { $0.id == id })?.providerId {
                applyRefreshOutcome(try await client.refresh([providerId]))
            }
            await load()
        }
    }

    func removeAccount(_ id: String) async {
        await perform(.account(id)) {
            _ = try await client.removeAccount(accountId: id)
            actionMessage = "Account removed. Usage history was kept."
            await load()
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
                await load()
            } else {
                actionError = message
                await load()
            }
            return
        }
        actionError = nil
        actionMessage = "Account and usage history deleted."
        await load()
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
        await load()
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
        await perform(.account(id)) {
            _ = try await client.updateAccount(accountId: id, displayName: name)
            actionMessage = "Account renamed to \(name)."
            await load()
            if let providerId = accounts.first(where: { $0.id == id })?.providerId,
               providerSetups[providerId] != nil {
                await loadProviderSetup(providerId)
            }
        }
    }

    func loadProviderSetup(_ providerId: String) async {
        guard supportsSetup(providerId) else {
            actionError = "\(providerName(providerId)) does not expose setup options."
            return
        }
        await perform(.accountProvider(providerId)) {
            providerSetups[providerId] = try await client.providerSetup(providerId: providerId)
        }
    }

    func updateProviderSetup(providerId: String, key: String, value: String?) async {
        guard supportsSetup(providerId) else {
            actionError = "\(providerName(providerId)) does not expose setup options."
            return
        }
        await perform(.accountProvider(providerId)) {
            let values: [String: String?] = [key: value]
            providerSetups[providerId] = try await client.updateProviderSetup(
                providerId: providerId,
                settings: values
            )
            actionMessage = "\(providerName(providerId)) setup updated."
            applyRefreshOutcome(try await client.refresh([providerId]))
            await load()
        }
    }

    func repairProvider(_ providerId: String, accountId: String? = nil) async {
        guard supportsRepair(providerId) else {
            actionError = "\(providerName(providerId)) does not support reconnecting accounts."
            return
        }
        let startedAt = Date()
        await perform(.accountProvider(providerId)) {
            let response = try await client.repairProvider(providerId: providerId, accountId: accountId)
            actionMessage = response.message
            if supportsMultipleAccounts(providerId) {
                await waitForProviderRepair(
                    providerId: providerId,
                    accountId: accountId,
                    startedAt: startedAt
                )
            }
        }
    }

    func launchProviderAccount(_ accountId: String) async {
        guard let providerId = accounts.first(where: { $0.id == accountId })?.providerId else {
            actionError = "The selected account is no longer available."
            return
        }
        guard supportsLaunchAccount(providerId) else {
            actionError = "\(providerName(providerId)) does not support launching account sessions."
            return
        }
        await perform(.account(accountId)) {
            let response = try await client.launchProviderAccount(accountId: accountId)
            actionMessage = response.message
        }
    }

    func completeOnboarding() {
        // A clean installation has no "previous version," so its current
        // release is not presented as something that was just updated.
        if ui.lastSeenReleaseNotesVersion == nil {
            ui.lastSeenReleaseNotesVersion = updater.currentVersion
        }
        ui.onboardingCompleted = true
        onboardingProviderDefaultsPending = false
        actionError = nil
        actionMessage = "Setup complete. Usage will update automatically."
    }

    func restartOnboarding() {
        // The daemon is already running for an existing installation, so rerunning
        // setup can go straight to account discovery and provider controls.
        onboardingDiscoveryStarted = true
        onboardingProviderDefaultsPending = false
        ui.onboardingCompleted = false
    }

    func discoverAccountsForOnboarding() async {
        guard !onboardingDiscoveryRunning else { return }
        onboardingDiscoveryStarted = true
        onboardingDiscoveryRunning = true
        actionError = nil
        actionMessage = nil
        defer { onboardingDiscoveryRunning = false }

        _ = await daemonSupervisor.ensureRunning(socketPath: socketPath)
        await load()
        guard daemon == .online else {
            actionError = message ?? "UsageTracker could not start its background service."
            return
        }

        do {
            if onboardingProviderDefaultsPending {
                let toggles = Self.onboardingDefaultProviderToggles(
                    providerIDs: serverProviderOrder
                )
                config = try await client.updateConfig(
                    pollIntervalSeconds: nil,
                    providers: toggles
                )
                onboardingProviderDefaultsPending = false
                build()
            }

            // Only inspect providers the user has enabled. Enabling a provider from
            // onboarding performs its own targeted refresh, so credentials for other
            // providers are never touched without an explicit opt-in.
            let firstScan = try await client.refresh(Array(enabledProviderIDs()).sorted())
            await load()

            let discoveredAccountIDs = Set(firstScan.providerResults.compactMap(\.accountId))
            let discoveredAccounts = accounts.filter { discoveredAccountIDs.contains($0.id) }
            for account in discoveredAccounts where !account.collectionEnabled {
                _ = try await client.updateAccount(
                    accountId: account.id,
                    collectionEnabled: true
                )
            }

            let providerIDs = Set(discoveredAccounts.map(\.providerId))
            if !providerIDs.isEmpty {
                let toggles = Dictionary(uniqueKeysWithValues: providerIDs.map { ($0, true) })
                config = try await client.updateConfig(
                    pollIntervalSeconds: nil,
                    providers: toggles
                )

                if discoveredAccounts.contains(where: { !$0.collectionEnabled }) {
                    _ = try await client.refresh(Array(providerIDs).sorted())
                }
            }
            await load()

            if discoveredAccounts.isEmpty {
                actionError = "No signed-in accounts were found. Sign in to an enabled provider, then check again."
            } else {
                let noun = discoveredAccounts.count == 1 ? "account" : "accounts"
                let failedProviders = firstScan.providerResults.filter {
                    switch $0.status {
                    case .ok, .disabled: false
                    default: true
                    }
                }.map(\.providerId)
                let suffix = failedProviders.isEmpty
                    ? ""
                    : " You can retry any provider that still needs attention below."
                actionMessage = "Found \(discoveredAccounts.count) \(noun).\(suffix)"
            }
        } catch {
            actionError = "Account discovery failed: \(describe(error))"
        }
    }

    static func onboardingDefaultProviderToggles(providerIDs: [String]) -> [String: Bool] {
        Dictionary(
            uniqueKeysWithValues: Set(providerIDs)
                .union(["codex"])
                .map { ($0, $0 == "codex") }
        )
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
    private func load(allowDaemonStart: Bool = true) async {
        if let current = loadTask {
            await current.value
            return
        }

        let task = Task {
            await performLoad(allowDaemonStart: allowDaemonStart)
            loadTask = nil
        }
        loadTask = task
        await task.value
    }

    private func performLoad(allowDaemonStart: Bool) async {
        do {
            let state = try await client.state()
            serverProviderOrder = state.server.providers.map(\.id)
            serverProviders = Dictionary(
                uniqueKeysWithValues: state.server.providers.map { ($0.id, $0) }
            )
            if config != state.config { config = state.config }
            updateSocketPath(from: state.config)
            if accounts != state.accounts { accounts = state.accounts }
            if health != state.health { health = state.health }
            if snapshots != state.snapshots { snapshots = state.snapshots }
            if forecasts != state.forecasts { forecasts = state.forecasts }
            if dashboardSummary != state.dashboard { dashboardSummary = state.dashboard }
            if windowProvenance != state.windowProvenance {
                windowProvenance = state.windowProvenance
            }
            daemon = .online; message = nil; build()
            await refreshNotificationAuthorization()
            if config?.notifications.enabled == true {
                await deliverPendingNotifications()
            }
        } catch {
            if allowDaemonStart, await daemonSupervisor.ensureRunning(socketPath: socketPath) {
                await performLoad(allowDaemonStart: false)
            } else {
                if await daemonSupervisor.launchAgentRequiresApproval() {
                    fail(DaemonLaunchAgentError.requiresApproval)
                } else {
                    fail(error)
                }
                build()
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
            if reloadBeforeChecking { await load() }

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
                await load()
            } catch {
                actionError = describe(error)
            }
        }
        automaticRecoveryTask = task
        await task.value
        if automaticRecoveryTask != nil { automaticRecoveryTask = nil }
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
    private func waitForProviderAccount(providerId: String, profileId: String) async {
        for _ in 0..<600 {
            guard !Task.isCancelled else { return }
            do {
                let discovered = try await client.accounts()
                if discovered.contains(where: { $0.providerId == providerId && $0.profileId == profileId }) {
                    accounts = discovered
                    actionError = nil
                    actionMessage = "\(providerName(providerId)) account connected."
                    await load()
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
                    await load()
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

    private func perform(
        _ pending: PendingAction,
        operation: () async throws -> Void
    ) async {
        setPending(pending, true)
        defer { setPending(pending, false) }
        actionError = nil
        do {
            try await operation()
        } catch {
            actionMessage = nil
            actionError = describe(error)
        }
    }

    private func setPending(_ action: PendingAction, _ active: Bool) {
        switch action {
        case .provider(let id):
            if active { pendingProviders.insert(id) } else { pendingProviders.remove(id) }
        case .accountProvider(let id):
            if active { pendingAccountProviders.insert(id) } else { pendingAccountProviders.remove(id) }
        case .account(let id):
            if active { pendingAccounts.insert(id) } else { pendingAccounts.remove(id) }
        case .interval:
            pendingInterval = active
        case .notifications:
            pendingNotifications = active
        }
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
        serverProviders[id]?.displayName ?? ProviderCatalog.name(for: id)
    }

    func supportsMultipleAccounts(_ providerId: String) -> Bool {
        providerSupports(providerId, capability: \.multipleAccounts, in: serverProviders)
    }

    func supportsAddAccount(_ providerId: String) -> Bool {
        providerSupports(providerId, capability: \.addAccount, in: serverProviders)
    }

    func supportsRepair(_ providerId: String) -> Bool {
        providerSupports(providerId, capability: \.repair, in: serverProviders)
    }

    func supportsLaunchAccount(_ providerId: String) -> Bool {
        providerSupports(providerId, capability: \.launchAccount, in: serverProviders)
    }

    func supportsSetup(_ providerId: String) -> Bool {
        providerSupports(providerId, capability: \.setup, in: serverProviders)
    }

    private func refreshStatusText(_ status: ProviderRefreshStatus) -> String {
        switch status {
        case .ok: "refreshed"
        case .credentialsMissing: "sign-in required"
        case .credentialsInvalid, .unauthorized: "credentials need repair"
        case .keychainAccessFailed: "keychain access failed"
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
        let serverProviders = serverProviders
        let serverProviderOrder = serverProviderOrder
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
                    serverProviders: serverProviders,
                    serverProviderOrder: serverProviderOrder,
                    ui: ui,
                    refreshingProviderIDs: refreshingProviderIDs,
                    visible: { config == nil || visibleProviderIds.contains($0) }
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
        ui.hiddenWindows != oldValue.hiddenWindows
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

    func showsReleaseNotes(_ notes: ReleaseNotes) -> Bool {
        ui.lastSeenReleaseNotesVersion != notes.version
    }

    func dismissReleaseNotes(_ notes: ReleaseNotes) {
        ui.lastSeenReleaseNotesVersion = notes.version
        updater.dismissInstalledReleaseNotes(version: notes.version)
    }

}
