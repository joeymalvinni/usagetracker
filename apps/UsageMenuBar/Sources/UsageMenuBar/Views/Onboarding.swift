import AppKit
import SwiftUI

struct Onboarding: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        providerContent
            .padding(Theme.Spacing.lg)
            .frame(width: Theme.Popover.width, height: Theme.Popover.height)
    }

    private var providerContent: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.md) {
            Text("Connect your accounts").font(Theme.Typography.title)
            if let error = state.actionError {
                SetupNotice(text: error, isError: true)
                HStack {
                    if state.daemon == .offline {
                        Link(
                            "Open Login Items",
                            destination: URL(
                                string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension"
                            )!
                        )
                        .buttonStyle(.link)
                    }
                    Spacer()
                    Button("Try again") {
                        Task { await state.prepareOnboarding() }
                    }
                    .buttonStyle(.chipProminent)
                    .disabled(state.onboardingDiscoveryRunning)
                }
            }

            ScrollView {
                VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
                    if state.onboardingDiscoveryRunning && state.serverProviderOrder.isEmpty {
                        HStack(spacing: Theme.Spacing.sm) {
                            ProgressView().controlSize(.small)
                            Text("Connecting accounts…")
                                .font(Theme.Typography.caption)
                                .foregroundStyle(.secondary)
                        }
                        .frame(maxWidth: .infinity, alignment: .center)
                        .padding(.vertical, Theme.Spacing.xxl)
                    } else {
                        ForEach(state.onboardingProviderOrder, id: \.self) {
                            ProviderConnectionCard(providerId: $0)
                        }
                    }
                }
                .padding(.bottom, Theme.Spacing.xs)
            }

            HStack {
                Spacer()
                Button(state.onboardingHasConnectedAccounts ? "View usage" : "Set up later") {
                    state.continueFromOnboarding()
                }
                .buttonStyle(.chipPrimary)
                .disabled(state.onboardingDiscoveryRunning)
            }
        }
        .task {
            await state.prepareOnboarding()
        }
    }
}

struct ProviderConnectionCard: View {
    @EnvironmentObject var state: AppState
    let providerId: String

    private var connection: ProviderConnectionPresentation {
        state.onboardingProviderConnection(providerId)
    }

    private var connectionState: ProviderConnectionState {
        connection.state
    }

    private var accounts: [Account] {
        state.onboardingConnectedAccounts.filter { $0.providerId == providerId }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
            HStack(spacing: Theme.Spacing.sm) {
                ProviderIcon(id: providerId, symbol: ProviderCatalog.symbol(for: providerId), size: 20)
                    .frame(width: 22)
                VStack(alignment: .leading, spacing: 2) {
                    Text(providerName).font(Theme.Typography.headline)
                    Label(statusText, systemImage: statusSymbol)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(statusColor)
                }
                Spacer()
                primaryControl
            }

            if !accounts.isEmpty {
                VStack(spacing: Theme.Spacing.xs) {
                    ForEach(accounts) { account in
                        onboardingAccountRow(account)
                    }
                }
            }

            secondaryControls

            if !accounts.isEmpty, state.supportsAddAccount(providerId) {
                Button("Add another account", systemImage: "person.badge.plus") {
                    Task { await state.addProviderAccount(providerId) }
                }
                .buttonStyle(.chip)
                .disabled(!state.canConnectProviders || state.providerRecoveryIsBusy(providerId))
            }

            if state.supportsSetup(providerId), let setup = state.providerSetups[providerId] {
                ProviderSetupFields(providerId: providerId, setup: setup, disabled: isBusy)
            }
        }
        .surfaceCard()
    }

    @ViewBuilder private var primaryControl: some View {
        switch connectionState {
        case .connecting:
            ProgressView().controlSize(.small).accessibilityLabel("Connecting \(providerName)")
        case .connected:
            Image(systemName: "checkmark.circle.fill")
                .foregroundStyle(.green)
                .accessibilityLabel("\(providerName) connected")
        case .waitingForSignIn:
            ProgressView().controlSize(.small).accessibilityLabel("Waiting for \(providerName) sign-in")
        case .idle:
            Button("Connect") { requestConnection() }
                .buttonStyle(.chipProminent)
                .disabled(!state.canConnectProviders || state.providerRecoveryIsBusy(providerId))
        case .needsPermission:
            if state.pendingAccountProviders.contains(providerId) {
                ProgressView().controlSize(.small).accessibilityLabel("Requesting credential access")
            } else {
                Button("Allow access") {
                    Task {
                        await state.allowProviderCredentialAccess(
                            providerId, accountId: accounts.first?.id, retryConnection: true
                        )
                    }
                }
                .buttonStyle(.chipProminent)
                .disabled(state.daemon != .online || state.providerRecoveryIsBusy(providerId))
            }
        case .failed:
            Button("Try again") { requestConnection() }
                .buttonStyle(.chipProminent)
                .disabled(!state.canConnectProviders || state.providerRecoveryIsBusy(providerId))
        case .needsSignIn:
            EmptyView()
        }
    }
    @ViewBuilder private var secondaryControls: some View {
        switch connectionState {
        case .needsSignIn:
            VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
                if !canSignIn {
                    Text("Open \(providerName), sign in there, then check again.")
                        .font(Theme.Typography.caption)
                        .foregroundStyle(.secondary)
                }
                HStack(spacing: Theme.Spacing.sm) {
                    if canSignIn {
                        Button("Sign in") {
                            Task { await openSignIn() }
                        }
                        .buttonStyle(.chipProminent)
                        Menu {
                            Button("Copy sign-in link") { Task { await copySignInLink() } }
                        } label: {
                            Image(systemName: "ellipsis")
                        }
                        .menuStyle(.borderlessButton)
                        .menuIndicator(.hidden)
                        .fixedSize()
                        .accessibilityLabel("Sign-in options")
                    }
                    Button("Check again") {
                        Task { await state.checkProviderAfterSignIn(providerId) }
                    }
                    .buttonStyle(.chip)
                }
            }
        case .waitingForSignIn:
            VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
                if state.providersAwaitingAuthenticationCode.contains(providerId) {
                    ProviderAuthenticationCodeEntry(providerId: providerId)
                }
                HStack(spacing: Theme.Spacing.sm) {
                    Button("Check connection") {
                        Task { await state.checkProviderAfterSignIn(providerId) }
                    }
                    .buttonStyle(.chipProminent)
                    Button("Cancel") { state.cancelProviderSignIn(providerId) }
                        .buttonStyle(.chip)
                }
            }
        case .connected:
            HStack(spacing: Theme.Spacing.sm) {
                if state.supportsSetup(providerId) {
                    Button(state.providerSetups[providerId] == nil ? "Find workspaces" : "Refresh workspaces") {
                        Task { await state.loadProviderSetup(providerId) }
                    }
                    .buttonStyle(.chip)
                }
            }
        case .idle, .connecting, .needsPermission, .failed:
            EmptyView()
        }
    }

    private func onboardingAccountRow(_ account: Account) -> some View {
        HStack(spacing: Theme.Spacing.sm) {
            Image(systemName: "person.crop.circle.fill")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                Text(accountTitle(account))
                    .font(Theme.Typography.caption.weight(.medium))
                    .lineLimit(1)
                if let email = account.email, email != accountTitle(account) {
                    Text(email)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            Spacer()
            if accounts.count > 1,
               let issue = collectionIssue(for: account),
               let action = issue.recoveryAction,
               state.canRecoverProvider(providerId, action: action) {
                Button(action.label) {
                    Task { await state.recoverProvider(providerId, accountId: account.id, action: action) }
                }
                .buttonStyle(.chip)
                .disabled(state.daemon != .online || state.providerRecoveryIsBusy(providerId))
            } else {
                Text(accountReading(account))
                    .font(Theme.Typography.caption.weight(.medium))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, Theme.Spacing.sm)
        .frame(height: 38)
        .background(
            RoundedRectangle(cornerRadius: Theme.Radius.md, style: .continuous)
                .fill(Color.primary.opacity(0.04))
        )
    }

    private func collectionIssue(for account: Account) -> ProviderCollectionIssue? {
        guard account.collectionEnabled, state.config?.providers[providerId]?.enabled == true else { return nil }
        let health = state.health.first { $0.providerId == providerId && $0.accountId == account.id }
            ?? state.health.first { $0.providerId == providerId && $0.accountId == nil }
        return health.flatMap(ProviderCollectionIssue.init)
    }

    private func accountReading(_ account: Account) -> String {
        guard account.collectionEnabled, state.config?.providers[providerId]?.enabled == true else { return "Paused" }
        let reading = state.settingsProviders
            .flatMap { $0.subAccounts ?? [$0] }
            .first { $0.accountId == account.id }
        if let reading, reading.percent != nil { return reading.primary }
        return "Added"
    }

    private func requestConnection() {
        Task { await state.connectProviderForOnboarding(providerId) }
    }

    private func openSignIn() async {
        await state.beginProviderSignIn(providerId, accountId: accounts.first?.id)
    }

    private func copySignInLink() async {
        await ProviderSignInActions.copyLink(
            state: state,
            providerId: providerId,
            displayName: providerName,
            accountId: accounts.first?.id
        )
    }

    private func accountTitle(_ account: Account) -> String {
        account.displayLabel
    }

    private var providerName: String {
        state.serverProviders[providerId]?.displayName ?? ProviderCatalog.name(for: providerId)
    }

    private var statusText: String {
        switch connectionState {
        case .idle: "Not connected"
        case .connecting: "Connecting…"
        case .connected: accounts.count == 1 ? "1 account connected" : "\(accounts.count) accounts connected"
        case .waitingForSignIn: "Finish sign-in in your browser"
        case .needsSignIn: "Sign in to connect"
        case .needsPermission: "Allow credential access"
        case .failed: connection.message
        }
    }

    private var statusSymbol: String {
        switch connectionState {
        case .connected: "checkmark.circle.fill"
        case .connecting, .waitingForSignIn: "clock"
        case .needsPermission: "lock"
        case .needsSignIn: "person.badge.key"
        case .failed: "info.circle"
        case .idle: state.serverProviders[providerId]?.detected == true ? "desktopcomputer" : "plus.circle"
        }
    }

    private var statusColor: Color {
        switch connectionState {
        case .connected: .green
        case .idle, .connecting, .waitingForSignIn, .needsPermission, .needsSignIn, .failed: .secondary
        }
    }

    private var isBusy: Bool {
        connectionState == .connecting || connectionState == .waitingForSignIn
    }

    private var canSignIn: Bool {
        state.supportsAddAccount(providerId) || state.supportsRepair(providerId)
    }


}

@MainActor private enum ProviderSignInActions {
    static func copyLink(
        state: AppState,
        providerId: String,
        displayName: String,
        accountId: String?
    ) async {
        guard let url = await state.providerSignInLink(
            providerId,
            accountId: accountId
        ) else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(url, forType: .string)
        state.actionError = nil
        state.actionMessage = "\(displayName) sign-in link copied."
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
                    Button("Sign in") {
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
                    Button(setup == nil ? "Find workspaces" : "Refresh workspaces") {
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
    }

    private var canConnectOrRepair: Bool {
        accounts.isEmpty
            ? state.supportsAddAccount(providerId) || state.supportsRepair(providerId)
            : state.supportsRepair(providerId)
    }

    private func copyAuthenticationURL() async {
        await ProviderSignInActions.copyLink(
            state: state,
            providerId: providerId,
            displayName: state.serverProviders[providerId]?.displayName
                ?? ProviderCatalog.name(for: providerId),
            accountId: accounts.first?.id
        )
    }

    private func connectOrRepair() async {
        await state.beginProviderSignIn(providerId, accountId: accounts.first?.id)
    }

    private var helpText: String {
        state.supportsSetup(providerId)
            ? "Workspace discovery runs only when you choose Find workspaces."
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
