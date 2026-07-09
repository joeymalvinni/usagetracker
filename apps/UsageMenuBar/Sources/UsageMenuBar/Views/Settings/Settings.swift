import SwiftUI

struct Settings: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg - 2) {
            Header(title: "Settings", subtitleStyle: state.daemon == .offline ? .offline : .custom("changes apply immediately"), showsRefresh: false)
            if let error = state.actionError {
                Text(error)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(.red)
                    .padding(Theme.Spacing.xs)
                    .surfaceInset()
            }
            ScrollView {
                VStack(alignment: .leading, spacing: Theme.Spacing.lg - 8) {
                    Text("Providers").font(Theme.Typography.caption.bold()).foregroundStyle(.secondary)
                    providerList
                    Text("Refresh").font(Theme.Typography.caption.bold()).foregroundStyle(.secondary)
                    LabeledContent("Poll interval") {
                        if state.pendingInterval {
                            ProgressView().controlSize(.small)
                        } else {
                            Picker("", selection: intervalBinding) {
                                ForEach(intervalOptions, id: \.self) { Text(intervalLabel($0)).tag($0) }
                            }
                            .labelsHidden().fixedSize().disabled(state.daemon == .offline)
                        }
                    }
                    .surfaceCard()
                    Divider()
                    LabeledContent("Socket", value: state.config?.socketPath ?? "USAGE_TRACKER_SOCKET or ~/.usagetracker/usage.sock")
                    LabeledContent("Config", value: state.config?.configPath ?? "unknown")
                    LabeledContent("Database", value: state.config?.dbPath ?? "unknown")
                    LabeledContent("UI config", value: UIPaths.config.path)
                }
                .padding(.bottom, Theme.Spacing.xs + 2)
            }
            Spacer(minLength: 0)
            Button("Quit Usage") { NSApp.terminate(nil) }.frame(maxWidth: .infinity, alignment: .trailing)
        }
        .padding(Theme.Spacing.lg)
    }

    private var providerList: some View {
        VStack(spacing: 0) {
            List {
                ForEach(state.providers) { p in
                    ProviderSettingsRow(provider: p)
                        .listRowSeparator(.hidden)
                        .listRowBackground(Color.clear)
                        .listRowInsets(EdgeInsets(top: 3, leading: 0, bottom: 3, trailing: 0))
                }
                .onMove { from, to in state.moveProviders(from: from, to: to) }
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            .scrollDisabled(true)
            .environment(\.defaultMinListRowHeight, 44)
            .frame(height: CGFloat(state.providers.count) * 50)
            Text("Drag to reorder. Order applies to the summary.")
                .font(Theme.Typography.micro)
                .foregroundStyle(.tertiary)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private var intervalOptions: [UInt64] {
        var options: [UInt64] = [60, 120, 300, 600, 900, 1800, 3600]
        let current = state.config?.pollIntervalSeconds ?? 300
        if !options.contains(current) { options.append(current); options.sort() }
        return options
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
        case let s where s % 60 == 0: "\(s / 60) min"
        default: "\(seconds) sec"
        }
    }
}
