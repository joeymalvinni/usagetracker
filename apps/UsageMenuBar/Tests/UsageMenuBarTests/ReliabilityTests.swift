import CryptoKit
import Foundation
import XCTest
@testable import UsageMenuBar

final class AppUpdaterTests: XCTestCase {
    private actor DownloadStub {
        struct Request: Sendable {
            let path: String
            let timeout: TimeInterval
            let userAgent: String?
        }

        var requests = [Request]()
        var metadataFailure = false
        var installerExitStatus = 0

        func download(_ request: URLRequest, maximumBytes _: Int) throws -> Data {
            let path = try XCTUnwrap(request.url?.lastPathComponent)
            requests.append(Request(
                path: path,
                timeout: request.timeoutInterval,
                userAgent: request.value(forHTTPHeaderField: "User-Agent")
            ))
            if path == "latest" {
                if metadataFailure { throw URLError(.notConnectedToInternet) }
                return Data(#"{"tag_name":"v0.2.0","draft":false,"prerelease":false,"body":"UsageTracker 0.2.0 is faster and clearer.\n\n## Highlights\n\n- Refreshes finish faster.\n- Errors are easier to understand.\n\n## Installation\n\nNot shown in the app."}"#.utf8)
            }
            if path == "install.sh" {
                return Data("#!/bin/bash\nexit \(installerExitStatus)\n".utf8)
            }
            if path == "SHA256SUMS" {
                let installer = Data("#!/bin/bash\nexit \(installerExitStatus)\n".utf8)
                let digest = SHA256.hash(data: installer)
                    .map { String(format: "%02x", $0) }
                    .joined()
                return Data("\(digest)  install.sh\n".utf8)
            }
            throw URLError(.badURL)
        }

        func setMetadataFailure(_ value: Bool) { metadataFailure = value }
        func setInstallerExitStatus(_ value: Int) { installerExitStatus = value }
    }

    func testSemanticVersionsCompareNumerically() throws {
        let current = try XCTUnwrap(SemanticVersion("0.9.12"))
        let latest = try XCTUnwrap(SemanticVersion("v0.10.2"))

        XCTAssertLessThan(current, latest)
        XCTAssertEqual(latest.description, "0.10.2")
        XCTAssertNil(SemanticVersion("0.10"))
        XCTAssertNil(SemanticVersion("v0.10.2-beta"))
    }

    func testOnlyOffersStrictlyNewerStableRelease() throws {
        let release = try XCTUnwrap(AppUpdatePolicy.newerRelease(
            currentVersion: "0.1.1",
            latestTag: "v0.2.0"
        ))

        XCTAssertEqual(release.tag, "v0.2.0")
        XCTAssertEqual(release.version, SemanticVersion("0.2.0"))
        XCTAssertNil(AppUpdatePolicy.newerRelease(currentVersion: "0.2.0", latestTag: "v0.2.0"))
        XCTAssertNil(AppUpdatePolicy.newerRelease(currentVersion: "0.2.0", latestTag: "v0.1.9"))
        XCTAssertNil(AppUpdatePolicy.newerRelease(currentVersion: "0.1.0", latestTag: "0.2.0"))
        XCTAssertNil(AppUpdatePolicy.newerRelease(currentVersion: "0.2.0", latestTag: "nightly"))
    }

    func testReleaseNotesUseOnlyTheSummaryAndFirstSixHighlights() throws {
        let body = """
            UsageTracker 0.2.0 improves everyday refreshes.

            ## Highlights

            - First
            - Second
            - Third
            - Fourth
            - Fifth
            - Sixth
            - Seventh

            ## Installation

            - This is not an in-app highlight.
            """

        let notes = try XCTUnwrap(ReleaseNotesParser.parse(
            body: body,
            version: try XCTUnwrap(SemanticVersion("0.2.0"))
        ))

        XCTAssertEqual(notes.summary, "UsageTracker 0.2.0 improves everyday refreshes.")
        XCTAssertEqual(notes.highlights, ["First", "Second", "Third", "Fourth", "Fifth", "Sixth"])
        XCTAssertNil(ReleaseNotesParser.parse(
            body: "A summary without the required section.",
            version: try XCTUnwrap(SemanticVersion("0.2.0"))
        ))
    }

    func testInstallerMustMatchPublishedChecksum() throws {
        let installer = Data("trusted installer".utf8)
        let digest = SHA256.hash(data: installer)
            .map { String(format: "%02x", $0) }
            .joined()
        let checksums = Data("\(digest)  install.sh\n".utf8)

        XCTAssertNoThrow(try UpdateIntegrity.verifyInstaller(installer, checksums: checksums))
        XCTAssertThrowsError(try UpdateIntegrity.verifyInstaller(Data("changed".utf8), checksums: checksums))
        XCTAssertThrowsError(try UpdateIntegrity.verifyInstaller(installer, checksums: Data()))
    }

    @MainActor func testSuccessfulChecksPreserveRequestConfigurationAndUseCooldown() async throws {
        let stub = DownloadStub()
        let bundleURL = try temporaryBundleURL()
        defer { try? FileManager.default.removeItem(at: bundleURL.deletingLastPathComponent()) }
        let updater = AppUpdater(
            bundleURL: bundleURL,
            currentVersion: "0.1.0",
            downloader: { try await stub.download($0, maximumBytes: $1) }
        )

        await updater.checkForUpdates()
        await updater.checkForUpdates()

        let requests = await stub.requests
        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests[0].path, "latest")
        XCTAssertEqual(requests[0].timeout, 10)
        XCTAssertEqual(requests[0].userAgent, "UsageTracker/0.1.0")
        XCTAssertEqual(updater.availableRelease?.tag, "v0.2.0")
        XCTAssertEqual(updater.availableRelease?.releaseNotes?.highlights.count, 2)
    }

    @MainActor func testCurrentReleaseNotesAppearAndCanBeDismissed() async throws {
        let stub = DownloadStub()
        let bundleURL = try temporaryBundleURL()
        let directory = bundleURL.deletingLastPathComponent()
        defer { try? FileManager.default.removeItem(at: directory) }
        let notesURL = directory.appending(path: "pending-release-notes.json")
        let notes = ReleaseNotes(
            version: "0.2.0",
            summary: "UsageTracker 0.2.0 is faster and clearer.",
            highlights: ["Refreshes finish faster."]
        )
        try ReleaseNotesStore(fileURL: notesURL).save(notes)
        let updater = AppUpdater(
            bundleURL: bundleURL,
            currentVersion: "0.2.0",
            downloader: { try await stub.download($0, maximumBytes: $1) },
            releaseNotesURL: notesURL
        )

        XCTAssertEqual(updater.installedReleaseNotes, notes)
        updater.dismissInstalledReleaseNotes(version: "0.2.0")
        XCTAssertNil(updater.installedReleaseNotes)
        XCTAssertFalse(FileManager.default.fileExists(atPath: notesURL.path))

        await updater.checkForUpdates()
        XCTAssertEqual(updater.installedReleaseNotes?.highlights.count, 2)
        XCTAssertNil(updater.availableRelease)
    }

    @MainActor func testFailedChecksCanRetryImmediately() async throws {
        let stub = DownloadStub()
        await stub.setMetadataFailure(true)
        let bundleURL = try temporaryBundleURL()
        defer { try? FileManager.default.removeItem(at: bundleURL.deletingLastPathComponent()) }
        let updater = AppUpdater(
            bundleURL: bundleURL,
            currentVersion: "0.1.0",
            downloader: { try await stub.download($0, maximumBytes: $1) }
        )

        await updater.checkForUpdates()
        await updater.checkForUpdates()

        let requests = await stub.requests
        XCTAssertEqual(requests.count, 2)
    }

    @MainActor func testInstallerSuccessAndFailureResetState() async throws {
        let stub = DownloadStub()
        await stub.setInstallerExitStatus(1)
        let temporaryDirectories = updaterTemporaryDirectories()
        let bundleURL = try temporaryBundleURL()
        let directory = bundleURL.deletingLastPathComponent()
        let notesURL = directory.appending(path: "pending-release-notes.json")
        defer { try? FileManager.default.removeItem(at: directory) }
        let updater = AppUpdater(
            bundleURL: bundleURL,
            currentVersion: "0.1.0",
            downloader: { try await stub.download($0, maximumBytes: $1) },
            releaseNotesURL: notesURL
        )
        await updater.checkForUpdates()

        await updater.installAvailableUpdate()
        try await waitForInstaller(updater)
        XCTAssertFalse(updater.isInstalling)
        XCTAssertNotNil(updater.availableRelease)
        XCTAssertNotNil(updater.installError)
        XCTAssertFalse(FileManager.default.fileExists(atPath: notesURL.path))

        await stub.setInstallerExitStatus(0)
        await updater.installAvailableUpdate()
        try await waitForInstaller(updater)
        XCTAssertFalse(updater.isInstalling)
        XCTAssertNil(updater.availableRelease)
        XCTAssertNil(updater.installError)
        XCTAssertEqual(
            ReleaseNotesStore(fileURL: notesURL).load(currentVersion: "0.2.0")?.highlights.count,
            2
        )
        XCTAssertEqual(updaterTemporaryDirectories(), temporaryDirectories)
    }

    @MainActor func testUnwritableInstallLocationIsRejected() async throws {
        let stub = DownloadStub()
        let bundleURL = try temporaryBundleURL()
        defer { try? FileManager.default.removeItem(at: bundleURL.deletingLastPathComponent()) }
        let updater = AppUpdater(
            bundleURL: bundleURL,
            currentVersion: "0.1.0",
            downloader: { try await stub.download($0, maximumBytes: $1) },
            isWritable: { _ in false }
        )
        await updater.checkForUpdates()

        await updater.installAvailableUpdate()

        XCTAssertFalse(updater.isInstalling)
        XCTAssertEqual(updater.installError, AppUpdateError.unsupportedInstallLocation.localizedDescription)
        let requests = await stub.requests
        XCTAssertEqual(requests.count, 1)
    }

    @MainActor func testRelaunchedAppConsumesPersistedInstallFailure() throws {
        let stub = DownloadStub()
        let bundleURL = try temporaryBundleURL()
        let directory = bundleURL.deletingLastPathComponent()
        defer { try? FileManager.default.removeItem(at: directory) }
        let marker = directory.appending(path: ".UsageTracker.update-failed")
        try Data("1\n".utf8).write(to: marker)

        let updater = AppUpdater(
            bundleURL: bundleURL,
            currentVersion: "0.1.0",
            downloader: { try await stub.download($0, maximumBytes: $1) }
        )

        XCTAssertNotNil(updater.installError)
        XCTAssertFalse(FileManager.default.fileExists(atPath: marker.path))
    }

    private func temporaryBundleURL() throws -> URL {
        let directory = FileManager.default.temporaryDirectory
            .appending(path: "usagetracker-updater-tests-\(UUID().uuidString)", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return directory.appending(path: "UsageTracker.app", directoryHint: .isDirectory)
    }

    @MainActor private func waitForInstaller(_ updater: AppUpdater) async throws {
        for _ in 0..<100 where updater.isInstalling {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertFalse(updater.isInstalling, "installer did not terminate")
    }

    private func updaterTemporaryDirectories() -> Set<String> {
        let urls = (try? FileManager.default.contentsOfDirectory(
            at: FileManager.default.temporaryDirectory,
            includingPropertiesForKeys: nil
        )) ?? []
        return Set(urls.map(\.lastPathComponent).filter { $0.hasPrefix("usagetracker-update-") })
    }
}

final class DaemonClientTests: XCTestCase {
    func testDecodesKeychainAccessFailureSeparatelyFromProviderAuthentication() throws {
        let healthStatus = try JSONDecoder().decode(
            ProviderHealthStatus.self,
            from: Data(#""keychain_access_failed""#.utf8)
        )
        let refreshStatus = try JSONDecoder().decode(
            ProviderRefreshStatus.self,
            from: Data(#""keychain_access_failed""#.utf8)
        )

        XCTAssertEqual(healthStatus, .keychainAccessFailed)
        XCTAssertEqual(healthStatus.friendly, "keychain access failed")
        XCTAssertEqual(refreshStatus, .keychainAccessFailed)
    }

    func testProviderCapabilitiesRemainIndependent() {
        let providers = ["fixture": ServerProviderDescriptor(
            id: "fixture",
            displayName: "Fixture",
            minimumRefreshIntervalSeconds: 60,
            capabilities: ProviderCapabilities(
                multipleAccounts: false,
                addAccount: true,
                repair: false,
                launchAccount: true,
                workspaceSetup: false
            )
        )]

        XCTAssertFalse(providerSupports("fixture", capability: \.multipleAccounts, in: providers))
        XCTAssertTrue(providerSupports("fixture", capability: \.addAccount, in: providers))
        XCTAssertFalse(providerSupports("fixture", capability: \.repair, in: providers))
        XCTAssertTrue(providerSupports("fixture", capability: \.launchAccount, in: providers))
        XCTAssertFalse(providerSupports("fixture", capability: \.workspaceSetup, in: providers))
        XCTAssertFalse(providerSupports("fixture", capability: \.setup, in: providers))
    }

    func testGenericProviderSetupCanExplicitlyClearAValue() throws {
        let request = DaemonRequest.updateProviderSetup(
            providerId: "future_provider",
            settings: ["region": nil]
        )
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: JSONEncoder.usage.encode(request))
                as? [String: Any]
        )
        let settings = try XCTUnwrap(object["settings"] as? [String: Any])

        XCTAssertTrue(settings["region"] is NSNull)
        XCTAssertNil(object["workspace_id"])
    }

    func testCopySignInLinkActionIsExplicitOnTheWire() throws {
        let request = DaemonRequest.repairProvider(
            providerId: "codex",
            accountId: nil,
            signInAction: .copyLink
        )
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: JSONEncoder.usage.encode(request))
                as? [String: Any]
        )

        XCTAssertEqual(object["method"] as? String, "repair_provider")
        XCTAssertEqual(object["sign_in_action"] as? String, "copy_link")
    }

    func testDecodesProviderAuthenticationURL() throws {
        let response = try JSONDecoder.usage.decode(
            DaemonResponse.self,
            from: Data(#"{"api_version":3,"type":"provider_action","action":{"provider_id":"codex","message":"Sign in","authentication_url":"https://auth.openai.com/oauth/authorize?state=test"}}"#.utf8)
        )
        guard case let .providerAction(action) = response else {
            return XCTFail("expected provider action")
        }

        XCTAssertEqual(
            action.authenticationUrl,
            "https://auth.openai.com/oauth/authorize?state=test"
        )
    }

    func testResponseLineBufferRejectsDataBeyondItsLimit() throws {
        var buffer = ResponseLineBuffer(maxBytes: 4)
        try buffer.append([1, 2, 3, 4])
        XCTAssertEqual(buffer.bytes, [1, 2, 3, 4])

        XCTAssertThrowsError(try buffer.append([5])) { error in
            XCTAssertEqual(error as? DaemonError, .responseTooLarge(4))
        }
        XCTAssertEqual(buffer.bytes, [1, 2, 3, 4])
    }

    func testStateUsesOneCombinedRequest() async throws {
        let response = try String(contentsOf: rustWireFixture("state_v3.json"), encoding: .utf8)
        let transport = RecordingTransport(response: response)
        let client = DaemonClient(socketPath: "/tmp/usage.sock", transport: transport)

        let state = try await client.state()

        XCTAssertEqual(state.config.pollIntervalSeconds, 300)
        let recordedRequest = await transport.lastRequest()
        let request = try XCTUnwrap(recordedRequest)
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(request.utf8)) as? [String: Any]
        )
        XCTAssertEqual(object["method"] as? String, "get_state")
        let requests = await transport.allRequests()
        XCTAssertEqual(requests.count, 1)
    }

    func testDecodesCheckedInRustWireFixtures() throws {
        let usageURL = rustWireFixture("usage_v3.json")
        let usage = try JSONDecoder.usage.decode(DaemonResponse.self, from: Data(contentsOf: usageURL))
        guard case let .usage(response) = usage else {
            return XCTFail("expected usage fixture")
        }
        XCTAssertEqual(response.snapshots.first?.providerId, "codex")
        XCTAssertEqual(response.dashboard.pricing.coveredPercent, 100)
        XCTAssertEqual(response.dashboard.pricing.unpricedModels, [])
        XCTAssertEqual(response.windowProvenance.first?.authoritative, true)

        let errorURL = rustWireFixture("error_v3.json")
        let error = try JSONDecoder.usage.decode(DaemonResponse.self, from: Data(contentsOf: errorURL))
        guard case let .error(apiError) = error else {
            return XCTFail("expected error fixture")
        }
        XCTAssertEqual(apiError.code, "unsupported_method")

        let serverInfoURL = rustWireFixture("server_info_v3.json")
        let serverInfo = try JSONDecoder.usage.decode(
            DaemonResponse.self,
            from: Data(contentsOf: serverInfoURL)
        )
        guard case let .serverInfo(info) = serverInfo else {
            return XCTFail("expected server info fixture")
        }
        let providers = Dictionary(uniqueKeysWithValues: info.providers.map { ($0.id, $0) })
        let codex = try XCTUnwrap(providers["codex"]?.capabilities)
        XCTAssertTrue(codex.multipleAccounts)
        XCTAssertTrue(codex.addAccount)
        XCTAssertTrue(codex.repair)
        XCTAssertFalse(codex.launchAccount)
        XCTAssertFalse(codex.workspaceSetup)

        XCTAssertTrue(try XCTUnwrap(providers["claude"]?.capabilities).launchAccount)
        XCTAssertFalse(try XCTUnwrap(providers["grok"]?.capabilities).launchAccount)

        let openCode = try XCTUnwrap(providers["opencode_go"]?.capabilities)
        XCTAssertFalse(openCode.multipleAccounts)
        XCTAssertFalse(openCode.addAccount)
        XCTAssertTrue(openCode.repair)
        XCTAssertFalse(openCode.launchAccount)
        XCTAssertTrue(openCode.workspaceSetup)
        XCTAssertTrue(openCode.setup)

        let stateURL = rustWireFixture("state_v3.json")
        let state = try JSONDecoder.usage.decode(
            DaemonResponse.self,
            from: Data(contentsOf: stateURL)
        )
        guard case let .state(snapshot) = state else {
            return XCTFail("expected state fixture")
        }
        XCTAssertEqual(snapshot.config.pollIntervalSeconds, 300)
        XCTAssertEqual(snapshot.config.providers["codex"]?.enabled, true)
        XCTAssertEqual(snapshot.dashboard.pricing.coveredPercent, 100)
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
            {"api_version":3,"type":"error","error":{"code":"unknown_account","message":"Account missing","retryable":false}}
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
        XCTAssertEqual(object["api_version"] as? Int, 3)
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
            {"api_version":3,"type":"refresh_started","coalesced":true,"job":{"id":"job-1","scope":{"providers":["codex"]},"trigger":"manual","status":"running","created_at":"2026-07-11T12:00:00Z","started_at":"2026-07-11T12:00:00Z","finished_at":null}}
            """,
            """
            {"api_version":3,"type":"refresh_job","job":{"id":"job-1","scope":{"providers":["codex"]},"trigger":"manual","status":"completed","created_at":"2026-07-11T12:00:00Z","started_at":"2026-07-11T12:00:00Z","finished_at":"2026-07-11T12:00:01Z","provider_results":[{"provider_id":"codex","account_id":"account-1","status":"ok","collection_mode":"provider_api","collected_at":"2026-07-11T12:00:01Z","message":null}],"failure_message":null}}
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

final class DaemonLaunchAgentPlistTests: XCTestCase {
    func testLaunchAgentIsPerUserPersistentAndRunsTheManagedDaemon() throws {
        let plistURL = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // UsageMenuBarTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // UsageMenuBar
            .appending(path: "LaunchAgents")
            .appending(path: daemonLaunchAgentPlistName)
        let object = try PropertyListSerialization.propertyList(
            from: Data(contentsOf: plistURL),
            format: nil
        )
        let plist = try XCTUnwrap(object as? [String: Any])
        XCTAssertEqual(plist["Label"] as? String, daemonLaunchAgentLabel)
        XCTAssertEqual(plist["BundleProgram"] as? String, "Contents/MacOS/usage-daemon")
        XCTAssertEqual(plist["KeepAlive"] as? Bool, true)
        XCTAssertEqual(plist["RunAtLoad"] as? Bool, true)
        XCTAssertEqual(plist["LimitLoadToSessionType"] as? String, "Aqua")
        XCTAssertEqual(
            plist["ProgramArguments"] as? [String],
            ["usage-daemon", "--foreground", "--managed"]
        )
    }
}

final class DaemonLaunchAgentControllerTests: XCTestCase {
    func testEnvironmentOverridesUseTheInheritedChildProcessLauncher() {
        XCTAssertTrue(SystemDaemonLaunchAgentController.supportsLaunchAgentEnvironment([:]))
        for name in [
            "USAGE_TRACKER_LOG_LEVEL",
            "USAGE_TRACKER_OPENCODE_GO_COOKIE",
            "USAGE_TRACKER_OPENCODE_COOKIE",
            "USAGE_TRACKER_OPENCODE_GO_WORKSPACE_ID",
            "USAGE_TRACKER_GROK_COOKIE",
            "USAGE_TRACKER_ALLOW_BROWSER_COOKIE_IMPORT",
            "RUST_LOG",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "GROK_HOME",
            "GROK_CLI_PATH",
            "XAI_API_KEY",
        ] {
            XCTAssertFalse(
                SystemDaemonLaunchAgentController.supportsLaunchAgentEnvironment([
                    name: "override"
                ]),
                name
            )
        }
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

    func testRestartForceTerminatesAStuckDaemonBeforeRelaunching() async throws {
        let transport = LockedTransport()
        let launcher = FakeProcessLauncher(ignoresTermination: { $0 == 0 }) {
            transport.setConnected(true)
        }
        let supervisor = makeSupervisor(transport: transport, launcher: launcher)
        let initiallyRunning = await supervisor.ensureRunning(socketPath: "/tmp/usage-test.sock")
        XCTAssertTrue(initiallyRunning)

        let stuckProcess = try XCTUnwrap(launcher.latestProcess)
        transport.setConnected(false)
        let restarted = await supervisor.restart(socketPath: "/tmp/usage-test.sock")
        XCTAssertTrue(restarted)

        XCTAssertEqual(stuckProcess.forceTerminationCount, 1)
        XCTAssertFalse(stuckProcess.isRunning)
        XCTAssertEqual(launcher.launchCount, 2)
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

    func testLaunchAgentOwnsProductionLifecycleAndRestartsWithoutChildProcesses() async throws {
        let transport = LockedTransport()
        let launcher = FakeProcessLauncher { XCTFail("launchd should own the daemon") }
        let launchAgent = FakeLaunchAgent {
            transport.setConnected(true)
        } onUnregister: {
            transport.setConnected(false)
        }
        let supervisor = makeSupervisor(
            transport: transport,
            launcher: launcher,
            launchAgent: launchAgent
        )

        let initiallyRunning = await supervisor.ensureRunning(socketPath: "/tmp/usage-test.sock")
        XCTAssertTrue(initiallyRunning)
        XCTAssertEqual(launchAgent.registerCount, 1)
        XCTAssertEqual(launcher.launchCount, 0)

        let restarted = await supervisor.restart(socketPath: "/tmp/usage-test.sock")
        XCTAssertTrue(restarted)
        XCTAssertEqual(launchAgent.unregisterCount, 1)
        XCTAssertEqual(launchAgent.registerCount, 2)
        XCTAssertEqual(launcher.launchCount, 0)
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
        launchAgent: (any DaemonLaunchAgentControlling)? = nil,
        maximumAutomaticRestarts: Int = 2,
        stabilityResetInterval: TimeInterval = 0
    ) -> DaemonSupervisor {
        var policy = DaemonSupervisorPolicy()
        policy.launchAttempts = 1
        policy.readinessChecks = 5
        policy.readinessProbeTimeout = 0.01
        policy.readinessPollInterval = 0.01
        policy.shutdownChecks = 3
        policy.shutdownProbeTimeout = 0.01
        policy.shutdownPollInterval = 0.01
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
            launchAgent: launchAgent,
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

final class AppStateTests: XCTestCase {
    @MainActor func testOnboardingDefaultsEnableOnlyCodex() {
        let toggles = AppState.onboardingDefaultProviderToggles(
            providerIDs: ["codex", "claude", "opencode_go", "grok"]
        )

        XCTAssertEqual(toggles["codex"], true)
        XCTAssertEqual(toggles["claude"], false)
        XCTAssertEqual(toggles["opencode_go"], false)
        XCTAssertEqual(toggles["grok"], false)
    }
}

final class ProviderCatalogTests: XCTestCase {
    func testBuiltInCatalogProvidesOptionalPresentationDecorations() {
        XCTAssertEqual(ProviderCatalog.supportedIDs, ["codex", "claude", "opencode_go", "grok"])
        XCTAssertTrue(ProviderCatalog.supports("grok"))
        XCTAssertTrue(ProviderCatalog.supports("opencode_go"))
        XCTAssertFalse(ProviderCatalog.supports("opencode"))
        XCTAssertFalse(ProviderCatalog.supports("future_provider"))
    }
}

final class MenuBarPresentationTests: XCTestCase {
    func testDarkModeIsEnabledByDefault() throws {
        XCTAssertTrue(UIConfig().darkModeEnabled)

        let decoded = try JSONDecoder().decode(UIConfig.self, from: Data("{}".utf8))
        XCTAssertTrue(decoded.darkModeEnabled)
        XCTAssertNil(decoded.lastSeenReleaseNotesVersion)
    }

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
            resetCreditSummary: nil,
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

private final class FakeLaunchAgent: DaemonLaunchAgentControlling, @unchecked Sendable {
    let isAvailable = true
    private let lock = NSLock()
    private var status: DaemonLaunchAgentRegistrationStatus = .notRegistered
    private var registrations = 0
    private var unregistrations = 0
    private let onRegister: @Sendable () -> Void
    private let onUnregister: @Sendable () -> Void

    init(
        onRegister: @escaping @Sendable () -> Void,
        onUnregister: @escaping @Sendable () -> Void
    ) {
        self.onRegister = onRegister
        self.onUnregister = onUnregister
    }

    var registerCount: Int { lock.withLock { registrations } }
    var unregisterCount: Int { lock.withLock { unregistrations } }

    func registrationStatus() -> DaemonLaunchAgentRegistrationStatus {
        lock.withLock { status }
    }

    func register() throws {
        lock.withLock {
            status = .enabled
            registrations += 1
        }
        onRegister()
    }

    func unregisterIfNeeded() async throws {
        let wasEnabled = lock.withLock {
            guard status == .enabled else { return false }
            status = .notRegistered
            unregistrations += 1
            return true
        }
        if wasEnabled { onUnregister() }
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
    private var forcedTerminations = 0
    private let ignoresTermination: Bool
    private let terminationHandler: @Sendable (pid_t) -> Void

    init(
        processIdentifier: pid_t,
        ignoresTermination: Bool,
        terminationHandler: @escaping @Sendable (pid_t) -> Void
    ) {
        self.processIdentifier = processIdentifier
        self.ignoresTermination = ignoresTermination
        self.terminationHandler = terminationHandler
    }

    var isRunning: Bool {
        lock.lock()
        defer { lock.unlock() }
        return running
    }

    var forceTerminationCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return forcedTerminations
    }

    func terminate() {
        if !ignoresTermination { crash() }
    }

    func forceTerminate() {
        lock.lock()
        forcedTerminations += 1
        lock.unlock()
        crash()
    }

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
    private let ignoresTermination: @Sendable (Int) -> Bool
    private let onLaunch: @Sendable () -> Void

    init(
        ignoresTermination: @escaping @Sendable (Int) -> Bool = { _ in false },
        onLaunch: @escaping @Sendable () -> Void
    ) {
        self.ignoresTermination = ignoresTermination
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
            ignoresTermination: ignoresTermination(processes.count),
            terminationHandler: terminationHandler
        )
        processes.append(process)
        lock.unlock()
        onLaunch()
        return process
    }
}
