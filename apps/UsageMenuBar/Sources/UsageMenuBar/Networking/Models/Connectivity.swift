import Foundation

struct Connectivity: Decodable, Equatable, Sendable {
    static let unknown = Connectivity(status: .unknown, changedAt: nil)

    let status: ConnectivityStatus
    let changedAt: Date?
}

enum ConnectivityStatus: Equatable, Decodable, Sendable {
    case online, offline, unknown, other(String)

    init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "online": self = .online
        case "offline": self = .offline
        case "unknown": self = .unknown
        case let value: self = .other(value)
        }
    }
}
