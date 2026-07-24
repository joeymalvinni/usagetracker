import AppKit
import SwiftUI

struct Onboarding: View {
    @EnvironmentObject var state: AppState
    @State private var welcomeAcknowledged = false

    var body: some View {
        Group {
            if state.onboardingDiscoveryStarted {
                setupContent
            } else if welcomeAcknowledged {
                providerAccessContent
            } else {
                welcomeContent
            }
        }
        .padding(Theme.Spacing.lg)
        .frame(width: Theme.Popover.width, height: Theme.Popover.height)
    }

    private var welcomeContent: some View {
        VStack(spacing: Theme.Spacing.xl) {
            Spacer()

            Image(nsImage: NSApplication.shared.applicationIconImage)
                .resizable()
                .interpolation(.high)
                .scaledToFit()
                .frame(width: 72, height: 72)
                .accessibilityHidden(true)

            VStack(spacing: Theme.Spacing.sm) {
                Text("Welcome to UsageTracker")
                    .font(.title2.bold())
                Text("See your AI usage and limits at a glance, without leaving your menu bar.")
                    .font(Theme.Typography.body)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }

            VStack(alignment: .leading, spacing: Theme.Spacing.md) {
                welcomeFeature(
                    "Always close by",
                    detail: "Click the UsageTracker icon in the menu bar whenever you want an update.",
                    symbol: "menubar.rectangle"
                )
                welcomeFeature(
                    "Only the providers you choose",
                    detail: "Start with Codex, then add Claude, OpenCode Go, or Grok at any time.",
                    symbol: "checkmark.circle"
                )
                welcomeFeature(
                    "Private by design",
                    detail: "Your usage data stays on this Mac.",
                    symbol: "lock.shield"
                )
            }
            .surfaceCard()

            Spacer()

            Button("Get Started") {
                welcomeAcknowledged = true
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .frame(maxWidth: .infinity, alignment: .trailing)
        }
    }

    private func welcomeFeature(_ title: String, detail: String, symbol: String) -> some View {
        HStack(alignment: .top, spacing: Theme.Spacing.md) {
            Image(systemName: symbol)
                .font(Theme.Typography.headline)
                .foregroundStyle(.tint)
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(Theme.Typography.headline)
                Text(detail)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private var providerAccessContent: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg) {
            onboardingHeader(
                title: "Before UsageTracker checks Codex",
                subtitle: "You stay in control of which providers UsageTracker can inspect."
            )
            providerAccessExplanation
            Spacer()
            HStack {
                Button("Back") { welcomeAcknowledged = false }
                Spacer()
                Button("Check Codex") {
                    Task { await state.discoverAccountsForOnboarding() }
                }
                .buttonStyle(.borderedProminent)
            }
        }
    }

    private var setupContent: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg) {
            onboardingHeader(
                title: "Set up your providers",
                subtitle: "Turn on another provider to check it. Providers left off are not scanned."
            )
            discoveryResults
        }
    }

    private func onboardingHeader(title: String, subtitle: String) -> some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.xs) {
            Text(title)
                .font(Theme.Typography.title)
            Text(subtitle)
                .font(Theme.Typography.body)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var providerAccessExplanation: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
            Label("Provider access is opt-in", systemImage: "checkmark.shield.fill")
                .font(Theme.Typography.headline)
            Text("Codex is enabled by default. UsageTracker will not inspect credentials for Claude, OpenCode Go, Grok, or other providers unless you turn them on.")
                .font(Theme.Typography.body)
                .fixedSize(horizontal: false, vertical: true)
            Text("When you enable a provider that uses the macOS Keychain, macOS may ask for access. Choose **Always Allow** if you want background refreshes without repeated prompts.")
                .font(Theme.Typography.body)
                .fixedSize(horizontal: false, vertical: true)
            Label("Credentials stay in provider files or Keychain and usage data stays on this Mac.", systemImage: "lock.shield")
                .font(Theme.Typography.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(Theme.Spacing.md)
        .surfaceCard()
    }

    private var discoveryResults: some View {
        Group {
            if let error = state.actionError {
                SetupNotice(text: error, isError: true)
            } else if let message = state.actionMessage {
                SetupNotice(text: message, isError: false)
            } else if state.onboardingDiscoveryRunning {
                SetupNotice(text: "Checking enabled providers…", isError: false)
            }

            ScrollView {
                VStack(spacing: Theme.Spacing.sm) {
                    ForEach(state.settingsProviders) { provider in
                        OnboardingProviderCard(providerId: provider.providerId)
                    }
                }
            }

            HStack {
                Button("Check enabled") {
                    Task { await state.discoverAccountsForOnboarding() }
                }
                .disabled(state.onboardingDiscoveryRunning)
                Spacer()
                Button("Finish setup") { state.completeOnboarding() }
                    .buttonStyle(.borderedProminent)
                    .disabled(state.daemon != .online || state.onboardingDiscoveryRunning)
            }
        }
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
        provider?.name ?? ProviderCatalog.name(for: providerId)
    }
    private var symbol: String {
        provider?.symbol ?? ProviderCatalog.symbol(for: providerId)
    }
    private var description: String {
        "Usage, limits, and account health"
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
                    Button("Open sign-in") {
                        Task { await connectOrRepair() }
                    }
                    .disabled(busy || state.daemon == .offline)

                    Button("Copy sign-in link", systemImage: "doc.on.doc") {
                        Task { await copyAuthenticationURL() }
                    }
                    .help("Copy the sign-in link to open it in another browser")
                    .disabled(busy || state.daemon == .offline)
                }

                if state.supportsAddAccount(providerId), !accounts.isEmpty {
                    Button("Add another account") { Task { await state.addProviderAccount(providerId) } }
                        .disabled(busy)
                }
                if state.supportsSetup(providerId) {
                    Button(setup == nil ? "Load setup" : "Reload setup") {
                        Task { await state.loadProviderSetup(providerId) }
                    }
                    .disabled(busy)
                }
                if busy { ProgressView().controlSize(.small) }
                Spacer()
            }
            .controlSize(.small)

            if state.supportsSetup(providerId), let setup {
                ProviderSetupFields(providerId: providerId, setup: setup, disabled: busy)
            }

            Text(helpText)
                .font(Theme.Typography.micro)
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .task {
            if state.providerSetups[providerId] == nil, state.supportsSetup(providerId) {
                await state.loadProviderSetup(providerId)
            }
        }
    }

    private var canConnectOrRepair: Bool {
        accounts.isEmpty
            ? state.supportsAddAccount(providerId) || state.supportsRepair(providerId)
            : state.supportsRepair(providerId)
    }

    private func copyAuthenticationURL() async {
        guard let url = await state.providerSignInLink(
            providerId,
            accountId: accounts.first?.id
        ) else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(url, forType: .string)
        state.actionError = nil
        state.actionMessage = "\(ProviderCatalog.name(for: providerId)) sign-in link copied."
    }

    private func connectOrRepair() async {
        if accounts.isEmpty, state.supportsAddAccount(providerId) {
            await state.addProviderAccount(providerId)
        } else if state.supportsRepair(providerId) {
            await state.repairProvider(providerId, accountId: accounts.first?.id)
        }
    }

    private var helpText: String {
        state.supportsSetup(providerId)
            ? "Connect the provider, review its setup options, then refresh usage."
            : "Finish sign-in; the account appears automatically after the next refresh."
    }
}

struct ProviderSetupFields: View {
    @EnvironmentObject var state: AppState
    let providerId: String
    let setup: ProviderSetupResponse
    let disabled: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.xs) {
            ForEach(setup.fields) { field in
                ProviderSetupFieldControl(providerId: providerId, field: field, disabled: disabled)
                    .id("\(field.key):\(field.value ?? "")")
            }
            if let error = setup.discoveryError {
                Text(error)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

private struct ProviderSetupFieldControl: View {
    @EnvironmentObject var state: AppState
    let providerId: String
    let field: ProviderSetupField
    let disabled: Bool
    @State private var draft: String

    init(providerId: String, field: ProviderSetupField, disabled: Bool) {
        self.providerId = providerId
        self.field = field
        self.disabled = disabled
        _draft = State(initialValue: field.value ?? "")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.xs) {
            if field.kind == "select" {
                Picker(field.label, selection: selectionBinding) {
                    if !field.required { Text("Automatic").tag("") }
                    ForEach(field.options, id: \.self) { Text($0).tag($0) }
                }
                .pickerStyle(.menu)
                .disabled(disabled)
            } else {
                HStack {
                    if field.kind == "secret" {
                        SecureField(field.label, text: $draft)
                    } else {
                        TextField(field.label, text: $draft)
                    }
                    Button("Apply") { update(draft) }
                        .disabled(disabled || (field.required && draft.isEmpty))
                }
                .controlSize(.small)
            }
            if let help = field.helpText {
                Text(help)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    private var selectionBinding: Binding<String> {
        Binding(
            get: { field.value ?? "" },
            set: { update($0) }
        )
    }

    private func update(_ value: String) {
        let normalized = value.trimmingCharacters(in: .whitespacesAndNewlines)
        Task {
            await state.updateProviderSetup(
                providerId: providerId,
                key: field.key,
                value: normalized.isEmpty ? nil : normalized
            )
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
