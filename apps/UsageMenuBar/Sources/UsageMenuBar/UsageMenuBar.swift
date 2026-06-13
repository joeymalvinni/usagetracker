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
        popover.contentSize = NSSize(width: 460, height: 560)
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
        let size = NSSize(width: 460, height: 560)
        let window = NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless], backing: .buffered, defer: false)
        window.isOpaque = false
        window.backgroundColor = .clear
        window.level = .floating
        let effect = NSVisualEffectView(frame: NSRect(origin: .zero, size: size))
        effect.material = .popover
        effect.state = .active
        effect.blendingMode = .behindWindow
        effect.wantsLayer = true
        effect.layer?.cornerRadius = 18
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
    @Published var cost = CostDashboardVM.empty
    @Published var menuTitle = NSAttributedString(string: "Usage")
    @Published var menuPreview = ""
    @Published var ui = UIConfig.load() {
        didSet { ui.save(); build() }
    }

    private var socketPath: String {
        didSet {
            if socketPath != oldValue { client = DaemonClient(socketPath: socketPath) }
        }
    }
    private var client: DaemonClient

    init() {
        let socketPath = AppState.defaultSocketPath()
        self.socketPath = socketPath
        self.client = DaemonClient(socketPath: socketPath)
    }

    init(socketPath: String) {
        self.socketPath = socketPath
        self.client = DaemonClient(socketPath: socketPath)
    }

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
        do { _ = try await client.refresh(nil); await load(all: true) } catch { fail(error) }
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
        var order = providers.map(\.id)
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
        do {
            config = try await client.config()
            updateSocketPath(from: config)
            if all { accounts = try await client.accounts() }
            async let h = client.health()
            async let u = client.usage()
            health = try await h; snapshots = try await u
            if !all && hasUnknownAccountReferences() { accounts = try await client.accounts() }
            daemon = .online; message = nil; build()
        } catch { fail(error); build() }
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
        cost = engine.costDashboard
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
                let displayed = max(0, min(100, ui.menuMetric == .used ? 100 - percent : percent))
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
        try Task.checkCancellation()
        let line = try String(decoding: encoder.encode(request) + [10], as: UTF8.self)
        let response = try Socket.line(path: socketPath, request: line, timeout: seconds)
        try Task.checkCancellation()
        let decoded = try decoder.decode(DaemonResponse.self, from: Data(response.utf8))
        if case let .error(error) = decoded { throw DaemonError.api(error.message) }
        return decoded
    }
}

enum Socket {
    static func line(path: String, request: String, timeout: Double) throws -> String {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw DaemonError.transport(errno) }
        defer { close(fd) }
        let deadline = Date().addingTimeInterval(timeout)

        let flags = fcntl(fd, F_GETFL, 0)
        guard flags >= 0, fcntl(fd, F_SETFL, flags | O_NONBLOCK) >= 0 else { throw DaemonError.transport(errno) }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8)
        let maxPathBytes = MemoryLayout.size(ofValue: addr.sun_path) - 1
        guard pathBytes.count <= maxPathBytes else { throw DaemonError.pathTooLong(path, maxPathBytes) }
        let bytes = pathBytes + [0]
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: bytes) }
        let len = socklen_t(MemoryLayout<sa_family_t>.size + bytes.count)
        let connected = withUnsafePointer(to: &addr) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, len) } }
        if connected != 0 {
            let code = errno
            guard code == EINPROGRESS || code == EWOULDBLOCK else { throw DaemonError.transport(code) }
            try wait(fd: fd, events: Int16(POLLOUT), deadline: deadline)
            var error = Int32(0)
            var length = socklen_t(MemoryLayout<Int32>.size)
            guard getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &length) == 0 else { throw DaemonError.transport(errno) }
            guard error == 0 else { throw DaemonError.transport(error) }
        }

        var out = Array(request.utf8)
        while !out.isEmpty {
            let sent = out.withUnsafeBytes { write(fd, $0.baseAddress!, out.count) }
            if sent > 0 {
                out.removeFirst(sent)
            } else if errno == EAGAIN || errno == EWOULDBLOCK {
                try wait(fd: fd, events: Int16(POLLOUT), deadline: deadline)
            } else if errno != EINTR {
                throw DaemonError.transport(errno)
            }
        }

        var data = [UInt8](), buf = [UInt8](repeating: 0, count: 4096)
        while true {
            let n = read(fd, &buf, buf.count)
            if n > 0 {
                if let i = buf[..<n].firstIndex(of: 10) { data += buf[..<i]; break }
                data += buf[..<n]
            } else if n == 0 {
                throw DaemonError.closed
            } else if errno == EAGAIN || errno == EWOULDBLOCK {
                try wait(fd: fd, events: Int16(POLLIN), deadline: deadline)
            } else if errno != EINTR {
                throw DaemonError.transport(errno)
            }
        }
        return String(decoding: data, as: UTF8.self)
    }

    private static func wait(fd: Int32, events: Int16, deadline: Date) throws {
        while true {
            var pollFd = pollfd(fd: fd, events: events, revents: 0)
            let result = poll(&pollFd, 1, remainingMilliseconds(until: deadline))
            if result > 0 { return }
            if result == 0 { throw DaemonError.timeout }
            if errno != EINTR { throw DaemonError.transport(errno) }
        }
    }

    private static func remainingMilliseconds(until deadline: Date) -> Int32 {
        let remaining = deadline.timeIntervalSinceNow
        guard remaining > 0 else { return 0 }
        return min(Int32.max, max(1, Int32((remaining * 1000).rounded(.up))))
    }
}

enum DaemonError: LocalizedError {
    case api(String), badResponse, closed, timeout, transport(Int32), pathTooLong(String, Int)
    var errorDescription: String? {
        switch self {
        case .api(let s): s
        case .badResponse: "Unexpected daemon response"
        case .closed: "Daemon closed the connection"
        case .timeout: "Daemon request timed out"
        case .transport(let code): String(cString: strerror(code))
        case .pathTooLong(let path, let maxBytes): "Unix socket path is too long (\(path.utf8.count) bytes, max \(maxBytes)): \(path)"
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
    var object: [String: JSONValue]? { if case .object(let value) = self { value } else { nil } }
    var array: [JSONValue]? { if case .array(let value) = self { value } else { nil } }
    var string: String? { if case .string(let value) = self { value } else { nil } }
    var double: Double? {
        switch self {
        case .number(let value): value
        case .string(let value): Double(value)
        default: nil
        }
    }
    var uint64: UInt64? {
        switch self {
        case .number(let value): value >= 0 ? UInt64(value.rounded()) : nil
        case .string(let value): UInt64(value)
        default: nil
        }
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
    static let dayKey: DateFormatter = { let f = DateFormatter(); f.calendar = .current; f.locale = Locale(identifier: "en_US_POSIX"); f.dateFormat = "yyyy-MM-dd"; return f }()
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
struct CostDashboardVM: Equatable {
    static let empty = CostDashboardVM(days: [], providers: [])
    let days: [CostDayVM]
    let providers: [CostProviderVM]

    var hasData: Bool { days.contains { $0.totalCost > 0 || $0.totalTokens > 0 } }
    var todayCost: Double { days.last?.totalCost ?? 0 }
    var todayTokens: UInt64 { days.last?.totalTokens ?? 0 }
    var cost30d: Double { days.reduce(0) { $0 + $1.totalCost } }
    var tokens30d: UInt64 { days.reduce(0) { $0.saturatingAdd($1.totalTokens) } }
}
struct CostProviderVM: Identifiable, Equatable { let id, name, symbol: String }
struct CostDayVM: Identifiable, Equatable {
    let id: String
    let date: Date
    let providers: [CostProviderDayVM]

    var totalCost: Double { providers.reduce(0) { $0 + $1.cost } }
    var totalTokens: UInt64 { providers.reduce(0) { $0.saturatingAdd($1.tokens) } }
}
struct CostProviderDayVM: Identifiable, Equatable {
    var id: String { providerId }
    let providerId, providerName, symbol: String
    let date: Date
    let dateKey: String
    let cost: Double
    let tokens: UInt64
}
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
    var costDashboard: CostDashboardVM {
        let calendar = Calendar.current
        let today = calendar.startOfDay(for: Date())
        let dayStarts = (0..<30).compactMap { offset in
            calendar.date(byAdding: .day, value: offset - 29, to: today)
        }
        let dayKeys = dayStarts.map { DateFormats.dayKey.string(from: $0) }
        let knownProviders = ordered(Array(Set(snapshots.map(\.providerId) + ["codex", "claude"])))
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
        let percent = (w.percentRemaining ?? computedPercent(w)).map { max(0, min(100, $0)) }
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
    private func pretty(_ id: String) -> String { id == "codex" ? "Codex" : id == "claude" ? "Claude" : id.capitalized }
    private func short(_ id: String) -> String { id == "codex" ? "Cdx" : id == "claude" ? "Clde" : String(pretty(id).prefix(4)) }
    private func symbol(_ id: String) -> String { id == "codex" ? "terminal" : id == "claude" ? "sparkles" : "chart.bar" }
    private func healthText(_ h: ProviderHealth) -> String { String(describing: h.status).camelSplit }
    private func time(_ d: Date) -> String { d.formatted(date: .omitted, time: .shortened) }
    private func relative(_ d: Date) -> String { d.formatted(.relative(presentation: .numeric)) }
}
extension UsageUnit { var label: String { switch self { case .tokens: "tokens"; case .requests: "requests"; case .credits: "credits"; case .usd: "USD"; case .percent: "%"; case .unknown: "units"; case .other(let s): s } } }
extension String { var camelSplit: String { replacingOccurrences(of: "([a-z])([A-Z])", with: "$1 $2", options: .regularExpression).lowercased() } }
extension UInt64 {
    func saturatingAdd(_ other: UInt64) -> UInt64 {
        let result = addingReportingOverflow(other)
        return result.overflow ? UInt64.max : result.partialValue
    }
}

func providerColor(_ id: String) -> Color {
    switch id {
    case "codex": .green
    case "claude": .orange
    default: .blue
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
    date.formatted(.dateTime.month(.abbreviated).day())
}

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
            Divider().opacity(0.30)
            Group {
                switch selection {
                case .summary: Summary()
                case .provider(let id): Detail(provider: state.providers.first { $0.id == id })
                case .settings: Settings()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(width: 460, height: 560)
        .liquidGlassRoot()
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
        .padding(.vertical, 14).frame(width: 58).liquidGlassRail()
    }
    private func rail(_ s: Selection, _ tip: String, @ViewBuilder icon: () -> some View) -> some View {
        Button { selection = s } label: {
            icon()
                .frame(width: 32, height: 32)
                .liquidGlassSelection(active: selection == s)
        }
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
                CostDashboard(dashboard: state.cost)
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
                .liquidGlassIconButton().help("Refresh")
        }
    }
}

enum CostRange: Int, CaseIterable {
    case seven = 7, thirty = 30
    var label: String { self == .seven ? "7d" : "30d" }
}

struct CostDashboard: View {
    let dashboard: CostDashboardVM
    @State private var range: CostRange = .seven
    @State private var hover: CostProviderDayVM?

    private var days: [CostDayVM] { Array(dashboard.days.suffix(range.rawValue)) }

    var body: some View {
        VStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Activity").font(.headline)
                        Text(hover.map(hoverText) ?? activitySubtitle).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                    Spacer()
                    Picker("", selection: $range) {
                        ForEach(CostRange.allCases, id: \.self) { Text($0.label).tag($0) }
                    }.pickerStyle(.segmented).labelsHidden().frame(width: 86)
                }
                CostActivityChart(days: days, hover: $hover)
                    .frame(height: 152)
                HStack(spacing: 10) {
                    ForEach(dashboard.providers) { provider in
                        HStack(spacing: 5) {
                            Circle().fill(providerColor(provider.id)).frame(width: 7, height: 7)
                            Text(provider.name).font(.caption2).foregroundStyle(.secondary)
                        }
                    }
                    Spacer()
                }
            }.glass()

            LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], spacing: 6) {
                CostKPI(title: "Today cost", value: formatUsd(dashboard.todayCost))
                CostKPI(title: "30d cost", value: formatUsd(dashboard.cost30d))
                CostKPI(title: "Today tokens", value: formatTokens(dashboard.todayTokens))
                CostKPI(title: "30d tokens", value: formatTokens(dashboard.tokens30d))
            }
        }
    }

    private var activitySubtitle: String {
        if dashboard.hasData { "\(range.label) spend" }
        else { "No cost data yet" }
    }
    private func hoverText(_ value: CostProviderDayVM) -> String {
        "\(value.providerName) · \(shortDate(value.date)): \(formatUsd(value.cost)) · \(formatTokens(value.tokens))"
    }
}

struct CostActivityChart: View {
    let days: [CostDayVM]
    @Binding var hover: CostProviderDayVM?

    private var maxValue: Double {
        max(1, days.map(total).max() ?? 1)
    }

    var body: some View {
        GeometryReader { geo in
            HStack(alignment: .bottom, spacing: days.count > 7 ? 3 : 8) {
                ForEach(Array(days.enumerated()), id: \.element.id) { index, day in
                    VStack(spacing: 5) {
                        ZStack(alignment: .bottom) {
                            RoundedRectangle(cornerRadius: 4).fill(.quaternary.opacity(0.45))
                            VStack(spacing: 1) {
                                Spacer(minLength: 0)
                                ForEach(day.providers.reversed().filter { value($0) > 0 }) { provider in
                                    RoundedRectangle(cornerRadius: 3)
                                        .fill(providerColor(provider.providerId).gradient)
                                        .frame(height: segmentHeight(provider, maxHeight: geo.size.height - 22))
                                        .onHover { inside in if inside { hover = provider } }
                                        .help("\(provider.providerName) \(shortDate(provider.date)): \(formatUsd(provider.cost))")
                                }
                            }
                        }
                        .frame(maxWidth: .infinity)
                        .clipShape(RoundedRectangle(cornerRadius: 4))

                        Text(label(for: day.date, index: index))
                            .font(.caption2.monospacedDigit())
                            .foregroundStyle(.tertiary)
                            .lineLimit(1)
                            .frame(height: 14)
                    }
                }
            }
            .onHover { inside in if !inside { hover = nil } }
        }
    }

    private func value(_ provider: CostProviderDayVM) -> Double {
        provider.cost
    }
    private func total(_ day: CostDayVM) -> Double {
        day.providers.reduce(0) { $0 + value($1) }
    }
    private func segmentHeight(_ provider: CostProviderDayVM, maxHeight: CGFloat) -> CGFloat {
        let scaled = maxHeight * CGFloat(value(provider) / maxValue)
        return max(4, scaled)
    }
    private func label(for date: Date, index: Int) -> String {
        if days.count <= 7 {
            return date.formatted(.dateTime.weekday(.narrow))
        }
        if index == 0 || index == days.count - 1 || index % 5 == 0 {
            return date.formatted(.dateTime.day())
        }
        return ""
    }
}

struct CostKPI: View {
    let title, value: String
    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(title).font(.caption2).foregroundStyle(.secondary)
            Text(value).font(.system(.subheadline, design: .rounded).weight(.semibold)).monospacedDigit().lineLimit(1)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 2)
        .padding(.vertical, 1)
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
    @ViewBuilder
    func liquidGlassRoot() -> some View {
        if #available(macOS 26.0, *) {
            glassEffect(
                .regular.tint(.white.opacity(0.08)),
                in: RoundedRectangle(cornerRadius: 18, style: .continuous)
            )
        } else {
            background(.regularMaterial)
        }
    }

    @ViewBuilder
    func liquidGlassRail() -> some View {
        if #available(macOS 26.0, *) {
            glassEffect(
                .regular.tint(.white.opacity(0.05)),
                in: UnevenRoundedRectangle(
                    topLeadingRadius: 18,
                    bottomLeadingRadius: 18,
                    bottomTrailingRadius: 0,
                    topTrailingRadius: 0,
                    style: .continuous
                )
            )
        } else {
            background(.thinMaterial)
        }
    }

    @ViewBuilder
    func liquidGlassSelection(active: Bool) -> some View {
        if active {
            background(AnyShapeStyle(.quaternary), in: RoundedRectangle(cornerRadius: 8))
        } else {
            self
        }
    }

    @ViewBuilder
    func liquidGlassIconButton() -> some View {
        buttonStyle(.borderless)
    }

    func glass(secondary: Bool = false) -> some View {
        padding(10).liquidGlassCard(secondary: secondary)
    }

    @ViewBuilder
    private func liquidGlassCard(secondary: Bool) -> some View {
        if #available(macOS 26.0, *) {
            glassEffect(
                .regular.tint(secondary ? .white.opacity(0.04) : .white.opacity(0.075)),
                in: RoundedRectangle(cornerRadius: 12, style: .continuous)
            )
            .overlay {
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(.white.opacity(secondary ? 0.08 : 0.12), lineWidth: 0.75)
            }
        } else {
            background(
                secondary ? AnyShapeStyle(.thinMaterial) : AnyShapeStyle(.ultraThinMaterial),
                in: RoundedRectangle(cornerRadius: 8)
            )
            .overlay(RoundedRectangle(cornerRadius: 8).stroke(.white.opacity(0.10)))
        }
    }
}
