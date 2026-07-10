import Darwin
import Foundation

final class DaemonSupervisor {
    private struct ExecutableIdentity: Equatable {
        let device: UInt64
        let inode: UInt64
        let size: UInt64
    }

    private var process: Process?
    private var didAttemptStart = false

    func ensureRunning(socketPath: String) async -> Bool {
        if Socket.canConnect(path: socketPath, timeout: 1) {
            if shouldReplaceBundledDaemon(listeningOn: socketPath) {
                return await restart(socketPath: socketPath)
            }
            return true
        }
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
        guard !Socket.canConnect(path: socketPath, timeout: 0.2) else { return false }
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

        if let bundled = bundledDaemonExecutable() { return bundled }

        let roots = candidateRoots()
        for root in roots {
            let candidate = root.appending(path: "target/debug/usage-daemon")
            if fm.isExecutableFile(atPath: candidate.path) { return candidate }
        }
        return nil
    }

    /// The daemon intentionally survives when the menu app exits. A later app
    /// build may replace the executable at the same path while the old vnode
    /// remains mapped by the running process. Comparing paths would call those
    /// binaries equal, so compare the mapped device/inode/size reported by
    /// `lsof` with the executable currently embedded in this app bundle.
    private func shouldReplaceBundledDaemon(listeningOn socketPath: String) -> Bool {
        guard ProcessInfo.processInfo.environment["USAGE_TRACKER_DAEMON"] == nil,
              let bundled = bundledDaemonExecutable(),
              let expected = fileIdentity(at: bundled) else {
            return false
        }

        return daemonPIDs(listeningOn: socketPath).contains { pid in
            guard let running = mappedExecutableIdentity(for: pid) else { return false }
            return running != expected
        }
    }

    private func bundledDaemonExecutable() -> URL? {
        guard Bundle.main.bundleURL.pathExtension == "app" else { return nil }
        let fm = FileManager.default
        if let bundled = Bundle.main.url(forAuxiliaryExecutable: "usage-daemon"),
           fm.isExecutableFile(atPath: bundled.path) {
            return bundled
        }
        let bundled = Bundle.main.bundleURL.appending(path: "Contents/MacOS/usage-daemon")
        return fm.isExecutableFile(atPath: bundled.path) ? bundled : nil
    }

    private func fileIdentity(at url: URL) -> ExecutableIdentity? {
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
              let device = attributes[.systemNumber] as? NSNumber,
              let inode = attributes[.systemFileNumber] as? NSNumber,
              let size = attributes[.size] as? NSNumber else {
            return nil
        }
        return ExecutableIdentity(
            device: device.uint64Value,
            inode: inode.uint64Value,
            size: size.uint64Value
        )
    }

    private func mappedExecutableIdentity(for pid: pid_t) -> ExecutableIdentity? {
        let process = Process()
        let output = Pipe()
        process.executableURL = URL(fileURLWithPath: "/usr/sbin/lsof")
        process.arguments = ["-a", "-p", String(pid), "-d", "txt", "-F", "fDsin"]
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
            process.waitUntilExit()
        } catch {
            return nil
        }

        let data = output.fileHandleForReading.readDataToEndOfFile()
        let lines = String(decoding: data, as: UTF8.self).split(whereSeparator: \.isNewline)
        var device: UInt64?
        var inode: UInt64?
        var size: UInt64?
        var name: String?

        func currentIdentity() -> ExecutableIdentity? {
            guard name.map({ URL(fileURLWithPath: $0).lastPathComponent }) == "usage-daemon",
                  let device, let inode, let size else {
                return nil
            }
            return ExecutableIdentity(device: device, inode: inode, size: size)
        }

        for line in lines {
            guard let field = line.first else { continue }
            let value = String(line.dropFirst())
            if field == "f" {
                if let identity = currentIdentity() { return identity }
                device = nil
                inode = nil
                size = nil
                name = nil
            } else if field == "D" {
                device = parseLsofNumber(value)
            } else if field == "i" {
                inode = parseLsofNumber(value)
            } else if field == "s" {
                size = parseLsofNumber(value)
            } else if field == "n" {
                name = value
            }
        }
        return currentIdentity()
    }

    private func parseLsofNumber(_ value: String) -> UInt64? {
        if value.hasPrefix("0x") {
            return UInt64(value.dropFirst(2), radix: 16)
        }
        return UInt64(value)
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
