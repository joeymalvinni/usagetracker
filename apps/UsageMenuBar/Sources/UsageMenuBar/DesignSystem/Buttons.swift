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
            .font(Theme.Typography.caption.weight(weight))
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

    private var weight: Font.Weight {
        // The prominent chip earns emphasis through a heavier label + fill
        // rather than an accent hue, so it stays calm alongside its card.
        kind == .prominent ? .semibold : .medium
    }

    private var foreground: Color {
        switch kind {
        case .standard, .prominent: .primary
        case .destructive: .red
        }
    }

    private func fill(pressed: Bool) -> Color {
        switch kind {
        case .standard:
            .primary.opacity(pressed ? 0.2 : 0.1)
        case .prominent:
            .primary.opacity(pressed ? 0.24 : 0.15)
        case .destructive:
            .red.opacity(pressed ? 0.2 : 0.1)
        }
    }
}

/// Filled primary call-to-action in the chip family. Solid fill so it carries
/// weight as *the* action on a screen, but a muted indigo drawn from the brand
/// palette rather than saturated macOS system blue — and the app's own radius
/// and type instead of the chunky `.borderedProminent` chrome.
struct PrimaryChipButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled

    /// Muted indigo, matched to the desaturated brand palette
    /// (see `ProviderBrand.palette`). Reads as accent without shouting blue.
    static let fill = Color(red: 0.40, green: 0.46, blue: 0.74)

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.Typography.body.weight(.semibold))
            .foregroundStyle(.white)
            .padding(.horizontal, Theme.Spacing.lg)
            .padding(.vertical, Theme.Spacing.sm + 2)
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.md, style: .continuous)
                    .fill(Self.fill.opacity(configuration.isPressed ? 0.82 : 1))
            )
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.md, style: .continuous))
            .opacity(isEnabled ? 1 : 0.4)
            .animation(.easeOut(duration: 0.1), value: configuration.isPressed)
    }
}

extension ButtonStyle where Self == ChipButtonStyle {
    /// Quiet chip — the default for secondary actions.
    static var chip: ChipButtonStyle { ChipButtonStyle() }
    /// Emphasized neutral chip for the primary action in a group — heavier
    /// label and fill than `.chip`, but no accent hue.
    static var chipProminent: ChipButtonStyle { ChipButtonStyle(kind: .prominent) }
    /// Red-tinted chip for destructive actions.
    static var chipDestructive: ChipButtonStyle { ChipButtonStyle(kind: .destructive) }
}

extension ButtonStyle where Self == PrimaryChipButtonStyle {
    /// Filled primary call-to-action — muted indigo, white label.
    static var chipPrimary: PrimaryChipButtonStyle { PrimaryChipButtonStyle() }
}
