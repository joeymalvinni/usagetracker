import XCTest
@testable import UsageMenuBar

final class AccountPresentationTests: XCTestCase {
    func testNamesAndEmailsStayRecognizable() {
        XCTAssertEqual(account(name: " Work ", email: "me@example.com").displayLabel, "Work")
        XCTAssertEqual(account(name: "  ", email: "me@example.com").displayLabel, "me@example.com")
        XCTAssertEqual(account(name: nil, email: nil, id: "someone.long@example.com").displayLabel,
            "someone.long@example.com")
    }

    func testOpaqueIdentifiersHaveConsistentFallbacks() {
        XCTAssertEqual(account(name: nil, email: nil, id: "12345678901234567890").displayLabel,
            "12345678…7890")
        XCTAssertEqual(account(name: nil, email: nil, id: " short ").displayLabel, "short")
        XCTAssertEqual(account(name: nil, email: nil, id: " ").displayLabel, "Account")
    }

    private func account(name: String?, email: String?, id: String = "external") -> Account {
        Account(id: "account", providerId: "claude", externalAccountId: id,
            profileId: nil, displayName: name, email: email, hidden: false,
            collectionEnabled: true, createdAt: .now, updatedAt: .now)
    }
}
