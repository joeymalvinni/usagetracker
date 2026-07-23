import Foundation
import XCTest
@testable import UsageMenuBar

final class DashboardBuilderTests: XCTestCase {
    func testOfflineProviderWithoutCachedUsageRemainsStale() throws {
        let output = DashboardBuilder(
            config: config(providers: ["codex": true]),
            accounts: [],
            health: [],
            snapshots: [],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            connectivity: .offline,
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        let provider = try XCTUnwrap(output.providers.first)
        XCTAssertEqual(provider.primary, "No data")
        XCTAssertEqual(provider.status, .stale)
    }

    func testOfflineMutesBarsAndSuppressesOnlyNetworkHealth() throws {
        let account = account(id: "codex-account", providerId: "codex")
        let snapshot = UsageSnapshot(
            providerId: "codex",
            accountId: account.id,
            collectedAt: Date().addingTimeInterval(-3_600),
            windows: [
                UsageWindow(
                    windowId: "weekly",
                    label: "Weekly limit",
                    kind: .weekly,
                    used: nil,
                    limit: nil,
                    remaining: nil,
                    percentUsed: 20,
                    percentRemaining: 80,
                    resetAt: nil
                ),
            ]
        )
        let networkHealth = ProviderHealth(
            providerId: "codex",
            accountId: account.id,
            status: .providerError,
            collectionMode: "oauth",
            lastSuccessAt: snapshot.collectedAt,
            lastFailureAt: Date(),
            lastErrorCode: "network",
            lastErrorMessage: "error fetching URL",
            updatedAt: Date()
        )
        let output = DashboardBuilder(
            config: config(providers: ["codex": true]),
            accounts: [account],
            health: [networkHealth],
            snapshots: [snapshot],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            connectivity: .offline,
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        let provider = try XCTUnwrap(output.providers.first)
        XCTAssertEqual(provider.status, .normal)
        XCTAssertNil(provider.errorDetail)
        XCTAssertTrue(try XCTUnwrap(provider.windows.first).isMuted)

        let onlineOutput = DashboardBuilder(
            config: config(providers: ["codex": true]),
            accounts: [account],
            health: [networkHealth],
            snapshots: [snapshot],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            connectivity: .online,
            ui: UIConfig(),
            visible: { _ in true }
        ).build()
        XCTAssertEqual(try XCTUnwrap(onlineOutput.providers.first).status, .error)

        let authHealth = ProviderHealth(
            providerId: "codex",
            accountId: account.id,
            status: .authFailed,
            collectionMode: "oauth",
            lastSuccessAt: snapshot.collectedAt,
            lastFailureAt: Date(),
            lastErrorCode: "unauthorized",
            lastErrorMessage: "Sign in again",
            updatedAt: Date()
        )
        let authOutput = DashboardBuilder(
            config: config(providers: ["codex": true]),
            accounts: [account],
            health: [authHealth],
            snapshots: [snapshot],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            connectivity: .offline,
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        XCTAssertEqual(try XCTUnwrap(authOutput.providers.first).status, .error)
    }

    func testStaleProviderShowsRefreshingOnlyWhileThatProviderRefreshes() throws {
        let snapshot = UsageSnapshot(
            providerId: "codex",
            accountId: "codex-account",
            collectedAt: Date().addingTimeInterval(-3_600),
            windows: []
        )

        func status(refreshingProviderIDs: Set<String>) throws -> DisplayStatus {
            let output = DashboardBuilder(
                config: config(providers: ["codex": true]),
                accounts: [],
                health: [],
                snapshots: [snapshot],
                forecasts: [],
                dashboard: .empty,
                windowProvenance: [],
                ui: UIConfig(),
                refreshingProviderIDs: refreshingProviderIDs,
                visible: { _ in true }
            ).build()
            return try XCTUnwrap(output.providers.first).status
        }

        XCTAssertEqual(try status(refreshingProviderIDs: ["codex"]), .refreshing)
        XCTAssertEqual(try status(refreshingProviderIDs: ["claude"]), .stale)
        XCTAssertEqual(DisplayStatus.refreshing.label, "refreshing…")
    }

    func testStaleProviderDetectionIgnoresFreshDisabledAndCollectionDisabledProviders() {
        let now = Date(timeIntervalSince1970: 10_000)
        let accounts = [
            account(id: "codex-account", providerId: "codex"),
            account(id: "claude-account", providerId: "claude"),
            account(id: "grok-account", providerId: "grok", collectionEnabled: false),
        ]
        let snapshots = [
            UsageSnapshot(
                providerId: "codex",
                accountId: "codex-account",
                collectedAt: now.addingTimeInterval(-601),
                windows: []
            ),
            UsageSnapshot(
                providerId: "claude",
                accountId: "claude-account",
                collectedAt: now.addingTimeInterval(-60),
                windows: []
            ),
        ]

        let stale = AppState.staleProviderIDs(
            config: config(providers: [
                "codex": true,
                "claude": true,
                "grok": true,
                "opencode_go": true,
                "disabled": false,
            ]),
            accounts: accounts,
            snapshots: snapshots,
            now: now
        )

        XCTAssertEqual(stale, ["codex", "opencode_go"])
    }

    func testSingleAccountNameDoesNotReplaceProviderName() throws {
        let account = Account(
            id: "claude-account",
            providerId: "claude",
            externalAccountId: "claude@example.test",
            profileId: nil,
            displayName: "Personal",
            email: "claude@example.test",
            hidden: false,
            collectionEnabled: true,
            createdAt: Date(timeIntervalSince1970: 0),
            updatedAt: Date(timeIntervalSince1970: 0)
        )
        let output = DashboardBuilder(
            config: nil,
            accounts: [account],
            health: [],
            snapshots: [],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        let provider = try XCTUnwrap(output.providers.first)
        XCTAssertEqual(provider.name, "Claude")
        XCTAssertEqual(provider.account, "Personal")
    }

    func testPercentageQuotaDoesNotShowRedundantAbsoluteRatio() throws {
        let snapshot = UsageSnapshot(
            providerId: "claude",
            accountId: "claude-account",
            collectedAt: Date(),
            windows: [
                UsageWindow(
                    windowId: "claude_usage",
                    label: "Weekly limit",
                    kind: .weekly,
                    used: UsageAmount(value: 0, unit: .percent),
                    limit: UsageAmount(value: 100, unit: .percent),
                    remaining: UsageAmount(value: 100, unit: .percent),
                    percentUsed: 0,
                    percentRemaining: 100,
                    resetAt: nil
                ),
                UsageWindow(
                    windowId: "extra_usage",
                    label: "Extra usage",
                    kind: .credits,
                    used: UsageAmount(value: 4, unit: .usd),
                    limit: UsageAmount(value: 10, unit: .usd),
                    remaining: UsageAmount(value: 6, unit: .usd),
                    percentUsed: 40,
                    percentRemaining: 60,
                    resetAt: nil
                )
            ]
        )

        let output = DashboardBuilder(
            config: nil,
            accounts: [],
            health: [],
            snapshots: [snapshot],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        let provider = try XCTUnwrap(output.providers.first)
        XCTAssertNil(try XCTUnwrap(provider.windows.first).absolute)
        XCTAssertEqual(try XCTUnwrap(provider.credits.first).absolute, "4 / 10")
    }

    func testCountOnlyResetSummaryReachesProviderViewModel() throws {
        let snapshot = UsageSnapshot(
            providerId: "codex",
            accountId: "codex-account",
            collectedAt: Date(),
            windows: []
        )
        let dashboard = UsageDashboardSummary(
            accounts: [
                AccountUsageSummary(
                    providerId: "codex",
                    accountId: "codex-account",
                    activity: nil,
                    cost: nil,
                    resetCredits: ResetCreditSummary(
                        availableCount: 4,
                        nextExpiresAt: nil,
                        credits: []
                    )
                )
            ],
            days: [],
            pricing: .empty,
            provenance: .empty
        )

        let output = DashboardBuilder(
            config: config(providers: ["codex": true]),
            accounts: [],
            health: [],
            snapshots: [snapshot],
            forecasts: [],
            dashboard: dashboard,
            windowProvenance: [],
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        let summary = try XCTUnwrap(output.providers.first?.resetCreditSummary)
        XCTAssertEqual(summary.availableCount, 4)
        XCTAssertNil(summary.nextExpiresAt)
        XCTAssertTrue(summary.credits.isEmpty)
        XCTAssertTrue(try XCTUnwrap(output.providers.first).windows.isEmpty)
    }

    func testServerRegisteredProviderNeedsNoCatalogEntry() throws {
        let descriptor = ServerProviderDescriptor(
            id: "future_provider",
            displayName: "Future Provider",
            minimumRefreshIntervalSeconds: 60,
            capabilities: ProviderCapabilities(
                multipleAccounts: false,
                addAccount: false,
                repair: false,
                launchAccount: false,
                workspaceSetup: false
            )
        )
        let output = DashboardBuilder(
            config: config(providers: ["future_provider": true]),
            accounts: [],
            health: [],
            snapshots: [],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            serverProviders: [descriptor.id: descriptor],
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        let provider = try XCTUnwrap(output.settingsProviders.first)
        XCTAssertEqual(provider.providerId, "future_provider")
        XCTAssertEqual(provider.name, "Future Provider")
        XCTAssertEqual(provider.symbol, "chart.bar")
    }

    func testServerRegistryOrderIsPreservedForUnknownProviders() {
        let descriptors = ["provider_z", "provider_a"].map { id in
            ServerProviderDescriptor(
                id: id,
                displayName: id,
                minimumRefreshIntervalSeconds: 60,
                capabilities: ProviderCapabilities(
                    multipleAccounts: false,
                    addAccount: false,
                    repair: false,
                    launchAccount: false,
                    workspaceSetup: false
                )
            )
        }
        let output = DashboardBuilder(
            config: config(providers: ["provider_z": true, "provider_a": true]),
            accounts: [],
            health: [],
            snapshots: [],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            serverProviders: Dictionary(uniqueKeysWithValues: descriptors.map { ($0.id, $0) }),
            serverProviderOrder: descriptors.map(\.id),
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        XCTAssertEqual(output.settingsProviders.map(\.providerId), ["provider_z", "provider_a"])
    }

    private func config(providers: [String: Bool]) -> ConfigResponse {
        ConfigResponse(
            pollIntervalSeconds: 300,
            notifications: NotificationConfig(enabled: false),
            configPath: "/tmp/config.json",
            socketPath: "/tmp/usage.sock",
            dbPath: "/tmp/usage.sqlite3",
            providers: providers.mapValues { ProviderToggle(enabled: $0) }
        )
    }

    private func account(
        id: String,
        providerId: String,
        collectionEnabled: Bool = true
    ) -> Account {
        Account(
            id: id,
            providerId: providerId,
            externalAccountId: id,
            profileId: nil,
            displayName: nil,
            email: nil,
            hidden: false,
            collectionEnabled: collectionEnabled,
            createdAt: Date(timeIntervalSince1970: 0),
            updatedAt: Date(timeIntervalSince1970: 0)
        )
    }
}
