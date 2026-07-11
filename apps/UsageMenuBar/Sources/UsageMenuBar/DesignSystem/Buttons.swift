import SwiftUI

/// Compact, quiet button used across Settings and setup flows. Replaces the
/// chunky default bordered button with a flat rounded chip that sits naturally
/// on the surface system — small type, subtle fill, no heavy chrome.
struct ChipButtonStyle: ButtonStyle {
    enum Kind { case standard, prominent, destructive }
    var kind: Kind = .standard

    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.Typography.caption.weight(.medium))
            .foregroundStyle(foreground)
            .padding(.horizontal, Theme.Spacing.sm + 2)
            .padding(.vertical, Theme.Spacing.xs + 1)
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.sm, style: .continuous)
                    .fill(fill(pressed: configuration.isPressed))
            )
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.sm, style: .continuous))
            .opacity(isEnabled ? 1 : 0.4)
            .animation(.easeOut(duration: 0.1), value: configuration.isPressed)
    }

    private var foreground: Color {
        switch kind {
        case .standard: .primary
        case .prominent: .accentColor
        case .destructive: .red
        }
    }

    private func fill(pressed: Bool) -> Color {
        let base: Color = switch kind {
        case .standard: .primary
        case .prominent: .accentColor
        case .destructive: .red
        }
        return base.opacity(pressed ? 0.2 : 0.1)
    }
}

extension ButtonStyle where Self == ChipButtonStyle {
    /// Quiet chip — the default for secondary actions.
    static var chip: ChipButtonStyle { ChipButtonStyle() }
    /// Accent-tinted chip for the primary action in a group.
    static var chipProminent: ChipButtonStyle { ChipButtonStyle(kind: .prominent) }
    /// Red-tinted chip for destructive actions.
    static var chipDestructive: ChipButtonStyle { ChipButtonStyle(kind: .destructive) }
}
