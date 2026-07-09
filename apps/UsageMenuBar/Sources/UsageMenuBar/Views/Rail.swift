import SwiftUI

struct Rail: View {
    @EnvironmentObject var state: AppState
    @Binding var selection: Selection

    var body: some View {
        VStack(spacing: Theme.Spacing.xs) {
            rail(.summary, "Summary") { Image(systemName: "gauge") }
            ForEach(state.providers) { p in
                rail(.provider(p.id, accountId: nil), p.name) {
                    ProviderIcon(id: p.providerId, symbol: p.symbol)
                        .frame(width: 18, height: 18)
                }
                .overlay(alignment: .topTrailing) {
                    if railShowsDot(p) {
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
        .background(Color.primary.opacity(0.04))
        .animation(.spring(duration: 0.3), value: state.providers.map(\.id))
    }

    // Actionable alerts (warning/critical/error) only show a dot until the user has viewed
    // the account; non-actionable states (stale/disabled) always show their gray dot.
    private func railShowsDot(_ p: ProviderVM) -> Bool {
        if p.status.isAlert { return p.hasUnseenAlert }
        return p.status.needsAttention
    }

    private func rail(_ s: Selection, _ label: String, @ViewBuilder icon: () -> some View) -> some View {
        Button { selection = s } label: {
            VStack(spacing: 2) {
                icon()
                    .frame(width: 28, height: 28)
                Text(label)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(selection.matchesProvider(s) ? AnyShapeStyle(.primary) : AnyShapeStyle(.secondary))
                    .lineLimit(1)
            }
            .frame(width: 60)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(label)
        .railBackground(isSelected: selection.matchesProvider(s))
    }
}

private extension Selection {
    func matchesProvider(_ other: Selection) -> Bool {
        switch (self, other) {
        case (.summary, .summary), (.settings, .settings): true
        case (.provider(let a, _), .provider(let b, _)): a == b
        default: false
        }
    }
}

private extension View {
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
