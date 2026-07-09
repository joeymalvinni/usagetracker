import SwiftUI

struct Summary: View {
    @EnvironmentObject var state: AppState
    @Binding var selection: Selection

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg - 2) {
            Header(title: "UsageTracker", subtitleStyle: state.daemon == .offline ? .offline : .custom(""))
            ScrollView {
                LazyVStack(spacing: Theme.Spacing.xs + 2) {
                    if state.providers.isEmpty {
                        EmptyState(
                            text: state.daemon == .offline ? "Daemon unavailable" : "No providers enabled",
                            retry: state.daemon == .offline ? { Task { await state.refreshAll() } } : nil,
                            isError: state.daemon == .offline
                        )
                    }
                    CostDashboard(dashboard: state.cost)
                    ForEach(state.providers) { p in
                        ProviderRow(provider: p) {
                            selection = .provider(p.id)
                        }
                            .transition(.scale(scale: 0.96).combined(with: .opacity))
                    }
                }
                .padding(.bottom, Theme.Spacing.sm)
                .animation(.spring(duration: 0.35), value: state.providers.map(\.id))
            }
        }
        .padding(Theme.Spacing.lg)
    }
}
