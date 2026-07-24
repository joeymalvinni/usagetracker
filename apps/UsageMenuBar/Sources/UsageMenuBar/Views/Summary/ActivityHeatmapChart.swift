import SwiftUI

/// GitHub-style contribution grid: each column is a local-calendar week
/// (Monday-based) and each row is a weekday. Cells are square and the grid
/// fills the card edge to edge — like GitHub's year view, the weeks older
/// than the 30-day data window render as blank squares padding the left.
///
/// The grid totals each day's providers, so it works for the aggregate
/// summary (single accent hue) and for a provider page (dashboard already
/// filtered to that provider, tinted with the provider color).
struct ActivityHeatmap: View {
    let days: [CostDayVM]
    let metric: CostMetric
    let color: Color
    @Binding var hover: CostProviderDayVM?
    /// Drill-down target for a tapped day: the day's top provider.
    var onSelectProvider: ((String) -> Void)?

    @State private var hoveredKey: String?

    private let calendar = ActivityCalendar()
    private let labelWidth: CGFloat = 22
    private let monthRowHeight: CGFloat = 12
    private let legendRowHeight: CGFloat = 12
    private let sectionSpacing: CGFloat = 4
    private let rowSpacing: CGFloat = 3
    private let columnSpacing: CGFloat = 3

    private var entries: [ActivityCalendar.Entry] {
        calendar.entries(for: days)
    }

    private var maximum: Double {
        max(1, entries.map { value($0.day) }.max() ?? 1)
    }

    var body: some View {
        GeometryReader { geo in
            let cell = cellSize(height: geo.size.height)
            let count = columnCount(width: geo.size.width, cell: cell)
            let spacing = resolvedSpacing(width: geo.size.width, cell: cell, count: count)
            let columns = self.columns(count: count)
            VStack(spacing: sectionSpacing) {
                Spacer(minLength: 0)
                VStack(alignment: .leading, spacing: sectionSpacing) {
                    monthRow(columns: columns, cellSize: cell, spacing: spacing)
                        .frame(height: monthRowHeight)
                        .padding(.leading, labelWidth + 6)
                    HStack(alignment: .top, spacing: 6) {
                        weekdayLabels(cellSize: cell)
                        weekColumns(columns: columns, cellSize: cell, spacing: spacing)
                    }
                }
                Spacer(minLength: 0)
                legend(swatch: cell)
                    .frame(height: legendRowHeight)
            }
        }
        .contentShape(Rectangle())
        .onHover { inside in
            if !inside {
                hover = nil
                hoveredKey = nil
            }
        }
    }

    /// Square cells, as large as the height allows.
    private func cellSize(height: CGFloat) -> CGFloat {
        max(6, floor((height
            - monthRowHeight - legendRowHeight - 2 * sectionSpacing
            - 6 * rowSpacing) / 7))
    }

    /// As many week columns as fit across the card.
    private func columnCount(width: CGFloat, cell: CGFloat) -> Int {
        max(1, Int((width - labelWidth - 6 + columnSpacing) / (cell + columnSpacing)))
    }

    /// Leftover points are spread across the column gaps so the grid touches
    /// both edges exactly.
    private func resolvedSpacing(width: CGFloat, cell: CGFloat, count: Int) -> CGFloat {
        guard count > 1 else { return columnSpacing }
        return (width - labelWidth - 6 - CGFloat(count) * cell) / CGFloat(count - 1)
    }

    /// Week columns, oldest first, ending at the week containing the most
    /// recent day (or today when there's no data yet).
    private func columns(count: Int) -> [Date] {
        let lastWeek = calendar.weekStart(for: days.last?.date ?? Date())
        return (0..<count).map {
            Calendar.current.date(byAdding: .day, value: -7 * (count - 1 - $0), to: lastWeek)
                ?? lastWeek
        }
    }

    /// Month abbreviations aligned with the week column where each month
    /// starts — same column widths as the grid so labels track the cells.
    private func monthRow(columns: [Date], cellSize: CGFloat, spacing: CGFloat) -> some View {
        let labels = shownMonthLabels(columns: columns)
        return HStack(spacing: spacing) {
            ForEach(Array(columns.enumerated()), id: \.offset) { index, _ in
                Text(labels[index] ?? "")
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                    .fixedSize()
                    .frame(width: cellSize, alignment: .leading)
            }
        }
    }

    /// A label needs roughly two column slots; on a collision the newer month
    /// wins — most of the window is in the newer month.
    private func shownMonthLabels(columns: [Date]) -> [Int: String] {
        var result: [Int: String] = [:]
        var lastShown = Int.max
        for (index, weekStart) in columns.enumerated().reversed() {
            let label = monthLabel(at: index, weekStart: weekStart, in: columns)
            guard !label.isEmpty, lastShown - index >= 2 else { continue }
            result[index] = label
            lastShown = index
        }
        return result
    }

    /// Label the column that contains the 1st of a month, like GitHub. A
    /// week straddling a month boundary therefore gets the new month's label
    /// even though its Monday is in the previous month.
    private func monthLabel(at index: Int, weekStart: Date, in columns: [Date]) -> String {
        func weekEnd(of week: Date) -> Date {
            Calendar.current.date(byAdding: .day, value: 6, to: week) ?? week
        }
        let month = Calendar.current.component(.month, from: weekEnd(of: weekStart))
        guard index == 0
                || month != Calendar.current.component(.month, from: weekEnd(of: columns[index - 1]))
        else { return "" }
        return weekEnd(of: weekStart).formatted(.dateTime.month(.abbreviated))
    }

    private func weekdayLabels(cellSize: CGFloat) -> some View {
        VStack(spacing: rowSpacing) {
            ForEach(0..<7, id: \.self) { weekday in
                Text(weekdayLabel(weekday))
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.tertiary)
                    .frame(width: labelWidth, height: cellSize, alignment: .leading)
            }
        }
    }

    private func weekColumns(columns: [Date], cellSize: CGFloat, spacing: CGFloat) -> some View {
        HStack(spacing: spacing) {
            ForEach(columns, id: \.self) { weekStart in
                VStack(spacing: rowSpacing) {
                    ForEach(0..<7, id: \.self) { weekday in
                        dayCell(weekStart: weekStart, weekday: weekday)
                            .frame(width: cellSize, height: cellSize)
                    }
                }
            }
        }
    }

    /// Days inside the data window are interactive. Days before it render as
    /// inert blank squares (the left padding), and days after today render as
    /// nothing, trimming the in-progress week the way GitHub trims the future.
    @ViewBuilder
    private func dayCell(weekStart: Date, weekday: Int) -> some View {
        if let entry = entries.first(where: {
            $0.weekStart == weekStart && $0.weekday == weekday
        }) {
            ActivityCell(
                value: value(entry.day),
                maximum: maximum,
                color: color
            )
            .overlay {
                if hoveredKey == entry.day.id {
                    RoundedRectangle(cornerRadius: 1.5, style: .continuous)
                        .stroke(Color.primary.opacity(0.4), lineWidth: 1)
                }
            }
            .contentShape(Rectangle())
            .onHover { inside in
                if inside {
                    hover = dayTotal(entry.day)
                    hoveredKey = entry.day.id
                } else if hoveredKey == entry.day.id {
                    hover = nil
                    hoveredKey = nil
                }
            }
            .onTapGesture {
                guard let top = entry.day.providers.max(by: { value($0) < value($1) }) else { return }
                onSelectProvider?(top.providerId)
            }
            .help(helpText(entry.day))
            .accessibilityLabel(helpText(entry.day))
        } else {
            let date = Calendar.current.date(byAdding: .day, value: weekday, to: weekStart)
            let isFuture = (date ?? weekStart) > Calendar.current.startOfDay(for: Date())
            if isFuture {
                Color.clear
            } else {
                ActivityCell(value: 0, maximum: 1, color: color)
            }
        }
    }

    private func legend(swatch: CGFloat) -> some View {
        HStack(spacing: 3) {
            Spacer()
            Text("Less")
            ForEach(0..<5, id: \.self) { level in
                RoundedRectangle(cornerRadius: 1.5, style: .continuous)
                    .fill(level == 0
                        ? Color.primary.opacity(0.055)
                        : color.opacity(0.22 + Double(level) * 0.195))
                    .frame(width: swatch, height: swatch)
            }
            Text("More")
        }
        .font(Theme.Typography.micro)
        .foregroundStyle(.tertiary)
    }

    private func value(_ day: CostDayVM) -> Double {
        metric == .cost ? day.totalCost : Double(day.totalTokens)
    }

    private func value(_ provider: CostProviderDayVM) -> Double {
        metric == .cost ? provider.cost : Double(provider.tokens)
    }

    /// Shared hover contract with the bar chart: the cards show this in their
    /// subtitle. Day totals stand in for the per-segment provider value.
    private func dayTotal(_ day: CostDayVM) -> CostProviderDayVM {
        CostProviderDayVM(
            providerId: "total",
            providerName: "All providers",
            symbol: "",
            date: day.date,
            dateKey: day.id,
            cost: day.totalCost,
            tokens: day.totalTokens
        )
    }

    private func helpText(_ day: CostDayVM) -> String {
        let formatted = metric == .cost ? formatUsd(day.totalCost) : formatTokens(day.totalTokens)
        let headline = "\(formatted) on \(shortDate(day.date))"
        let breakdown = day.providers
            .sorted { value($0) > value($1) }
            .prefix(3)
            .map { "\($0.providerName): \(metric == .cost ? formatUsd($0.cost) : formatTokens($0.tokens))" }
        return ([headline] + breakdown).joined(separator: "\n")
    }

    private func weekdayLabel(_ weekday: Int) -> String {
        switch weekday {
        case 0: "Mon"
        case 2: "Wed"
        case 4: "Fri"
        default: ""
        }
    }
}

struct ActivityCell: View {
    let value: Double
    let maximum: Double
    let color: Color

    var body: some View {
        RoundedRectangle(cornerRadius: 1.5, style: .continuous)
            .fill(fill)
            .overlay {
                RoundedRectangle(cornerRadius: 1.5, style: .continuous)
                    .stroke(Color.primary.opacity(value > 0 ? 0.06 : 0.04), lineWidth: 0.5)
            }
    }

    private var fill: Color {
        guard value > 0, maximum > 0 else {
            return Color.primary.opacity(0.055)
        }
        let normalized = min(1, max(0, value / maximum))
        let level = max(1, Int(ceil(sqrt(normalized) * 4)))
        return color.opacity(0.22 + Double(level) * 0.195)
    }
}

struct ActivityCalendar {
    struct Entry {
        let day: CostDayVM
        let weekStart: Date
        let weekday: Int
    }

    private var calendar: Calendar {
        var value = Calendar(identifier: .gregorian)
        value.timeZone = .autoupdatingCurrent
        return value
    }

    /// Monday of the week containing `date`.
    func weekStart(for date: Date) -> Date {
        let start = calendar.startOfDay(for: date)
        let weekday = (calendar.component(.weekday, from: start) + 5) % 7
        return calendar.date(byAdding: .day, value: -weekday, to: start) ?? start
    }

    func entries(for days: [CostDayVM]) -> [Entry] {
        days.map { day in
            let start = calendar.startOfDay(for: day.date)
            let weekday = (calendar.component(.weekday, from: start) + 5) % 7
            return Entry(day: day, weekStart: weekStart(for: day.date), weekday: weekday)
        }
    }
}
