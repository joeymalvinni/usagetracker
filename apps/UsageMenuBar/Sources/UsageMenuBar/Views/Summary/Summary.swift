import SwiftUI

struct Summary: View {
    @EnvironmentObject var state: AppState
    @ObservedObject var updater: AppUpdater
    @Binding var selection: Selection
    @State private var showsReleaseNotes = false

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg - 2) {
            Header(
                title: "UsageTracker",
                subtitleStyle: summarySubtitle,
                updateAction: updateAction
            )
            if let error = updater.installError {
                SetupNotice(text: error, isError: true)
            }
            ScrollView {
                LazyVStack(spacing: Theme.Spacing.xs + 2) {
                    if state.cost.hasData {
                        CostDashboard(dashboard: state.cost) { providerId in
                            selection = .provider(providerId, accountId: nil)
                        }
                    }
                    if state.providers.isEmpty {
                        EmptyState(
                            text: state.daemon == .offline ? "Usage is unavailable right now" : "Connect an account to see your usage",
                            retry: state.daemon == .offline ? { Task { await state.refreshAll() } } : nil,
                            isError: state.daemon == .offline
                        )
                        Button("Connect an account") {
                            Task { await state.restartOnboarding() }
                        }
                        .buttonStyle(.chipProminent)
                    }
                    ForEach(state.providers) { group in
                        if let subAccounts = group.subAccounts, subAccounts.count > 1 {
                            AccountCarouselRow(provider: group, accounts: subAccounts) { accountId in
                                selection = .provider(group.id, accountId: accountId)
                            }
                                .transition(.scale(scale: 0.96).combined(with: .opacity))
                        } else {
                            ProviderRow(provider: group) {
                                selection = .provider(group.id, accountId: nil)
                            }
                                .transition(.scale(scale: 0.96).combined(with: .opacity))
                        }
                    }
                    if state.shouldOfferNotifications {
                        NotificationOfferCard()
                    }
                    if let notes = updater.installedReleaseNotes,
                       state.showsReleaseNotes(notes) {
                        DisclosureGroup("What’s new in \(notes.version)", isExpanded: $showsReleaseNotes) {
                            ReleaseNotesCard(notes: notes) {
                                showsReleaseNotes = false
                                state.dismissReleaseNotes(notes)
                            }
                            .padding(.top, Theme.Spacing.sm)
                        }
                        .font(Theme.Typography.caption)
                        .foregroundStyle(.secondary)
                        .padding(.top, Theme.Spacing.sm)
                    }
                }
                .padding(.bottom, Theme.Spacing.sm)
                .animation(.spring(duration: 0.35), value: state.providers.map(\.id))
                .animation(.spring(duration: 0.3), value: updater.installedReleaseNotes?.version)
            }
        }
        .padding(Theme.Spacing.lg)
    }

    private var updateAction: HeaderUpdateAction? {
        guard let release = updater.availableRelease else { return nil }
        return HeaderUpdateAction(
            version: release.version.description,
            isInstalling: updater.isInstalling,
            perform: { Task { await updater.installAvailableUpdate() } }
        )
    }

    private var summarySubtitle: HeaderSubtitleStyle {
        if state.daemon == .offline { return .offline }
        if state.connectivity.status == .offline { return .networkOffline }
        guard let date = state.lastSuccessfulRefresh else {
            return .custom(state.providers.isEmpty ? "Your AI usage in one place" : "Getting your usage…")
        }
        return .custom("last refreshed \(DateFormats.relative.localizedString(for: date, relativeTo: Date()))")
    }
}

private struct NotificationOfferCard: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
            HStack(alignment: .top, spacing: Theme.Spacing.sm) {
                Image(systemName: "bell.badge")
                    .foregroundStyle(.tint)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Get a heads-up before you run low")
                        .font(Theme.Typography.headline)
                    Text("Get notified when your remaining allowance drops or a limit resets.")
                        .font(Theme.Typography.caption)
                        .foregroundStyle(.secondary)
                }
            }
            HStack {
                Button("Not now") { state.dismissNotificationOffer() }
                    .buttonStyle(.chip)
                Spacer()
                Button("Enable alerts") {
                    Task { await state.acceptNotificationOffer() }
                }
                .buttonStyle(.chipProminent)
            }
        }
        .surfaceCard()
    }
}
