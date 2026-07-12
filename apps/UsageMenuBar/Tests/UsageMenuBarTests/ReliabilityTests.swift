import Foundation
import XCTest
@testable import UsageMenuBar

final class DaemonClientTests: XCTestCase {
    func testDecodesCheckedInRustWireFixtures() throws {
        let usageURL = rustWireFixture("usage_v2.json")
        let usage = try JSONDecoder.usage.decode(DaemonResponse.self, from: Data(contentsOf: usageURL))
        guard case let .usage(response) = usage else {
            return XCTFail("expected usage fixture")
        }
        XCTAssertEqual(response.snapshots.first?.providerId, "codex")
        XCTAssertEqual(response.dashboard.pricing.coveredPercent, 100)
        XCTAssertEqual(response.dashboard.pricing.unpricedModels, [])
        XCTAssertEqual(response.windowProvenance.first?.authoritative, true)

        let errorURL = rustWireFixture("error_v2.json")
        let error = try JSONDecoder.usage.decode(DaemonResponse.self, from: Data(contentsOf: errorURL))
        guard case let .error(apiError) = error else {
            return XCTFail("expected error fixture")
        }
        XCTAssertEqual(apiError.code, "unsupported_method")
    }

    private func rustWireFixture(_ name: String) -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // UsageMenuBarTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // UsageMenuBar
            .deletingLastPathComponent() // apps
            .deletingLastPathComponent() // repository root
            .appendingPathComponent("crates/usage-core/wire-fixtures")
            .appendingPathComponent(name)
    }

    func testPreservesStructuredAPIErrorAndWritesProtocolVersion() async throws {
        let transport = RecordingTransport(response: """
            {"api_version":2,"type":"error","error":{"code":"unknown_account","message":"Account missing","retryable":false}}
            """)
        let client = DaemonClient(socketPath: "/tmp/usage.sock", transport: transport)

        do {
            try await client.deleteAccount(accountId: "account-1")
            XCTFail("expected an API error")
        } catch let error as DaemonError {
            XCTAssertEqual(error, .api(code: "unknown_account", message: "Account missing"))
        }

        let recordedRequest = await transport.lastRequest()
        let request = try XCTUnwrap(recordedRequest)
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(request.utf8)) as? [String: Any]
        )
        XCTAssertEqual(object["api_version"] as? Int, 2)
        XCTAssertEqual(object["method"] as? String, "delete_account")
        let timeout = await transport.lastTimeout()
        XCTAssertEqual(timeout, 10)
    }

    func testRejectsMissingProtocolVersionWithStableCode() async throws {
        let transport = RecordingTransport(response: """
            {"type":"accounts","accounts":[]}
            """)
        let client = DaemonClient(socketPath: "/tmp/usage.sock", transport: transport)

        do {
            _ = try await client.accounts()
            XCTFail("expected an incompatible protocol error")
        } catch let error as DaemonError {
            guard case let .api(code, _) = error else {
                XCTFail("unexpected daemon error: \(error)")
                return
            }
            XCTAssertEqual(code, "incompatible_protocol")
        }
    }

    func testDefaultsOmittedNotificationRulesToEmpty() throws {
        let data = Data("""
            {
              "enabled": true,
              "thresholds_percent_remaining": [50, 25, 10, 5, 0],
              "reset_alerts": true,
              "predictive_alerts": false,
              "cooldown_minutes": 15
            }
            """.utf8)

        let notifications = try JSONDecoder.usage.decode(NotificationConfig.self, from: data)
        XCTAssertEqual(notifications.rules, [])
    }

    func testPollsRefreshJobWithoutHoldingTheRefreshSocketOpen() async throws {
        let transport = RecordingTransport(responses: [
            """
            {"api_version":2,"type":"refresh_started","coalesced":true,"job":{"id":"job-1","scope":{"providers":["codex"]},"trigger":"manual","status":"running","created_at":"2026-07-11T12:00:00Z","started_at":"2026-07-11T12:00:00Z","finished_at":null}}
            """,
            """
            {"api_version":2,"type":"refresh_job","job":{"id":"job-1","scope":{"providers":["codex"]},"trigger":"manual","status":"completed","created_at":"2026-07-11T12:00:00Z","started_at":"2026-07-11T12:00:00Z","finished_at":"2026-07-11T12:00:01Z","provider_results":[{"provider_id":"codex","account_id":"account-1","status":"ok","collection_mode":"provider_api","collected_at":"2026-07-11T12:00:01Z","message":null}],"failure_message":null}}
            """,
        ])
        let client = DaemonClient(
            socketPath: "/tmp/usage.sock",
            transport: transport,
            refreshPollInterval: .zero,
            refreshWaitTimeout: .seconds(1)
        )

        let response = try await client.refresh(["codex"])
        XCTAssertEqual(response.providerResults.count, 1)
        XCTAssertEqual(response.providerResults[0].providerId, "codex")

        let recorded = await transport.allRequests()
        XCTAssertEqual(recorded.count, 2)
        let start = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(recorded[0].utf8)) as? [String: Any]
        )
        let poll = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(recorded[1].utf8)) as? [String: Any]
        )
        XCTAssertEqual(start["method"] as? String, "refresh")
        XCTAssertEqual(poll["method"] as? String, "get_refresh_job")
        XCTAssertEqual(poll["job_id"] as? String, "job-1")
    }
}

final class DaemonSupervisorTests: XCTestCase {
    func testConcurrentEnsureRunningCallsShareOneLaunch() async throws {
        let transport = LockedTransport()
        let launcher = FakeProcessLauncher { transport.setConnected(true) }
        let supervisor = makeSupervisor(transport: transport, launcher: launcher)

        async let first = supervisor.ensureRunning(socketPath: "/tmp/usage-test.sock")
        async let second = supervisor.ensureRunning(socketPath: "/tmp/usage-test.sock")
        let results = await (first, second)

        XCTAssertTrue(results.0)
        XCTAssertTrue(results.1)
        XCTAssertEqual(launcher.launchCount, 1)
        let status = await supervisor.currentStatus()
        XCTAssertEqual(status, .running)
    }

    func testTerminationTransitionsThroughBackoffAndRelaunches() async throws {
        let transport = LockedTransport()
        let launcher = FakeProcessLauncher { transport.setConnected(true) }
        let supervisor = makeSupervisor(transport: transport, launcher: launcher)
        let initiallyRunning = await supervisor.ensureRunning(socketPath: "/tmp/usage-test.sock")
        XCTAssertTrue(initiallyRunning)

        let firstProcess = try XCTUnwrap(launcher.latestProcess)
        transport.setConnected(false)
        firstProcess.crash()

        for _ in 0..<100 where launcher.launchCount < 2 {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(launcher.launchCount, 2)
        let status = await supervisor.currentStatus()
        XCTAssertEqual(status, .running)
    }

    func testAutomaticCrashRecoveryIsBoundedUntilTheDaemonIsStable() async throws {
        let transport = LockedTransport()
        let launcher = FakeProcessLauncher { transport.setConnected(true) }
        let supervisor = makeSupervisor(
            transport: transport,
            launcher: launcher,
            maximumAutomaticRestarts: 1,
            stabilityResetInterval: 10
        )
        let initiallyRunning = await supervisor.ensureRunning(socketPath: "/tmp/usage-test.sock")
        XCTAssertTrue(initiallyRunning)

        transport.setConnected(false)
        try XCTUnwrap(launcher.latestProcess).crash()
        for _ in 0..<100 where launcher.launchCount < 2 {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(launcher.launchCount, 2)

        transport.setConnected(false)
        try XCTUnwrap(launcher.latestProcess).crash()
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(launcher.launchCount, 2)
        let status = await supervisor.currentStatus()
        XCTAssertEqual(status, .backingOff)
    }

    private func makeSupervisor(
        transport: LockedTransport,
        launcher: FakeProcessLauncher,
        maximumAutomaticRestarts: Int = 2,
        stabilityResetInterval: TimeInterval = 0
    ) -> DaemonSupervisor {
        var policy = DaemonSupervisorPolicy()
        policy.launchAttempts = 1
        policy.readinessChecks = 5
        policy.readinessProbeTimeout = 0.01
        policy.readinessPollInterval = 0.01
        policy.initialBackoff = 0.01
        policy.maximumBackoff = 0.02
        policy.maximumAutomaticRestarts = maximumAutomaticRestarts
        policy.stabilityResetInterval = stabilityResetInterval

        var logPolicy = DaemonLogPolicy()
        logPolicy.checkInterval = 0
        return DaemonSupervisor(
            transport: transport,
            executableLocator: FakeExecutableLocator(),
            processLauncher: launcher,
            environment: [:],
            rootURL: FileManager.default.temporaryDirectory,
            policy: policy,
            logPolicy: logPolicy
        )
    }
}

final class DaemonLogRotatorTests: XCTestCase {
    func testRotationBoundsArchiveCountAndSize() throws {
        let root = FileManager.default.temporaryDirectory
            .appending(path: "usage-log-tests-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: root) }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let log = root.appending(path: "usage-daemon.log")
        try Data("0123456789".utf8).write(to: log)

        let rotator = DaemonLogRotator(policy: DaemonLogPolicy(
            maxBytes: 8,
            retainedArchives: 2,
            checkInterval: 0
        ))
        try rotator.prepareForLaunch(at: log)
        XCTAssertEqual(try fileSize(log), 0)
        XCTAssertEqual(try fileSize(URL(fileURLWithPath: "\(log.path).1")), 8)

        try Data("abcdefghij".utf8).write(to: log)
        try rotator.rotateActiveLogIfNeeded(at: log)
        XCTAssertEqual(try fileSize(log), 0)
        XCTAssertEqual(try fileSize(URL(fileURLWithPath: "\(log.path).1")), 8)
        XCTAssertEqual(try fileSize(URL(fileURLWithPath: "\(log.path).2")), 8)
        XCTAssertFalse(FileManager.default.fileExists(atPath: "\(log.path).3"))
    }

    private func fileSize(_ url: URL) throws -> UInt64 {
        let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
        return try XCTUnwrap(attributes[.size] as? NSNumber).uint64Value
    }
}

final class ProviderCatalogTests: XCTestCase {
    func testUsesAnExplicitProviderAllowlist() {
        XCTAssertEqual(ProviderCatalog.supportedIDs, ["codex", "claude", "opencode_go", "grok"])
        XCTAssertTrue(ProviderCatalog.supports("grok"))
        XCTAssertTrue(ProviderCatalog.supports("opencode_go"))
        XCTAssertFalse(ProviderCatalog.supports("opencode"))
        XCTAssertFalse(ProviderCatalog.supports("future_provider"))
    }
}

final class MenuBarPresentationTests: XCTestCase {
    func testProviderCountControlsBothTooltipAndIconRows() {
        var ui = UIConfig()
        ui.maxMenuProviders = 1
        let providers = [
            provider("codex", short: "C", percent: 80),
            provider("claude", short: "A", percent: 60),
        ]

        let presentation = AppState.menuContent(
            providers: providers,
            daemon: .online,
            ui: ui,
            eligibleProviderIDs: Set(providers.map(\.providerId))
        )

        XCTAssertEqual(presentation.preview, "C 80%")
        XCTAssertEqual(presentation.bars.map(\.providerId), ["codex"])
    }

    func testUsedMetricAndTooltipNamePreferenceApplyTogether() {
        var ui = UIConfig()
        ui.maxMenuProviders = 2
        ui.menuMetric = .used
        ui.showProviderLabels = false
        let providers = [
            provider("codex", short: "C", percent: 80),
            provider("claude", short: "A", percent: 60),
        ]

        let presentation = AppState.menuContent(
            providers: providers,
            daemon: .online,
            ui: ui,
            eligibleProviderIDs: Set(providers.map(\.providerId))
        )

        XCTAssertEqual(presentation.preview, "20%  40%")
        XCTAssertEqual(presentation.bars.map(\.percent), [20, 40])
    }

    func testStatusMenuProvidersAreEligibleAndCappedAtFive() {
        var providers = (1...8).map { provider("provider-\($0)", short: "P\($0)", percent: 50) }
        providers[2] = provider("provider-3", short: "P3", percent: 50, enabled: false)
        let eligible = Set(providers.map(\.providerId)).subtracting(["provider-2"])

        let selected = StatusMenuProviderSelection.select(
            from: providers,
            eligibleProviderIDs: eligible
        )

        XCTAssertEqual(selected.map(\.providerId), [
            "provider-1", "provider-4", "provider-5", "provider-6", "provider-7",
        ])
    }

    func testSavedProviderCountIsClampedToIconCapacity() throws {
        let data = Data(#"{"maxMenuProviders":99}"#.utf8)
        let ui = try JSONDecoder().decode(UIConfig.self, from: data)

        XCTAssertEqual(ui.maxMenuProviders, .some(MenuBarProgressIcon.maxRows))
    }

    func testDefaultProviderCountTracksConnectedProviders() {
        let ui = UIConfig()
        let providers = [
            provider("claude", short: "A", percent: 60),
            provider("codex", short: "C", percent: 80),
        ]

        let oneProvider = AppState.menuContent(
            providers: providers,
            daemon: .online,
            ui: ui,
            eligibleProviderIDs: ["codex"]
        )
        let twoProviders = AppState.menuContent(
            providers: providers,
            daemon: .online,
            ui: ui,
            eligibleProviderIDs: ["codex", "claude"]
        )

        XCTAssertEqual(oneProvider.bars.count, 1)
        XCTAssertEqual(oneProvider.bars.first?.providerId, "codex")
        XCTAssertEqual(twoProviders.bars.count, 2)
    }

    private func provider(
        _ id: String,
        short: String,
        percent: Double,
        enabled: Bool = true
    ) -> ProviderVM {
        ProviderVM(
            id: id,
            providerId: id,
            accountId: nil,
            name: id,
            short: short,
            symbol: "circle",
            primary: "\(Int(percent))%",
            detail: "updated now",
            percent: percent,
            status: .normal,
            spend: [],
            windows: [],
            credits: [],
            resetCredits: [],
            account: nil,
            healthText: "all good",
            visibleInMenu: true,
            enabled: enabled,
            secondary: "",
            sparkline: [],
            costDashboard: .empty,
            subAccounts: nil
        )
    }
}

private actor RecordingTransport: DaemonTransport {
    private var responses: [String]
    private var requests = [String]()
    private var timeouts = [TimeInterval]()

    init(response: String) {
        responses = [response]
    }

    init(responses: [String]) {
        self.responses = responses
    }

    func line(path: String, request: String, timeout: TimeInterval) async throws -> String {
        requests.append(request)
        timeouts.append(timeout)
        guard !responses.isEmpty else { throw DaemonError.closed }
        return responses.removeFirst()
    }

    func canConnect(path: String, timeout: TimeInterval) async -> Bool { false }
    func lastRequest() -> String? { requests.last }
    func lastTimeout() -> TimeInterval? { timeouts.last }
    func allRequests() -> [String] { requests }
}

private final class LockedTransport: DaemonTransport, @unchecked Sendable {
    private let lock = NSLock()
    private var connected = false

    func line(path: String, request: String, timeout: TimeInterval) async throws -> String {
        throw DaemonError.closed
    }

    func canConnect(path: String, timeout: TimeInterval) async -> Bool {
        lock.withLock { connected }
    }

    func setConnected(_ connected: Bool) {
        lock.withLock { self.connected = connected }
    }
}

private struct FakeExecutableLocator: DaemonExecutableLocating {
    func executableURL() -> URL? { URL(fileURLWithPath: "/tmp/fake-usage-daemon") }
    func bundledExecutableURL() -> URL? { nil }
}

private final class FakeProcess: DaemonProcessHandle, @unchecked Sendable {
    let processIdentifier: pid_t
    private let lock = NSLock()
    private var running = true
    private let terminationHandler: @Sendable (pid_t) -> Void

    init(processIdentifier: pid_t, terminationHandler: @escaping @Sendable (pid_t) -> Void) {
        self.processIdentifier = processIdentifier
        self.terminationHandler = terminationHandler
    }

    var isRunning: Bool {
        lock.lock()
        defer { lock.unlock() }
        return running
    }

    func terminate() { crash() }

    func crash() {
        lock.lock()
        guard running else {
            lock.unlock()
            return
        }
        running = false
        lock.unlock()
        terminationHandler(processIdentifier)
    }
}

private final class FakeProcessLauncher: DaemonProcessLaunching, @unchecked Sendable {
    private let lock = NSLock()
    private var processes = [FakeProcess]()
    private let onLaunch: @Sendable () -> Void

    init(onLaunch: @escaping @Sendable () -> Void) {
        self.onLaunch = onLaunch
    }

    var launchCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return processes.count
    }

    var latestProcess: FakeProcess? {
        lock.lock()
        defer { lock.unlock() }
        return processes.last
    }

    func launch(
        executable: URL,
        arguments: [String],
        logURL: URL,
        terminationHandler: @escaping @Sendable (pid_t) -> Void
    ) throws -> any DaemonProcessHandle {
        lock.lock()
        let process = FakeProcess(
            processIdentifier: pid_t(1_000 + processes.count),
            terminationHandler: terminationHandler
        )
        processes.append(process)
        lock.unlock()
        onLaunch()
        return process
    }
}
