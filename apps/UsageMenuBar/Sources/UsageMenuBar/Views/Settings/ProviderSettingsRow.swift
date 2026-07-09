import SwiftUI

struct ProviderSettingsRow: View {
    @EnvironmentObject var state: AppState
    let provider: ProviderVM

    var body: some View {
        HStack(spacing: Theme.Spacing.sm) {
            Image(systemName: "line.3.horizontal")
                .font(Theme.Typography.caption)
                .foregroundStyle(.tertiary)
                .frame(width: 10)
            ProviderIcon(id: provider.id, symbol: provider.symbol, size: 16)
                .foregroundStyle(.secondary)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 1) {
                Text(provider.name)
                    .font(Theme.Typography.body)
                    .lineLimit(1)
                Text(subtitle)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
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
        .padding(.horizontal, Theme.Spacing.md - 2)
        .frame(height: 44)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: Theme.Radius.md, style: .continuous)
                .fill(Color.primary.opacity(0.05))
        )
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
