import SwiftUI

struct Summary: View {
    @EnvironmentObject var state: AppState
    @ObservedObject var updater: AppUpdater
    @Binding var selection: Selection

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
                    if let notes = updater.installedReleaseNotes,
                       state.showsReleaseNotes(notes) {
                        ReleaseNotesCard(notes: notes) {
                            state.dismissReleaseNotes(notes)
                        }
                        .transition(.move(edge: .top).combined(with: .opacity))
                    }
                    if state.providers.isEmpty {
                        EmptyState(
                            text: state.daemon == .offline ? "Daemon unavailable" : "No providers enabled",
                            retry: state.daemon == .offline ? { Task { await state.refreshAll() } } : nil,
                            isError: state.daemon == .offline
                        )
                    }
                    CostDashboard(dashboard: state.cost) { providerId in
                        selection = .provider(providerId, accountId: nil)
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
        guard let date = state.lastSuccessfulRefresh else { return .custom("waiting for first successful refresh") }
        return .custom("last refreshed \(DateFormats.relative.localizedString(for: date, relativeTo: Date()))")
    }
}
