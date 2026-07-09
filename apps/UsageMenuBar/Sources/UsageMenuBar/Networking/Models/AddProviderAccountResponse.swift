import Foundation

struct AddProviderAccountResponse: Decodable, Equatable {
    let providerId: String
    let profileId: String
    let displayName: String?
    let profilePath: String
}
