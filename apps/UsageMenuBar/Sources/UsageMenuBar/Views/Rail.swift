import SwiftUI

struct Rail: View {
    @EnvironmentObject var state: AppState
    @Binding var selection: Selection

    var body: some View {
        VStack(spacing: Theme.Spacing.xs) {
            rail(.summary, "Summary") { Image(systemName: "gauge") }
            ForEach(state.providers) { p in
                rail(.provider(p.id), p.name) {
                    ProviderIcon(id: p.id, symbol: p.symbol)
                        .frame(width: 18, height: 18)
                }
                .overlay(alignment: .topTrailing) {
                    // Attention badge: only rendered when the provider needs
                    // attention, tinted by severity. Healthy providers show
                    // nothing — color always means "look here".
                    if p.status.needsAttention {
                        Circle()
                            .fill(p.status.tint)
                            .frame(width: 7, height: 7)
                            .padding(.top, 2)
                            .padding(.trailing, 12)
                            .help(p.status.label)
                    }
                }
            }
            Spacer()
            rail(.settings, "Settings") { Image(systemName: "gearshape") }
        }
        .padding(.vertical, Theme.Spacing.lg)
        .frame(width: 68)
        // Flat sidebar tint — the popover shell already carries translucency;
        // stacking a second material here reads as clutter.
        .background(Color.primary.opacity(0.04))
        .animation(.spring(duration: 0.3), value: state.providers.map(\.id))
    }

    private func rail(_ s: Selection, _ label: String, @ViewBuilder icon: () -> some View) -> some View {
        Button { selection = s } label: {
            VStack(spacing: 2) {
                icon()
                    .frame(width: 28, height: 28)
                Text(label)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(selection == s ? AnyShapeStyle(.primary) : AnyShapeStyle(.secondary))
                    .lineLimit(1)
            }
            .frame(width: 60)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(label)
        .railBackground(isSelected: selection == s)
    }
}

private extension View {
    /// Subtle hover and selection background for rail rows.
    func railBackground(isSelected: Bool) -> some View {
        modifier(RailBackgroundModifier(isSelected: isSelected))
    }
}

private struct RailBackgroundModifier: ViewModifier {
    let isSelected: Bool
    @State private var hovered = false
    func body(content: Content) -> some View {
        content
            .background(
                RoundedRectangle(cornerRadius: Theme.Radius.sm)
                    .fill(.quaternary.opacity(isSelected || hovered ? 0.5 : 0))
                    .animation(.easeOut(duration: 0.15), value: hovered)
                    .animation(.easeOut(duration: 0.15), value: isSelected)
            )
            .onHover { hovered = $0 }
    }
}
