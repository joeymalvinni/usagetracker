import Foundation
import XCTest
@testable import UsageMenuBar

final class DashboardBuilderTests: XCTestCase {
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
}
