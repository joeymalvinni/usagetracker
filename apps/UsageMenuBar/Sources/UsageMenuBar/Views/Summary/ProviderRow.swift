import SwiftUI

struct ProviderRow: View {
    let provider: ProviderVM
    let onSelect: () -> Void

    var body: some View {
        Button(action: onSelect) {
            VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
                HStack(alignment: .firstTextBaseline, spacing: Theme.Spacing.sm) {
                    Label {
                        Text(provider.name)
                    } icon: {
                        ProviderIcon(id: provider.id, symbol: provider.symbol)
                    }
                    .font(Theme.Typography.headline)
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
                        StatusChip(status: provider.status, healthText: provider.healthText)
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
            .surfaceCard()
            .hoverRow()
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.lg, style: .continuous))
        }
        .buttonStyle(.plain)
        .help("Open \(provider.name)")
    }

    private var resetWindow: WindowVM? {
        guard provider.id == "codex" else { return nil }
        return provider.windows.first { $0.id == "\(provider.id)_rate_limit_resets" }
    }

    private var limitWindows: [WindowVM] {
        provider.windows.filter { $0.id != "\(provider.id)_rate_limit_resets" }
    }

    private var resetText: String? {
        guard let resetWindow else { return nil }
        let value = resetWindow.value.replacingOccurrences(of: " available", with: "")
        let noun = value == "1" ? "reset" : "resets"
        return "\(value) \(noun) available · \(resetWindow.reset)"
    }

    private var limitCountText: String {
        "\(limitWindows.count) limits"
    }
}

struct StatusChip: View {
    let status: DisplayStatus
    var healthText: String = ""

    private var text: String {
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
