import AppKit
import SwiftUI

private struct HiddenWindowEntry: Identifiable {
    /// Composite `providerId|windowId` key, also used to restore the window.
    let id: String
    let label: String
    let providerName: String
}

private enum SettingsTab: String, CaseIterable {
    case general
    case providers

    var label: String {
        switch self {
        case .general: "General"
        case .providers: "Accounts"
        }
    }
}

struct Settings: View {
    private static let notificationSettingsURL = URL(
        string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension"
    )!

    @EnvironmentObject var state: AppState
    @State private var showsOtherProviders = false
    @State private var showsRemovedAccounts = false
    @State private var showsAdvanced = false
    @State private var showsDeleteAll = false
    @State private var selectedTab: SettingsTab = .general

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg - 2) {
            Header(
                title: "Settings",
                subtitleStyle: state.daemon == .offline ? .offline : .custom("changes apply immediately"),
                showsRefresh: false
            )
            if let error = state.actionError { SetupNotice(text: error, isError: true) }
            if let error = state.preferencesError { SetupNotice(text: error, isError: true) }
            if let error = state.notificationError { SetupNotice(text: error, isError: true) }
            if let message = state.actionMessage { SetupNotice(text: message, isError: false) }

            Picker("Settings section", selection: $selectedTab) {
                ForEach(SettingsTab.allCases, id: \.self) { tab in
                    Text(tab.label).tag(tab)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .accessibilityLabel("Settings section")

            ScrollView {
                selectedTabContent
                .padding(.bottom, Theme.Spacing.xs + 2)
            }

            Spacer(minLength: 0)
        }
        .padding(Theme.Spacing.lg)
        .alert("Delete all accounts?", isPresented: $showsDeleteAll) {
            Button("Delete all", role: .destructive) {
                Task { await state.deleteAllAccounts() }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This permanently deletes all \(state.accounts.count) accounts and their local usage history. Provider accounts are not affected.")
        }
    }

    @ViewBuilder
    private var selectedTabContent: some View {
        switch selectedTab {
        case .general:
            generalSettings
        case .providers:
            providerSettings
        }
    }

    private var generalSettings: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.md) {
            sectionTitle("General")
            VStack(alignment: .leading, spacing: Theme.Spacing.md) {
                LabeledContent("Dark mode") {
                    Toggle("", isOn: darkModeBinding)
                        .labelsHidden()
                        .accessibilityLabel("Dark mode")
                }
                LabeledContent("Activity chart") {
                    Picker("", selection: activityChartStyleBinding) {
                        ForEach(UIConfig.ActivityChartStyle.allCases, id: \.self) {
                            Text($0.label).tag($0)
                        }
                    }
                    .labelsHidden()
                    .fixedSize()
                }
                LabeledContent {
                    if state.pendingNotifications {
                        ProgressView().controlSize(.small)
                    } else {
                        Toggle("", isOn: notificationsBinding)
                            .labelsHidden()
                            .accessibilityLabel("Usage alerts")
                            .disabled(state.daemon == .offline)
                    }
                } label: {
                    VStack(alignment: .leading, spacing: 1) {
                        Text("Usage alerts")
                        if state.config?.notifications.enabled == true {
                            HStack(spacing: Theme.Spacing.xs) {
                                Text(notificationPermissionText)
                                    .foregroundStyle(state.notificationAuthorization == .denied ? .red : .secondary)
                                if state.notificationAuthorization == .denied {
                                    Link("Open Settings", destination: Self.notificationSettingsURL)
                                        .buttonStyle(.link)
                                }
                            }
                            .font(Theme.Typography.micro)
                        }
                    }
                }
                LabeledContent("Refresh every") {
                    if state.pendingInterval {
                        ProgressView().controlSize(.small)
                    } else {
                        Picker("", selection: intervalBinding) {
                            ForEach(intervalOptions, id: \.self) { Text(intervalLabel($0)).tag($0) }
                        }
                        .labelsHidden()
                        .fixedSize()
                        .disabled(state.daemon == .offline)
                    }
                }

            }
            .surfaceCard()

            if state.isDeveloperMode {
                DisclosureGroup(isExpanded: $showsAdvanced) {
                    VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
                        LabeledContent("Socket", value: state.config?.socketPath ?? "unknown")
                        LabeledContent("Config", value: state.config?.configPath ?? "unknown")
                        LabeledContent("Database", value: state.config?.dbPath ?? "unknown")
                        LabeledContent("UI config", value: UIPaths.config.path)
                    }
                    .font(Theme.Typography.micro)
                    .padding(.top, Theme.Spacing.sm)
                } label: {
                    Text("Advanced (developer)").font(Theme.Typography.caption.weight(.medium))
                }
                .surfaceCard()
            }

            Button("Quit Usage") { NSApp.terminate(nil) }
                .buttonStyle(.chip)
                .frame(maxWidth: .infinity, alignment: .trailing)
        }
    }

    private var providerSettings: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.md) {
            ForEach(connectedProviders) { provider in
                ProviderAccountCard(provider: provider)
            }

            if !otherProviders.isEmpty {
                if connectedProviders.isEmpty {
                    Text("Connect an account to see its usage here.")
                        .font(Theme.Typography.caption)
                        .foregroundStyle(.secondary)
                    availableProviders
                } else {
                    DisclosureGroup("Add a provider", isExpanded: $showsOtherProviders) {
                        availableProviders.padding(.top, Theme.Spacing.sm)
                    }
                    .font(Theme.Typography.caption.weight(.medium))
                }
            }

            if !removedAccounts.isEmpty {
                DisclosureGroup(isExpanded: $showsRemovedAccounts) {
                    VStack(spacing: Theme.Spacing.xs) {
                        ForEach(removedAccounts) { account in
                            AccountSettingsRow(account: account, isRemoved: true)
                        }
                    }
                    .padding(.top, Theme.Spacing.sm)
                } label: {
                    Text("Removed accounts (\(removedAccounts.count))")
                        .font(Theme.Typography.caption.weight(.medium))
                }
                .surfaceCard()
            }

            if !hiddenWindowEntries.isEmpty {
                sectionTitle("Hidden metrics")
                VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
                    ForEach(hiddenWindowEntries) { entry in
                        HStack {
                            VStack(alignment: .leading, spacing: 1) {
                                Text(entry.label).lineLimit(1)
                                Text(entry.providerName)
                                    .font(Theme.Typography.micro)
                                    .foregroundStyle(.secondary)
                            }
                            Spacer()
                            Button("Show") { state.showWindow(entry.id) }
                                .buttonStyle(.link)
                        }
                        .font(Theme.Typography.caption)
                    }
                }
                .surfaceCard()
            }
            Menu("Manage accounts") {
                Button("Connect accounts…") { Task { await state.restartOnboarding() } }
                if !state.accounts.isEmpty {
                    Divider()
                    Button("Delete all accounts…", role: .destructive) { showsDeleteAll = true }
                        .disabled(state.daemon != .online || !state.pendingAccounts.isEmpty)
                }
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .font(Theme.Typography.caption)
        }
    }

    private var connectedProviders: [ProviderVM] {
        state.settingsProviders.filter { provider in
            provider.enabled || state.accounts.contains {
                $0.providerId == provider.providerId && !($0.hidden && !$0.collectionEnabled)
            }
        }
    }

    private var otherProviders: [ProviderVM] {
        let connected = Set(connectedProviders.map(\.providerId))
        return state.settingsProviders.filter { !connected.contains($0.providerId) }
    }

    private var availableProviders: some View {
        VStack(spacing: Theme.Spacing.sm) {
            ForEach(otherProviders) { provider in
                ProviderConnectionCard(providerId: provider.providerId)
            }
        }
    }

    private var hiddenWindowEntries: [HiddenWindowEntry] {
        state.ui.hiddenWindows.map { key, label in
            let providerId = String(key.prefix { $0 != "|" })
            let providerName = state.settingsProviders.first { $0.providerId == providerId }?.name ?? providerId
            return HiddenWindowEntry(id: key, label: label, providerName: providerName)
        }
        .sorted {
            let byProvider = $0.providerName.localizedStandardCompare($1.providerName)
            if byProvider != .orderedSame { return byProvider == .orderedAscending }
            return $0.label.localizedStandardCompare($1.label) == .orderedAscending
        }
    }

    private var removedAccounts: [Account] {
        state.accounts
            .filter { $0.hidden && !$0.collectionEnabled }
            .sorted { accountLabel($0).localizedStandardCompare(accountLabel($1)) == .orderedAscending }
    }

    private func sectionTitle(_ title: String) -> some View {
        Text(title).font(Theme.Typography.caption.bold()).foregroundStyle(.secondary)
    }

    private func accountLabel(_ account: Account) -> String {
        account.displayLabel
    }

    private var intervalOptions: [UInt64] {
        var options: [UInt64] = [60, 120, 300, 600, 900, 1800, 3600]
        let current = state.config?.pollIntervalSeconds ?? 300
        if !options.contains(current) { options.append(current); options.sort() }
        return options
    }

    private var notificationsBinding: Binding<Bool> {
        Binding(
            get: { state.notificationsEffectivelyEnabled },
            set: { enabled in Task { await state.setNotificationsEnabled(enabled) } }
        )
    }

    private var darkModeBinding: Binding<Bool> {
        Binding(
            get: { state.ui.darkModeEnabled },
            set: { state.ui.darkModeEnabled = $0 }
        )
    }

    private var activityChartStyleBinding: Binding<UIConfig.ActivityChartStyle> {
        Binding(
            get: { state.ui.activityChartStyle },
            set: { state.ui.activityChartStyle = $0 }
        )
    }

    private var notificationPermissionText: String {
        guard state.notificationAuthorizationAvailable else {
            return "Native macOS permission is unavailable under swift run; use a bundled app build"
        }
        return switch state.notificationAuthorization {
        case .authorized, .provisional, .ephemeral: "Allowed by macOS"
        case .denied: "Blocked by macOS"
        case .notDetermined: "macOS will ask for permission when alerts are enabled"
        @unknown default: "Notification permission status unavailable"
        }
    }

    private var intervalBinding: Binding<UInt64> {
        Binding(
            get: { state.config?.pollIntervalSeconds ?? 300 },
            set: { seconds in Task { await state.setPollInterval(seconds) } }
        )
    }

    private func intervalLabel(_ seconds: UInt64) -> String {
        switch seconds {
        case ..<60: "\(seconds) sec"
        case 3600: "1 hour"
        case let seconds where seconds % 60 == 0: "\(seconds / 60) min"
        default: "\(seconds) sec"
        }
    }
}

private struct ProviderAccountCard: View {
    @EnvironmentObject var state: AppState
    let provider: ProviderVM

    private var accounts: [Account] {
        state.accounts
            .filter { $0.providerId == provider.providerId && !($0.hidden && !$0.collectionEnabled) }
            .sorted { accountLabel($0).localizedStandardCompare(accountLabel($1)) == .orderedAscending }
    }

    private var setup: ProviderSetupResponse? { state.providerSetups[provider.providerId] }
    private var busy: Bool { state.providerRecoveryIsBusy(provider.providerId) }
    private var connection: ProviderConnectionPresentation {
        state.onboardingProviderConnection(provider.providerId)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.md) {
            HStack(spacing: Theme.Spacing.sm) {
                ProviderIcon(id: provider.providerId, symbol: provider.symbol, size: 18)
                    .frame(width: 20)
                VStack(alignment: .leading, spacing: 1) {
                    Text(provider.name).font(Theme.Typography.headline)
                    Text(provider.enabled ? "Tracking" : "Not tracking")
                        .font(Theme.Typography.micro)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if state.pendingProviders.contains(provider.providerId) {
                    ProgressView().controlSize(.small)
                } else {
                    Toggle("", isOn: trackingBinding)
                        .labelsHidden()
                        .toggleStyle(.switch)
                        .accessibilityLabel("Track \(provider.name)")
                        .disabled(state.daemon == .offline)
                        .help(provider.enabled
                            ? "Stop tracking \(provider.name)"
                            : "Track \(provider.name)")
                }
            }

            Divider()
            if accounts.isEmpty {
                Text(connection.state == .idle ? "No account connected" : connection.message)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                VStack(spacing: Theme.Spacing.xs) {
                    ForEach(accounts) { account in
                        AccountSettingsRow(account: account)
                    }
                }
            }

            if state.supportsSetup(provider.providerId), let setup {
                ProviderSetupFields(providerId: provider.providerId, setup: setup, disabled: busy)
            }

            if state.providersAwaitingAuthenticationCode.contains(provider.providerId) {
                ProviderAuthenticationCodeEntry(providerId: provider.providerId)
            }

            HStack(spacing: Theme.Spacing.sm) {
                if accounts.isEmpty {
                    Button(connectionActionLabel) { Task { await connectAccount() } }
                        .buttonStyle(.chipProminent)
                        .disabled(busy || state.daemon == .offline)
                } else if state.supportsAddAccount(provider.providerId) {
                    Button("Add another account") {
                        Task { await state.addProviderAccount(provider.providerId) }
                    }
                    .buttonStyle(.chip)
                    .disabled(busy || state.daemon == .offline)
                }
                if state.isProviderSignInActive(provider.providerId) {
                    Button("Cancel sign-in") { state.cancelProviderSignIn(provider.providerId) }
                        .buttonStyle(.chip)
                }
                if state.supportsSetup(provider.providerId) {
                    Button(setup == nil ? "Find workspaces" : "Refresh workspaces") {
                        Task { await state.loadProviderSetup(provider.providerId) }
                    }
                    .buttonStyle(.chip)
                    .disabled(busy || state.daemon == .offline)
                }
                Spacer(minLength: 0)
                if state.supportsAddAccount(provider.providerId)
                    || (accounts.isEmpty && state.supportsRepair(provider.providerId)) {
                    Menu {
                        Button(accounts.isEmpty ? "Copy sign-in link" : "Copy link to add account") {
                            Task { await copySignInLink() }
                        }
                    } label: {
                        Image(systemName: "ellipsis").frame(width: 18, height: 18)
                    }
                    .menuStyle(.borderlessButton)
                    .menuIndicator(.hidden)
                    .fixedSize()
                    .disabled(busy || state.daemon == .offline)
                    .accessibilityLabel("Account connection options")
                }
            }
        }
        .surfaceCard()
    }

    private var trackingBinding: Binding<Bool> {
        Binding(
            get: { provider.enabled },
            set: { enabled in Task { await state.setProviderEnabled(provider.providerId, enabled) } }
        )
    }

    private var connectionActionLabel: String {
        switch connection.state {
        case .needsPermission: "Allow access"
        case .needsSignIn:
            state.supportsAddAccount(provider.providerId) || state.supportsRepair(provider.providerId)
                ? "Sign in" : "Check connection"
        case .failed: "Try again"
        default: "Connect account"
        }
    }

    private func connectAccount() async {
        if connection.state == .needsPermission {
            await state.allowProviderCredentialAccess(provider.providerId, retryConnection: true)
        } else if connection.state == .needsSignIn,
           state.supportsAddAccount(provider.providerId) || state.supportsRepair(provider.providerId) {
            await state.beginProviderSignIn(provider.providerId)
        } else {
            await state.connectProviderForOnboarding(provider.providerId)
        }
    }

    private func copySignInLink() async {
        guard let url = await state.providerSignInLink(
            provider.providerId,
            accountId: accounts.first?.id,
            addAccount: state.supportsAddAccount(provider.providerId)
        ) else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(url, forType: .string)
        state.actionError = nil
        state.actionMessage = "\(provider.name) sign-in link copied."
    }

    private func accountLabel(_ account: Account) -> String {
        account.displayLabel
    }
}

private struct AccountSettingsRow: View {
    @EnvironmentObject var state: AppState
    let account: Account
    var isRemoved = false
    @State private var showsRemovalOptions = false
    @State private var showsPermanentDelete = false
    @State private var showsRename = false
    @State private var draftName = ""

    var body: some View {
        HStack(spacing: Theme.Spacing.sm) {
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(Theme.Typography.body).lineLimit(1)
                Text(account.email.flatMap { $0 == title ? nil : "\($0) · \(statusText)" } ?? statusText)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer(minLength: Theme.Spacing.sm)

            if state.pendingAccounts.contains(account.id) {
                ProgressView().controlSize(.small)
            } else if isRemoved {
                Button("Restore") { Task { await state.restoreAccount(account.id) } }
                    .buttonStyle(.chip)
                Menu {
                    Button("Delete permanently", role: .destructive) { showsPermanentDelete = true }
                } label: {
                    Image(systemName: "ellipsis").frame(width: 18, height: 18)
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
            } else {
                if let action = collectionIssue?.recoveryAction,
                   state.canRecoverProvider(account.providerId, action: action) {
                    Button(action.label) {
                        Task {
                            await state.recoverProvider(account.providerId, accountId: account.id, action: action)
                        }
                    }
                    .buttonStyle(.chip)
                    .disabled(state.daemon != .online || state.providerRecoveryIsBusy(account.providerId))
                }
                accountMenu
            }
        }
        .padding(.horizontal, Theme.Spacing.sm)
        .frame(height: 42)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: Theme.Radius.md, style: .continuous)
                .fill(Color.primary.opacity(0.04))
        )
        .alert("Rename account", isPresented: $showsRename) {
            TextField("Account name", text: $draftName)
            Button("Cancel", role: .cancel) {}
            Button("Save") { Task { await state.renameAccount(account.id, displayName: draftName) } }
        } message: {
            Text("This only changes the name shown in UsageTracker.")
        }
        .confirmationDialog("Remove \(title)?", isPresented: $showsRemovalOptions) {
            Button("Remove and keep history", role: .destructive) {
                Task { await state.removeAccount(account.id) }
            }
            Button("Delete account and history", role: .destructive) { showsPermanentDelete = true }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This removes the account from UsageTracker, not from the provider.")
        }
        .alert("Delete \(title) permanently?", isPresented: $showsPermanentDelete) {
            Button("Delete permanently", role: .destructive) {
                Task { await state.deleteAccount(account.id) }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("All locally stored usage history for this account will be deleted. This cannot be undone.")
        }
    }

    private var accountMenu: some View {
        Menu {
            if !isRemoved, state.supportsLaunchAccount(account.providerId) {
                Button("Open \(ProviderCatalog.name(for: account.providerId)) session") {
                    if state.supportsLaunchOptions(account.providerId) {
                        Task { @MainActor in
                            await state.prepareOpenSession(account.id)
                            if let model = state.openSession {
                                OpenSessionWindow.shared.present(state: state, model: model)
                            }
                        }
                    } else {
                        Task { await state.launchProviderAccount(account.id) }
                    }
                }
            }
            if !isRemoved, state.supportsImportAccountData(account.providerId) {
                Button("Import from local Claude…") {
                    Task { @MainActor in
                        await state.prepareImportLocalClaude(account.id)
                        if let model = state.importLocalClaude {
                            ImportLocalClaudeWindow.shared.present(state: state, model: model)
                        }
                    }
                }
            }
            if !isRemoved,
               state.supportsLaunchAccount(account.providerId)
                   || state.supportsImportAccountData(account.providerId) {
                Divider()
            }
            Button("Rename") {
                draftName = title
                showsRename = true
            }
            Button(account.collectionEnabled ? "Pause tracking" : "Resume tracking") {
                Task { await state.setAccountCollectionEnabled(account.id, !account.collectionEnabled) }
            }
            Button(account.hidden ? "Show in summary" : "Hide from summary") {
                Task { await state.setAccountHidden(account.id, !account.hidden) }
            }
            if collectionIssue == .invalidCredentials, state.supportsRepair(account.providerId) {
                Button("Sign in to this account…") {
                    Task { await state.repairProvider(account.providerId, accountId: account.id) }
                }
            }
            Divider()
            Button("Remove account…", role: .destructive) { showsRemovalOptions = true }
        } label: {
            Image(systemName: "ellipsis").frame(width: 18, height: 18)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .disabled(state.daemon != .online || state.providerRecoveryIsBusy(account.providerId))
        .accessibilityLabel("Options for \(title)")
    }

    private var title: String { account.displayLabel }

    private var accountHealth: ProviderHealth? {
        state.health.first { $0.providerId == account.providerId && $0.accountId == account.id }
            ?? state.health.first { $0.providerId == account.providerId && $0.accountId == nil }
    }

    private var collectionIssue: ProviderCollectionIssue? {
        guard account.collectionEnabled,
              state.config?.providers[account.providerId]?.enabled == true else { return nil }
        return accountHealth.flatMap(ProviderCollectionIssue.init)
    }

    private var statusText: String {
        if isRemoved { return "Removed · history kept" }
        if !account.collectionEnabled || state.config?.providers[account.providerId]?.enabled != true {
            return "Paused"
        }
        if let issue = collectionIssue { return issue.summary }
        return account.hidden ? "Active · hidden from summary" : "Active"
    }
}
