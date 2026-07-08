import SwiftUI

/// Surface system.
///
/// Translucency belongs to the popover shell only: `NSGlassEffectView` on
/// macOS 26, an `NSVisualEffectView` on 14/15 (see `AppDelegate`). Content
/// sits on quiet flat fills on top of it, so text keeps full vibrancy and
/// surfaces never stack material-on-material.
extension View {
    /// Top-level content card, one level deep inside the popover. Always
    /// full-width so stacked cards share a common edge.
    func surfaceCard() -> some View {
        frame(maxWidth: .infinity, alignment: .leading)
            .padding(Theme.Spacing.lg)
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.lg, style: .continuous)
                    .fill(Color.primary.opacity(0.05))
            )
    }

    /// Small inset affordance used *inside* a card (window rows, error text).
    func surfaceInset() -> some View {
        padding(Theme.Spacing.sm)
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.md, style: .continuous)
                    .fill(Color.primary.opacity(0.05))
            )
    }
}
