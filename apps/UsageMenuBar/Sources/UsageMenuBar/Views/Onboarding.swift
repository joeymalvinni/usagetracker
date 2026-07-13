import SwiftUI

struct Onboarding: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg) {
            VStack(alignment: .leading, spacing: Theme.Spacing.xs) {
                Text("Welcome to UsageTracker")
                    .font(Theme.Typography.title)
                Text("Choose the accounts you want to track. Usage stays on this Mac; credentials remain in provider files or Keychain.")
                    .font(Theme.Typography.body)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            if let error = state.actionError {
                SetupNotice(text: error, isError: true)
            } else if let message = state.actionMessage {
                SetupNotice(text: message, isError: false)
            }

            ScrollView {
                VStack(spacing: Theme.Spacing.sm) {
                    OnboardingProviderCard(providerId: "codex")
                    OnboardingProviderCard(providerId: "claude")
                    OnboardingProviderCard(providerId: "opencode_go")
                    OnboardingProviderCard(providerId: "grok")
                }
            }

            HStack {
                Label("Cost figures from local logs are estimates and are labeled in the dashboard.", systemImage: "info.circle")
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Finish setup") { state.completeOnboarding() }
                    .buttonStyle(.borderedProminent)
                    .disabled(state.daemon == .offline)
            }
        }
        .padding(Theme.Spacing.lg)
        .frame(width: Theme.Popover.width, height: Theme.Popover.height)
    }
}

private struct OnboardingProviderCard: View {
    @EnvironmentObject var state: AppState
    let providerId: String

    private var provider: ProviderVM? {
        state.settingsProviders.first { $0.providerId == providerId }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
            HStack {
                ProviderIcon(id: providerId, symbol: symbol, size: 18)
                    .frame(width: 20)
                VStack(alignment: .leading, spacing: 1) {
                    Text(name).font(Theme.Typography.headline)
                    Text(description)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Toggle("", isOn: enabledBinding)
                    .labelsHidden()
                    .disabled(state.daemon == .offline || state.pendingProviders.contains(providerId))
            }
            if provider?.enabled == true {
                ProviderSetupControls(providerId: providerId, compact: true)
            }
        }
        .surfaceCard()
    }

    private var enabledBinding: Binding<Bool> {
        Binding(
            get: { provider?.enabled ?? false },
            set: { enabled in Task { await state.setProviderEnabled(providerId, enabled) } }
        )
    }

    private var name: String {
        ProviderCatalog.name(for: providerId)
    }
    private var symbol: String {
        ProviderCatalog.symbol(for: providerId)
    }
    private var description: String {
        switch providerId {
        case "codex": "Rate limits plus estimated local token activity"
        case "claude": "Claude Code limits plus estimated local activity"
        case "opencode_go": "Workspace usage, balance, and local fallback"
        case "grok": "Shared Grok usage from Grok Build or grok.com"
        default: "Provider usage and limits"
        }
    }
}

struct ProviderSetupControls: View {
    @EnvironmentObject var state: AppState
    let providerId: String
    var compact = false

    private var setup: ProviderSetupResponse? { state.providerSetups[providerId] }
    private var busy: Bool { state.pendingAccountProviders.contains(providerId) }
    private var accounts: [Account] { state.accounts.filter { $0.providerId == providerId } }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.xs) {
            HStack(spacing: Theme.Spacing.sm) {
                if canConnectOrRepair {
                    Button(repairLabel) {
                        Task { await connectOrRepair() }
                    }
                    .disabled(busy || state.daemon == .offline)
                }

                if state.supportsAddAccount(providerId), !accounts.isEmpty {
                    Button("Add another account") { Task { await state.addProviderAccount(providerId) } }
                        .disabled(busy)
                }
                if state.supportsWorkspaceSetup(providerId) {
                    Button(setup == nil ? "Discover workspaces" : "Discover again") {
                        Task { await state.loadProviderSetup(providerId) }
                    }
                    .disabled(busy)
                }
                if busy { ProgressView().controlSize(.small) }
                Spacer()
            }
            .controlSize(.small)

            if state.supportsWorkspaceSetup(providerId), let setup {
                if !setup.workspaceOptions.isEmpty {
                    Picker("Workspace", selection: workspaceBinding(setup)) {
                        ForEach(setup.workspaceOptions, id: \.self) { Text($0).tag($0) }
                    }
                    .pickerStyle(.menu)
                    .disabled(busy)
                } else if let error = setup.discoveryError {
                    Text(error)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            Text(helpText)
                .font(Theme.Typography.micro)
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .task {
            if state.providerSetups[providerId] == nil, state.supportsWorkspaceSetup(providerId) {
                await state.loadProviderSetup(providerId)
            }
        }
    }

    private var canConnectOrRepair: Bool {
        accounts.isEmpty
            ? state.supportsAddAccount(providerId) || state.supportsRepair(providerId)
            : state.supportsRepair(providerId)
    }

    private func connectOrRepair() async {
        if accounts.isEmpty, state.supportsAddAccount(providerId) {
            await state.addProviderAccount(providerId)
        } else if state.supportsRepair(providerId) {
            await state.repairProvider(providerId, accountId: accounts.first?.id)
        }
    }

    private func workspaceBinding(_ setup: ProviderSetupResponse) -> Binding<String> {
        let fallback = setup.selectedWorkspaceId ?? setup.workspaceOptions.first ?? ""
        return Binding(
            get: { state.providerSetups[providerId]?.selectedWorkspaceId ?? fallback },
            set: { workspace in Task { await state.selectWorkspace(providerId: providerId, workspaceId: workspace) } }
        )
    }

    private var repairLabel: String {
        switch providerId {
        case "codex": accounts.isEmpty ? "Connect Codex" : "Sign in again"
        case "claude": accounts.isEmpty ? "Connect Claude" : "Sign in to Claude again"
        case "opencode_go": "Open OpenCode login"
        case "grok": accounts.isEmpty ? "Connect Grok" : "Sign in to Grok again"
        default: "Connect provider"
        }
    }

    private var helpText: String {
        switch providerId {
        case "codex": "Each account uses an isolated Codex profile. Finish the browser sign-in; the account appears automatically."
        case "claude": "Each account uses an isolated Claude profile. After sign-in, open its profile terminal from Settings to keep activity separate."
        case "opencode_go": "Sign in at opencode.ai, then discover and choose the workspace to track."
        case "grok": "Each CLI-backed account uses an isolated Grok profile. Browser-only sign-in remains available for the default account."
        default: "Connect the provider, then refresh usage."
        }
    }
}

struct SetupNotice: View {
    let text: String
    let isError: Bool

    var body: some View {
        Label(text, systemImage: isError ? "exclamationmark.triangle.fill" : "checkmark.circle.fill")
            .font(Theme.Typography.caption)
            .foregroundStyle(isError ? .red : .secondary)
            .fixedSize(horizontal: false, vertical: true)
            .padding(Theme.Spacing.sm)
            .frame(maxWidth: .infinity, alignment: .leading)
            .surfaceInset()
    }
}
