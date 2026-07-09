import Combine
import Foundation

enum DaemonState { case unknown, online, offline }

@MainActor final class AppState: ObservableObject {
    @Published var daemon: DaemonState = .unknown
    @Published var config: ConfigResponse?
    @Published var accounts = [Account]()
    @Published var health = [ProviderHealth]()
    @Published var snapshots = [UsageSnapshot]()
    @Published var refreshing = false
    @Published var message: String?
    @Published var actionError: String?
    @Published var pendingProviders = Set<String>()
    @Published var pendingInterval = false
    @Published var providers = [ProviderVM]()
    @Published var settingsProviders = [ProviderVM]()
    @Published var cost = CostDashboardVM.empty
    @Published var menuPreview = "Usage"
    @Published var menuStatus = DisplayStatus.stale
    @Published var menuBars = [MenuBarProviderVM]()
    @Published var ui = UIConfig.load() {
        didSet { ui.save(); build() }
    }

    private var socketPath: String {
        didSet {
            if socketPath != oldValue { client = DaemonClient(socketPath: socketPath) }
        }
    }
    private var client: DaemonClient
    private let daemonSupervisor = DaemonSupervisor()

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
        await load(all: true)
    }
    func refreshForPopoverOpen() async { await load(all: false) }
    func pollLoop() async {
        while !Task.isCancelled {
            let seconds = max(15, Int(config?.pollIntervalSeconds ?? 60))
            try? await Task.sleep(for: .seconds(seconds))
            await load(all: false)
        }
    }
    func refreshAll() async {
        refreshing = true; defer { refreshing = false }
        do { _ = try await client.refresh(nil); await load(all: true) } catch { fail(error) }
    }
    func refreshProvider(_ id: String) async {
        refreshing = true; defer { refreshing = false }
        do { _ = try await client.refresh([id]); await load(all: true) } catch { fail(error) }
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
            if enabled { _ = try? await client.refresh([id]) }
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

    func moveProviders(from source: IndexSet, to destination: Int) {
        var order = settingsProviders.map(\.id)
        order.move(fromOffsets: source, toOffset: destination)
        ui.providerOrder = order
    }

    private static func defaultSocketPath() -> String {
        ProcessInfo.processInfo.environment["USAGE_TRACKER_SOCKET"] ?? UIPaths.socket.path
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
        do {
            config = try await client.config()
            updateSocketPath(from: config)
            if all { accounts = try await client.accounts() }
            async let h = client.health()
            async let u = client.usage()
            health = try await h; snapshots = try await u
            if !all && hasUnknownAccountReferences() { accounts = try await client.accounts() }
            daemon = .online; message = nil; build()
        } catch {
            if allowDaemonStart, await daemonSupervisor.ensureRunning(socketPath: socketPath) {
                await load(all: all, allowDaemonStart: false)
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
    private func fail(_ error: Error) {
        daemon = .offline
        message = describe(error)
    }
    private func describe(_ error: Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
    }
    private func build() {
        let engine = MetricEngine(config: config, accounts: accounts, health: health, snapshots: snapshots, ui: ui, visible: uiVisible)
        providers = engine.providers
        settingsProviders = engine.settingsProviders
        cost = engine.costDashboard
        let (preview, status, bars) = menuContent()
        menuPreview = preview
        menuStatus = status
        menuBars = bars
    }
    private func menuContent() -> (preview: String, status: DisplayStatus, bars: [MenuBarProviderVM]) {
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
            return MenuBarProviderVM(providerId: provider.id, short: provider.short, percent: displayed, status: provider.status)
        }
        if preview.isEmpty { return ("Usage", .stale, []) }
        return (preview, menuStatus(for: shown), bars)
    }

    private func menuStatus(for providers: [ProviderVM]) -> DisplayStatus {
        providers.map(\.status).max { severity($0) < severity($1) } ?? .stale
    }

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
