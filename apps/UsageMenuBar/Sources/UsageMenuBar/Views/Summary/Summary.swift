import SwiftUI

struct Summary: View {
    @EnvironmentObject var state: AppState
    @Binding var selection: Selection

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg - 2) {
            Header(title: "UsageTracker", subtitleStyle: summarySubtitle)
            ScrollView {
                LazyVStack(spacing: Theme.Spacing.xs + 2) {
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
                            providerGroupSection(group: group, accounts: subAccounts)
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
            }
        }
        .padding(Theme.Spacing.lg)
    }

    private var summarySubtitle: HeaderSubtitleStyle {
        if state.daemon == .offline { return .offline }
        guard let date = state.lastSuccessfulRefresh else { return .custom("waiting for first successful refresh") }
        return .custom("last refreshed \(DateFormats.relative.localizedString(for: date, relativeTo: Date()))")
    }

    @ViewBuilder
    private func providerGroupSection(group: ProviderVM, accounts: [ProviderVM]) -> some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.xs) {
            HStack(spacing: Theme.Spacing.sm) {
                ProviderIcon(id: group.providerId, symbol: group.symbol)
                    .frame(width: 15, height: 15)
                Text(group.name)
                    .font(Theme.Typography.caption.bold())
                    .foregroundStyle(.secondary)
                Spacer()
                Text("\(accounts.count) accounts")
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.tertiary)
            }
            .padding(.horizontal, Theme.Spacing.sm)
            .padding(.top, Theme.Spacing.xs)

            ForEach(accounts) { account in
                ProviderRow(provider: account) {
                    selection = .provider(group.id, accountId: account.accountId)
                }
                    .transition(.scale(scale: 0.96).combined(with: .opacity))
            }
        }
    }
}
