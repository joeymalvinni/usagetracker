import Foundation

/// Collection problems never determine quota colors or quota alert acknowledgements.
enum ProviderCollectionIssue: Equatable, Sendable {
    case permissionRequired, accountUnavailable, invalidCredentials, signInRequired
    case rateLimited, temporarilyUnavailable, responseChanged

    init?(health: ProviderHealth) {
        switch health.status {
        case .ok, .disabled: return nil
        case .keychainAccessFailed: self = .permissionRequired
        case .credentialsMissing: self = .accountUnavailable
        case .authFailed:
            self = health.lastErrorCode == "unauthorized" ? .signInRequired : .invalidCredentials
        case .rateLimited, .backingOff: self = .rateLimited
        case .parseError: self = .responseChanged
        case .providerError, .other: self = .temporarilyUnavailable
        }
    }

    var summary: String {
        switch self {
        case .permissionRequired: "Credential access needed"
        case .accountUnavailable: "Account unavailable"
        case .invalidCredentials: "Connection needs checking"
        case .signInRequired: "Reconnect to update usage"
        case .rateLimited: "Waiting to update"
        case .temporarilyUnavailable: "Updates temporarily unavailable"
        case .responseChanged: "Usage update unavailable"
        }
    }

    var explanation: String {
        switch self {
        case .permissionRequired:
            "Allow UsageTracker to read your existing sign-in. macOS may ask for permission."
        case .accountUnavailable:
            "An existing sign-in could not be found for this account."
        case .invalidCredentials:
            "UsageTracker could not read this account’s sign-in. Check the connection before signing in again."
        case .signInRequired:
            "The provider rejected this account’s sign-in. Reconnect to resume updates."
        case .rateLimited:
            "The provider asked us to wait. Usage will update automatically when it allows another request."
        case .temporarilyUnavailable:
            "UsageTracker will try again automatically."
        case .responseChanged:
            "This version could not read the provider’s latest usage response."
        }
    }

    var recoveryAction: ProviderRecoveryAction? {
        switch self {
        case .permissionRequired: .allowAccess
        case .accountUnavailable: .connect
        case .invalidCredentials: .checkConnection
        case .signInRequired: .reconnect
        case .rateLimited, .temporarilyUnavailable, .responseChanged: nil
        }
    }
}

enum ProviderRecoveryAction: Equatable, Sendable {
    case allowAccess, checkConnection, connect, reconnect

    var label: String {
        switch self {
        case .allowAccess: "Allow access"
        case .checkConnection: "Check connection"
        case .connect: "Connect account"
        case .reconnect: "Reconnect"
        }
    }
}
