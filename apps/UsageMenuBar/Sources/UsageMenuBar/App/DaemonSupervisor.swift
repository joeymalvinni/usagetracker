import Foundation

final class DaemonSupervisor {
    private var process: Process?
    private var didAttemptStart = false

    func ensureRunning(socketPath: String) async -> Bool {
        if Socket.canConnect(path: socketPath, timeout: 1) { return true }
        guard !didAttemptStart else { return false }
        didAttemptStart = true
        guard let executable = findDaemonExecutable() else { return false }

        do {
            try FileManager.default.createDirectory(at: UIPaths.root, withIntermediateDirectories: true)
            let log = UIPaths.root.appending(path: "usage-daemon.log")
            FileManager.default.createFile(atPath: log.path, contents: nil)
            let output = try FileHandle(forWritingTo: log)
            try output.seekToEnd()

            let process = Process()
            process.executableURL = executable
            process.arguments = ["--foreground", "--socket-path", socketPath]
            process.standardOutput = output
            process.standardError = output
            try process.run()
            self.process = process
        } catch {
            return false
        }

        for _ in 0..<30 {
            if Socket.canConnect(path: socketPath, timeout: 1) { return true }
            try? await Task.sleep(for: .milliseconds(250))
        }
        return false
    }

    private func findDaemonExecutable() -> URL? {
        let env = ProcessInfo.processInfo.environment
        let fm = FileManager.default
        if let override = env["USAGE_TRACKER_DAEMON"], fm.isExecutableFile(atPath: override) {
            return URL(fileURLWithPath: override)
        }

        let roots = candidateRoots()
        for root in roots {
            let candidate = root.appending(path: "target/debug/usage-daemon")
            if fm.isExecutableFile(atPath: candidate.path) { return candidate }
        }
        return nil
    }

    private func candidateRoots() -> [URL] {
        let fm = FileManager.default
        var roots = [URL(fileURLWithPath: fm.currentDirectoryPath)]
        if let pwd = ProcessInfo.processInfo.environment["PWD"] {
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
