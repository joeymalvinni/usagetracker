import Foundation

protocol DaemonTransport: Sendable {
    func line(path: String, request: String, timeout: TimeInterval) async throws -> String
    func canConnect(path: String, timeout: TimeInterval) async -> Bool
}

/// Runs the blocking POSIX socket implementation on a cooperative worker rather
/// than whichever actor initiated the request. In particular, daemon requests
/// made by `AppState` never pin the main actor while connect/read deadlines run.
struct POSIXDaemonTransport: DaemonTransport {
    func line(path: String, request: String, timeout: TimeInterval) async throws -> String {
        let worker = Task.detached(priority: .userInitiated) {
            try Socket.line(
                path: path,
                request: request,
                timeout: timeout,
                isCancelled: { Task.isCancelled }
            )
        }
        return try await withTaskCancellationHandler {
            try await worker.value
        } onCancel: {
            worker.cancel()
        }
    }

    func canConnect(path: String, timeout: TimeInterval) async -> Bool {
        let worker = Task.detached(priority: .utility) {
            Socket.canConnect(
                path: path,
                timeout: timeout,
                isCancelled: { Task.isCancelled }
            )
        }
        return await withTaskCancellationHandler {
            await worker.value
        } onCancel: {
            worker.cancel()
        }
    }
}
