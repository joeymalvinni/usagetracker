import Darwin
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

    func restart(socketPath: String) async -> Bool {
        if let process, process.isRunning {
            process.terminate()
        } else {
            for pid in daemonPIDs(listeningOn: socketPath) {
                Darwin.kill(pid, SIGTERM)
            }
        }

        for _ in 0..<30 {
            if !Socket.canConnect(path: socketPath, timeout: 0.2) { break }
            try? await Task.sleep(for: .milliseconds(100))
        }
        didAttemptStart = false
        process = nil
        return await ensureRunning(socketPath: socketPath)
    }

    private func findDaemonExecutable() -> URL? {
        let env = ProcessInfo.processInfo.environment
        let fm = FileManager.default
        if let override = env["USAGE_TRACKER_DAEMON"], fm.isExecutableFile(atPath: override) {
            return URL(fileURLWithPath: override)
        }

        if let bundled = Bundle.main.url(forAuxiliaryExecutable: "usage-daemon"),
           fm.isExecutableFile(atPath: bundled.path) {
            return bundled
        }
        let bundled = Bundle.main.bundleURL.appending(path: "Contents/MacOS/usage-daemon")
        if fm.isExecutableFile(atPath: bundled.path) { return bundled }

        let roots = candidateRoots()
        for root in roots {
            let candidate = root.appending(path: "target/debug/usage-daemon")
            if fm.isExecutableFile(atPath: candidate.path) { return candidate }
        }
        return nil
    }

    private func daemonPIDs(listeningOn socketPath: String) -> [pid_t] {
        let process = Process()
        let output = Pipe()
        process.executableURL = URL(fileURLWithPath: "/usr/sbin/lsof")
        process.arguments = ["-t", "--", socketPath]
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
            process.waitUntilExit()
        } catch {
            return []
        }

        let data = output.fileHandleForReading.readDataToEndOfFile()
        let text = String(decoding: data, as: UTF8.self)
        return text
            .split(whereSeparator: \.isNewline)
            .compactMap { pid_t($0) }
            .filter(isUsageDaemon)
    }

    private func isUsageDaemon(_ pid: pid_t) -> Bool {
        let process = Process()
        let output = Pipe()
        process.executableURL = URL(fileURLWithPath: "/bin/ps")
        process.arguments = ["-p", String(pid), "-o", "comm="]
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
            process.waitUntilExit()
        } catch {
            return false
        }
        let data = output.fileHandleForReading.readDataToEndOfFile()
        let command = String(decoding: data, as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return URL(fileURLWithPath: command).lastPathComponent == "usage-daemon"
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
