import Foundation
import ServiceManagement

let daemonLaunchAgentLabel = "engineering.super.usagetracker.daemon"
let daemonLaunchAgentPlistName = "\(daemonLaunchAgentLabel).plist"

enum DaemonLaunchAgentRegistrationStatus: Equatable, Sendable {
    case notRegistered
    case enabled
    case requiresApproval
    case notFound
}

protocol DaemonLaunchAgentControlling: Sendable {
    var isAvailable: Bool { get }
    func registrationStatus() -> DaemonLaunchAgentRegistrationStatus
    func register() throws
    func unregisterIfNeeded() async throws
}

struct SystemDaemonLaunchAgentController: DaemonLaunchAgentControlling, @unchecked Sendable {
    private let environment: [String: String]
    private let bundle: Bundle

    init(
        environment: [String: String] = ProcessInfo.processInfo.environment,
        bundle: Bundle = .main
    ) {
        self.environment = environment
        self.bundle = bundle
    }

    var isAvailable: Bool {
        guard Self.supportsLaunchAgentEnvironment(environment),
              bundle.bundleIdentifier == "engineering.super.usagetracker",
              bundle.bundleURL.pathExtension == "app" else {
            return false
        }
        return FileManager.default.fileExists(
            atPath: bundle.bundleURL
                .appending(path: "Contents/Library/LaunchAgents")
                .appending(path: daemonLaunchAgentPlistName)
                .path
        )
    }

    static func supportsLaunchAgentEnvironment(_ environment: [String: String]) -> Bool {
        let daemonOverrides = [
            "USAGE_TRACKER_HOME",
            "USAGE_TRACKER_CONFIG",
            "USAGE_TRACKER_DB",
            "USAGE_TRACKER_SOCKET",
            "USAGE_TRACKER_LOG_LEVEL",
            "USAGE_TRACKER_POLL_INTERVAL_SECONDS",
            "USAGE_TRACKER_OPENCODE_GO_COOKIE",
            "USAGE_TRACKER_OPENCODE_COOKIE",
            "USAGE_TRACKER_OPENCODE_GO_WORKSPACE_ID",
            "USAGE_TRACKER_GROK_COOKIE",
            "USAGE_TRACKER_ALLOW_BROWSER_COOKIE_IMPORT",
            "RUST_LOG",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "GROK_HOME",
            "GROK_CLI_PATH",
            "XAI_API_KEY",
        ]
        return environment["USAGE_TRACKER_DAEMON"] == nil
            && environment["USAGE_TRACKER_FIXTURE"]?.isEmpty != false
            && !daemonOverrides.contains(where: { environment[$0]?.isEmpty == false })
    }

    private var service: SMAppService {
        SMAppService.agent(plistName: daemonLaunchAgentPlistName)
    }

    func registrationStatus() -> DaemonLaunchAgentRegistrationStatus {
        switch service.status {
        case .notRegistered: .notRegistered
        case .enabled: .enabled
        case .requiresApproval: .requiresApproval
        case .notFound: .notFound
        @unknown default: .notFound
        }
    }

    func register() throws {
        let status = registrationStatus()
        if status == .enabled { return }
        if status == .requiresApproval {
            throw DaemonLaunchAgentError.requiresApproval
        }
        try service.register()
    }

    func unregisterIfNeeded() async throws {
        switch registrationStatus() {
        case .notRegistered, .notFound:
            return
        case .requiresApproval:
            throw DaemonLaunchAgentError.requiresApproval
        case .enabled:
            break
        }
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let completion = DaemonUnregistrationCompletion(continuation)
            service.unregister { error in
                completion.finish(error: error)
            }
            DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + 20) {
                completion.finish(error: DaemonLaunchAgentError.operationTimedOut)
            }
        }
    }
}

private final class DaemonUnregistrationCompletion: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Void, Error>?

    init(_ continuation: CheckedContinuation<Void, Error>) {
        self.continuation = continuation
    }

    func finish(error: Error?) {
        let pending = lock.withLock {
            defer { continuation = nil }
            return continuation
        }
        guard let pending else { return }
        if let error {
            pending.resume(throwing: error)
        } else {
            pending.resume(returning: ())
        }
    }
}

enum DaemonLaunchAgentError: LocalizedError {
    case operationTimedOut
    case requiresApproval

    var errorDescription: String? {
        switch self {
        case .operationTimedOut:
            "macOS did not finish updating the background service in time."
        case .requiresApproval:
            "UsageTracker's background service requires approval in System Settings → General → Login Items."
        }
    }
}

enum DaemonLaunchAgentCommand {
    static let prepareUpdateArgument = "--prepare-daemon-agent-update"
    static let unregisterArgument = "--unregister-daemon-agent"
    static let reconcileArgument = "--reconcile-daemon-agent"

    /// Handles installer and uninstaller lifecycle commands before AppKit starts.
    /// Returns an exit status when a command was present, or nil for a normal app launch.
    static func runIfRequested(arguments: [String] = CommandLine.arguments) -> Int32? {
        guard arguments.contains(prepareUpdateArgument)
                || arguments.contains(unregisterArgument)
                || arguments.contains(reconcileArgument) else {
            return nil
        }

        let controller = SystemDaemonLaunchAgentController()
        guard controller.isAvailable else { return 0 }

        if arguments.contains(prepareUpdateArgument),
           controller.registrationStatus() == .requiresApproval {
            // Preserve the user's explicit disabled state across an update.
            // There is no running service to stop, and the relative executable
            // path remains valid when the bundle is replaced in place.
            return 3
        }

        if arguments.contains(prepareUpdateArgument) || arguments.contains(unregisterArgument) {
            do {
                let wasEnabled = controller.registrationStatus() == .enabled
                try unregisterAndWaitIfNeeded()
                return arguments.contains(prepareUpdateArgument) && wasEnabled ? 4 : 0
            } catch {
                fputs("Could not unregister UsageTracker background service: \(error)\n", stderr)
                return 1
            }
        }

        do {
            // A clean installation must not start collectors before onboarding
            // has explained their Keychain access. Existing installations can
            // resume the service immediately after an update.
            guard try UIConfig.load().onboardingCompleted else { return 0 }
            if controller.registrationStatus() == .enabled {
                // ServiceManagement requires a full unregister/register cycle
                // whenever an update replaces the helper executable.
                try unregisterAndWaitIfNeeded()
            }
            try controller.register()
            return 0
        } catch {
            fputs("Could not register UsageTracker background service: \(error)\n", stderr)
            return 1
        }
    }

    private static func unregisterAndWaitIfNeeded() throws {
        let service = SMAppService.agent(plistName: daemonLaunchAgentPlistName)
        switch service.status {
        case .notRegistered, .notFound:
            return
        case .enabled, .requiresApproval:
            break
        @unknown default:
            return
        }

        let semaphore = DispatchSemaphore(value: 0)
        let lock = NSLock()
        var commandError: Error?
        service.unregister { error in
            lock.lock()
            commandError = error
            lock.unlock()
            semaphore.signal()
        }
        guard semaphore.wait(timeout: .now() + 20) == .success else {
            throw DaemonLaunchAgentError.operationTimedOut
        }
        lock.lock()
        defer { lock.unlock() }
        if let commandError { throw commandError }
    }
}
