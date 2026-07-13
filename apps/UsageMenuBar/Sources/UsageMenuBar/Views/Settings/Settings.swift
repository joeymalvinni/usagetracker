import SwiftUI

private struct HiddenWindowEntry: Identifiable {
    /// Composite `providerId|windowId` key, also used to restore the window.
    let id: String
    let label: String
    let providerName: String
}

struct Settings: View {
    private static let notificationSettingsURL = URL(
        string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension"
    )!

    @EnvironmentObject var state: AppState
    @State private var showsRemovedAccounts = false
    @State private var showsAdvanced = false
    @State private var showsDeleteAll = false

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

            ScrollView {
                VStack(alignment: .leading, spacing: Theme.Spacing.md) {
                    sectionTitle("Accounts & Providers")
                    ForEach(state.settingsProviders) { provider in
                        ProviderAccountCard(provider: provider)
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

                    sectionTitle("General")
                    VStack(alignment: .leading, spacing: Theme.Spacing.md) {
                        LabeledContent("Dark mode") {
                            Toggle("", isOn: darkModeBinding)
                                .labelsHidden()
                        }
                        LabeledContent {
                            if state.pendingNotifications {
                                ProgressView().controlSize(.small)
                            } else {
                                Toggle("", isOn: notificationsBinding)
                                    .labelsHidden()
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
                        Divider()
                        HStack {
                            Button("Run setup assistant") { state.restartOnboarding() }
                                .buttonStyle(.link)
                            Spacer()
                            if !state.accounts.isEmpty {
                                Button("Delete all accounts…", role: .destructive) {
                                    showsDeleteAll = true
                                }
                                .buttonStyle(.link)
                                .disabled(state.daemon == .offline || !state.pendingAccounts.isEmpty)
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
                }
                .padding(.bottom, Theme.Spacing.xs + 2)
            }

            Spacer(minLength: 0)
            Button("Quit Usage") { NSApp.terminate(nil) }
                .buttonStyle(.chip)
                .frame(maxWidth: .infinity, alignment: .trailing)
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
        account.displayName?.isEmpty == false ? account.displayName! : account.externalAccountId
    }

    private var intervalOptions: [UInt64] {
        var options: [UInt64] = [60, 120, 300, 600, 900, 1800, 3600]
        let current = state.config?.pollIntervalSeconds ?? 300
        if !options.contains(current) { options.append(current); options.sort() }
        return options
    }

    private var notificationsBinding: Binding<Bool> {
        Binding(
            get: { state.config?.notifications.enabled ?? true },
            set: { enabled in Task { await state.setNotificationsEnabled(enabled) } }
        )
    }

    private var darkModeBinding: Binding<Bool> {
        Binding(
            get: { state.ui.darkModeEnabled },
            set: { state.ui.darkModeEnabled = $0 }
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
    private var busy: Bool {
        state.pendingProviders.contains(provider.providerId)
            || state.pendingAccountProviders.contains(provider.providerId)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.md) {
            HStack(spacing: Theme.Spacing.sm) {
                ProviderIcon(id: provider.providerId, symbol: provider.symbol, size: 18)
                    .frame(width: 20)
                VStack(alignment: .leading, spacing: 1) {
                    Text(provider.name).font(Theme.Typography.headline)
                    Text(provider.visibleInMenu ? provider.healthText : "Hidden")
                        .font(Theme.Typography.micro)
                        .foregroundStyle(provider.visibleInMenu ? provider.status.tint : .secondary)
                }
                Spacer()
                if state.pendingProviders.contains(provider.providerId) {
                    ProgressView().controlSize(.small)
                } else {
                    Toggle("", isOn: visibilityBinding)
                        .labelsHidden()
                        .toggleStyle(.switch)
                        .disabled(state.daemon == .offline)
                        .help(provider.visibleInMenu ? "Hide \(provider.name)" : "Show \(provider.name)")
                }
            }

            Divider()
            if accounts.isEmpty {
                Text("No account connected")
                    .font(Theme.Typography.caption)
                    .foregroundStyle(.secondary)
            } else {
                VStack(spacing: Theme.Spacing.xs) {
                    ForEach(accounts) { account in
                        AccountSettingsRow(account: account)
                    }
                }
            }

            if state.supportsWorkspaceSetup(provider.providerId) { workspaceControl }

            if hasPrimaryAction {
                HStack(spacing: Theme.Spacing.sm) {
                    Button(actionLabel) { Task { await primaryAction() } }
                        .buttonStyle(.chipProminent)
                        .disabled(busy || state.daemon == .offline)
                    if busy { ProgressView().controlSize(.small) }
                    Spacer()
                }
            }
        }
        .surfaceCard()
        .task {
            if state.supportsWorkspaceSetup(provider.providerId), setup == nil {
                await state.loadProviderSetup(provider.providerId)
            }
        }
    }

    @ViewBuilder private var workspaceControl: some View {
        if let setup, !setup.workspaceOptions.isEmpty {
            Picker("Workspace", selection: workspaceBinding(setup)) {
                ForEach(setup.workspaceOptions, id: \.self) { Text($0).tag($0) }
            }
            .pickerStyle(.menu)
            .controlSize(.small)
            .disabled(busy)
        }
    }

    private var visibilityBinding: Binding<Bool> {
        Binding(
            get: { provider.visibleInMenu },
            set: { enabled in Task { await state.setProviderEnabled(provider.providerId, enabled) } }
        )
    }

    private func workspaceBinding(_ setup: ProviderSetupResponse) -> Binding<String> {
        let fallback = setup.selectedWorkspaceId ?? setup.workspaceOptions.first ?? ""
        return Binding(
            get: { state.providerSetups[provider.providerId]?.selectedWorkspaceId ?? fallback },
            set: { workspace in Task { await state.selectWorkspace(providerId: provider.providerId, workspaceId: workspace) } }
        )
    }

    private var actionLabel: String {
        if state.supportsAddAccount(provider.providerId) {
            accounts.isEmpty ? "Connect account" : "Add account"
        } else {
            accounts.isEmpty ? "Sign in" : "Reconnect"
        }
    }

    private var hasPrimaryAction: Bool {
        state.supportsAddAccount(provider.providerId) || state.supportsRepair(provider.providerId)
    }

    private func primaryAction() async {
        if state.supportsAddAccount(provider.providerId) {
            await state.addProviderAccount(provider.providerId)
        } else if state.supportsRepair(provider.providerId) {
            await state.repairProvider(provider.providerId, accountId: accounts.first?.id)
        }
    }

    private func accountLabel(_ account: Account) -> String {
        account.displayName?.isEmpty == false ? account.displayName! : account.externalAccountId
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
                Text(account.email.map { "\($0) · \(statusText)" } ?? statusText)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(needsSignIn ? .orange : .secondary)
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
                if needsSignIn, state.supportsRepair(account.providerId) {
                    Button("Reconnect") {
                        Task { await state.repairProvider(account.providerId, accountId: account.id) }
                    }
                    .buttonStyle(.chip)
                }
                Toggle("", isOn: collectionBinding)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .disabled(state.daemon == .offline)
                    .help(account.collectionEnabled ? "Pause tracking" : "Resume tracking")
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
            if state.supportsLaunchAccount(account.providerId), !isRemoved {
                Button("Open \(ProviderCatalog.name(for: account.providerId)) session") {
                    Task { await state.launchProviderAccount(account.id) }
                }
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
            if state.supportsRepair(account.providerId) {
                Button("Reconnect") {
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
    }

    private var collectionBinding: Binding<Bool> {
        Binding(
            get: { account.collectionEnabled },
            set: { enabled in Task { await state.setAccountCollectionEnabled(account.id, enabled) } }
        )
    }

    private var title: String {
        if let displayName = account.displayName, !displayName.isEmpty { return displayName }
        return account.externalAccountId
    }

    private var accountHealth: ProviderHealth? {
        state.health.first { $0.accountId == account.id }
    }

    private var needsSignIn: Bool {
        switch accountHealth?.status {
        case .credentialsMissing, .authFailed: true
        default: false
        }
    }

    private var statusText: String {
        if isRemoved { return "Removed · history kept" }
        if !account.collectionEnabled { return "Paused" }
        if needsSignIn { return "Needs sign-in" }
        return account.hidden ? "Active · hidden from summary" : "Active"
    }
}
