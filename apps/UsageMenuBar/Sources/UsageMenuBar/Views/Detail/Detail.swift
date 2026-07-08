import SwiftUI

struct Detail: View {
    @EnvironmentObject var state: AppState
    let provider: ProviderVM?

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg - 2) {
            if let p = provider {
                Header(title: p.name, subtitleStyle: .custom([p.account, p.detail, p.healthText].compactMap(\.self).joined(separator: " · ")))
                ScrollView {
                    VStack(spacing: Theme.Spacing.xs + 2) {
                        ForEach(p.windows) { WindowRow(window: $0) }
                        if !p.credits.isEmpty {
                            Text("Credits")
                                .font(Theme.Typography.caption.bold())
                                .foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity, alignment: .leading)
                            ForEach(p.credits) { WindowRow(window: $0) }
                        }
                    }
                }
            } else {
                EmptyState(text: "Provider not found", isError: true)
            }
        }
        .padding(Theme.Spacing.lg)
        .transition(.opacity.combined(with: .move(edge: .trailing)))
    }
}