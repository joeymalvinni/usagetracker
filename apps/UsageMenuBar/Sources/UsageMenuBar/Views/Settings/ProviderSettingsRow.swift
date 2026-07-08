import SwiftUI

struct ProviderSettingsRow: View {
    @EnvironmentObject var state: AppState
    let provider: ProviderVM

    var body: some View {
        HStack(spacing: Theme.Spacing.xs + 2) {
            Image(systemName: "line.3.horizontal").foregroundStyle(.tertiary)
            Label {
                VStack(alignment: .leading) {
                    Text(provider.name)
                    Text(subtitle)
                        .font(Theme.Typography.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            } icon: { ProviderIcon(id: provider.id, symbol: provider.symbol) }
            Spacer()
            if state.pendingProviders.contains(provider.id) {
                ProgressView().controlSize(.small)
            } else {
                Toggle("", isOn: enabledBinding)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .disabled(state.daemon == .offline)
                    .help(provider.enabled ? "Stop collecting \(provider.name) usage" : "Start collecting \(provider.name) usage")
            }
        }
        .surfaceCard()
    }

    private var subtitle: String {
        if let account = provider.account { return account }
        if !provider.enabled { return "collection off" }
        return provider.healthText
    }
    private var enabledBinding: Binding<Bool> {
        Binding(get: { provider.enabled }, set: { on in Task { await state.setProviderEnabled(provider.id, on) } })
    }
}