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
}
