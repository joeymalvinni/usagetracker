import Darwin
import Foundation

enum DaemonSupervisorStatus: Equatable, Sendable {
    case stopped
    case starting
    case running
    case backingOff
}

actor DaemonSupervisor {
    private struct ExecutableIdentity: Equatable, Sendable {
        let device: UInt64
        let inode: UInt64
        let size: UInt64
    }

    private struct RunningDaemon {
        let generation: Int
        let process: (any DaemonProcessHandle)?
        let socketPath: String
    }

    private enum State {
        case stopped
        case starting(generation: Int, task: Task<Bool, Never>)
        case running(RunningDaemon)
        case backingOff(until: Date, failures: Int)
    }

    private let transport: any DaemonTransport
    private let executableLocator: any DaemonExecutableLocating
    private let processLauncher: any DaemonProcessLaunching
    private let environment: [String: String]
    private let rootURL: URL
    private let policy: DaemonSupervisorPolicy
    private let logRotator: DaemonLogRotator
    private let now: @Sendable () -> Date
    private let sleep: @Sendable (TimeInterval) async throws -> Void

    private var state = State.stopped
    private var generation = 0
    private var consecutiveFailures = 0
    private var lastSocketPath: String?
    private var recoveryTask: Task<Void, Never>?
    private var logRotationTask: Task<Void, Never>?
    private var stabilityTask: Task<Void, Never>?

    init(
        transport: any DaemonTransport = POSIXDaemonTransport(),
        executableLocator: any DaemonExecutableLocating = SystemDaemonExecutableLocator(),
        processLauncher: any DaemonProcessLaunching = FoundationDaemonProcessLauncher(),
        environment: [String: String] = ProcessInfo.processInfo.environment,
        rootURL: URL = UIPaths.root,
        policy: DaemonSupervisorPolicy = DaemonSupervisorPolicy(),
        logPolicy: DaemonLogPolicy = DaemonLogPolicy(),
        now: @escaping @Sendable () -> Date = { Date() },
        sleep: @escaping @Sendable (TimeInterval) async throws -> Void = { seconds in
            try await Task.sleep(for: .seconds(seconds))
        }
    ) {
        self.transport = transport
        self.executableLocator = executableLocator
        self.processLauncher = processLauncher
        self.environment = environment
        self.rootURL = rootURL
        self.policy = policy
        self.logRotator = DaemonLogRotator(policy: logPolicy)
        self.now = now
        self.sleep = sleep
    }

    func currentStatus() -> DaemonSupervisorStatus {
        switch state {
        case .stopped: .stopped
        case .starting: .starting
        case .running: .running
        case .backingOff: .backingOff
        }
    }

    func ensureRunning(socketPath: String) async -> Bool {
        lastSocketPath = socketPath

        if case .starting(_, let task) = state {
            return await task.value
        }

        if await transport.canConnect(path: socketPath, timeout: 1) {
            if await shouldReplaceBundledDaemon(listeningOn: socketPath) {
                return await restart(socketPath: socketPath)
            }
            consecutiveFailures = 0
            cancelRecovery()
            cancelStabilityMonitoring()
            if case .running(let daemon) = state, daemon.socketPath == socketPath {
                return true
            }
            generation += 1
            state = .running(RunningDaemon(
                generation: generation,
                process: nil,
                socketPath: socketPath
            ))
            return true
        }

        switch state {
        case .running(let daemon):
            if daemon.process?.isRunning == true {
                return await restart(socketPath: socketPath)
            }
            stopLogMonitoring()
            cancelStabilityMonitoring()
            state = .stopped
        case .backingOff(let until, _):
            guard now() >= until else { return false }
            cancelRecovery()
            state = .stopped
        case .stopped:
            break
        case .starting:
            // Handled before probing; retained for exhaustiveness across awaits.
            if case .starting(_, let task) = state { return await task.value }
        }

        return await beginStartup(socketPath: socketPath)
    }

    func restart(socketPath: String) async -> Bool {
        lastSocketPath = socketPath
        generation += 1 // Invalidates callbacks and any in-progress startup.
        cancelRecovery()
        stopLogMonitoring()
        cancelStabilityMonitoring()

        var startupTask: Task<Bool, Never>?
        var ownedProcess: (any DaemonProcessHandle)?
        switch state {
        case .starting(_, let task): startupTask = task
        case .running(let daemon): ownedProcess = daemon.process
        case .stopped, .backingOff: break
        }
        state = .stopped

        startupTask?.cancel()
        if let startupTask { _ = await startupTask.value }

        if let ownedProcess, ownedProcess.isRunning {
            ownedProcess.terminate()
        } else {
            let pids = await Task.detached(priority: .utility) {
                Self.daemonPIDs(listeningOn: socketPath)
            }.value
            for pid in pids { Darwin.kill(pid, SIGTERM) }
        }

        for check in 0..<policy.shutdownChecks {
            let connected = await transport.canConnect(
                path: socketPath,
                timeout: policy.shutdownProbeTimeout
            )
            if !connected { break }
            guard check + 1 < policy.shutdownChecks else {
                enterBackoff()
                return false
            }
            do {
                try await sleep(policy.shutdownPollInterval)
            } catch {
                return false
            }
        }

        return await beginStartup(socketPath: socketPath)
    }

    private func beginStartup(socketPath: String) async -> Bool {
        if case .starting(_, let task) = state { return await task.value }
        cancelRecovery()
        generation += 1
        let startupGeneration = generation
        let task = Task { [weak self] in
            guard let self else { return false }
            return await self.performStartup(
                socketPath: socketPath,
                generation: startupGeneration
            )
        }
        state = .starting(generation: startupGeneration, task: task)
        return await task.value
    }

    private func performStartup(socketPath: String, generation startupGeneration: Int) async -> Bool {
        guard isCurrent(startupGeneration), !Task.isCancelled else { return false }
        guard let executable = executableLocator.executableURL() else {
            enterBackoff()
            return false
        }

        var arguments = ["--foreground", "--socket-path", socketPath]
        if let fixture = environment["USAGE_TRACKER_FIXTURE"], !fixture.isEmpty {
            arguments += ["--fixture", fixture]
        }
        let logURL = rootURL.appending(path: "usage-daemon.log")

        for attempt in 0..<policy.launchAttempts {
            guard isCurrent(startupGeneration), !Task.isCancelled else { return false }
            do {
                try logRotator.prepareForLaunch(at: logURL)
                let process = try processLauncher.launch(
                    executable: executable,
                    arguments: arguments,
                    logURL: logURL,
                    terminationHandler: { [weak self] pid in
                        Task { [weak self] in
                            await self?.daemonTerminated(
                                pid: pid,
                                generation: startupGeneration
                            )
                        }
                    }
                )

                guard isCurrent(startupGeneration), !Task.isCancelled else {
                    process.terminate()
                    return false
                }
                if await waitUntilReady(
                    process: process,
                    socketPath: socketPath,
                    generation: startupGeneration
                ) {
                    guard isCurrent(startupGeneration), !Task.isCancelled else {
                        process.terminate()
                        return false
                    }
                    state = .running(RunningDaemon(
                        generation: startupGeneration,
                        process: process,
                        socketPath: socketPath
                    ))
                    startLogMonitoring(at: logURL, generation: startupGeneration)
                    startStabilityMonitoring(generation: startupGeneration)
                    return true
                }
                process.terminate()
            } catch {
                // A later bounded attempt may recover from a transient launch or log error.
            }

            guard attempt + 1 < policy.launchAttempts else { break }
            do {
                try await sleep(backoffDelay(for: attempt + 1))
            } catch {
                return false
            }
        }

        guard isCurrent(startupGeneration), !Task.isCancelled else { return false }
        enterBackoff()
        return false
    }

    private func waitUntilReady(
        process: any DaemonProcessHandle,
        socketPath: String,
        generation startupGeneration: Int
    ) async -> Bool {
        for check in 0..<policy.readinessChecks {
            guard isCurrent(startupGeneration), !Task.isCancelled else { return false }
            if await transport.canConnect(
                path: socketPath,
                timeout: policy.readinessProbeTimeout
            ) {
                return true
            }
            guard process.isRunning, check + 1 < policy.readinessChecks else { return false }
            do {
                try await sleep(policy.readinessPollInterval)
            } catch {
                return false
            }
        }
        return false
    }

    private func daemonTerminated(pid: pid_t, generation terminatedGeneration: Int) {
        guard case .running(let daemon) = state,
              daemon.generation == terminatedGeneration,
              daemon.process?.processIdentifier == pid else { return }
        stopLogMonitoring()
        cancelStabilityMonitoring()
        enterBackoff()
    }

    private func enterBackoff() {
        stopLogMonitoring()
        cancelStabilityMonitoring()
        consecutiveFailures += 1
        let delay = backoffDelay(for: consecutiveFailures)
        state = .backingOff(
            until: now().addingTimeInterval(delay),
            failures: consecutiveFailures
        )
        scheduleAutomaticRecovery(after: delay, generation: generation)
    }

    private func scheduleAutomaticRecovery(after delay: TimeInterval, generation expectedGeneration: Int) {
        cancelRecovery()
        guard consecutiveFailures <= policy.maximumAutomaticRestarts,
              let socketPath = lastSocketPath else { return }
        let sleep = self.sleep
        recoveryTask = Task { [weak self] in
            do {
                try await sleep(delay)
            } catch {
                return
            }
            guard !Task.isCancelled else { return }
            await self?.resumeAfterBackoff(
                socketPath: socketPath,
                generation: expectedGeneration
            )
        }
    }

    private func resumeAfterBackoff(socketPath: String, generation expectedGeneration: Int) async {
        guard generation == expectedGeneration,
              case .backingOff(let until, _) = state,
              now() >= until else { return }
        recoveryTask = nil
        state = .stopped
        _ = await beginStartup(socketPath: socketPath)
    }

    private func backoffDelay(for failure: Int) -> TimeInterval {
        guard policy.initialBackoff > 0 else { return 0 }
        let exponent = min(max(0, failure - 1), 20)
        return min(
            policy.maximumBackoff,
            policy.initialBackoff * pow(2, Double(exponent))
        )
    }

    private func isCurrent(_ expectedGeneration: Int) -> Bool {
        generation == expectedGeneration
    }

    private func cancelRecovery() {
        recoveryTask?.cancel()
        recoveryTask = nil
    }

    private func startLogMonitoring(at logURL: URL, generation monitoredGeneration: Int) {
        stopLogMonitoring()
        guard logRotator.policy.checkInterval > 0 else { return }
        let interval = logRotator.policy.checkInterval
        let sleep = self.sleep
        logRotationTask = Task { [weak self] in
            while !Task.isCancelled {
                do {
                    try await sleep(interval)
                } catch {
                    return
                }
                guard !Task.isCancelled,
                      let self,
                      await self.rotateLogIfRunning(
                        at: logURL,
                        generation: monitoredGeneration
                      ) else { return }
            }
        }
    }

    private func rotateLogIfRunning(at logURL: URL, generation monitoredGeneration: Int) -> Bool {
        guard case .running(let daemon) = state,
              daemon.generation == monitoredGeneration else { return false }
        try? logRotator.rotateActiveLogIfNeeded(at: logURL)
        return true
    }

    private func stopLogMonitoring() {
        logRotationTask?.cancel()
        logRotationTask = nil
    }

    private func startStabilityMonitoring(generation monitoredGeneration: Int) {
        cancelStabilityMonitoring()
        guard policy.stabilityResetInterval > 0 else {
            consecutiveFailures = 0
            return
        }
        let interval = policy.stabilityResetInterval
        let sleep = self.sleep
        stabilityTask = Task { [weak self] in
            do {
                try await sleep(interval)
            } catch {
                return
            }
            guard !Task.isCancelled else { return }
            await self?.markStable(generation: monitoredGeneration)
        }
    }

    private func markStable(generation monitoredGeneration: Int) {
        guard case .running(let daemon) = state,
              daemon.generation == monitoredGeneration else { return }
        consecutiveFailures = 0
        stabilityTask = nil
    }

    private func cancelStabilityMonitoring() {
        stabilityTask?.cancel()
        stabilityTask = nil
    }

    /// The daemon intentionally survives when the menu app exits. A later app
    /// build may replace the executable at the same path while the old vnode
    /// remains mapped, so compare the mapped identity rather than path strings.
    private func shouldReplaceBundledDaemon(listeningOn socketPath: String) async -> Bool {
        if environment["USAGE_TRACKER_FIXTURE"]?.isEmpty == false { return true }
        guard environment["USAGE_TRACKER_DAEMON"] == nil,
              let bundled = executableLocator.bundledExecutableURL(),
              let expected = Self.fileIdentity(at: bundled) else {
            return false
        }

        return await Task.detached(priority: .utility) {
            Self.daemonPIDs(listeningOn: socketPath).contains { pid in
                guard let running = Self.mappedExecutableIdentity(for: pid) else { return false }
                return running != expected
            }
        }.value
    }

    private static func fileIdentity(at url: URL) -> ExecutableIdentity? {
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

    private static func mappedExecutableIdentity(for pid: pid_t) -> ExecutableIdentity? {
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

    private static func parseLsofNumber(_ value: String) -> UInt64? {
        if value.hasPrefix("0x") {
            return UInt64(value.dropFirst(2), radix: 16)
        }
        return UInt64(value)
    }

    private static func daemonPIDs(listeningOn socketPath: String) -> [pid_t] {
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
        return String(decoding: data, as: UTF8.self)
            .split(whereSeparator: \.isNewline)
            .compactMap { pid_t($0) }
            .filter(isUsageDaemon)
    }

    private static func isUsageDaemon(_ pid: pid_t) -> Bool {
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
        let command = String(
            decoding: output.fileHandleForReading.readDataToEndOfFile(),
            as: UTF8.self
        ).trimmingCharacters(in: .whitespacesAndNewlines)
        return URL(fileURLWithPath: command).lastPathComponent == "usage-daemon"
    }
}
