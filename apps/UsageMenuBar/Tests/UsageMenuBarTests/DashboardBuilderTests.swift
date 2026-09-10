import Foundation
import XCTest
@testable import UsageMenuBar

final class DashboardBuilderTests: XCTestCase {
    func testCachedClaudePlaceholdersDoNotBecomeQuotaMeters() throws {
        let account = account(id: "claude-account", providerId: "claude")
        let snapshot = UsageSnapshot(providerId: "claude", accountId: account.id,
            collectedAt: Date(), windows: ["nimbus_quill", "five_hour"].map {
                UsageWindow(windowId: "claude_usage_utilization_\($0)", label: "Claude \($0)",
                    kind: .session, used: nil, limit: nil, remaining: nil,
                    percentUsed: 0, percentRemaining: 100, resetAt: nil)
            })
        let output = DashboardBuilder(config: config(providers: ["claude": true]),
            accounts: [account], health: [], snapshots: [snapshot], forecasts: [],
            dashboard: .empty, windowProvenance: [], ui: UIConfig(), visible: { _ in true }).build()
        let provider = try XCTUnwrap(output.providers.first)
        XCTAssertEqual(provider.windows.count, 1)
        XCTAssertFalse(provider.windows.contains { $0.label.contains("nimbus") })
    }

    func testCodexAccountActivityAndMissingLocalCostStayIsolated() throws {
        let today = DateFormats.dayKey.string(from: Date())
        let accounts = [account(id: "main", providerId: "codex"), account(id: "second", providerId: "codex")]
        let snapshots = accounts.map {
            UsageSnapshot(providerId: "codex", accountId: $0.id, collectedAt: Date(), windows: [])
        }
        let provenance = DataProvenance(source: .providerReported, scope: .accountWide,
            quality: .authoritative, completeness: .complete, confidence: .high)
        let summaries = zip(accounts, [UInt64(113_897_797), UInt64(8_245)]).map { account, tokens in
            AccountUsageSummary(providerId: "codex", accountId: account.id,
                activity: ActivitySummary(provenance: provenance,
                    days: [DailyUsagePoint(dateKey: today, tokens: tokens, costUsd: nil, pricedTokens: 0, unpricedTokens: 0)],
                    todayTokens: tokens, lookbackTokens: tokens, lifetimeTokens: tokens),
                cost: nil, resetCredits: nil)
        }
        let output = DashboardBuilder(config: config(providers: ["codex": true]), accounts: accounts,
            health: [], snapshots: snapshots, forecasts: [],
            dashboard: UsageDashboardSummary(accounts: summaries, days: [], pricing: .empty, provenance: .empty),
            windowProvenance: [], ui: UIConfig(), visible: { _ in true }).build()
        let group = try XCTUnwrap(output.providers.first)
        let main = try XCTUnwrap(group.subAccounts?.first { $0.accountId == "main" })
        let second = try XCTUnwrap(group.subAccounts?.first { $0.accountId == "second" })
        XCTAssertEqual(main.costDashboard.todayTokens, 113_897_797)
        XCTAssertEqual(second.costDashboard.todayTokens, 8_245)
        XCTAssertEqual(group.costDashboard.todayTokens, 113_906_042)
        XCTAssertFalse(second.hasCostData)
        XCTAssertEqual(second.activitySourceLabel, "Account activity reported by Codex; may include other devices.")
    }

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
        let onlineProvider = try XCTUnwrap(onlineOutput.providers.first)
        XCTAssertEqual(onlineProvider.status, .stale)
        XCTAssertEqual(onlineProvider.collectionIssue, .temporarilyUnavailable)
        XCTAssertNil(onlineProvider.alertSignature)

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

        let authProvider = try XCTUnwrap(authOutput.providers.first)
        XCTAssertEqual(authProvider.status, .normal)
        XCTAssertEqual(authProvider.collectionIssue?.recoveryAction, .reconnect)
        XCTAssertNil(authProvider.alertSignature)

        let keychainHealth = ProviderHealth(
            providerId: "codex",
            accountId: account.id,
            status: .keychainAccessFailed,
            collectionMode: "oauth",
            lastSuccessAt: snapshot.collectedAt,
            lastFailureAt: Date(),
            lastErrorCode: "keychain_access_failed",
            lastErrorMessage: "macOS denied credential access",
            updatedAt: Date()
        )
        let keychainOutput = DashboardBuilder(
            config: config(providers: ["codex": true]),
            accounts: [account],
            health: [keychainHealth],
            snapshots: [snapshot],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            connectivity: .online,
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        let keychainProvider = try XCTUnwrap(keychainOutput.providers.first)
        XCTAssertEqual(keychainProvider.status, .stale)
        XCTAssertEqual(keychainProvider.collectionIssue?.recoveryAction, .allowAccess)
        XCTAssertNil(keychainProvider.alertSignature)
    }

    func testCollectionFailuresPreserveQuotaAndOfferOnlyRelevantRecovery() throws {
        let account = account(id: "personal", providerId: "codex")
        let cases: [(ProviderHealthStatus, String, ProviderCollectionIssue, ProviderRecoveryAction?)] = [
            (.keychainAccessFailed, "keychain_access_failed", .permissionRequired, .allowAccess),
            (.credentialsMissing, "credentials_missing", .accountUnavailable, .connect),
            (.authFailed, "credentials_invalid", .invalidCredentials, .checkConnection),
            (.authFailed, "unauthorized", .signInRequired, .reconnect),
            (.providerError, "network", .temporarilyUnavailable, nil),
            (.rateLimited, "rate_limited", .rateLimited, nil),
            (.backingOff, "rate_limited", .rateLimited, nil),
            (.parseError, "parse", .responseChanged, nil),
        ]

        for (status, code, issue, action) in cases {
            for remaining in [80.0, 5.0] {
                let output = DashboardBuilder(
                    config: config(providers: ["codex": true]),
                    accounts: [account],
                    health: [health(accountId: account.id, status: status, code: code)],
                    snapshots: [UsageSnapshot(providerId: "codex", accountId: account.id,
                        collectedAt: Date(), windows: [limit(id: "weekly", remaining: remaining)])],
                    forecasts: [], dashboard: .empty, windowProvenance: [],
                    ui: UIConfig(), visible: { _ in true }
                ).build()

                let provider = try XCTUnwrap(output.providers.first)
                XCTAssertEqual(provider.accountId, account.id, code)
                XCTAssertEqual(provider.percent, remaining, code)
                XCTAssertEqual(provider.status, remaining == 80 ? .normal : .critical, code)
                XCTAssertEqual(provider.hasUnseenAlert, remaining == 5, code)
                XCTAssertEqual(provider.alertSignature != nil, remaining == 5, code)
                XCTAssertEqual(provider.collectionIssue, issue, code)
                XCTAssertEqual(provider.collectionIssue?.recoveryAction, action, code)
            }
        }
    }

    func testHeadlineUsesSameVisibleLimitForNumberBarLabelAndReset() throws {
        let account = account(id: "personal", providerId: "codex")
        let sessionReset = Date().addingTimeInterval(3_600)
        let weeklyReset = Date().addingTimeInterval(86_400)
        let snapshot = UsageSnapshot(providerId: "codex", accountId: account.id,
            collectedAt: Date(), windows: [
                limit(id: "session", remaining: 80, resetAt: sessionReset),
                limit(id: "weekly", remaining: 20, resetAt: weeklyReset),
            ])
        func provider(ui: UIConfig) throws -> ProviderVM {
            let output = DashboardBuilder(config: config(providers: ["codex": true]),
                accounts: [account], health: [], snapshots: [snapshot], forecasts: [],
                dashboard: .empty, windowProvenance: [], ui: ui, visible: { _ in true }).build()
            return try XCTUnwrap(output.providers.first)
        }

        let bothVisible = try provider(ui: UIConfig())
        XCTAssertEqual(bothVisible.primary, "20%")
        XCTAssertEqual(bothVisible.percent, bothVisible.headlineWindow?.percent)
        XCTAssertEqual(bothVisible.headlineWindow?.label, "weekly")
        XCTAssertEqual(bothVisible.headlineWindow?.resetAt, weeklyReset)

        var ui = UIConfig()
        ui.hiddenWindows[AppState.windowKey("codex", "weekly")] = "weekly"
        let weeklyHidden = try provider(ui: ui)
        XCTAssertEqual(weeklyHidden.primary, "80%")
        XCTAssertEqual(weeklyHidden.percent, weeklyHidden.headlineWindow?.percent)
        XCTAssertEqual(weeklyHidden.headlineWindow?.label, "session")
        XCTAssertEqual(weeklyHidden.headlineWindow?.resetAt, sessionReset)
        XCTAssertEqual(weeklyHidden.status, .normal)
    }

    func testAccountRecoveryDoesNotLeakToAnotherAccountOrProviderGroup() throws {
        let accounts = [account(id: "personal", providerId: "codex"), account(id: "work", providerId: "codex")]
        let output = DashboardBuilder(config: config(providers: ["codex": true]), accounts: accounts,
            health: [health(accountId: "personal", status: .authFailed, code: "unauthorized")],
            snapshots: accounts.map {
                UsageSnapshot(providerId: "codex", accountId: $0.id, collectedAt: Date(),
                    windows: [limit(id: "weekly", remaining: 80)])
            },
            forecasts: [], dashboard: .empty, windowProvenance: [],
            ui: UIConfig(), visible: { _ in true }).build()

        let group = try XCTUnwrap(output.providers.first)
        let personal = try XCTUnwrap(group.subAccounts?.first { $0.accountId == "personal" })
        let work = try XCTUnwrap(group.subAccounts?.first { $0.accountId == "work" })
        XCTAssertEqual(personal.collectionIssue?.recoveryAction, .reconnect)
        XCTAssertNil(work.collectionIssue)
        XCTAssertNil(work.errorDetail)
        XCTAssertNil(group.collectionIssue)
        XCTAssertNil(group.accountId)
        XCTAssertNil(group.errorDetail)
        XCTAssertEqual(group.status, .normal)
        XCTAssertFalse(group.hasUnseenAlert)
    }

    func testMissingUsageDoesNotCreateQuotaAlertAndPausedAccountNeedsNoRecovery() throws {
        for enabled in [true, false] {
            let output = DashboardBuilder(config: config(providers: ["codex": true]),
                accounts: [account(id: "personal", providerId: "codex", collectionEnabled: enabled)],
                health: [health(accountId: "personal", status: .keychainAccessFailed, code: "keychain_access_failed")],
                snapshots: [], forecasts: [], dashboard: .empty, windowProvenance: [],
                ui: UIConfig(), visible: { _ in true }).build()

            let provider = try XCTUnwrap(output.providers.first)
            XCTAssertEqual(provider.primary, "No data")
            XCTAssertEqual(provider.status, enabled ? .stale : .disabled)
            XCTAssertEqual(provider.collectionIssue, enabled ? .permissionRequired : nil)
            XCTAssertNil(provider.alertSignature)
            XCTAssertFalse(provider.hasUnseenAlert)
        }
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

    func testExpiredWindowTriggersRecoveryEvenWithFreshSnapshot() throws {
        let now = Date()
        let expiredWindow = UsageWindow(
            windowId: "weekly", label: "Weekly", kind: .weekly,
            used: nil, limit: nil, remaining: nil,
            percentUsed: 50, percentRemaining: 50,
            resetAt: now.addingTimeInterval(-14 * 3_600)
        )
        let snapshot = UsageSnapshot(
            providerId: "codex", accountId: "codex-account",
            collectedAt: now, windows: [expiredWindow]
        )
        XCTAssertEqual(AppState.staleProviderIDs(
            config: config(providers: ["codex": true]), accounts: [],
            snapshots: [snapshot], now: now
        ), ["codex"])
        let output = DashboardBuilder(
            config: config(providers: ["codex": true]), accounts: [], health: [],
            snapshots: [snapshot], forecasts: [], dashboard: .empty,
            windowProvenance: [], ui: UIConfig(), visible: { _ in true }
        ).build()
        let provider = try XCTUnwrap(output.providers.first)
        XCTAssertEqual(provider.status, .stale)
        let window = try XCTUnwrap(provider.windows.first)
        XCTAssertEqual(window.status, .stale)
        XCTAssertTrue(window.isMuted)
        XCTAssertNil(window.forecast)
        XCTAssertEqual(window.percent, 50) // Retain the last known value; never invent a reset.
        XCTAssertEqual(window.reset, "Reset passed · awaiting update")
    }

    func testResetLabelHandlesPastExactAndFutureDeadlines() {
        let now = Date(timeIntervalSince1970: 10_000)
        for reset in [now.addingTimeInterval(-50_400), now] {
            XCTAssertEqual(DateFormats.resetLabel(for: reset, relativeTo: now),
                           "Reset passed · awaiting update")
        }
        XCTAssertTrue(DateFormats.resetLabel(
            for: now.addingTimeInterval(3_600), relativeTo: now
        ).hasPrefix("Resets in "))
        let fresh = UsageSnapshot(
            providerId: "codex", accountId: "codex-account", collectedAt: now,
            windows: [UsageWindow(
                windowId: "weekly", label: "Weekly", kind: .weekly,
                used: nil, limit: nil, remaining: nil,
                percentUsed: 50, percentRemaining: 50,
                resetAt: now.addingTimeInterval(3_600)
            )]
        )
        XCTAssertEqual(AppState.staleProviderIDs(
            config: config(providers: ["codex": true]), accounts: [],
            snapshots: [fresh], now: now
        ), [])
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

    func testClaudeBackoffDoesNotMasqueradeAsLowQuota() throws {
        let account = account(id: "claude-account", providerId: "claude")
        let snapshot = UsageSnapshot(
            providerId: "claude",
            accountId: account.id,
            collectedAt: Date(),
            windows: [
                UsageWindow(
                    windowId: "claude_usage_utilization_seven_day",
                    label: "Claude seven day",
                    kind: .weekly,
                    used: UsageAmount(value: 5, unit: .percent),
                    limit: UsageAmount(value: 100, unit: .percent),
                    remaining: UsageAmount(value: 95, unit: .percent),
                    percentUsed: 5,
                    percentRemaining: 95,
                    resetAt: nil
                ),
            ]
        )
        let health = ProviderHealth(
            providerId: "claude",
            accountId: account.id,
            status: .backingOff,
            collectionMode: nil,
            lastSuccessAt: snapshot.collectedAt,
            lastFailureAt: Date(),
            lastErrorCode: "rate_limited",
            lastErrorMessage: "retrying later",
            updatedAt: Date()
        )

        let output = DashboardBuilder(
            config: config(providers: ["claude": true]),
            accounts: [account],
            health: [health],
            snapshots: [snapshot],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        let provider = try XCTUnwrap(output.providers.first)
        XCTAssertEqual(provider.percent, 95)
        XCTAssertEqual(provider.status, .normal)
        XCTAssertEqual(provider.healthText, "backing off")
        XCTAssertNil(provider.alertSignature)
    }

    func testClaudeBackoffPreservesGenuineLowQuotaWarning() throws {
        let account = account(id: "claude-account", providerId: "claude")
        let snapshot = UsageSnapshot(
            providerId: "claude",
            accountId: account.id,
            collectedAt: Date(),
            windows: [
                UsageWindow(
                    windowId: "claude_usage_utilization_seven_day",
                    label: "Claude seven day",
                    kind: .weekly,
                    used: UsageAmount(value: 80, unit: .percent),
                    limit: UsageAmount(value: 100, unit: .percent),
                    remaining: UsageAmount(value: 20, unit: .percent),
                    percentUsed: 80,
                    percentRemaining: 20,
                    resetAt: nil
                ),
            ]
        )
        let health = ProviderHealth(
            providerId: "claude",
            accountId: account.id,
            status: .backingOff,
            collectionMode: nil,
            lastSuccessAt: snapshot.collectedAt,
            lastFailureAt: Date(),
            lastErrorCode: "rate_limited",
            lastErrorMessage: "retrying later",
            updatedAt: Date()
        )

        let output = DashboardBuilder(
            config: config(providers: ["claude": true]),
            accounts: [account],
            health: [health],
            snapshots: [snapshot],
            forecasts: [],
            dashboard: .empty,
            windowProvenance: [],
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        let provider = try XCTUnwrap(output.providers.first)
        XCTAssertEqual(provider.percent, 20)
        XCTAssertEqual(provider.status, .warning)
        XCTAssertNotNil(provider.alertSignature)
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

    func testActivityDashboardPreservesAvailableHistoryAndLifetimeTotals() throws {
        let calendar = Calendar(identifier: .gregorian)
        let today = calendar.startOfDay(for: Date())
        let oldDate = try XCTUnwrap(calendar.date(byAdding: .day, value: -60, to: today))
        let oldKey = DateFormats.dayKey.string(from: oldDate)
        let todayKey = DateFormats.dayKey.string(from: today)
        let provenance = DataProvenance(
            source: .localLogs,
            scope: .thisDevice,
            quality: .estimated,
            completeness: .partial,
            confidence: .high
        )
        let oldPoint = DailyUsagePoint(
            dateKey: oldKey,
            tokens: 10,
            costUsd: 1.25,
            pricedTokens: 10,
            unpricedTokens: 0
        )
        let todayPoint = DailyUsagePoint(
            dateKey: todayKey,
            tokens: 20,
            costUsd: 2.50,
            pricedTokens: 20,
            unpricedTokens: 0
        )
        let dashboard = UsageDashboardSummary(
            accounts: [
                AccountUsageSummary(
                    providerId: "codex",
                    accountId: "codex-account",
                    activity: ActivitySummary(
                        provenance: provenance,
                        days: [oldPoint, todayPoint],
                        todayTokens: 20,
                        lookbackTokens: 20,
                        lifetimeTokens: 500
                    ),
                    cost: CostSummary(
                        provenance: provenance,
                        days: [oldPoint, todayPoint],
                        todayCostUsd: 2.50,
                        lookbackCostUsd: 2.50,
                        pricing: .empty
                    ),
                    resetCredits: nil
                )
            ],
            days: [oldPoint, todayPoint],
            pricing: .empty,
            provenance: .empty
        )

        let output = DashboardBuilder(
            config: config(providers: ["codex": true]),
            accounts: [],
            health: [],
            snapshots: [],
            forecasts: [],
            dashboard: dashboard,
            windowProvenance: [],
            ui: UIConfig(),
            visible: { _ in true }
        ).build()

        XCTAssertEqual(output.costDashboard.days.first?.id, oldKey)
        XCTAssertEqual(output.costDashboard.days.last?.id, todayKey)
        XCTAssertEqual(output.costDashboard.allTimeCost, 3.75, accuracy: 0.001)
        XCTAssertEqual(output.costDashboard.allTimeTokens, 500)
        XCTAssertEqual(output.costDashboard.cost30d, 2.50, accuracy: 0.001)
        XCTAssertEqual(output.costDashboard.tokens30d, 20)
    }

    private func limit(id: String, remaining: Double, resetAt: Date? = nil) -> UsageWindow {
        UsageWindow(windowId: id, label: id, kind: .weekly, used: nil, limit: nil, remaining: nil,
            percentUsed: 100 - remaining, percentRemaining: remaining, resetAt: resetAt)
    }

    private func health(accountId: String, status: ProviderHealthStatus, code: String) -> ProviderHealth {
        ProviderHealth(providerId: "codex", accountId: accountId, status: status, collectionMode: "oauth",
            lastSuccessAt: nil, lastFailureAt: Date(), lastErrorCode: code,
            lastErrorMessage: "Collection failed: \(code)", updatedAt: Date())
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
