import Foundation

public enum ProviderHTTPError: Error, CustomStringConvertible {
    case invalidURL(String)
    case status(Int, body: String)

    public var description: String {
        switch self {
        case let .invalidURL(path):
            "invalid URL: \(path)"
        case let .status(status, body):
            "HTTP \(status): \(body.prefix(240))"
        }
    }
}
