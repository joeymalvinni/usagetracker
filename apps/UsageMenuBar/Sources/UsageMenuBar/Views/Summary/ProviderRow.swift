import SwiftUI

struct ProviderRow: View {
    let provider: ProviderVM
    let onSelect: () -> Void

    var body: some View {
        Button(action: onSelect) {
            ProviderRowContent(provider: provider)
                .surfaceCard()
                .hoverRow()
                .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.lg, style: .continuous))
        }
        .buttonStyle(.plain)
        .help(provider.errorDetail ?? "Open \(provider.name)")
    }
}

struct AccountCarouselRow: View {
    let provider: ProviderVM
    let accounts: [ProviderVM]
    let onSelect: (String?) -> Void

    @State private var selectedAccountId: String?
    @State private var slideDirection = 1
    @GestureState private var dragOffset: CGFloat = 0

    init(provider: ProviderVM, accounts: [ProviderVM], onSelect: @escaping (String?) -> Void) {
        self.provider = provider
        self.accounts = accounts
        self.onSelect = onSelect
        _selectedAccountId = State(initialValue: accounts.first?.accountId)
    }

    private var selectedIndex: Int {
        accounts.firstIndex { $0.accountId == selectedAccountId } ?? 0
    }

    private var selectedAccount: ProviderVM? {
        guard accounts.indices.contains(selectedIndex) else { return nil }
        return accounts[selectedIndex]
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
            if let selectedAccount {
                Button {
                    onSelect(selectedAccount.accountId)
                } label: {
                    ZStack(alignment: .leading) {
                        ProviderRowContent(provider: selectedAccount)
                            .id(selectedAccount.id)
                            .transition(accountTransition)
                    }
                    .offset(x: dragOffset)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .clipped()
                .help(selectedAccount.errorDetail ?? "Open \(selectedAccount.name)")
            }

            HStack(spacing: Theme.Spacing.xs) {
                Text("\(provider.name) · Account \(selectedIndex + 1) of \(accounts.count)")
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)

                Spacer()

                pageButton(systemName: "chevron.left", label: "Previous account", delta: -1)
                pageButton(systemName: "chevron.right", label: "Next account", delta: 1)
            }
        }
        .surfaceCard()
        .hoverRow()
        .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.lg, style: .continuous))
        .simultaneousGesture(swipeGesture)
        .onChange(of: accounts.map(\.id)) { _, _ in
            if !accounts.contains(where: { $0.accountId == selectedAccountId }) {
                selectedAccountId = accounts.first?.accountId
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityAction(named: "Previous account") { move(by: -1) }
        .accessibilityAction(named: "Next account") { move(by: 1) }
    }

    private var accountTransition: AnyTransition {
        let incoming: Edge = slideDirection > 0 ? .trailing : .leading
        let outgoing: Edge = slideDirection > 0 ? .leading : .trailing
        return .asymmetric(
            insertion: .move(edge: incoming).combined(with: .opacity),
            removal: .move(edge: outgoing).combined(with: .opacity)
        )
    }

    private var swipeGesture: some Gesture {
        DragGesture(minimumDistance: 12)
            .updating($dragOffset) { value, offset, _ in
                guard abs(value.translation.width) > abs(value.translation.height) else { return }
                offset = value.translation.width
            }
            .onEnded { value in
                guard abs(value.translation.width) > abs(value.translation.height),
                      abs(value.translation.width) >= 36 else { return }
                move(by: value.translation.width < 0 ? 1 : -1)
            }
    }

    private func pageButton(systemName: String, label: String, delta: Int) -> some View {
        Button {
            move(by: delta)
        } label: {
            Image(systemName: systemName)
                .font(Theme.Typography.micro.weight(.semibold))
                .frame(width: 22, height: 18)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(.secondary)
        .help(label)
        .accessibilityLabel(label)
    }

    private func move(by delta: Int) {
        guard accounts.count > 1 else { return }
        slideDirection = delta
        let nextIndex = (selectedIndex + delta + accounts.count) % accounts.count
        withAnimation(.easeOut(duration: 0.2)) {
            selectedAccountId = accounts[nextIndex].accountId
        }
    }
}

private struct ProviderRowContent: View {
    let provider: ProviderVM

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
            HStack(alignment: .top, spacing: Theme.Spacing.sm) {
                ProviderIcon(id: provider.providerId, symbol: provider.symbol)
                    .padding(.top, 2)
                VStack(alignment: .leading, spacing: 1) {
                    Text(provider.name)
                        .font(Theme.Typography.headline)
                        .lineLimit(1)
                    if let email = provider.accountEmail,
                       !email.isEmpty,
                       email.localizedCaseInsensitiveCompare(provider.name) != .orderedSame {
                        Text(email)
                            .font(Theme.Typography.micro)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
                Spacer()
                NumText(value: provider.primary, font: Theme.Typography.metric)
                if !provider.sparkline.isEmpty {
                    Sparkline(values: provider.sparkline)
                        .frame(width: 64, height: 16)
                }
                Image(systemName: "chevron.right")
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.tertiary)
            }

            HStack(spacing: Theme.Spacing.xs) {
                Text(provider.secondary)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
                Spacer()
                if provider.status.needsAttention {
                    StatusChip(status: provider.status, healthText: provider.healthText, percent: provider.percent)
                }
            }

            if let primaryWindow = limitWindows.first {
                VStack(spacing: 0) {
                    WindowRow(window: primaryWindow, compact: true)
                    if limitWindows.count > 1 || resetWindow != nil {
                        HStack(spacing: Theme.Spacing.xs) {
                            if let resetText {
                                Text(resetText)
                                    .fontWeight(.medium)
                            }
                            Spacer(minLength: Theme.Spacing.sm)
                            if limitWindows.count > 1 {
                                Text(limitCountText)
                            }
                        }
                        .font(Theme.Typography.micro)
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                        .padding(.top, Theme.Spacing.xs)
                    }
                }
            }
        }
    }

    private var resetWindow: WindowVM? {
        guard provider.providerId == "codex" else { return nil }
        return provider.windows.first { $0.id == "\(provider.providerId)_rate_limit_resets" }
    }

    private var limitWindows: [WindowVM] {
        provider.windows.filter { $0.id != "\(provider.providerId)_rate_limit_resets" }
    }

    private var resetText: String? {
        guard let resetWindow else { return nil }
        // Count comes from the window (drives visibility); the "· expires in N"
        // relative deadline comes from the soonest credit when we have detail.
        let value = resetWindow.value.replacingOccurrences(of: " available", with: "")
        let noun = value == "1" ? "reset" : "resets"
        guard let expiresAt = provider.resetCredits.first?.expiresAt else {
            return "\(value) \(noun)"
        }
        let relative = DateFormats.resetRelativeString(for: expiresAt)
        let verb = expiresAt <= Date() ? "expired" : "expires"
        return "\(value) \(noun) · \(verb) \(relative)"
    }

    private var limitCountText: String {
        "\(limitWindows.count) limits"
    }
}

struct StatusChip: View {
    let status: DisplayStatus
    var healthText: String = ""
    var percent: Double? = nil

    private var text: String {
        if status == .critical, let percent, percent <= 0 {
            return "limit reached"
        }
        let generic = ["all good", "unknown", ""]
        return generic.contains(healthText) ? status.label : healthText
    }

    var body: some View {
        Text(text)
            .font(Theme.Typography.micro.weight(.medium))
            .foregroundStyle(status.tint)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(status.tint.opacity(0.12), in: Capsule())
            .lineLimit(1)
    }
}

struct Sparkline: View {
    let values: [Double]

    var body: some View {
        GeometryReader { geo in
            let maxV = max(values.max() ?? 0, 1)
            let step = geo.size.width / max(1, CGFloat(values.count - 1))
            Path { path in
                guard values.count > 1 else { return }
                for (index, value) in values.enumerated() {
                    let x = CGFloat(index) * step
                    let y = geo.size.height - CGFloat(value / maxV) * geo.size.height
                    if index == 0 { path.move(to: CGPoint(x: x, y: y)) }
                    else { path.addLine(to: CGPoint(x: x, y: y)) }
                }
            }
            .stroke(Color.secondary.opacity(0.6), style: StrokeStyle(lineWidth: 1.2, lineCap: .round))
        }
    }
}

extension View {
    func hoverRow() -> some View { modifier(HoverRowModifier()) }
}

private struct HoverRowModifier: ViewModifier {
    @State private var hovered = false
    func body(content: Content) -> some View {
        content
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.lg, style: .continuous)
                    .fill(.quaternary.opacity(hovered ? 0.5 : 0))
                    .animation(.easeOut(duration: 0.15), value: hovered)
            )
            .onHover { hovered = $0 }
    }
}
