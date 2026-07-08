import Foundation
struct ConfigResponse: Decodable, Equatable {
    let pollIntervalSeconds: UInt64
    let configPath, socketPath, dbPath: String
    let enabledProviders: [String]
    let providers: [String: ProviderToggle]
}

struct ProviderToggle: Codable, Equatable { let enabled: Bool }

struct ApiError: Decodable, Equatable { let code, message: String }