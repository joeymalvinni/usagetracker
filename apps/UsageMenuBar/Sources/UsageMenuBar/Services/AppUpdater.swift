import Combine
import CryptoKit
import Foundation

struct SemanticVersion: Comparable, CustomStringConvertible, Equatable, Sendable {
    let major: UInt
    let minor: UInt
    let patch: UInt

    init?(_ value: String) {
        let version = value.hasPrefix("v") ? String(value.dropFirst()) : value
        let components = version.split(separator: ".", omittingEmptySubsequences: false)
        guard components.count == 3,
              components.allSatisfy({ !$0.isEmpty && $0.allSatisfy(\.isNumber) }),
              let major = UInt(components[0]),
              let minor = UInt(components[1]),
              let patch = UInt(components[2]) else {
            return nil
        }
        self.major = major
        self.minor = minor
        self.patch = patch
    }

    var description: String { "\(major).\(minor).\(patch)" }

    static func < (lhs: SemanticVersion, rhs: SemanticVersion) -> Bool {
        if lhs.major != rhs.major { return lhs.major < rhs.major }
        if lhs.minor != rhs.minor { return lhs.minor < rhs.minor }
        return lhs.patch < rhs.patch
    }
}

struct AppRelease: Equatable, Sendable {
    let tag: String
    let version: SemanticVersion
    let releaseNotes: ReleaseNotes?
}

struct ReleaseNotes: Codable, Equatable, Sendable {
    let version: String
    let summary: String
    let highlights: [String]
}

enum ReleaseNotesParser {
    static let maximumHighlights = 6
    private static let maximumSummaryCharacters = 240
    private static let maximumHighlightCharacters = 180

    static func parse(body: String?, version: SemanticVersion) -> ReleaseNotes? {
        guard let body else { return nil }
        let lines = body.replacingOccurrences(of: "\r\n", with: "\n")
            .split(separator: "\n", omittingEmptySubsequences: false)
            .map(String.init)
        guard let highlightsHeading = lines.firstIndex(where: {
            $0.trimmingCharacters(in: .whitespaces)
                .caseInsensitiveCompare("## Highlights") == .orderedSame
        }) else { return nil }

        let leadingLines = lines[..<highlightsHeading]
        guard let summaryStart = leadingLines.firstIndex(where: {
            !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }) else { return nil }

        var summaryLines = [String]()
        for line in leadingLines[summaryStart...] {
            let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
            if trimmed.isEmpty { break }
            guard !trimmed.hasPrefix("#") else { return nil }
            summaryLines.append(trimmed)
        }
        let summary = limited(summaryLines.joined(separator: " "), to: maximumSummaryCharacters)
        guard !summary.isEmpty else { return nil }

        var highlights = [String]()
        for line in lines.dropFirst(highlightsHeading + 1) {
            let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
            if trimmed.hasPrefix("## ") { break }
            guard trimmed.hasPrefix("- ") || trimmed.hasPrefix("* ") else { continue }
            let text = limited(
                String(trimmed.dropFirst(2)).trimmingCharacters(in: .whitespacesAndNewlines),
                to: maximumHighlightCharacters
            )
            if !text.isEmpty { highlights.append(text) }
            if highlights.count == maximumHighlights { break }
        }
        guard !highlights.isEmpty else { return nil }

        return ReleaseNotes(
            version: version.description,
            summary: summary,
            highlights: highlights
        )
    }

    private static func limited(_ value: String, to maximum: Int) -> String {
        guard value.count > maximum else { return value }
        return String(value.prefix(maximum - 1)) + "…"
    }
}

struct ReleaseNotesStore: Sendable {
    let fileURL: URL

    func load(currentVersion: String) -> ReleaseNotes? {
        guard let data = try? Data(contentsOf: fileURL),
              data.count <= 64 * 1024,
              let notes = try? JSONDecoder().decode(ReleaseNotes.self, from: data),
              notes.version == currentVersion,
              !notes.summary.isEmpty,
              (1...ReleaseNotesParser.maximumHighlights).contains(notes.highlights.count)
        else { return nil }
        return notes
    }

    func save(_ notes: ReleaseNotes) throws {
        try FileManager.default.createDirectory(
            at: fileURL.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        try encoder.encode(notes).write(to: fileURL, options: .atomic)
    }

    func remove(version: String) {
        guard let data = try? Data(contentsOf: fileURL),
              let notes = try? JSONDecoder().decode(ReleaseNotes.self, from: data),
              notes.version == version
        else { return }
        try? FileManager.default.removeItem(at: fileURL)
    }
}

enum AppUpdatePolicy {
    static func newerRelease(
        currentVersion: String,
        latestTag: String,
        releaseNotes: ReleaseNotes? = nil
    ) -> AppRelease? {
        guard let current = SemanticVersion(currentVersion),
              let latest = SemanticVersion(latestTag),
              latestTag == "v\(latest)",
              latest > current else {
            return nil
        }
        return AppRelease(tag: "v\(latest)", version: latest, releaseNotes: releaseNotes)
    }
}

enum UpdateIntegrity {
    static func verifyInstaller(_ installer: Data, checksums: Data) throws {
        guard let contents = String(data: checksums, encoding: .utf8),
              let expected = checksum(named: "install.sh", in: contents) else {
            throw AppUpdateError.missingInstallerChecksum
        }
        let actual = SHA256.hash(data: installer)
            .map { String(format: "%02x", $0) }
            .joined()
        guard actual.caseInsensitiveCompare(expected) == .orderedSame else {
            throw AppUpdateError.installerChecksumMismatch
        }
    }

    private static func checksum(named name: String, in contents: String) -> String? {
        for line in contents.split(whereSeparator: \.isNewline) {
            let fields = line.split(whereSeparator: \.isWhitespace)
            guard fields.count == 2, fields[1] == Substring(name) else { continue }
            let checksum = String(fields[0])
            guard checksum.count == SHA256.byteCount * 2,
                  checksum.allSatisfy({ $0.isHexDigit }) else {
                return nil
            }
            return checksum
        }
        return nil
    }
}

enum AppUpdateError: LocalizedError {
    case invalidServerResponse
    case responseTooLarge
    case missingInstallerChecksum
    case installerChecksumMismatch
    case unsupportedInstallLocation
    case cannotCreateInstaller

    var errorDescription: String? {
        switch self {
        case .invalidServerResponse:
            "GitHub returned an invalid update response."
        case .responseTooLarge:
            "GitHub returned an unexpectedly large update response."
        case .missingInstallerChecksum:
            "The release does not include a valid installer checksum."
        case .installerChecksumMismatch:
            "The update installer did not match its published checksum."
        case .unsupportedInstallLocation:
            "This copy of UsageTracker cannot be updated in place. Move it to a writable Applications folder and try again."
        case .cannotCreateInstaller:
            "The update installer could not be prepared."
        }
    }
}

private struct GitHubReleaseResponse: Decodable {
    let tagName: String
    let draft: Bool
    let prerelease: Bool
    let body: String?

    enum CodingKeys: String, CodingKey {
        case tagName = "tag_name"
        case draft, prerelease, body
    }
}

@MainActor final class AppUpdater: ObservableObject {
    private struct Installation {
        let bundleURL: URL
        let currentVersion: String
    }

    typealias Downloader = @Sendable (URLRequest, Int) async throws -> Data

    @Published private(set) var availableRelease: AppRelease?
    @Published private(set) var installedReleaseNotes: ReleaseNotes?
    @Published private(set) var isInstalling = false
    @Published private(set) var installError: String?

    private static let repository = "joeymalvinni/usagetracker"
    private static let bundleIdentifier = "app.usagetracker"
    private static let maximumMetadataBytes = 1_000_000
    private static let checkInterval: TimeInterval = 60 * 60
    private static let installFailureMessage = "Update failed while installing the new app."

    private let installation: Installation?
    private let downloader: Downloader
    private let isWritable: @Sendable (String) -> Bool
    private let releaseNotesStore: ReleaseNotesStore?
    private var lastCheckedAt: Date?
    private var isChecking = false
    private var installerProcess: Process?

    convenience init(bundle: Bundle = .main) {
        let bundleURL = bundle.bundleURL.standardizedFileURL
        let version = bundle.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        let installation: Installation?
        if bundle.bundleIdentifier == Self.bundleIdentifier,
           bundleURL.pathExtension == "app",
           bundleURL.lastPathComponent == "UsageTracker.app",
           let version,
           SemanticVersion(version) != nil {
            installation = Installation(bundleURL: bundleURL, currentVersion: version)
        } else {
            installation = nil
        }
        self.init(
            installation: installation,
            downloader: { try await Self.download($0, maximumBytes: $1) },
            isWritable: { FileManager.default.isWritableFile(atPath: $0) },
            releaseNotesStore: ReleaseNotesStore(
                fileURL: UIPaths.ui.appending(path: "pending-release-notes.json")
            )
        )
    }

    convenience init(
        bundleURL: URL,
        currentVersion: String,
        downloader: @escaping Downloader,
        isWritable: @escaping @Sendable (String) -> Bool = { _ in true },
        releaseNotesURL: URL? = nil
    ) {
        self.init(
            installation: Installation(bundleURL: bundleURL, currentVersion: currentVersion),
            downloader: downloader,
            isWritable: isWritable,
            releaseNotesStore: releaseNotesURL.map(ReleaseNotesStore.init(fileURL:))
        )
    }

    private init(
        installation: Installation?,
        downloader: @escaping Downloader,
        isWritable: @escaping @Sendable (String) -> Bool,
        releaseNotesStore: ReleaseNotesStore?
    ) {
        self.installation = installation
        self.downloader = downloader
        self.isWritable = isWritable
        self.releaseNotesStore = releaseNotesStore
        self.installedReleaseNotes = installation.flatMap {
            releaseNotesStore?.load(currentVersion: $0.currentVersion)
        }
        consumeInstallFailure()
    }

    var currentVersion: String? { installation?.currentVersion }

    func checkForUpdates() async {
        guard let installation, !isChecking, !isInstalling else { return }
        if let lastCheckedAt,
           Date().timeIntervalSince(lastCheckedAt) < Self.checkInterval {
            return
        }

        isChecking = true
        defer { isChecking = false }
        do {
            let request = Self.request(
                url: URL(string: "https://api.github.com/repos/\(Self.repository)/releases/latest")!,
                timeout: 10,
                userAgent: "UsageTracker/\(installation.currentVersion)",
                accept: "application/vnd.github+json"
            )
            let data = try await downloader(request, Self.maximumMetadataBytes)
            let release = try JSONDecoder().decode(GitHubReleaseResponse.self, from: data)
            lastCheckedAt = Date()
            guard !release.draft, !release.prerelease else { return }
            let latestVersion = SemanticVersion(release.tagName)
            let releaseNotes = latestVersion.flatMap {
                ReleaseNotesParser.parse(body: release.body, version: $0)
            }
            if latestVersion == SemanticVersion(installation.currentVersion),
               let releaseNotes {
                installedReleaseNotes = releaseNotes
            }
            availableRelease = AppUpdatePolicy.newerRelease(
                currentVersion: installation.currentVersion,
                latestTag: release.tagName,
                releaseNotes: releaseNotes
            )
        } catch {
            // Update checks are opportunistic. A network or GitHub failure should
            // not add an error banner to an otherwise healthy usage dashboard.
        }
    }

    func installAvailableUpdate() async {
        guard let installation, let release = availableRelease, !isInstalling else { return }
        let appDirectory = installation.bundleURL.deletingLastPathComponent()
        guard isWritable(appDirectory.path) else {
            installError = AppUpdateError.unsupportedInstallLocation.localizedDescription
            return
        }

        isInstalling = true
        installError = nil
        removeInstallFailure()
        do {
            let base = "https://github.com/\(Self.repository)/releases/download/\(release.tag)"
            let userAgent = "UsageTracker/\(installation.currentVersion) updater"
            async let installer = downloader(
                Self.request(
                    url: URL(string: "\(base)/install.sh")!,
                    timeout: 30,
                    userAgent: userAgent
                ),
                Self.maximumMetadataBytes
            )
            async let checksums = downloader(
                Self.request(
                    url: URL(string: "\(base)/SHA256SUMS")!,
                    timeout: 30,
                    userAgent: userAgent
                ),
                Self.maximumMetadataBytes
            )
            let (installerData, checksumData) = try await (installer, checksums)
            try UpdateIntegrity.verifyInstaller(installerData, checksums: checksumData)
            if let releaseNotes = release.releaseNotes {
                try? releaseNotesStore?.save(releaseNotes)
            }

            let directory = FileManager.default.temporaryDirectory
                .appending(path: "usagetracker-update-\(UUID().uuidString)", directoryHint: .isDirectory)
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            let installerURL = directory.appending(path: "install.sh")
            do {
                try installerData.write(to: installerURL, options: [.atomic, .completeFileProtectionUnlessOpen])
            } catch {
                try? FileManager.default.removeItem(at: directory)
                throw AppUpdateError.cannotCreateInstaller
            }
            try launchInstaller(
                at: installerURL,
                directory: directory,
                release: release,
                appDirectory: appDirectory
            )
        } catch {
            releaseNotesStore?.remove(version: release.version.description)
            isInstalling = false
            installError = "Update failed: \(error.localizedDescription)"
        }
    }

    private static func request(
        url: URL,
        timeout: TimeInterval,
        userAgent: String,
        accept: String? = nil
    ) -> URLRequest {
        var request = URLRequest(url: url, timeoutInterval: timeout)
        request.setValue(userAgent, forHTTPHeaderField: "User-Agent")
        if let accept { request.setValue(accept, forHTTPHeaderField: "Accept") }
        return request
    }

    private static func download(_ request: URLRequest, maximumBytes: Int) async throws -> Data {
        let (bytes, response) = try await URLSession.shared.bytes(for: request)
        guard let response = response as? HTTPURLResponse,
              (200..<300).contains(response.statusCode) else {
            throw AppUpdateError.invalidServerResponse
        }
        guard response.expectedContentLength <= maximumBytes else {
            throw AppUpdateError.responseTooLarge
        }
        var data = Data()
        data.reserveCapacity(max(0, Int(response.expectedContentLength)))
        for try await byte in bytes {
            guard data.count < maximumBytes else { throw AppUpdateError.responseTooLarge }
            data.append(byte)
        }
        return data
    }

    private func launchInstaller(
        at installerURL: URL,
        directory: URL,
        release: AppRelease,
        appDirectory: URL
    ) throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/bash")
        process.arguments = [
            "-c",
            """
            installer="$1"
            directory="$2"
            shift 2
            /bin/bash "$installer" "$@"
            status=$?
            /bin/rm -rf -- "$directory"
            exit "$status"
            """,
            "usagetracker-updater",
            installerURL.path,
            directory.path,
            "--version", release.tag,
            "--app-only",
            "--app-dir", appDirectory.path,
        ]
        var environment = ProcessInfo.processInfo.environment
        environment["USAGETRACKER_REPOSITORY"] = Self.repository
        environment["USAGETRACKER_UPDATE_STATUS_FILE"] = installFailureURL?.path
        process.environment = environment
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        process.terminationHandler = { [weak self] process in
            let status = process.terminationStatus
            Task { @MainActor [weak self] in
                guard let self else { return }
                self.installerProcess = nil
                self.isInstalling = false
                if status == 0 {
                    self.availableRelease = nil
                    self.installError = nil
                } else {
                    self.releaseNotesStore?.remove(version: release.version.description)
                    self.removeInstallFailure()
                    self.installError = Self.installFailureMessage
                }
            }
        }
        do {
            try process.run()
        } catch {
            try? FileManager.default.removeItem(at: directory)
            throw error
        }
        installerProcess = process
    }

    func dismissInstalledReleaseNotes(version: String) {
        if installedReleaseNotes?.version == version {
            installedReleaseNotes = nil
        }
        releaseNotesStore?.remove(version: version)
    }

    private var installFailureURL: URL? {
        installation?.bundleURL.deletingLastPathComponent()
            .appending(path: ".UsageTracker.update-failed")
    }

    private func consumeInstallFailure() {
        guard let installFailureURL,
              FileManager.default.fileExists(atPath: installFailureURL.path) else { return }
        removeInstallFailure()
        installError = Self.installFailureMessage
    }

    private func removeInstallFailure() {
        guard let installFailureURL else { return }
        try? FileManager.default.removeItem(at: installFailureURL)
    }
}
