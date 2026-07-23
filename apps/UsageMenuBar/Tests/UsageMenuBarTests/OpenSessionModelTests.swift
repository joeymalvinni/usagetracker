import XCTest
@testable import UsageMenuBar
import struct UsageMenuBar.LaunchFlags

final class OpenSessionModelTests: XCTestCase {
    private func settings(
        workingDirectory: String? = nil,
        launch: LaunchFlags? = nil,
        hasManagedConfigDir: Bool = true
    ) -> AccountLaunchSettingsResponse {
        AccountLaunchSettingsResponse(
            providerId: "claude",
            accountId: "account-1",
            workingDirectory: workingDirectory,
            launch: launch,
            hasManagedConfigDir: hasManagedConfigDir
        )
    }

    func testPrefillsFromSavedSettingsAndDefaultsRememberOff() {
        let model = OpenSessionModel(
            accountId: "account-1",
            accountTitle: "Work",
            providerId: "claude",
            settings: settings(
                workingDirectory: "~/Projects/demo",
                launch: LaunchFlags(model: "fable", effort: "xhigh", dangerouslySkipPermissions: true)
            )
        )
        XCTAssertEqual(model.workingDirectory, "~/Projects/demo")
        XCTAssertEqual(model.model, "fable")
        XCTAssertEqual(model.effort, .xhigh)
        XCTAssertTrue(model.dangerouslySkipPermissions)
        // The dangerous flag is use-once by default.
        XCTAssertFalse(model.rememberDangerous)
        XCTAssertTrue(model.hasManagedConfigDir)
    }

    func testUnknownEffortAndMissingPrefsFallBackToDefaults() {
        let model = OpenSessionModel(
            accountId: "account-1",
            accountTitle: "Work",
            providerId: "claude",
            settings: settings(launch: LaunchFlags(effort: "warp-speed"))
        )
        XCTAssertEqual(model.effort, .systemDefault)
        XCTAssertEqual(model.workingDirectory, "")
        XCTAssertEqual(model.model, "")
        XCTAssertFalse(model.dangerouslySkipPermissions)
    }

    func testWireMappingTrimsAndAlwaysSendsWholeFlagObject() {
        var model = OpenSessionModel(
            accountId: "account-1",
            accountTitle: "Work",
            providerId: "claude",
            settings: settings()
        )
        model.model = "  fable  "
        model.effort = .systemDefault
        model.workingDirectory = "  /tmp/demo  "

        XCTAssertEqual(model.wireFlags, LaunchFlags(model: "fable"))
        XCTAssertEqual(model.trimmedWorkingDirectory, "/tmp/demo")

        // Clearing every field still produces a full (default) flag object —
        // the sheet is authoritative and a cleared sheet clears saved prefs.
        model.model = "   "
        XCTAssertEqual(model.wireFlags, LaunchFlags())

        // Blank working directory is sent as an empty string (wire semantics:
        // blank clears the saved value; nil would mean "use saved").
        model.workingDirectory = "   "
        XCTAssertEqual(model.trimmedWorkingDirectory, "")
    }

    func testEffortChoicesRoundTripWireValues() {
        XCTAssertNil(EffortChoice.systemDefault.wireValue)
        for choice in EffortChoice.allCases where choice != .systemDefault {
            XCTAssertEqual(EffortChoice(rawValue: choice.wireValue ?? ""), choice)
        }
        XCTAssertEqual(EffortChoice.allCases.map(\.rawValue),
                       ["default", "low", "medium", "high", "xhigh", "max"])
    }
}
