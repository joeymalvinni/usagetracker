import Darwin
import Foundation

protocol DaemonExecutableLocating: Sendable {
    func executableURL() -> URL?
    func bundledExecutableURL() -> URL?
}

struct SystemDaemonExecutableLocator: DaemonExecutableLocating {
    private let environment: [String: String]

    init(environment: [String: String] = ProcessInfo.processInfo.environment) {
        self.environment = environment
    }

    func executableURL() -> URL? {
        let fileManager = FileManager.default
        if let override = environment["USAGE_TRACKER_DAEMON"],
           fileManager.isExecutableFile(atPath: override) {
            return URL(fileURLWithPath: override)
        }

        if let bundled = bundledExecutableURL() { return bundled }
        for root in candidateRoots() {
            let candidate = root.appending(path: "target/debug/usage-daemon")
            if fileManager.isExecutableFile(atPath: candidate.path) { return candidate }
        }
        return nil
    }

    func bundledExecutableURL() -> URL? {
        guard Bundle.main.bundleURL.pathExtension == "app" else { return nil }
        let fileManager = FileManager.default
        if let bundled = Bundle.main.url(forAuxiliaryExecutable: "usage-daemon"),
           fileManager.isExecutableFile(atPath: bundled.path) {
            return bundled
        }
        let bundled = Bundle.main.bundleURL.appending(path: "Contents/MacOS/usage-daemon")
        return fileManager.isExecutableFile(atPath: bundled.path) ? bundled : nil
    }

    private func candidateRoots() -> [URL] {
        let fileManager = FileManager.default
        var roots = [URL(fileURLWithPath: fileManager.currentDirectoryPath)]
        if let pwd = environment["PWD"] {
            roots.append(URL(fileURLWithPath: pwd))
        }
        roots.append(Bundle.main.bundleURL)

        var expanded = [URL]()
        for root in roots {
            var cursor = root.standardizedFileURL
            for _ in 0..<8 {
                expanded.append(cursor)
                let parent = cursor.deletingLastPathComponent()
                if parent.path == cursor.path { break }
                cursor = parent
            }
        }

        var seen = Set<String>()
        return expanded.filter { seen.insert($0.path).inserted }
    }
}

protocol DaemonProcessHandle: AnyObject, Sendable {
    var processIdentifier: pid_t { get }
    var isRunning: Bool { get }
    func terminate()
    func forceTerminate()
}

protocol DaemonProcessLaunching: Sendable {
    func launch(
        executable: URL,
        arguments: [String],
        logURL: URL,
        terminationHandler: @escaping @Sendable (pid_t) -> Void
    ) throws -> any DaemonProcessHandle
}

struct FoundationDaemonProcessLauncher: DaemonProcessLaunching {
    func launch(
        executable: URL,
        arguments: [String],
        logURL: URL,
        terminationHandler: @escaping @Sendable (pid_t) -> Void
    ) throws -> any DaemonProcessHandle {
        let descriptor = Darwin.open(logURL.path, O_WRONLY | O_CREAT | O_APPEND, S_IRUSR | S_IWUSR)
        guard descriptor >= 0 else { throw DaemonError.transport(errno) }
        let output = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)

        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        process.standardOutput = output
        process.standardError = output

        let handle = FoundationDaemonProcessHandle(process: process, output: output)
        process.terminationHandler = { [weak handle] process in
            handle?.closeOutput()
            terminationHandler(process.processIdentifier)
        }
        do {
            try process.run()
            return handle
        } catch {
            handle.closeOutput()
            throw error
        }
    }
}

private final class FoundationDaemonProcessHandle: DaemonProcessHandle, @unchecked Sendable {
    private let process: Process
    private let output: FileHandle
    private let lock = NSLock()
    private var outputClosed = false

    init(process: Process, output: FileHandle) {
        self.process = process
        self.output = output
    }

    var processIdentifier: pid_t {
        lock.lock()
        defer { lock.unlock() }
        return process.processIdentifier
    }

    var isRunning: Bool {
        lock.lock()
        defer { lock.unlock() }
        return process.isRunning
    }

    func terminate() {
        lock.lock()
        let shouldTerminate = process.isRunning
        lock.unlock()
        if shouldTerminate { process.terminate() }
    }

    func forceTerminate() {
        lock.lock()
        let pid = process.processIdentifier
        let shouldTerminate = process.isRunning
        lock.unlock()
        if shouldTerminate {
            Darwin.kill(pid, SIGKILL)
        }
    }

    func closeOutput() {
        lock.lock()
        guard !outputClosed else {
            lock.unlock()
            return
        }
        outputClosed = true
        lock.unlock()
        try? output.close()
    }

    deinit {
        closeOutput()
    }
}

struct DaemonLogPolicy: Equatable, Sendable {
    var maxBytes: UInt64 = 5 * 1_024 * 1_024
    var retainedArchives = 3
    var checkInterval: TimeInterval = 30
}

struct DaemonLogRotator: Sendable {
    let policy: DaemonLogPolicy

    func prepareForLaunch(at logURL: URL) throws {
        let fileManager = FileManager.default
        try fileManager.createDirectory(
            at: logURL.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        guard fileManager.fileExists(atPath: logURL.path) else {
            guard fileManager.createFile(atPath: logURL.path, contents: nil) else {
                throw CocoaError(.fileWriteUnknown)
            }
            return
        }
        guard try size(of: logURL) >= policy.maxBytes else { return }

        try shiftArchives(for: logURL)
        if policy.retainedArchives > 0 {
            let newest = archiveURL(for: logURL, index: 1)
            try fileManager.moveItem(at: logURL, to: newest)
            try trimToNewestBytes(at: newest)
        } else {
            try fileManager.removeItem(at: logURL)
        }
        guard fileManager.createFile(atPath: logURL.path, contents: nil) else {
            throw CocoaError(.fileWriteUnknown)
        }
    }

    /// The daemon opens its log with O_APPEND, so truncating the live inode is
    /// safe: its next write resumes at the new end instead of recreating a hole.
    func rotateActiveLogIfNeeded(at logURL: URL) throws {
        let fileManager = FileManager.default
        guard fileManager.fileExists(atPath: logURL.path),
              try size(of: logURL) >= policy.maxBytes else { return }

        try shiftArchives(for: logURL)
        if policy.retainedArchives > 0 {
            let newest = archiveURL(for: logURL, index: 1)
            try fileManager.copyItem(at: logURL, to: newest)
            try trimToNewestBytes(at: newest)
        }
        guard Darwin.truncate(logURL.path, 0) == 0 else {
            throw DaemonError.transport(errno)
        }
    }

    private func shiftArchives(for logURL: URL) throws {
        let fileManager = FileManager.default
        guard policy.retainedArchives > 0 else { return }
        let oldest = archiveURL(for: logURL, index: policy.retainedArchives)
        if fileManager.fileExists(atPath: oldest.path) {
            try fileManager.removeItem(at: oldest)
        }
        guard policy.retainedArchives > 1 else { return }
        for index in stride(from: policy.retainedArchives - 1, through: 1, by: -1) {
            let source = archiveURL(for: logURL, index: index)
            guard fileManager.fileExists(atPath: source.path) else { continue }
            try fileManager.moveItem(
                at: source,
                to: archiveURL(for: logURL, index: index + 1)
            )
        }
    }

    private func trimToNewestBytes(at url: URL) throws {
        let fileSize = try size(of: url)
        guard fileSize > policy.maxBytes else { return }
        let file = try FileHandle(forReadingFrom: url)
        defer { try? file.close() }
        try file.seek(toOffset: fileSize - policy.maxBytes)
        let data = try file.readToEnd() ?? Data()
        try data.write(to: url, options: .atomic)
    }

    private func size(of url: URL) throws -> UInt64 {
        let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
        return (attributes[.size] as? NSNumber)?.uint64Value ?? 0
    }

    private func archiveURL(for logURL: URL, index: Int) -> URL {
        URL(fileURLWithPath: "\(logURL.path).\(index)")
    }
}

struct DaemonSupervisorPolicy: Equatable, Sendable {
    var launchAttempts = 2
    var readinessChecks = 30
    var readinessProbeTimeout: TimeInterval = 0.2
    var readinessPollInterval: TimeInterval = 0.25
    var shutdownChecks = 30
    var shutdownProbeTimeout: TimeInterval = 0.2
    var shutdownPollInterval: TimeInterval = 0.1
    var initialBackoff: TimeInterval = 1
    var maximumBackoff: TimeInterval = 30
    var maximumAutomaticRestarts = 3
    var stabilityResetInterval: TimeInterval = 60
}
