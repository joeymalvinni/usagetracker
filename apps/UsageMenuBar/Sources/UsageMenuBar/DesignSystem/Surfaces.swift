import SwiftUI

/// Surface system.
///
/// Translucency belongs to the popover shell only: `NSGlassEffectView` on
/// macOS 26, an `NSVisualEffectView` on 14/15 (see `AppDelegate`). Content
/// sits on quiet flat fills on top of it, so text keeps full vibrancy and
/// surfaces never stack material-on-material.
///
/// The fills are keyed to the color scheme rather than the semantic
/// `Color.primary`: a light-mode card lifts with a faint dark wash, but the
/// same treatment inverts to a faint *white* wash in dark mode that all but
/// disappears over the translucent shell. Dark surfaces therefore use a
/// stronger fill plus a hairline top-light edge so cards keep visible depth.
extension View {
    /// Top-level content card, one level deep inside the popover. Always
    /// full-width so stacked cards share a common edge.
    func surfaceCard() -> some View {
        modifier(SurfaceStyle(cornerRadius: Theme.Radius.lg, padding: Theme.Spacing.lg, fullWidth: true))
    }

    /// Small inset affordance used *inside* a card (window rows, error text).
    func surfaceInset() -> some View {
        modifier(SurfaceStyle(cornerRadius: Theme.Radius.md, padding: Theme.Spacing.sm, fullWidth: false))
    }
}

private struct SurfaceStyle: ViewModifier {
    @Environment(\.colorScheme) private var colorScheme
    let cornerRadius: CGFloat
    let padding: CGFloat
    let fullWidth: Bool

    func body(content: Content) -> some View {
        sized(content)
            .padding(padding)
            .background(
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .fill(fill)
            )
            .overlay(
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .strokeBorder(stroke, lineWidth: 0.5)
            )
    }

    @ViewBuilder private func sized(_ content: Content) -> some View {
        if fullWidth {
            content.frame(maxWidth: .infinity, alignment: .leading)
        } else {
            content
        }
    }

    private var fill: Color {
        colorScheme == .dark ? Color.white.opacity(0.07) : Color.black.opacity(0.05)
    }

    private var stroke: Color {
        colorScheme == .dark ? Color.white.opacity(0.10) : Color.black.opacity(0.045)
    }
}
