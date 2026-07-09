import SwiftUI

/// Polished stacked bar chart with rounded top segment, subtle gridlines,
/// and an axis baseline.
struct CostActivityChart: View {
    let days: [CostDayVM]
    let metric: CostMetric
    @Binding var hover: CostProviderDayVM?
    var providerColor: Color?
    var onSelectProvider: ((String) -> Void)?

    private var maxValue: Double {
        max(1, days.map(total).max() ?? 1)
    }

    private func value(_ provider: CostProviderDayVM) -> Double {
        metric == .cost ? provider.cost : Double(provider.tokens)
    }
    private func total(_ day: CostDayVM) -> Double {
        day.providers.reduce(0) { $0 + value($1) }
    }

    var body: some View {
        GeometryReader { geo in
            let chartHeight = geo.size.height - 18
            let baselineY = chartHeight - 2
            VStack(spacing: 0) {
                ZStack(alignment: .bottom) {
                    gridlines(in: geo.size, chartHeight: chartHeight)
                    HStack(alignment: .bottom, spacing: days.count > 7 ? 2 : 6) {
                        ForEach(Array(days.enumerated()), id: \.element.id) { index, day in
                            VStack(spacing: Theme.Spacing.xs - 2) {
                                ZStack(alignment: .bottom) {
                                    RoundedRectangle(cornerRadius: 4)
                                        .fill(.quaternary.opacity(0.30))
                                    barStack(day: day, maxHeight: chartHeight - 4)
                                }
                                .frame(maxWidth: .infinity)
                                .clipShape(RoundedRectangle(cornerRadius: 4))
                                Text(label(for: day.date, index: index))
                                    .font(Theme.Typography.micro.monospacedDigit())
                                    .foregroundStyle(.tertiary)
                                    .lineLimit(1)
                                    .frame(height: 12)
                            }
                        }
                    }
                    baseline(at: baselineY)
                }
            }
            .onHover { inside in if !inside { hover = nil } }
        }
    }

    @ViewBuilder
    private func barStack(day: CostDayVM, maxHeight: CGFloat) -> some View {
        VStack(spacing: 1) {
            Spacer(minLength: 0)
            ForEach(Array(day.providers.reversed().enumerated()), id: \.element.id) { index, provider in
                let visible = value(provider) > 0
                if visible {
                    RoundedRectangle(cornerRadius: index == 0 ? 4 : 1)
                        .fill(providerColor ?? Theme.chartColor(provider.providerId))
                        .frame(height: segmentHeight(provider, maxHeight: maxHeight))
                        .contentShape(Rectangle())
                        .onTapGesture {
                            onSelectProvider?(provider.providerId)
                        }
                        .onHover { inside in if inside { hover = provider } }
                        .help("\(provider.providerName) \(shortDate(provider.date)): \(metric == .cost ? formatUsd(provider.cost) : formatTokens(provider.tokens))")
                }
            }
        }
    }

    @ViewBuilder
    private func gridlines(in size: CGSize, chartHeight: CGFloat) -> some View {
        VStack {
            ForEach(0..<4, id: \.self) { i in
                Rectangle()
                    .fill(.quaternary.opacity(0.25))
                    .frame(height: 0.5)
                    .frame(maxWidth: .infinity)
                if i < 3 { Spacer().frame(height: (chartHeight - 4) / 3) }
            }
            Spacer(minLength: 0)
        }
    }

    @ViewBuilder
    private func baseline(at y: CGFloat) -> some View {
        Rectangle()
            .fill(.quaternary.opacity(0.60))
            .frame(height: 1)
            .frame(maxWidth: .infinity)
            .offset(y: y - (y / 2))
    }

    private func segmentHeight(_ provider: CostProviderDayVM, maxHeight: CGFloat) -> CGFloat {
        let scaled = maxHeight * CGFloat(value(provider) / maxValue)
        return max(3, scaled)
    }

    private func label(for date: Date, index: Int) -> String {
        if days.count <= 7 {
            return date.formatted(.dateTime.weekday(.narrow))
        }
        if index == 0 || index == days.count - 1 || index % 5 == 0 {
            return date.formatted(.dateTime.day())
        }
        return ""
    }
}
