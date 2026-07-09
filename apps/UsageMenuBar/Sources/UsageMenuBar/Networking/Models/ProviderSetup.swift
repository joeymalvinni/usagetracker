import Foundation

struct ProviderSetupResponse: Decodable, Equatable {
    let providerId: String
    let profiles: [ProviderProfileOption]
    let selectedWorkspaceId: String?
    let workspaceOptions: [String]
    let discoveryError: String?
}

struct ProviderProfileOption: Decodable, Identifiable, Equatable {
    let id: String
    let displayName: String?
    let enabled: Bool

    var label: String { displayName?.isEmpty == false ? displayName! : id }
}

struct ProviderActionResponse: Decodable, Equatable {
    let providerId: String
    let message: String
}
