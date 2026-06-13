import AppKit
import Combine
import Darwin
import SwiftUI

@main enum UsageMenuBar {
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.accessory)
        app.run()
    }
}

@MainActor final class AppDelegate: NSObject, NSApplicationDelegate {
    private let state = AppState()
    private let popover = NSPopover()
    private var item: NSStatusItem!
    private var bag = Set<AnyCancellable>()

    func applicationDidFinishLaunching(_ note: Notification) {
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        item.button?.image = NSImage(systemSymbolName: "gauge.with.dots.needle.67percent", accessibilityDescription: "Usage")
        item.button?.imagePosition = .imageLeading
        item.button?.target = self
        item.button?.action = #selector(toggle)

        popover.behavior = .transient
        popover.contentSize = NSSize(width: 540, height: 560)
        popover.contentViewController = NSHostingController(rootView: Popover().environmentObject(state))

        state.$menuTitle.receive(on: RunLoop.main).sink { [weak self] title in self?.item.button?.attributedTitle = title }.store(in: &bag)
        Task { await state.bootstrap(); await state.pollLoop() }

        if ProcessInfo.processInfo.environment["USAGE_POPOVER_DEBUG"] == "1" { showDebugWindow() }
    }

    @objc private func toggle() {
        guard let button = item.button else { return }
        if popover.isShown { popover.performClose(nil) } else {
            popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
            Task { await state.refreshForPopoverOpen() }
        }
    }

    // Renders the popover content in a floating window so the UI can be
    // inspected/screenshotted without clicking the status item.
    private var debugWindow: NSWindow?
    private func showDebugWindow() {
        let size = NSSize(width: 540, height: 560)
        let window = NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless], backing: .buffered, defer: false)
        window.isOpaque = false
        window.backgroundColor = .clear
        window.level = .floating
        let effect = NSVisualEffectView(frame: NSRect(origin: .zero, size: size))
        effect.material = .popover
        effect.state = .active
        effect.blendingMode = .behindWindow
        effect.wantsLayer = true
        effect.layer?.cornerRadius = 12
        effect.layer?.masksToBounds = true
        let host = NSHostingView(rootView: Popover().environmentObject(state))
        host.frame = effect.bounds
        host.autoresizingMask = [.width, .height]
        effect.addSubview(host)
        window.contentView = effect
        if let screen = NSScreen.main {
            window.setFrameTopLeftPoint(NSPoint(x: 60, y: screen.frame.maxY - 60))
        }
        window.orderFrontRegardless()
        debugWindow = window
    }
}

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
    @Published var menuTitle = NSAttributedString(string: "Usage")
    @Published var menuPreview = ""
    @Published var ui = UIConfig.load() {
        didSet { ui.save(); build() }
    }

    private var socketPath = ProcessInfo.processInfo.environment["USAGE_TRACKER_SOCKET"] ?? UIPaths.socket.path

    func bootstrap() async { await load(all: true) }
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
        do { _ = try await client.refresh(nil); await load(all: false) } catch { fail(error) }
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
            await load(all: false)
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
        } catch {
            actionError = describe(error)
        }
    }

    func moveProviders(from source: IndexSet, to destination: Int) {
        var order = providers.map(\.id)
        order.move(fromOffsets: source, toOffset: destination)
        ui.providerOrder = order
    }

    private var client: DaemonClient { DaemonClient(socketPath: socketPath) }
    private func uiVisible(_ id: String) -> Bool { ui.hiddenProviders.contains(id) == false }
    private func load(all: Bool) async {
        do {
            config = try await client.config(); socketPath = config?.socketPath ?? socketPath
            if all { accounts = try await client.accounts() }
            async let h = client.health()
            async let u = client.usage()
            health = try await h; snapshots = try await u
            daemon = .online; message = nil; build()
        } catch { fail(error); build() }
    }
    private func fail(_ error: Error) {
        daemon = .offline
        message = describe(error)
    }
    private func describe(_ error: Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
    }
    private func build() {
        providers = MetricEngine(config: config, accounts: accounts, health: health, snapshots: snapshots, ui: ui, visible: uiVisible).providers
        let (title, preview) = menuContent()
        menuTitle = title
        menuPreview = preview
    }
    private func menuContent() -> (NSAttributedString, String) {
        guard daemon != .offline else { return (NSAttributedString(string: "Usage offline"), "Usage offline") }
        let title = NSMutableAttributedString()
        var preview = ""
        let shown = providers.filter { $0.enabled && $0.visibleInMenu }.prefix(max(0, ui.maxMenuProviders))
        for (index, provider) in shown.enumerated() {
            if index > 0 { title.append(NSAttributedString(string: "  ")); preview += "  " }
            let value: String
            if let percent = provider.percent {
                let displayed = ui.menuMetric == .used ? 100 - percent : percent
                value = "\(Int(displayed.rounded()))%"
            } else {
                value = provider.primary
            }
            let text = ui.showProviderLabels ? "\(provider.short) \(value)" : value
            var attributes = [NSAttributedString.Key: Any]()
            if ui.colorByStatus, let color = provider.status.menuColor { attributes[.foregroundColor] = color }
            title.append(NSAttributedString(string: text, attributes: attributes))
            preview += text
        }
        if title.string.isEmpty { return (NSAttributedString(string: "Usage"), "Usage") }
        return (title, preview)
    }
}

enum DaemonState { case unknown, online, offline }

enum UIPaths {
    static let root = FileManager.default.homeDirectoryForCurrentUser.appending(path: ".usagetracker")
    static let ui = root.appending(path: "ui")
    static let socket = root.appending(path: "usage.sock")
    static let config = ui.appending(path: "config.json")
}

struct UIConfig: Codable, Equatable {
    enum MenuMetric: String, Codable, CaseIterable {
        case remaining, used
        var label: String { self == .remaining ? "% left" : "% used" }
    }

    var hiddenProviders = Set<String>()
    var providerOrder = [String]()
    var menuMetric = MenuMetric.remaining
    var showProviderLabels = true
    var maxMenuProviders = 2
    var colorByStatus = true

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        hiddenProviders = try c.decodeIfPresent(Set<String>.self, forKey: .hiddenProviders) ?? []
        providerOrder = try c.decodeIfPresent([String].self, forKey: .providerOrder) ?? []
        menuMetric = try c.decodeIfPresent(MenuMetric.self, forKey: .menuMetric) ?? .remaining
        showProviderLabels = try c.decodeIfPresent(Bool.self, forKey: .showProviderLabels) ?? true
        maxMenuProviders = try c.decodeIfPresent(Int.self, forKey: .maxMenuProviders) ?? 2
        colorByStatus = try c.decodeIfPresent(Bool.self, forKey: .colorByStatus) ?? true
    }

    static func load() -> Self {
        guard let data = try? Data(contentsOf: UIPaths.config),
              let config = try? JSONDecoder().decode(Self.self, from: data)
        else { let config = Self(); config.save(); return config }
        return config
    }

    func save() {
        try? FileManager.default.createDirectory(at: UIPaths.ui, withIntermediateDirectories: true)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        if let data = try? encoder.encode(self) { try? data.write(to: UIPaths.config, options: .atomic) }
    }
}

struct DaemonClient {
    let socketPath: String
    private let decoder = JSONDecoder.usage
    private let encoder = JSONEncoder()

    func config() async throws -> ConfigResponse { guard case let .config(v) = try await send(.getConfig, 3) else { throw DaemonError.badResponse }; return v }
    func accounts() async throws -> [Account] { guard case let .accounts(v) = try await send(.getAccounts, 3) else { throw DaemonError.badResponse }; return v }
    func health() async throws -> [ProviderHealth] { guard case let .providerHealth(v) = try await send(.getProviderHealth, 3) else { throw DaemonError.badResponse }; return v }
    func usage() async throws -> [UsageSnapshot] { guard case let .usage(v) = try await send(.getUsage, 3) else { throw DaemonError.badResponse }; return v }
    func refresh(_ providers: [String]?) async throws -> RefreshResponse { guard case let .refresh(v) = try await send(.refresh(providers), 30) else { throw DaemonError.badResponse }; return v }
    func updateConfig(pollIntervalSeconds: UInt64?, providers: [String: Bool]?) async throws -> ConfigResponse {
        guard case let .config(v) = try await send(.updateConfig(pollIntervalSeconds: pollIntervalSeconds, providers: providers), 5) else { throw DaemonError.badResponse }
        return v
    }

    private func send(_ request: DaemonRequest, _ seconds: Double) async throws -> DaemonResponse {
        try await withThrowingTaskGroup(of: DaemonResponse.self) { group in
            group.addTask {
                let line = try String(decoding: encoder.encode(request) + [10], as: UTF8.self)
                let response = try Socket.line(path: socketPath, request: line)
                let decoded = try decoder.decode(DaemonResponse.self, from: Data(response.utf8))
                if case let .error(error) = decoded { throw DaemonError.api(error.message) }
                return decoded
            }
            group.addTask { try await Task.sleep(for: .seconds(seconds)); throw DaemonError.timeout }
            let value = try await group.next()!
            group.cancelAll()
            return value
        }
    }
}

enum Socket {
    static func line(path: String, request: String) throws -> String {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw DaemonError.transport(errno) }
        defer { close(fd) }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8.prefix(MemoryLayout.size(ofValue: addr.sun_path) - 1)) + [0]
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: bytes) }
        let len = socklen_t(MemoryLayout<sa_family_t>.size + bytes.count)
        let connected = withUnsafePointer(to: &addr) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, len) } }
        guard connected == 0 else { throw DaemonError.transport(errno) }

        var out = Array(request.utf8)
        while !out.isEmpty {
            let sent = out.withUnsafeBytes { write(fd, $0.baseAddress!, out.count) }
            guard sent > 0 else { throw DaemonError.transport(errno) }
            out.removeFirst(sent)
        }

        var data = [UInt8](), buf = [UInt8](repeating: 0, count: 4096)
        while true {
            let n = read(fd, &buf, buf.count)
            guard n > 0 else { throw DaemonError.closed }
            if let i = buf[..<n].firstIndex(of: 10) { data += buf[..<i]; break }
            data += buf[..<n]
        }
        return String(decoding: data, as: UTF8.self)
    }
}

enum DaemonError: LocalizedError {
    case api(String), badResponse, closed, timeout, transport(Int32)
    var errorDescription: String? {
        switch self {
        case .api(let s): s
        case .badResponse: "Unexpected daemon response"
        case .closed: "Daemon closed the connection"
        case .timeout: "Daemon request timed out"
        case .transport(let code): String(cString: strerror(code))
        }
    }
}

enum DaemonRequest: Encodable {
    case getUsage, refresh([String]?), getProviderHealth, getAccounts, getConfig
    case updateConfig(pollIntervalSeconds: UInt64?, providers: [String: Bool]?)
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: K.self)
        switch self {
        case .getUsage: try c.encode("get_usage", forKey: .method)
        case .getProviderHealth: try c.encode("get_provider_health", forKey: .method)
        case .getAccounts: try c.encode("get_accounts", forKey: .method)
        case .getConfig: try c.encode("get_config", forKey: .method)
        case .refresh(let ids): try c.encode("refresh", forKey: .method); try c.encode(ids, forKey: .providers)
        case .updateConfig(let interval, let providers):
            try c.encode("update_config", forKey: .method)
            try c.encodeIfPresent(interval, forKey: .pollIntervalSeconds)
            try c.encodeIfPresent(providers?.mapValues { ProviderToggle(enabled: $0) }, forKey: .providers)
        }
    }
    enum K: String, CodingKey { case method, providers, pollIntervalSeconds = "poll_interval_seconds" }
}

enum DaemonResponse: Decodable {
    case usage([UsageSnapshot]), refresh(RefreshResponse), providerHealth([ProviderHealth]), accounts([Account]), config(ConfigResponse), error(ApiError)
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        switch try c.decode(String.self, forKey: .type) {
        case "usage": self = .usage(try c.decode([UsageSnapshot].self, forKey: .snapshots))
        case "refresh": self = .refresh(try RefreshResponse(from: decoder))
        case "provider_health": self = .providerHealth(try c.decode([ProviderHealth].self, forKey: .health))
        case "accounts": self = .accounts(try c.decode([Account].self, forKey: .accounts))
        case "config": self = .config(try c.decode(ConfigResponse.self, forKey: .config))
        case "error": self = .error(try c.decode(ApiError.self, forKey: .error))
        default: throw DecodingError.dataCorrupted(.init(codingPath: c.codingPath, debugDescription: "unknown response"))
        }
    }
    enum K: String, CodingKey { case type, snapshots, health, accounts, config, error }
}

struct Account: Decodable, Identifiable, Equatable {
    let id, providerId, externalAccountId: String
    let displayName: String?
    let createdAt, updatedAt: Date
}
struct UsageSnapshot: Decodable, Identifiable, Equatable {
    var id: String { "\(providerId):\(accountId)" }
    let providerId, accountId: String
    let collectedAt: Date
    let windows: [UsageWindow]
    let metadata: JSONValue
}
struct UsageWindow: Decodable, Identifiable, Equatable {
    var id: String { windowId }
    let windowId, label: String
    let kind: UsageWindowKind
    let used, limit, remaining: UsageAmount?
    let percentUsed, percentRemaining: Double?
    let resetAt: Date?
}
struct UsageAmount: Decodable, Equatable { let value: Double; let unit: UsageUnit }
struct ProviderHealth: Decodable, Identifiable, Equatable {
    var id: String { providerId }
    let providerId: String
    let accountId: String?
    let status: ProviderHealthStatus
    let collectionMode: String?
    let lastSuccessAt, lastFailureAt: Date?
    let lastErrorCode, lastErrorMessage: String?
    let updatedAt: Date
}
struct RefreshResponse: Decodable, Equatable {
    let startedAt, finishedAt: Date
    let providerResults: [ProviderRefreshResult]
}
struct ProviderRefreshResult: Decodable, Equatable {
    let providerId: String
    let accountId: String?
    let status: ProviderRefreshStatus
    let collectionMode: String?
    let collectedAt: Date?
    let message: String?
}
struct ConfigResponse: Decodable, Equatable {
    let pollIntervalSeconds: UInt64
    let configPath, socketPath, dbPath: String
    let enabledProviders: [String]
    let providers: [String: ProviderToggle]
}
struct ProviderToggle: Codable, Equatable { let enabled: Bool }
struct ApiError: Decodable, Equatable { let code, message: String }

enum UsageWindowKind: Equatable, Decodable {
    case session, daily, weekly, monthly, credits, tokens, other(String)
    init(from decoder: Decoder) throws {
        if let s = try? decoder.singleValueContainer().decode(String.self) { self = Self.named(s); return }
        let o = try decoder.singleValueContainer().decode([String: String].self)
        self = .other(o["other"] ?? o.first?.value ?? "other")
    }
    private static func named(_ s: String) -> Self {
        switch s { case "session": .session; case "daily": .daily; case "weekly": .weekly; case "monthly": .monthly; case "credits": .credits; case "tokens": .tokens; default: .other(s) }
    }
}
enum UsageUnit: Equatable, Decodable { case tokens, requests, credits, usd, percent, unknown, other(String)
    init(from decoder: Decoder) throws { switch try decoder.singleValueContainer().decode(String.self) { case "tokens": self = .tokens; case "requests": self = .requests; case "credits": self = .credits; case "usd": self = .usd; case "percent": self = .percent; case "unknown": self = .unknown; case let s: self = .other(s) } }
}
enum ProviderHealthStatus: Equatable, Decodable { case ok, credentialsMissing, authFailed, rateLimited, providerError, parseError, backingOff, disabled, other(String)
    init(from decoder: Decoder) throws { switch try decoder.singleValueContainer().decode(String.self) { case "ok": self = .ok; case "credentials_missing": self = .credentialsMissing; case "auth_failed": self = .authFailed; case "rate_limited": self = .rateLimited; case "provider_error": self = .providerError; case "parse_error": self = .parseError; case "backing_off": self = .backingOff; case "disabled": self = .disabled; case let s: self = .other(s) } }
}
enum ProviderRefreshStatus: Equatable, Decodable { case ok, credentialsMissing, credentialsInvalid, unauthorized, rateLimited, network, parse, providerUnavailable, storageError, other(String)
    init(from decoder: Decoder) throws { switch try decoder.singleValueContainer().decode(String.self) { case "ok": self = .ok; case "credentials_missing": self = .credentialsMissing; case "credentials_invalid": self = .credentialsInvalid; case "unauthorized": self = .unauthorized; case "rate_limited": self = .rateLimited; case "network": self = .network; case "parse": self = .parse; case "provider_unavailable": self = .providerUnavailable; case "storage_error": self = .storageError; case let s: self = .other(s) } }
}

enum JSONValue: Decodable, Equatable {
    case string(String), number(Double), bool(Bool), object([String: JSONValue]), array([JSONValue]), null
    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let v = try? c.decode(Bool.self) { self = .bool(v) }
        else if let v = try? c.decode(Double.self) { self = .number(v) }
        else if let v = try? c.decode(String.self) { self = .string(v) }
        else if let v = try? c.decode([JSONValue].self) { self = .array(v) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }
}

extension JSONDecoder {
    static var usage: JSONDecoder {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        d.dateDecodingStrategy = .custom { decoder in
            let s = try decoder.singleValueContainer().decode(String.self)
            for f in [DateFormats.fractional, DateFormats.whole] { if let date = f.date(from: s) { return date } }
            throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "invalid date \(s)"))
        }
        return d
    }
}
enum DateFormats {
    static let fractional: ISO8601DateFormatter = { let f = ISO8601DateFormatter(); f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]; return f }()
    static let whole: ISO8601DateFormatter = { let f = ISO8601DateFormatter(); f.formatOptions = [.withInternetDateTime]; return f }()
}

struct ProviderVM: Identifiable, Equatable {
    let id, name, short, symbol, primary, detail: String
    let percent: Double?
    let status: DisplayStatus
    let windows, credits: [WindowVM]
    let account: String?
    let healthText: String
    let visibleInMenu: Bool
    let enabled: Bool
}
struct WindowVM: Identifiable, Equatable { let id, label, value, reset: String; let percent: Double?; let status: DisplayStatus }
enum DisplayStatus { case normal, warning, critical, stale, error, disabled, offline
    var color: Color { switch self { case .normal: .primary; case .warning: .orange; case .critical, .error, .offline: .red; case .stale, .disabled: .secondary } }
    var menuColor: NSColor? { switch self { case .warning: .systemOrange; case .critical, .error: .systemRed; default: nil } }
}

struct MetricEngine {
    let config: ConfigResponse?
    let accounts: [Account]
    let health: [ProviderHealth]
    let snapshots: [UsageSnapshot]
    let ui: UIConfig
    let visible: (String) -> Bool

    var providers: [ProviderVM] {
        var ids = Set((config?.enabledProviders ?? []) + health.map(\.providerId) + snapshots.map(\.providerId))
        if let config { ids.formUnion(config.providers.keys) }
        return ordered(Array(ids)).map(model)
    }
    private func ordered(_ ids: [String]) -> [String] {
        let preferred = ui.providerOrder + ["codex", "claude"]
        var seen = Set<String>()
        let ranked = preferred.filter { ids.contains($0) && seen.insert($0).inserted }
        return ranked + ids.filter { !ranked.contains($0) }.sorted()
    }
    private func model(_ id: String) -> ProviderVM {
        let latest = snapshots.filter { $0.providerId == id }.max { $0.collectedAt < $1.collectedAt }
        let h = selectedHealth(providerId: id, accountId: latest?.accountId)
        let account = accounts.first { $0.id == latest?.accountId || $0.id == h?.accountId }
        let windows = (latest?.windows ?? []).filter { $0.kind != .credits }.map(window)
        let credits = (latest?.windows ?? []).filter { $0.kind == .credits }.map(window)
        let primary = windows.compactMap(\.percent).min()
        let enabled = config?.providers[id]?.enabled ?? (config?.enabledProviders.contains(id) ?? (h?.status != .disabled))
        let status = status(id: id, percent: primary, latest: latest, health: h, enabled: enabled)
        return ProviderVM(
            id: id, name: pretty(id), short: short(id), symbol: symbol(id),
            primary: primary.map { "\(Int($0.rounded()))%" } ?? windows.first?.value ?? "No data",
            detail: latest.map { "updated \(relative($0.collectedAt))" } ?? "waiting for data",
            percent: primary, status: status, windows: windows, credits: credits,
            account: account?.displayName ?? account?.externalAccountId,
            healthText: h.map(healthText) ?? "unknown",
            visibleInMenu: visible(id),
            enabled: enabled
        )
    }
    private func selectedHealth(providerId: String, accountId: String?) -> ProviderHealth? {
        let providerHealth = health.filter { $0.providerId == providerId }
        if let accountId, let accountHealth = providerHealth.first(where: { $0.accountId == accountId }) {
            return accountHealth
        }
        return providerHealth.max { $0.updatedAt < $1.updatedAt }
    }
    private func window(_ w: UsageWindow) -> WindowVM {
        let percent = w.percentRemaining ?? computedPercent(w)
        let status: DisplayStatus = percent.map { $0 < 10 ? .critical : ($0 < 25 ? .warning : .normal) } ?? .normal
        return WindowVM(id: w.id, label: w.label, value: percent.map { "\(Int($0.rounded()))% left" } ?? amount(w.remaining ?? w.used), reset: w.resetAt.map { "resets \(time($0))" } ?? "", percent: percent, status: status)
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
        return "\(Int(a.value.rounded())) \(a.unit.label)"
    }
    private func pretty(_ id: String) -> String { id == "codex" ? "Codex" : id == "claude" ? "Claude" : id.capitalized }
    private func short(_ id: String) -> String { id == "codex" ? "Cdx" : id == "claude" ? "Clde" : String(pretty(id).prefix(4)) }
    private func symbol(_ id: String) -> String { id == "codex" ? "terminal" : id == "claude" ? "sparkles" : "chart.bar" }
    private func healthText(_ h: ProviderHealth) -> String { String(describing: h.status).camelSplit }
    private func time(_ d: Date) -> String { d.formatted(date: .omitted, time: .shortened) }
    private func relative(_ d: Date) -> String { d.formatted(.relative(presentation: .numeric)) }
}
extension UsageUnit { var label: String { switch self { case .tokens: "tokens"; case .requests: "requests"; case .credits: "credits"; case .usd: "USD"; case .percent: "%"; case .unknown: "units"; case .other(let s): s } } }
extension String { var camelSplit: String { replacingOccurrences(of: "([a-z])([A-Z])", with: "$1 $2", options: .regularExpression).lowercased() } }

enum ProviderBrand {
    @MainActor private static var cache = [String: NSImage?]()

    @MainActor static func image(_ id: String) -> NSImage? {
        if let cached = cache[id] { return cached }
        let name: String? = switch id {
        case "codex": "chatgpt"
        case "claude": "claude"
        default: nil
        }
        var image: NSImage?
        if let name,
           let url = Bundle.module.url(forResource: name, withExtension: "svg", subdirectory: "Resources"),
           let loaded = NSImage(contentsOf: url) {
            loaded.isTemplate = true
            image = loaded
        }
        cache[id] = image
        return image
    }
}

/// Brand logo rendered as a tintable template, falling back to the provider's SF Symbol.
struct ProviderIcon: View {
    let id: String
    let symbol: String
    var size: CGFloat = 15

    var body: some View {
        if let image = ProviderBrand.image(id) {
            Image(nsImage: image)
                .resizable()
                .renderingMode(.template)
                .scaledToFit()
                .frame(width: size, height: size)
        } else {
            Image(systemName: symbol)
        }
    }
}

struct Popover: View {
    @EnvironmentObject var state: AppState
    @State private var selection: Selection = {
        switch ProcessInfo.processInfo.environment["USAGE_DEBUG_PAGE"] {
        case "settings": .settings
        case let page? where page.hasPrefix("provider:"): .provider(String(page.dropFirst("provider:".count)))
        default: .summary
        }
    }()
    var body: some View {
        HStack(spacing: 0) {
            Rail(selection: $selection)
            Divider().opacity(0.45)
            Group {
                switch selection {
                case .summary: Summary()
                case .provider(let id): Detail(provider: state.providers.first { $0.id == id })
                case .settings: Settings()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(width: 540, height: 560)
        .background(.regularMaterial)
    }
}
enum Selection: Hashable { case summary, provider(String), settings }

struct Rail: View {
    @EnvironmentObject var state: AppState
    @Binding var selection: Selection
    var body: some View {
        VStack(spacing: 10) {
            rail(.summary, "Summary") { Image(systemName: "gauge") }
            ForEach(state.providers) { p in
                rail(.provider(p.id), p.name) { ProviderIcon(id: p.id, symbol: p.symbol) }.foregroundStyle(p.status.color)
            }
            Spacer()
            rail(.settings, "Settings") { Image(systemName: "gearshape") }
        }
        .padding(.vertical, 14).frame(width: 58).background(.thinMaterial)
    }
    private func rail(_ s: Selection, _ tip: String, @ViewBuilder icon: () -> some View) -> some View {
        Button { selection = s } label: { icon().frame(width: 32, height: 32).background(selection == s ? AnyShapeStyle(.quaternary) : AnyShapeStyle(.clear), in: RoundedRectangle(cornerRadius: 8)) }
            .buttonStyle(.plain).help(tip)
    }
}

struct Summary: View {
    @EnvironmentObject var state: AppState
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Header(title: "Usage", subtitle: state.daemon == .offline ? (state.message ?? "daemon offline") : "daemon healthy")
            ScrollView { LazyVStack(spacing: 10) {
                if state.providers.isEmpty { EmptyState(text: state.daemon == .offline ? "Daemon unavailable" : "No providers enabled") }
                ForEach(state.providers) { ProviderRow(provider: $0) }
            }.padding(.bottom, 10) }
        }.padding(18)
    }
}

struct Header: View {
    @EnvironmentObject var state: AppState
    let title, subtitle: String
    var body: some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) { Text(title).font(.title3.bold()); Text(subtitle).font(.caption).foregroundStyle(.secondary).lineLimit(1) }
            Spacer()
            Button { Task { await state.refreshAll() } } label: { Image(systemName: state.refreshing ? "arrow.triangle.2.circlepath.circle" : "arrow.clockwise").frame(width: 28, height: 28) }
                .buttonStyle(.borderless).help("Refresh")
        }
    }
}

struct ProviderRow: View {
    let provider: ProviderVM
    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack { Label(provider.name, systemImage: provider.symbol).font(.headline); Spacer(); Text(provider.primary).font(.system(.title3, design: .rounded).weight(.semibold)).foregroundStyle(provider.status.color) }
            ForEach(provider.windows.prefix(3)) { WindowRow(window: $0) }
            if let account = provider.account { Text(account).font(.caption).foregroundStyle(.secondary) }
            if provider.healthText != "ok" { Text(provider.healthText).font(.caption).foregroundStyle(provider.status.color) }
        }.glass()
    }
}

struct Detail: View {
    let provider: ProviderVM?
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            if let p = provider {
                Header(title: p.name, subtitle: [p.account, p.detail, p.healthText].compactMap(\.self).joined(separator: " · "))
                ScrollView { VStack(spacing: 10) {
                    ForEach(p.windows) { WindowRow(window: $0).glass() }
                    if !p.credits.isEmpty {
                        Text("Credits").font(.caption.bold()).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
                        ForEach(p.credits) { WindowRow(window: $0).glass(secondary: true) }
                    }
                } }
            } else { EmptyState(text: "Provider not found") }
        }.padding(18)
    }
}

struct WindowRow: View {
    let window: WindowVM
    var body: some View {
        VStack(spacing: 5) {
            HStack { Text(window.label).lineLimit(1); Spacer(); Text(window.value).foregroundStyle(window.status.color); if !window.reset.isEmpty { Text(window.reset).foregroundStyle(.secondary) } }
                .font(.caption)
            if let p = window.percent { ProgressView(value: p, total: 100).tint(window.status.color).controlSize(.small) }
        }
    }
}

struct Settings: View {
    @EnvironmentObject var state: AppState
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Header(title: "Settings", subtitle: state.daemon == .offline ? "daemon offline — provider settings unavailable" : "changes apply immediately")
            if let error = state.actionError { Text(error).font(.caption).foregroundStyle(.red) }
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    Text("Providers").font(.caption.bold()).foregroundStyle(.secondary)
                    providerList
                    Text("Menu bar").font(.caption.bold()).foregroundStyle(.secondary)
                    ForEach(state.providers.filter(\.enabled)) { p in
                        Toggle(isOn: visibleBinding(p.id)) {
                            Label { Text("Show \(p.name)") } icon: { ProviderIcon(id: p.id, symbol: p.symbol).foregroundStyle(p.status.color) }
                        }.toggleStyle(.switch).glass()
                    }
                    VStack(spacing: 8) {
                        LabeledContent("Metric") {
                            Picker("", selection: $state.ui.menuMetric) {
                                ForEach(UIConfig.MenuMetric.allCases, id: \.self) { Text($0.label).tag($0) }
                            }.labelsHidden().fixedSize()
                        }
                        LabeledContent("Providers shown") {
                            Stepper(value: $state.ui.maxMenuProviders, in: 0...4) { Text("\(state.ui.maxMenuProviders)").monospacedDigit().frame(minWidth: 14) }.fixedSize()
                        }
                        LabeledContent("Short labels") { Toggle("", isOn: $state.ui.showProviderLabels).labelsHidden().toggleStyle(.switch).controlSize(.small) }
                        LabeledContent("Color by status") { Toggle("", isOn: $state.ui.colorByStatus).labelsHidden().toggleStyle(.switch).controlSize(.small) }
                        LabeledContent("Preview") { Text(state.menuPreview.isEmpty ? "Usage" : state.menuPreview).font(.caption.monospacedDigit()).foregroundStyle(.secondary) }
                    }.glass()
                    Text("Refresh").font(.caption.bold()).foregroundStyle(.secondary)
                    LabeledContent("Poll interval") {
                        if state.pendingInterval { ProgressView().controlSize(.small) } else {
                            Picker("", selection: intervalBinding) {
                                ForEach(intervalOptions, id: \.self) { Text(intervalLabel($0)).tag($0) }
                            }.labelsHidden().fixedSize().disabled(state.daemon == .offline)
                        }
                    }.glass()
                    Divider()
                    LabeledContent("Socket", value: state.config?.socketPath ?? "USAGE_TRACKER_SOCKET or ~/.usagetracker/usage.sock")
                    LabeledContent("Config", value: state.config?.configPath ?? "unknown")
                    LabeledContent("Database", value: state.config?.dbPath ?? "unknown")
                    LabeledContent("UI config", value: UIPaths.config.path)
                }.padding(.bottom, 10)
            }
            Spacer(minLength: 0)
            Button("Quit Usage") { NSApp.terminate(nil) }.frame(maxWidth: .infinity, alignment: .trailing)
        }.padding(18)
    }

    private var providerList: some View {
        VStack(spacing: 0) {
            List {
                ForEach(state.providers) { p in
                    ProviderSettingsRow(provider: p)
                        .listRowSeparator(.hidden)
                        .listRowBackground(Color.clear)
                        .listRowInsets(EdgeInsets(top: 4, leading: 0, bottom: 4, trailing: 0))
                }
                .onMove { from, to in state.moveProviders(from: from, to: to) }
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            .scrollDisabled(true)
            .frame(height: CGFloat(state.providers.count) * 64)
            Text("Drag to reorder. Order applies to the menu bar and summary.")
                .font(.caption2).foregroundStyle(.tertiary).frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func visibleBinding(_ id: String) -> Binding<Bool> {
        Binding(get: { state.visible(id) }, set: { state.setVisible(id, $0) })
    }

    private var intervalOptions: [UInt64] {
        var options: [UInt64] = [60, 120, 300, 600, 900, 1800, 3600]
        let current = state.config?.pollIntervalSeconds ?? 300
        if !options.contains(current) { options.append(current); options.sort() }
        return options
    }
    private var intervalBinding: Binding<UInt64> {
        Binding(
            get: { state.config?.pollIntervalSeconds ?? 300 },
            set: { seconds in Task { await state.setPollInterval(seconds) } }
        )
    }
    private func intervalLabel(_ seconds: UInt64) -> String {
        switch seconds {
        case ..<60: "\(seconds) sec"
        case 3600: "1 hour"
        case let s where s % 60 == 0: "\(s / 60) min"
        default: "\(seconds) sec"
        }
    }
}

struct ProviderSettingsRow: View {
    @EnvironmentObject var state: AppState
    let provider: ProviderVM

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "line.3.horizontal").foregroundStyle(.tertiary)
            Label {
                VStack(alignment: .leading) {
                    Text(provider.name)
                    Text(subtitle).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                }
            } icon: { ProviderIcon(id: provider.id, symbol: provider.symbol).foregroundStyle(provider.status.color) }
            Spacer()
            if state.pendingProviders.contains(provider.id) {
                ProgressView().controlSize(.small)
            } else {
                Toggle("", isOn: enabledBinding).labelsHidden().toggleStyle(.switch)
                    .disabled(state.daemon == .offline)
                    .help(provider.enabled ? "Stop collecting \(provider.name) usage" : "Start collecting \(provider.name) usage")
            }
        }.glass()
    }

    private var subtitle: String {
        if let account = provider.account { return account }
        if !provider.enabled { return "collection off" }
        return provider.healthText
    }
    private var enabledBinding: Binding<Bool> {
        Binding(get: { provider.enabled }, set: { on in Task { await state.setProviderEnabled(provider.id, on) } })
    }
}

struct EmptyState: View { let text: String; var body: some View { Text(text).foregroundStyle(.secondary).frame(maxWidth: .infinity, maxHeight: .infinity) } }

extension View {
    func glass(secondary: Bool = false) -> some View {
        padding(10).background(secondary ? AnyShapeStyle(.thinMaterial) : AnyShapeStyle(.ultraThinMaterial), in: RoundedRectangle(cornerRadius: 8)).overlay(RoundedRectangle(cornerRadius: 8).stroke(.white.opacity(0.10)))
    }
}
