import XCTest
@testable import UsageMenuBar

final class ImportLocalClaudeModelTests: XCTestCase {
    private func preview(
        hasManagedConfigDir: Bool = true,
        defaultOptions: ImportOptions = ImportOptions(),
        toggles: [ImportToggleSize]? = nil
    ) -> AccountImportPreview {
        AccountImportPreview(
            providerId: "claude",
            accountId: "account-1",
            sourceHome: "/Users/demo/.claude",
            sourceClaudeJson: "/Users/demo/.claude.json",
            destination: "/Users/demo/.usagetracker/profiles/claude/account-1",
            hasManagedConfigDir: hasManagedConfigDir,
            sourceIdentity: "user@example.com",
            defaultMode: .prefsOnly,
            defaultOptions: defaultOptions,
            toggles: toggles ?? Self.defaultToggles
        )
    }

    private static let defaultToggles: [ImportToggleSize] = [
        ImportToggleSize(key: "prefs", enabledByDefault: true, supported: true, bytes: 1024, note: nil),
        ImportToggleSize(key: "project_trust", enabledByDefault: true, supported: true, bytes: nil, note: nil),
        ImportToggleSize(key: "prompt_history", enabledByDefault: true, supported: true, bytes: 4096, note: nil),
        ImportToggleSize(
            key: "plugins",
            enabledByDefault: false,
            supported: false,
            bytes: nil,
            note: "plugins require a path rewrite and are not imported yet"
        ),
        ImportToggleSize(
            key: "project_transcripts",
            enabledByDefault: false,
            supported: false,
            bytes: nil,
            note: "transcripts are excluded to preserve usage attribution"
        ),
        ImportToggleSize(
            key: "file_history",
            enabledByDefault: false,
            supported: false,
            bytes: nil,
            note: "file-history import is not supported yet"
        ),
        ImportToggleSize(
            key: "tasks_teams",
            enabledByDefault: false,
            supported: false,
            bytes: nil,
            note: "tasks/teams import is not supported yet"
        ),
        ImportToggleSize(
            key: "sessions",
            enabledByDefault: false,
            supported: false,
            bytes: nil,
            note: "sessions import is not supported yet"
        ),
    ]

    func testPrefillsToggleDefaultsFromPreview() {
        let model = ImportLocalClaudeModel(
            accountId: "account-1",
            accountTitle: "Work",
            providerId: "claude",
            preview: preview(
                defaultOptions: ImportOptions(
                    prefs: true,
                    projectTrust: false,
                    promptHistory: true
                )
            )
        )

        XCTAssertEqual(model.accountTitle, "Work")
        XCTAssertEqual(model.sourceHome, "/Users/demo/.claude")
        XCTAssertEqual(model.destination, "/Users/demo/.usagetracker/profiles/claude/account-1")
        XCTAssertEqual(model.sourceIdentity, "user@example.com")
        XCTAssertEqual(model.mode, .prefsOnly)
        XCTAssertTrue(model.hasManagedConfigDir)
        XCTAssertNil(model.managedProfileError)

        XCTAssertEqual(model.toggles.filter(\.supported).map(\.enabled), [true, false, true])
        XCTAssertEqual(model.toggles.filter { !$0.supported }.map(\.enabled), [false, false, false, false, false])
    }

    func testUnsupportedTogglesCarryNotesAndStayDisabled() {
        let model = ImportLocalClaudeModel(
            accountId: "account-1",
            accountTitle: "Work",
            providerId: "claude",
            preview: preview()
        )

        let stretch = model.toggles.filter { !$0.supported }
        XCTAssertEqual(stretch.count, 5)
        XCTAssertEqual(stretch[0].key, "plugins")
        XCTAssertEqual(stretch[0].note, "plugins require a path rewrite and are not imported yet")
        XCTAssertFalse(stretch[0].supported)

        var editable = model
        editable.setToggle("plugins", enabled: true)
        XCTAssertFalse(editable.toggles.first { $0.key == "plugins" }!.enabled)
    }

    func testWireOptionsMapsOnlyEditableComfortToggles() {
        var model = ImportLocalClaudeModel(
            accountId: "account-1",
            accountTitle: "Work",
            providerId: "claude",
            preview: preview()
        )
        model.setToggle("prefs", enabled: false)
        model.setToggle("project_trust", enabled: true)
        model.setToggle("prompt_history", enabled: false)
        model.mode = .replace

        XCTAssertEqual(
            model.wireOptions,
            ImportOptions(prefs: false, projectTrust: true, promptHistory: false)
        )
    }

    func testMissingManagedProfileSurfacesErrorAndBlocksImport() {
        let model = ImportLocalClaudeModel(
            accountId: "account-1",
            accountTitle: "Work",
            providerId: "claude",
            preview: preview(hasManagedConfigDir: false)
        )

        XCTAssertFalse(model.canImport)
        XCTAssertEqual(
            model.managedProfileError,
            "This account has no managed profile, so local Claude settings cannot be imported."
        )
    }
}
