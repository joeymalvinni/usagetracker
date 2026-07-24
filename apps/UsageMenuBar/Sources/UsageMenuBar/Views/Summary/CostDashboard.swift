import SwiftUI

enum CostRange: Int, CaseIterable {
    case seven = 7, thirty = 30
    var label: String { self == .seven ? "7d" : "30d" }
}

enum CostMetric: String, CaseIterable, Hashable {
    case cost, tokens
    var label: String { self == .cost ? "Cost" : "Tokens" }
}

struct CostDashboard: View {
    @EnvironmentObject var state: AppState
    let dashboard: CostDashboardVM
    var onSelectProvider: ((String) -> Void)?

    @State private var range: CostRange = .seven
    @State private var metric: CostMetric = .tokens
    @State private var hover: CostProviderDayVM?

    private var days: [CostDayVM] { Array(dashboard.days.suffix(range.rawValue)) }

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
            Group {
                if state.ui.activityChartStyle == .contributions {
                    ActivityHeatmap(
                        days: days,
                        metric: metric,
                        color: .accentColor,
                        hover: $hover,
                        onSelectProvider: onSelectProvider
                    )
                } else {
                    CostActivityChart(
                        days: days,
                        metric: metric,
                        hover: $hover,
                        providerColor: nil,
                        onSelectProvider: onSelectProvider
                    )
                }
            }
            .frame(height: state.ui.activityChartStyle == .contributions ? 110 : 120)
            // Single KPI strip driven by the active metric — no longer showing
            // cost and tokens simultaneously (the old 4-grid duplicated the
            // chart). Two figures: today + 30d, in the active metric's units.
            HStack(spacing: Theme.Spacing.sm) {
                CostKPI(title: "Today", value: todayValue)
                Divider().frame(height: 24)
                CostKPI(title: "30d", value: totalValue)
                Spacer()
            }
        }
        .surfaceCard()
        .animation(.spring(duration: 0.3), value: metric)
    }

    private var todayValue: String {
        metric == .cost ? formatUsd(dashboard.todayCost) : formatTokens(dashboard.todayTokens)
    }
    private var totalValue: String {
        metric == .cost ? formatUsd(dashboard.cost30d) : formatTokens(dashboard.tokens30d)
    }

    private var activitySubtitle: String {
        if dashboard.hasData {
            return "\(range.label) \(metric == .cost ? "cost" : "tokens")"
        }
        return "No activity yet"
    }
    private func hoverText(_ value: CostProviderDayVM) -> String {
        if metric == .cost {
            return "\(value.providerName) · \(shortDate(value.date)): \(formatUsd(value.cost))"
        } else {
            return "\(value.providerName) · \(shortDate(value.date)): \(formatTokens(value.tokens))"
        }
    }
}
