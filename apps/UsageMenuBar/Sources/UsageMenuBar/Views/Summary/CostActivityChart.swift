import SwiftUI

/// Polished stacked bar chart with rounded top segment, subtle gridlines,
/// and an axis baseline.
struct CostActivityChart: View {
    let days: [CostDayVM]
    let metric: CostMetric
    @Binding var hover: CostProviderDayVM?
    var providerColor: Color?
    var onSelectProvider: ((String) -> Void)?

    private let axisHeight: CGFloat = 16
    private let axisLabelWidth: CGFloat = 28

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
            let chartHeight = max(0, geo.size.height - axisHeight)
            let barSpacing: CGFloat = days.count > 7 ? 2 : 6
            VStack(spacing: 0) {
                ZStack(alignment: .bottom) {
                    gridlines(chartHeight: chartHeight)
                    HStack(alignment: .bottom, spacing: barSpacing) {
                        ForEach(days) { day in
                            ZStack(alignment: .bottom) {
                                RoundedRectangle(cornerRadius: 4)
                                    .fill(.quaternary.opacity(0.30))
                                barStack(day: day, maxHeight: chartHeight - 4)
                            }
                            .frame(maxWidth: .infinity)
                            .clipShape(RoundedRectangle(cornerRadius: 4))
                        }
                    }
                    baseline
                }
                .frame(height: chartHeight)
                axis(width: geo.size.width, barSpacing: barSpacing)
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
    private func gridlines(chartHeight: CGFloat) -> some View {
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

    private var baseline: some View {
        Rectangle()
            .fill(.quaternary.opacity(0.60))
            .frame(height: 1)
            .frame(maxWidth: .infinity)
    }

    private func segmentHeight(_ provider: CostProviderDayVM, maxHeight: CGFloat) -> CGFloat {
        let scaled = maxHeight * CGFloat(value(provider) / maxValue)
        return max(3, scaled)
    }

    private func axis(width: CGFloat, barSpacing: CGFloat) -> some View {
        ZStack(alignment: .topLeading) {
            ForEach(Array(days.enumerated()), id: \.element.id) { index, day in
                if shouldShowLabel(at: index) {
                    Text(label(for: day.date))
                        .font(Theme.Typography.micro.monospacedDigit())
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                        .frame(width: axisLabelWidth)
                        .position(
                            x: labelCenter(
                                at: index,
                                chartWidth: width,
                                barSpacing: barSpacing
                            ),
                            y: axisHeight / 2
                        )
                }
            }
        }
        .frame(width: width, height: axisHeight, alignment: .topLeading)
    }

    private func shouldShowLabel(at index: Int) -> Bool {
        if days.count <= 7 {
            return true
        }
        return index == 0 || index == days.count - 1 || index % 5 == 0
    }

    private func label(for date: Date) -> String {
        if days.count <= 7 {
            return date.formatted(.dateTime.weekday(.abbreviated))
        }
        return date.formatted(.dateTime.day())
    }

    private func labelCenter(at index: Int, chartWidth: CGFloat, barSpacing: CGFloat) -> CGFloat {
        guard !days.isEmpty else { return chartWidth / 2 }
        let spacingWidth = barSpacing * CGFloat(max(0, days.count - 1))
        let barWidth = max(0, (chartWidth - spacingWidth) / CGFloat(days.count))
        let barCenter = CGFloat(index) * (barWidth + barSpacing) + barWidth / 2
        let inset = axisLabelWidth / 2
        return min(max(inset, barCenter), max(inset, chartWidth - inset))
    }
}
