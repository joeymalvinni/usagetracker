import SwiftUI

struct Settings: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg - 2) {
            Header(title: "Settings", subtitleStyle: state.daemon == .offline ? .offline : .custom("changes apply immediately"))
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
                    Text("Menu bar").font(Theme.Typography.caption.bold()).foregroundStyle(.secondary)
                    ForEach(state.providers.filter(\.enabled)) { p in
                        Toggle(isOn: visibleBinding(p.id)) {
                            Label { Text("Show \(p.name)") } icon: { ProviderIcon(id: p.id, symbol: p.symbol) }
                        }
                        .toggleStyle(.switch)
                        .surfaceCard()
                    }
                    VStack(spacing: Theme.Spacing.sm) {
                        LabeledContent("Metric") {
                            Picker("", selection: $state.ui.menuMetric) {
                                ForEach(UIConfig.MenuMetric.allCases, id: \.self) { Text($0.label).tag($0) }
                            }
                            .labelsHidden().fixedSize()
                        }
                        LabeledContent("Providers shown") {
                            Stepper(value: $state.ui.maxMenuProviders, in: 0...4) {
                                Text("\(state.ui.maxMenuProviders)").monospacedDigit().frame(minWidth: 14)
                            }
                            .fixedSize()
                        }
                        LabeledContent("Short labels") { Toggle("", isOn: $state.ui.showProviderLabels).labelsHidden().toggleStyle(.switch).controlSize(.small) }
                        LabeledContent("Color by status") { Toggle("", isOn: $state.ui.colorByStatus).labelsHidden().toggleStyle(.switch).controlSize(.small) }
                        LabeledContent("Preview") {
                            Text(state.menuPreview.isEmpty ? "Usage" : state.menuPreview)
                                .font(Theme.Typography.caption.monospacedDigit())
                                .foregroundStyle(.secondary)
                        }
                    }
                    .surfaceCard()
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
                        .listRowInsets(EdgeInsets(top: 4, leading: 0, bottom: 4, trailing: 0))
                }
                .onMove { from, to in state.moveProviders(from: from, to: to) }
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            .scrollDisabled(true)
            .frame(height: CGFloat(state.providers.count) * 64)
            Text("Drag to reorder. Order applies to the menu bar and summary.")
                .font(Theme.Typography.micro)
                .foregroundStyle(.tertiary)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func visibleBinding(_ id: String) -> Binding<Bool> {
        Binding(get: { state.visible(id) }, set: { state.setVisible(id, $0) })
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
