import Foundation

/// Structured launch flags mirrored from the daemon's `LaunchFlags`.
/// Decoding rides the snake_case-converting decoder; encoding writes explicit
/// snake_case keys because `JSONEncoder.usage` has no key strategy.
struct LaunchFlags: Equatable, Sendable, Codable {
    var model: String?
    var effort: String?
    var dangerouslySkipPermissions: Bool

    init(model: String? = nil, effort: String? = nil, dangerouslySkipPermissions: Bool = false) {
        self.model = model
        self.effort = effort
        self.dangerouslySkipPermissions = dangerouslySkipPermissions
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: DecodeKeys.self)
        model = try c.decodeIfPresent(String.self, forKey: .model)
        effort = try c.decodeIfPresent(String.self, forKey: .effort)
        dangerouslySkipPermissions =
            try c.decodeIfPresent(Bool.self, forKey: .dangerouslySkipPermissions) ?? false
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: EncodeKeys.self)
        try c.encodeIfPresent(model, forKey: .model)
        try c.encodeIfPresent(effort, forKey: .effort)
        if dangerouslySkipPermissions {
            try c.encode(true, forKey: .dangerouslySkipPermissions)
        }
    }

    private enum DecodeKeys: String, CodingKey { case model, effort, dangerouslySkipPermissions }
    private enum EncodeKeys: String, CodingKey {
        case model, effort
        case dangerouslySkipPermissions = "dangerously_skip_permissions"
    }
}

struct AccountLaunchSettingsResponse: Decodable, Equatable, Sendable {
    let providerId: String
    let accountId: String
    let workingDirectory: String?
    let launch: LaunchFlags?
    let hasManagedConfigDir: Bool
}
