import Foundation
struct Account: Decodable, Identifiable, Equatable {
    let id, providerId, externalAccountId: String
    let displayName: String?
    let createdAt, updatedAt: Date
}