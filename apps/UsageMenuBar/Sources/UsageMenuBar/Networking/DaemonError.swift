import Foundation

enum DaemonError: LocalizedError, Equatable, Sendable {
    case api(code: String, message: String)
    case badResponse
    case closed
    case timeout
    case refreshFailed(jobId: String, message: String)
    case transport(Int32)
    case pathTooLong(String, Int)

    var errorDescription: String? {
        switch self {
        case .api(_, let message): message
        case .badResponse: "Unexpected daemon response"
        case .closed: "Daemon closed the connection"
        case .timeout: "Daemon request timed out"
        case .refreshFailed(_, let message): message
        case .transport(let code): String(cString: strerror(code))
        case .pathTooLong(let path, let maxBytes): "Unix socket path is too long (\(path.utf8.count) bytes, max \(maxBytes)): \(path)"
        }
    }
}
