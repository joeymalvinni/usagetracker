import Foundation
struct Account: Decodable, Identifiable, Equatable {
    let id, providerId, externalAccountId: String
    let profileId: String?
    let displayName: String?
    let email: String?
    let hidden: Bool
    let collectionEnabled: Bool
    let createdAt, updatedAt: Date
}
