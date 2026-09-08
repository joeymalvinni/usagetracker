import Combine
import Foundation

enum ProviderConnectionState: Equatable, Sendable {
    case idle
    case connecting
    case connected
    case waitingForSignIn
    case needsSignIn
    case needsPermission
    case failed
}

struct ProviderConnectionPresentation: Equatable, Sendable {
    let state: ProviderConnectionState
    let message: String
}

/// Owns only transient provider-connection state. Stable state is derived from
/// daemon accounts, health, and provider descriptors so it cannot drift from
/// the source of truth.
@MainActor final class ProviderConnectionCoordinator: ObservableObject {
    @Published private var overrides = [String: ProviderConnectionPresentation]()
    @Published private var monitors = [String: Monitor]()

    private struct Monitor {
        let generation: UUID
        let task: Task<Void, Never>
    }

    func presentation(
        for providerId: String,
        descriptor: ServerProviderDescriptor?,
        accounts: [Account],
        health: [ProviderHealth]
    ) -> ProviderConnectionPresentation {
        if let override = overrides[providerId],
           override.state == .connecting || override.state == .waitingForSignIn {
            return override
        }

        if !accounts.isEmpty {
            if health.contains(where: { $0.status == .keychainAccessFailed }) {
                return ProviderConnectionPresentation(
                    state: .needsPermission,
                    message: "The account is saved, but macOS credential access is needed."
                )
            }
            if health.contains(where: {
                $0.status == .credentialsMissing || $0.status == .authFailed
            }) {
                return ProviderConnectionPresentation(
                    state: .needsSignIn,
                    message: "The saved account needs to sign in again."
                )
            }
        }

        if let override = overrides[providerId] {
            return override
        }

        if !accounts.isEmpty {
            let count = accounts.count
            return ProviderConnectionPresentation(
                state: .connected,
                message: count == 1 ? "1 account connected" : "\(count) accounts connected"
            )
        }

        return ProviderConnectionPresentation(
            state: .idle,
            message: descriptor?.detected == true
                ? "Found on this Mac"
                : "Available to connect"
        )
    }

    func set(
        _ state: ProviderConnectionState,
        message: String,
        for providerId: String
    ) {
        precondition(state != .idle && state != .connected)
        overrides[providerId] = ProviderConnectionPresentation(
            state: state,
            message: message
        )
    }

    func clearOverride(for providerId: String) {
        overrides.removeValue(forKey: providerId)
    }

    func monitor(
        providerId: String,
        operation: @escaping @MainActor @Sendable () async -> Void
    ) {
        let generation = UUID()
        monitors[providerId]?.task.cancel()
        let task = Task { [weak self] in
            await operation()
            self?.finishMonitor(providerId: providerId, generation: generation)
        }
        monitors[providerId] = Monitor(generation: generation, task: task)
    }

    func cancelMonitor(for providerId: String) {
        monitors.removeValue(forKey: providerId)?.task.cancel()
    }

    func reset() {
        for monitor in monitors.values {
            monitor.task.cancel()
        }
        monitors.removeAll()
        overrides.removeAll()
    }

    func isMonitoring(_ providerId: String) -> Bool {
        monitors[providerId] != nil
    }

    private func finishMonitor(providerId: String, generation: UUID) {
        guard monitors[providerId]?.generation == generation else { return }
        monitors.removeValue(forKey: providerId)
    }
}
