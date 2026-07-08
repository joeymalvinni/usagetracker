import Foundation

enum DaemonError: LocalizedError {
    case api(String), badResponse, closed, timeout, transport(Int32), pathTooLong(String, Int)
    var errorDescription: String? {
        switch self {
        case .api(let s): s
        case .badResponse: "Unexpected daemon response"
        case .closed: "Daemon closed the connection"
        case .timeout: "Daemon request timed out"
        case .transport(let code): String(cString: strerror(code))
        case .pathTooLong(let path, let maxBytes): "Unix socket path is too long (\(path.utf8.count) bytes, max \(maxBytes)): \(path)"
        }
    }
}
