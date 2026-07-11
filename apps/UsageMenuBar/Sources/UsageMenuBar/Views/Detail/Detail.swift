import SwiftUI

struct Detail: View {
    @EnvironmentObject var state: AppState
    let providerId: String
    let initialAccountId: String?
    @State private var selectedAccountId: String?

    private var group: ProviderVM? {
        state.providers.first { $0.id == providerId || $0.providerId == providerId }
    }

    private var accounts: [ProviderVM] {
        group?.subAccounts ?? (group.map { [$0] } ?? [])
    }

    private var selectedAccount: ProviderVM? {
        if let selectedAccountId, let account = accounts.first(where: { $0.accountId == selectedAccountId }) {
            return account
        }
        return accounts.first
    }

    private var activeProvider: ProviderVM {
        selectedAccount ?? group ?? ProviderVM(
            id: providerId, providerId: providerId, accountId: nil,
            name: providerId, short: "", symbol: "chart.bar",
            primary: "No data", detail: "waiting for data",
            percent: nil, status: .stale,
            spend: [], windows: [], credits: [], resetCredits: [],
            account: nil, healthText: "unknown",
            visibleInMenu: false, enabled: false,
            secondary: "no activity", sparkline: [],
            costDashboard: .empty, subAccounts: nil
        )
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.lg - 2) {
            if let group {
                Header(title: group.name, subtitleStyle: subtitleStyle) {
                    Task { await state.refreshProvider(providerId) }
                }
                if state.showsAlertBanner(activeProvider) {
                    AlertBanner(
                        provider: activeProvider,
                        actionLabel: activeProvider.repairRecommended ? "Repair login" : "Refresh",
                        onAction: {
                            if activeProvider.repairRecommended {
                                Task { await state.repairProvider(providerId, accountId: activeProvider.accountId) }
                            } else {
                                Task { await state.refreshProvider(providerId) }
                            }
                        },
                        onDismiss: { state.dismissAlert(activeProvider) }
                    )
                }
                if accounts.count > 1 {
                    accountPicker
                }
                ScrollView {
                    VStack(alignment: .leading, spacing: Theme.Spacing.md) {
                        ProviderActivityCard(provider: activeProvider, dashboard: activeProvider.costDashboard)
                        if !limitWindows(activeProvider).isEmpty {
                            ProviderSection(title: "Limits") {
                                ForEach(limitWindows(activeProvider)) { window in
                                    WindowRow(window: window, resetExpandable: true, onHide: { state.hideWindow(window) })
                                }
                            }
                        }
                        if activeProvider.providerId == "codex", !activeProvider.resetCredits.isEmpty {
                            ProviderSection(title: "Resets") {
                                ResetCreditDisclosure(provider: activeProvider)
                            }
                        }
                        if !activeProvider.spend.isEmpty {
                            ProviderSection(title: "Cost") {
                                ForEach(SpendLine.grouped(activeProvider.spend)) { line in
                                    SpendLineRow(line: line)
                                }
                            }
                        }
                        if !activeProvider.credits.isEmpty {
                            ProviderSection(title: "Credits") {
                                ForEach(activeProvider.credits) { window in
                                    WindowRow(window: window, onHide: { state.hideWindow(window) })
                                }
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
        .onAppear {
            if selectedAccountId == nil {
                selectedAccountId = initialAccountId ?? accounts.first?.accountId
            }
            state.markAlertSeen(activeProvider)
        }
        .onChange(of: initialAccountId) { _, newValue in
            selectedAccountId = newValue ?? accounts.first?.accountId
        }
        .onChange(of: selectedAccountId) { _, _ in
            state.markAlertSeen(activeProvider)
        }
    }

    private var subtitleStyle: HeaderSubtitleStyle {
        let success = activeProvider.lastSuccessAt.map { "success \(DateFormats.relative.localizedString(for: $0, relativeTo: Date()))" }
        let parts = [selectedAccount?.account, success ?? activeProvider.detail, activeProvider.healthText].compactMap(\.self)
        return .custom(parts.joined(separator: " · "))
    }

    @ViewBuilder
    private var accountPicker: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: Theme.Spacing.xs) {
                ForEach(accounts, id: \.accountId) { account in
                    Button {
                        withAnimation(.spring(response: 0.4, dampingFraction: 0.82)) {
                            selectedAccountId = account.accountId
                        }
                    } label: {
                        HStack(spacing: Theme.Spacing.xs) {
                            if account.hasUnseenAlert {
                                Circle()
                                    .fill(account.status.tint)
                                    .frame(width: 6, height: 6)
                            }
                            Text(accountRowLabel(account))
                        }
                            .font(Theme.Typography.caption.weight(.medium))
                            .lineLimit(1)
                            .padding(.horizontal, Theme.Spacing.md)
                            .padding(.vertical, 6)
                            .background(
                                RoundedRectangle(cornerRadius: Theme.Radius.md, style: .continuous)
                                    .fill(selectedAccountId == account.accountId
                                        ? Color.primary.opacity(0.10)
                                        : Color.primary.opacity(0.04))
                            )
                            .overlay(
                                RoundedRectangle(cornerRadius: Theme.Radius.md, style: .continuous)
                                    .stroke(selectedAccountId == account.accountId
                                        ? Theme.chartColor(account.providerId).opacity(0.4)
                                        : Color.clear, lineWidth: 1)
                            )
                    }
                    .buttonStyle(.plain)
                    .help(account.account ?? account.name)
                }
            }
        }
    }

    private func accountRowLabel(_ account: ProviderVM) -> String {
        if let displayName = account.account, !displayName.isEmpty {
            return displayName
        }
        return account.name
    }

    private func providerAccount(_ id: String) -> ProviderVM? {
        state.providers.first { $0.id == id || $0.providerId == id }
    }

    private func limitWindows(_ provider: ProviderVM) -> [WindowVM] {
        provider.windows.filter { $0.id != "\(provider.providerId)_rate_limit_resets" }
    }
}

private struct AlertBanner: View {
    let provider: ProviderVM
    let actionLabel: String
    let onAction: () -> Void
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: Theme.Spacing.sm) {
            Image(systemName: provider.status == .critical ? "exclamationmark.triangle.fill" : "exclamationmark.circle.fill")
                .foregroundStyle(provider.status.tint)
                .font(Theme.Typography.caption)
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(Theme.Typography.caption.weight(.semibold))
                    .fixedSize(horizontal: false, vertical: true)
                if let subtitle {
                    Text(subtitle)
                        .font(Theme.Typography.micro)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: Theme.Spacing.sm)
            Button(actionLabel, action: onAction)
                .controlSize(.small)
            Button(action: onDismiss) {
                Image(systemName: "xmark")
                    .font(Theme.Typography.micro.weight(.bold))
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
            .help("Dismiss")
        }
        .padding(Theme.Spacing.sm)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(provider.status.tint.opacity(0.12), in: RoundedRectangle(cornerRadius: Theme.Radius.md, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: Theme.Radius.md, style: .continuous)
                .stroke(provider.status.tint.opacity(0.35), lineWidth: 1)
        )
    }

    private var worstWindow: WindowVM? {
        provider.windows
            .filter { $0.id != "\(provider.providerId)_rate_limit_resets" && $0.percent != nil }
            .min { ($0.percent ?? 100) < ($1.percent ?? 100) }
    }

    private var title: String {
        switch provider.status {
        case .critical:
            if (worstWindow?.percent ?? provider.percent ?? 100) <= 0 {
                return "You've reached your usage limit"
            }
            return "You're almost out of your usage limit"
        case .warning: return "You're running low on your usage limit"
        default: return provider.healthText == "unknown" ? provider.status.label.capitalized : provider.healthText.capitalized
        }
    }

    private var subtitle: String? {
        if provider.status == .error, let error = provider.errorDetail, !error.isEmpty {
            return error
        }
        if let window = worstWindow {
            let reset = window.reset.isEmpty ? "" : " · \(window.reset)"
            return "\(window.label): \(window.value)\(reset)"
        }
        return provider.status.isAlert && !provider.detail.isEmpty ? provider.detail : nil
    }
}

/// The "Resets" section on the Codex detail page. Codex hands out a pool of
/// rate-limit *reset credits* that each expire; this collapses them to a
/// one-line summary (count + soonest expiry, relative) and reveals the full
/// per-credit list on tap. The whole header toggles — not just the caret —
/// and the body is roomy enough to grow a pace estimate later.
private struct ResetCreditDisclosure: View {
    let provider: ProviderVM
    @State private var expanded = false

    var body: some View {
        VStack(spacing: 0) {
            Button {
                withAnimation(.spring(duration: 0.28)) { expanded.toggle() }
            } label: {
                header
            }
            .buttonStyle(.plain)

            if expanded {
                VStack(spacing: Theme.Spacing.xs) {
                    ForEach(provider.resetCredits) { credit in
                        Divider().opacity(0.35)
                        creditRow(credit)
                    }
                }
                .padding(.top, Theme.Spacing.sm)
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .surfaceInset()
        .help(headerHelp)
    }

    private var header: some View {
        HStack(spacing: Theme.Spacing.sm) {
            Image(systemName: "clock.arrow.circlepath")
                .foregroundStyle(.secondary)
            Text(countText)
                .fontWeight(.medium)
            if let nextExpiry {
                Text("·").foregroundStyle(.tertiary)
                Text(nextExpiry.text)
                    .foregroundStyle(nextExpiry.tint)
                    .monospacedDigit()
            }
            Spacer(minLength: Theme.Spacing.sm)
            Image(systemName: "chevron.down")
                .font(Theme.Typography.micro.weight(.semibold))
                .foregroundStyle(.tertiary)
                .rotationEffect(.degrees(expanded ? 0 : -90))
        }
        .font(Theme.Typography.caption)
        .lineLimit(1)
        .contentShape(Rectangle())
    }

    private func creditRow(_ credit: ResetCreditVM) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: Theme.Spacing.sm) {
            VStack(alignment: .leading, spacing: 2) {
                Text(credit.title)
                    .lineLimit(1)
                Text(credit.status.capitalized)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.tertiary)
            }
            Spacer(minLength: Theme.Spacing.sm)
            VStack(alignment: .trailing, spacing: 2) {
                Text(relativeExpiry(credit.expiresAt))
                    .monospacedDigit()
                    .foregroundStyle(expiryTint(credit.expiresAt))
                Text(credit.expiresText)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.tertiary)
            }
            .lineLimit(1)
        }
        .font(Theme.Typography.caption)
    }

    private var countText: String {
        let count = provider.resetCredits.count
        return "\(count) credit\(count == 1 ? "" : "s")"
    }

    /// Soonest-expiring credit, rendered relative ("expires in 2 days") with a
    /// tint that escalates as the deadline nears. `resetCredits` is sorted
    /// earliest-first upstream, so `.first` is the soonest boundary.
    private var nextExpiry: (text: String, tint: Color)? {
        guard let date = provider.resetCredits.first?.expiresAt else { return nil }
        let expired = date <= Date()
        let text = expired ? "expired \(relative(date))" : "expires \(relative(date))"
        return (text, expiryTint(date))
    }

    private func relativeExpiry(_ date: Date?) -> String {
        guard let date else { return "expiry unknown" }
        return date <= Date() ? "expired" : relative(date)
    }

    private func relative(_ date: Date) -> String {
        DateFormats.resetRelativeString(for: date)
    }

    private func expiryTint(_ date: Date?) -> Color {
        guard let date else { return .secondary }
        if date <= Date() { return .red }
        return date.timeIntervalSinceNow < 24 * 60 * 60 ? .orange : .secondary
    }

    private var headerHelp: String {
        guard let next = provider.resetCredits.first else { return countText }
        return "\(countText) · next \(next.expiresText)"
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
            .replacingOccurrences(of: " estimated cost ", with: " ")
            .replacingOccurrences(of: " cost ", with: " ")
            .replacingOccurrences(of: " spend ", with: " ")
            .replacingOccurrences(of: " tokens ", with: " ")
    }

    private static func normalizedLabel(_ label: String) -> String {
        label
            .replacingOccurrences(of: " estimated cost ", with: " ")
            .replacingOccurrences(of: " cost ", with: " ")
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
        dashboard.days.suffix(range.rawValue)
    }

    private var providerDays: [CostProviderDayVM] {
        days.flatMap(\.providers)
    }

    private var hasData: Bool {
        providerDays.contains { $0.cost > 0 || $0.tokens > 0 }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
            VStack(spacing: Theme.Spacing.xs) {
                HStack(alignment: .top, spacing: Theme.Spacing.sm) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Activity").font(Theme.Typography.headline)
                        Text(hover.map(hoverText) ?? activitySubtitle)
                            .font(Theme.Typography.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    Spacer(minLength: Theme.Spacing.sm)
                }
                HStack(spacing: Theme.Spacing.xs) {
                    Spacer()
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
            }

            CostActivityChart(
                days: days,
                metric: metric,
                hover: $hover,
                providerColor: Theme.chartColor(provider.providerId),
                onSelectProvider: nil
            )
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
        .animation(.spring(response: 0.4, dampingFraction: 0.82), value: provider.id)
    }

    private var activitySubtitle: String {
        guard hasData else { return "No recent cost or token activity" }
        return "\(range.label) \(metric == .cost ? "cost" : "tokens")"
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
