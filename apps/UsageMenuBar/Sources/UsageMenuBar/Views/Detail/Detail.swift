import SwiftUI

struct Detail: View {
    @EnvironmentObject var state: AppState
    let provider: ProviderVM?

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg - 2) {
            if let p = provider {
                Header(title: p.name, subtitleStyle: .custom([p.account, p.detail, p.healthText].compactMap(\.self).joined(separator: " · "))) {
                    Task { await state.refreshProvider(p.id) }
                }
                ScrollView {
                    VStack(alignment: .leading, spacing: Theme.Spacing.md) {
                        ProviderActivityCard(provider: p, dashboard: state.cost)
                        if !limitWindows(p).isEmpty {
                            ProviderSection(title: "Limits") {
                                ForEach(limitWindows(p)) { WindowRow(window: $0) }
                            }
                        }
                        if p.id == "codex", !p.resetCredits.isEmpty {
                            ProviderSection(title: "Resets") {
                                ResetCreditDisclosure(provider: p)
                            }
                        }
                        if !p.spend.isEmpty {
                            ProviderSection(title: "Spend") {
                                ForEach(SpendLine.grouped(p.spend)) { line in
                                    SpendLineRow(line: line)
                                }
                            }
                        }
                        if !p.credits.isEmpty {
                            ProviderSection(title: "Credits") {
                                ForEach(p.credits) { WindowRow(window: $0) }
                            }
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

    private func limitWindows(_ provider: ProviderVM) -> [WindowVM] {
        provider.windows.filter { $0.id != "\(provider.id)_rate_limit_resets" }
    }
}

private struct ResetCreditDisclosure: View {
    let provider: ProviderVM
    @State private var expanded = false

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            VStack(alignment: .leading, spacing: Theme.Spacing.xs) {
                ForEach(provider.resetCredits) { credit in
                    HStack(alignment: .firstTextBaseline, spacing: Theme.Spacing.sm) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(credit.title)
                                .lineLimit(1)
                            Text(credit.status.capitalized)
                                .font(Theme.Typography.micro)
                                .foregroundStyle(.tertiary)
                        }
                        Spacer(minLength: Theme.Spacing.sm)
                        Text(credit.expiresText)
                            .monospacedDigit()
                            .foregroundStyle(credit.expiresAt.map { $0 <= Date() } == true ? .red : .secondary)
                            .lineLimit(1)
                    }
                    .font(Theme.Typography.caption)
                }
            }
            .padding(.top, Theme.Spacing.xs)
        } label: {
            HStack(spacing: Theme.Spacing.sm) {
                Label {
                    Text(summaryText)
                } icon: {
                    Image(systemName: "clock.arrow.circlepath")
                }
                .lineLimit(1)
                Spacer(minLength: Theme.Spacing.sm)
                Text("\(provider.resetCredits.count)")
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            .font(Theme.Typography.caption.weight(.medium))
        }
        .buttonStyle(.plain)
        .surfaceInset()
        .help(summaryText)
    }

    private var summaryText: String {
        guard let resetWindow else { return "\(provider.resetCredits.count) resets available" }
        let value = resetWindow.value.replacingOccurrences(of: " available", with: "")
        let noun = value == "1" ? "reset" : "resets"
        return "\(value) \(noun) available · \(resetWindow.reset)"
    }

    private var resetWindow: WindowVM? {
        provider.windows.first { $0.id == "\(provider.id)_rate_limit_resets" }
    }
}

private struct SpendLine: Identifiable {
    let id: String
    let label: String
    let values: [String]
    let providerName: String

    static func grouped(_ windows: [WindowVM]) -> [SpendLine] {
        var groups: [String: (label: String, values: [String], providerName: String)] = [:]
        var order: [String] = []

        for window in windows {
            let key = normalizedKey(window.label)
            if groups[key] == nil {
                groups[key] = (label: normalizedLabel(window.label), values: [], providerName: window.providerName)
                order.append(key)
            }
            groups[key]?.values.append(window.value)
        }

        return order.compactMap { key in
            guard let group = groups[key] else { return nil }
            return SpendLine(id: key, label: group.label, values: group.values, providerName: group.providerName)
        }
    }

    private static func normalizedKey(_ label: String) -> String {
        label
            .lowercased()
            .replacingOccurrences(of: " spend ", with: " ")
            .replacingOccurrences(of: " tokens ", with: " ")
    }

    private static func normalizedLabel(_ label: String) -> String {
        label
            .replacingOccurrences(of: " spend ", with: " ")
            .replacingOccurrences(of: " tokens ", with: " ")
    }
}

private struct SpendLineRow: View {
    let line: SpendLine

    var body: some View {
        HStack {
            Text(line.label)
                .lineLimit(1)
            Spacer()
            Text(line.values.joined(separator: " · "))
                .foregroundStyle(.secondary)
                .monospacedDigit()
                .lineLimit(1)
        }
        .font(Theme.Typography.caption)
        .surfaceInset()
        .help("\(line.providerName) · \(line.label)")
    }
}

private struct ProviderActivityCard: View {
    let provider: ProviderVM
    let dashboard: CostDashboardVM

    @State private var range: CostRange = .seven
    @State private var metric: CostMetric = .tokens
    @State private var hover: CostProviderDayVM?

    private var days: [CostDayVM] {
        dashboard.days.suffix(range.rawValue).map { day in
            CostDayVM(
                id: day.id,
                date: day.date,
                providers: day.providers.filter { $0.providerId == provider.id }
            )
        }
    }

    private var providerDays: [CostProviderDayVM] {
        days.flatMap(\.providers)
    }

    private var hasData: Bool {
        providerDays.contains { $0.cost > 0 || $0.tokens > 0 }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
            HStack(alignment: .top, spacing: Theme.Spacing.sm) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Activity").font(Theme.Typography.headline)
                    Text(hover.map(hoverText) ?? activitySubtitle)
                        .font(Theme.Typography.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer(minLength: Theme.Spacing.sm)
                Picker("", selection: $range) {
                    ForEach(CostRange.allCases, id: \.self) { Text($0.label).tag($0) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(width: 82)
                Picker("", selection: $metric) {
                    ForEach(CostMetric.allCases, id: \.self) { Text($0.label).tag($0) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(width: 110)
            }

            CostActivityChart(days: days, metric: metric, hover: $hover)
                .frame(height: 126)
                .opacity(hasData ? 1 : 0.55)

            HStack(spacing: Theme.Spacing.sm) {
                CostKPI(title: "Today", value: todayValue)
                Divider().frame(height: 24)
                CostKPI(title: "\(range.label) total", value: totalValue)
                Divider().frame(height: 24)
                CostKPI(title: "Peak", value: peakValue)
            }
        }
        .surfaceCard()
        .animation(.spring(duration: 0.3), value: range)
        .animation(.spring(duration: 0.3), value: metric)
    }

    private var activitySubtitle: String {
        guard hasData else { return "No recent cost or token activity" }
        return "\(range.label) \(metric == .cost ? "spend" : "tokens")"
    }

    private var todayValue: String {
        guard let today = providerDays.last else { return metric == .cost ? formatUsd(0) : formatTokens(0) }
        return formatted(today)
    }

    private var totalValue: String {
        if metric == .cost {
            return formatUsd(providerDays.reduce(0) { $0 + $1.cost })
        }
        return formatTokens(providerDays.reduce(UInt64(0)) { $0.saturatingAdd($1.tokens) })
    }

    private var peakValue: String {
        guard let peak = providerDays.max(by: { value($0) < value($1) }) else {
            return metric == .cost ? formatUsd(0) : formatTokens(0)
        }
        return formatted(peak)
    }

    private func value(_ day: CostProviderDayVM) -> Double {
        metric == .cost ? day.cost : Double(day.tokens)
    }

    private func formatted(_ day: CostProviderDayVM) -> String {
        metric == .cost ? formatUsd(day.cost) : formatTokens(day.tokens)
    }

    private func hoverText(_ day: CostProviderDayVM) -> String {
        "\(shortDate(day.date)): \(formatted(day))"
    }
}

private struct ProviderSection<Content: View>: View {
    let title: String
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.xs) {
            Text(title)
                .font(Theme.Typography.caption.bold())
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, Theme.Spacing.sm)
                .padding(.bottom, 2)
            content
        }
    }
}
