import Foundation

struct ProviderSetupResponse: Decodable, Equatable {
    let providerId: String
    let profiles: [ProviderProfileOption]
    let fields: [ProviderSetupField]
    let selectedWorkspaceId: String?
    let workspaceOptions: [String]
    let discoveryError: String?

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        providerId = try c.decode(String.self, forKey: .providerId)
        profiles = try c.decode([ProviderProfileOption].self, forKey: .profiles)
        selectedWorkspaceId = try c.decodeIfPresent(String.self, forKey: .selectedWorkspaceId)
        workspaceOptions = try c.decodeIfPresent([String].self, forKey: .workspaceOptions) ?? []
        discoveryError = try c.decodeIfPresent(String.self, forKey: .discoveryError)
        if let decoded = try c.decodeIfPresent([ProviderSetupField].self, forKey: .fields) {
            fields = decoded
        } else if !workspaceOptions.isEmpty || selectedWorkspaceId != nil {
            fields = [ProviderSetupField(
                key: "workspace_id",
                label: "Workspace",
                kind: "select",
                value: selectedWorkspaceId,
                options: workspaceOptions,
                required: false,
                helpText: nil
            )]
        } else {
            fields = []
        }
    }

    private enum CodingKeys: String, CodingKey {
        case providerId, profiles, fields, selectedWorkspaceId, workspaceOptions, discoveryError
    }
}

struct ProviderSetupField: Decodable, Identifiable, Equatable {
    let key: String
    let label: String
    let kind: String
    let value: String?
    let options: [String]
    let required: Bool
    let helpText: String?

    var id: String { key }

    init(
        key: String,
        label: String,
        kind: String,
        value: String?,
        options: [String],
        required: Bool,
        helpText: String?
    ) {
        self.key = key
        self.label = label
        self.kind = kind
        self.value = value
        self.options = options
        self.required = required
        self.helpText = helpText
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        key = try c.decode(String.self, forKey: .key)
        label = try c.decode(String.self, forKey: .label)
        kind = try c.decode(String.self, forKey: .kind)
        value = try c.decodeIfPresent(String.self, forKey: .value)
        options = try c.decodeIfPresent([String].self, forKey: .options) ?? []
        required = try c.decodeIfPresent(Bool.self, forKey: .required) ?? false
        helpText = try c.decodeIfPresent(String.self, forKey: .helpText)
    }

    private enum CodingKeys: String, CodingKey {
        case key, label, kind, value, options, required, helpText
    }
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
    let authenticationUrl: String?
}
