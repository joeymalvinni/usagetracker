import AppKit
import SwiftUI

struct Rail: View {
    @EnvironmentObject var state: AppState
    @Binding var selection: Selection
    @State private var providerDrag: ProviderDragState?
    @State private var rowHeight: CGFloat = 0

    private var rowPitch: CGFloat { rowHeight + Theme.Spacing.xs }

    private var previewProviders: [ProviderVM] {
        guard let providerDrag,
              let source = state.providers.firstIndex(where: { $0.providerId == providerDrag.providerId }),
              state.providers.indices.contains(providerDrag.targetIndex)
        else { return state.providers }

        var providers = state.providers
        let provider = providers.remove(at: source)
        providers.insert(provider, at: providerDrag.targetIndex)
        return providers
    }

    var body: some View {
        VStack(spacing: Theme.Spacing.xs) {
            rail(.summary, "Summary") { Image(systemName: "gauge") }

            ForEach(Array(previewProviders.enumerated()), id: \.element.id) { index, provider in
                providerSlot(provider, at: index)
            }

            Spacer()
            rail(.settings, "Settings") { Image(systemName: "gearshape") }
        }
        .padding(.vertical, Theme.Spacing.lg)
        .frame(width: 68)
        .background(Color.primary.opacity(0.04))
        .coordinateSpace(name: "providerRail")
        .onPreferenceChange(RowHeightKey.self) { rowHeight = $0 }
        .onDisappear { providerDrag = nil }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didResignActiveNotification)) { _ in
            cancelProviderDrag()
        }
        .animation(.spring(response: 0.24, dampingFraction: 0.86), value: previewProviders.map(\.id))
        .animation(.easeOut(duration: 0.12), value: providerDrag?.providerId)
    }

    @ViewBuilder
    private func providerSlot(_ provider: ProviderVM, at index: Int) -> some View {
        let isDragging = providerDrag?.providerId == provider.providerId

        ZStack {
            if isDragging {
                RoundedRectangle(cornerRadius: Theme.Radius.sm, style: .continuous)
                    .fill(Color.primary.opacity(0.035))
                    .overlay {
                        RoundedRectangle(cornerRadius: Theme.Radius.sm, style: .continuous)
                            .stroke(
                                Color(nsColor: .separatorColor).opacity(0.7),
                                style: StrokeStyle(lineWidth: 1, dash: [2.5, 2.5])
                            )
                    }
                    .padding(.horizontal, Theme.Spacing.xs)
                    .frame(height: rowHeight)
                    .allowsHitTesting(false)
            }

            providerRow(provider)
                .background {
                    GeometryReader { proxy in
                        Color.clear.preference(key: RowHeightKey.self, value: proxy.size.height)
                    }
                }
                .background {
                    if isDragging {
                        RoundedRectangle(cornerRadius: Theme.Radius.sm, style: .continuous)
                            .fill(Color(nsColor: .controlBackgroundColor))
                            .overlay {
                                RoundedRectangle(cornerRadius: Theme.Radius.sm, style: .continuous)
                                    .stroke(Color(nsColor: .separatorColor), lineWidth: 0.75)
                            }
                    }
                }
                .shadow(color: .black.opacity(isDragging ? 0.18 : 0), radius: 7, y: 3)
                // The list item moves to the preview slot, while this correction keeps its
                // rendered x/y position attached to the pointer's vertical movement only.
                .offset(x: 0, y: floatingOffset(for: provider, displayedAt: index))
        }
        // Shapes have no intrinsic height. Without this constraint the empty drop slot
        // greedily consumes the rail's Spacer, which pushes the last provider to the
        // bottom and makes one row of pointer travel span most of the popover.
        .frame(height: rowHeight > 0 ? rowHeight : nil)
        .contentShape(Rectangle())
        .zIndex(isDragging ? 10 : 0)
        .highPriorityGesture(providerReorderGesture(for: provider))
    }

    private func providerRow(_ provider: ProviderVM) -> some View {
        rail(.provider(provider.id, accountId: nil), provider.name) {
            ProviderIcon(id: provider.providerId, symbol: provider.symbol)
                .frame(width: 18, height: 18)
        }
        .overlay(alignment: .topTrailing) {
            if railShowsDot(provider) {
                Circle()
                    .fill(provider.status.tint)
                    .frame(width: 7, height: 7)
                    .padding(.top, 2)
                    .padding(.trailing, 12)
                    .help(provider.status.label)
            }
        }
        .contextMenu {
            Button("Move Up") { state.moveProvider(provider.providerId, by: -1) }
                .disabled(!state.canMoveProvider(provider.providerId, by: -1))
            Button("Move Down") { state.moveProvider(provider.providerId, by: 1) }
                .disabled(!state.canMoveProvider(provider.providerId, by: 1))
            Divider()
            Button("Reset Provider Order") { state.resetProviderOrder() }
                .disabled(state.ui.providerOrder.isEmpty)
        }
        .accessibilityHint("Drag vertically to reorder providers")
        .accessibilityAction(named: "Move up") {
            state.moveProvider(provider.providerId, by: -1)
        }
        .accessibilityAction(named: "Move down") {
            state.moveProvider(provider.providerId, by: 1)
        }
    }

    private func providerReorderGesture(for provider: ProviderVM) -> some Gesture {
        DragGesture(minimumDistance: 4, coordinateSpace: .named("providerRail"))
            .onChanged { value in
                updateProviderDrag(provider, translation: value.translation.height)
            }
            .onEnded { _ in
                finishProviderDrag()
            }
    }

    private func updateProviderDrag(_ provider: ProviderVM, translation: CGFloat) {
        guard rowPitch > Theme.Spacing.xs,
              let currentIndex = state.providers.firstIndex(where: { $0.providerId == provider.providerId })
        else { return }

        let drag = providerDrag ?? ProviderDragState(
            providerId: provider.providerId,
            startIndex: currentIndex,
            targetIndex: currentIndex,
            translation: 0
        )
        guard drag.providerId == provider.providerId else { return }

        let minimum = -CGFloat(drag.startIndex) * rowPitch
        let maximum = CGFloat(state.providers.count - 1 - drag.startIndex) * rowPitch
        let constrainedTranslation = min(max(translation, minimum), maximum)
        let rawIndex = CGFloat(drag.startIndex) + constrainedTranslation / rowPitch
        let targetIndex = min(max(Int(rawIndex.rounded()), 0), state.providers.count - 1)

        if targetIndex != drag.targetIndex {
            NSHapticFeedbackManager.defaultPerformer.perform(.alignment, performanceTime: .now)
        }

        providerDrag = ProviderDragState(
            providerId: drag.providerId,
            startIndex: drag.startIndex,
            targetIndex: targetIndex,
            translation: constrainedTranslation
        )
    }

    private func finishProviderDrag() {
        guard let drag = providerDrag else { return }
        let providers = state.providers
        let moved = drag.targetIndex != drag.startIndex
        let targetProviderId = providers.indices.contains(drag.targetIndex)
            ? providers[drag.targetIndex].providerId
            : nil

        withAnimation(.spring(response: 0.24, dampingFraction: 0.86)) {
            providerDrag = nil
            if moved, let targetProviderId {
                state.moveProvider(
                    drag.providerId,
                    relativeTo: targetProviderId,
                    after: drag.targetIndex > drag.startIndex
                )
            }
        }

        if moved {
            NSHapticFeedbackManager.defaultPerformer.perform(.generic, performanceTime: .now)
        }
    }

    private func cancelProviderDrag() {
        guard providerDrag != nil else { return }
        withAnimation(.spring(response: 0.2, dampingFraction: 0.9)) {
            providerDrag = nil
        }
    }

    private func floatingOffset(for provider: ProviderVM, displayedAt index: Int) -> CGFloat {
        guard let drag = providerDrag, drag.providerId == provider.providerId else { return 0 }
        return drag.translation - CGFloat(index - drag.startIndex) * rowPitch
    }

    // Actionable alerts (warning/critical/error) only show a dot until the user has viewed
    // the account; non-actionable states (stale/disabled) always show their gray dot.
    private func railShowsDot(_ provider: ProviderVM) -> Bool {
        if provider.status.isAlert { return provider.hasUnseenAlert }
        return provider.status.needsAttention
    }

    private func rail(_ selection: Selection, _ label: String, @ViewBuilder icon: () -> some View) -> some View {
        Button { self.selection = selection } label: {
            VStack(spacing: 2) {
                icon()
                    .frame(width: 28, height: 28)
                Text(label)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(self.selection.matchesProvider(selection) ? AnyShapeStyle(.primary) : AnyShapeStyle(.secondary))
                    .lineLimit(1)
            }
            .frame(width: 60)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(selection.isProvider ? "\(label) — drag vertically to reorder" : label)
        .railBackground(isSelected: self.selection.matchesProvider(selection))
    }
}

private struct ProviderDragState: Equatable {
    let providerId: String
    let startIndex: Int
    let targetIndex: Int
    let translation: CGFloat
}

private struct RowHeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

private extension Selection {
    var isProvider: Bool {
        if case .provider = self { return true }
        return false
    }

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
