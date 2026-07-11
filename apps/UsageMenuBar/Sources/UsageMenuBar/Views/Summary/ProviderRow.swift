import AppKit
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

/// Summary card for a provider with more than one account. Pages through the
/// accounts one at a time. The switcher is surfaced up front — a labelled
/// "N accounts" badge in the header, tappable page dots, and flanking pager
/// buttons — and the swap uses an interactive, spring-settled slide so it reads
/// as a deck of cards rather than content quietly changing underneath you.
///
/// The pager wraps: stepping past the last account rolls forward into the
/// first (and past the first rolls back to the last). We render a clone of the
/// first account after the strip and a clone of the last before it, so the
/// wrap keeps sliding in the same direction instead of scrubbing back across
/// every card. Once the animation lands on a clone we silently re-centre onto
/// the identical real panel, which is invisible to the eye.
struct AccountCarouselRow: View {
    let provider: ProviderVM
    let accounts: [ProviderVM]
    let onSelect: (String?) -> Void

    @State private var selectedAccountId: String?
    /// Position within the cloned strip: real account `i` lives at `i + 1`,
    /// with the leading clone at 0 and the trailing clone at `accounts.count + 1`.
    @State private var displayIndex: Int
    @State private var dragOffset: CGFloat = 0
    @State private var cardWidth: CGFloat = 0

    /// Distance past which a release commits to the neighbouring account.
    private static let commitThreshold: CGFloat = 44
    private static let switchAnimation = Animation.spring(response: 0.4, dampingFraction: 0.82)

    init(provider: ProviderVM, accounts: [ProviderVM], onSelect: @escaping (String?) -> Void) {
        self.provider = provider
        self.accounts = accounts
        self.onSelect = onSelect
        let start = accounts.first?.accountId
        _selectedAccountId = State(initialValue: start)
        _displayIndex = State(initialValue: 1)
    }

    private var accent: Color { Theme.chartColor(provider.providerId) }

    private var selectedIndex: Int {
        accounts.firstIndex { $0.accountId == selectedAccountId } ?? 0
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
            header
            pager
            controlBar
        }
        .background(widthReader)
        .surfaceCard()
        .hoverRow()
        .background {
            TrackpadSwipeMonitor(
                onChanged: updateDrag,
                onEnded: finishDrag
            )
        }
        .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.lg, style: .continuous))
        .onChange(of: accounts.map(\.id)) { _, _ in
            if !accounts.contains(where: { $0.accountId == selectedAccountId }) {
                selectedAccountId = accounts.first?.accountId
            }
            // Re-anchor the strip onto the (possibly shifted) selected panel
            // without animating a phantom slide.
            var transaction = Transaction()
            transaction.disablesAnimations = true
            withTransaction(transaction) { displayIndex = selectedIndex + 1 }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("\(provider.name), account \(selectedIndex + 1) of \(accounts.count)")
        .accessibilityAction(named: "Previous account") { move(by: -1) }
        .accessibilityAction(named: "Next account") { move(by: 1) }
    }

    // MARK: - Header

    private var header: some View {
        HStack(spacing: Theme.Spacing.xs) {
            Text(provider.name)
                .font(Theme.Typography.micro.weight(.semibold))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)
                .lineLimit(1)

            Spacer(minLength: Theme.Spacing.sm)

            HStack(spacing: 3) {
                Image(systemName: "person.2.fill")
                Text("\(accounts.count) accounts")
            }
            .font(Theme.Typography.micro.weight(.semibold))
            .foregroundStyle(accent)
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(accent.opacity(0.14), in: Capsule())
            .accessibilityHidden(true)
        }
        .padding(.bottom, Theme.Spacing.xs)
    }

    // MARK: - Pager

    /// A horizontal strip of one full-width panel per account, flanked by a
    /// clone of the last account (leading) and the first account (trailing) so
    /// the wrap slides continuously in the same direction. We slide the whole
    /// strip and let the card clip it, so the drag moves the actual card
    /// material and the neighbouring account peeks in from the side.
    private var pager: some View {
        HStack(spacing: 0) {
            if let last = accounts.last { panel(for: last) }
            ForEach(accounts) { account in panel(for: account) }
            if let first = accounts.first { panel(for: first) }
        }
        .offset(x: -CGFloat(displayIndex) * cardWidth + dragOffset)
        .frame(width: cardWidth, alignment: .leading)
        .clipped()
        // Give the paging drag priority over the panel's navigation button.
        // A click still opens the account, while movement pages the carousel.
        .highPriorityGesture(swipeGesture)
    }

    private func panel(for account: ProviderVM) -> some View {
        Button {
            onSelect(account.accountId)
        } label: {
            ProviderRowContent(provider: account)
                .frame(width: cardWidth, alignment: .leading)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(account.errorDetail ?? "Open \(account.name)")
    }

    /// Measures the card's content width without constraining it (the strip
    /// reports only one panel's width, so this can't feed back on itself).
    private var widthReader: some View {
        GeometryReader { proxy in
            Color.clear
                .onAppear { cardWidth = proxy.size.width }
                .onChange(of: proxy.size.width) { _, width in cardWidth = width }
        }
    }

    // MARK: - Controls

    private var controlBar: some View {
        HStack(spacing: Theme.Spacing.sm) {
            pageButton(systemName: "chevron.left", label: "Previous account", delta: -1)
            Spacer(minLength: 0)
            pageDots
            Spacer(minLength: 0)
            pageButton(systemName: "chevron.right", label: "Next account", delta: 1)
        }
    }

    private var pageDots: some View {
        HStack(spacing: 5) {
            ForEach(accounts.indices, id: \.self) { index in
                Capsule()
                    .fill(index == selectedIndex ? accent : Color.secondary.opacity(0.3))
                    .frame(width: index == selectedIndex ? 16 : 6, height: 6)
                    .contentShape(Capsule())
                    .onTapGesture { move(to: index) }
                    .help(accounts[index].account ?? accounts[index].name)
            }
        }
        .animation(Self.switchAnimation, value: selectedIndex)
        .accessibilityHidden(true)
    }

    private func pageButton(systemName: String, label: String, delta: Int) -> some View {
        Button {
            move(by: delta)
        } label: {
            Image(systemName: systemName)
                .font(Theme.Typography.caption.weight(.bold))
                .foregroundStyle(.secondary)
                .frame(width: 26, height: 26)
                .background(Color.primary.opacity(0.06), in: Circle())
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .help(label)
        .accessibilityLabel(label)
    }

    // MARK: - Motion

    private var swipeGesture: some Gesture {
        DragGesture(minimumDistance: 12)
            .onChanged { value in
                guard abs(value.translation.width) > abs(value.translation.height) else { return }
                updateDrag(value.translation.width)
            }
            .onEnded { value in
                guard abs(value.translation.width) > abs(value.translation.height) else {
                    settle()
                    return
                }
                finishDrag(value.translation.width)
            }
    }

    private func updateDrag(_ translation: CGFloat) {
        // A clone waits one card-width out on either side, so the drag is free
        // in both directions but never exposes space beyond the cloned runway.
        dragOffset = max(-cardWidth, min(cardWidth, translation))
    }

    private func finishDrag(_ translation: CGFloat) {
        guard abs(translation) >= Self.commitThreshold else {
            settle()
            return
        }
        move(by: translation < 0 ? 1 : -1)
    }

    /// Steps `delta` accounts, wrapping around either end. The strip slides to
    /// a clone when it crosses an edge, then silently re-centres onto the twin
    /// real panel once the spring settles.
    private func move(by delta: Int) {
        guard accounts.count > 1, delta != 0 else {
            settle()
            return
        }
        let count = accounts.count
        let wrapped = ((selectedIndex + delta) % count + count) % count
        let target = displayIndex + delta
        withAnimation(Self.switchAnimation) {
            dragOffset = 0
            displayIndex = target
            selectedAccountId = accounts[wrapped].accountId
        } completion: {
            let canonical = wrapped + 1
            guard displayIndex != canonical else { return }
            var transaction = Transaction()
            transaction.disablesAnimations = true
            withTransaction(transaction) { displayIndex = canonical }
        }
    }

    private func move(to index: Int) {
        guard accounts.indices.contains(index), index != selectedIndex else {
            settle()
            return
        }
        withAnimation(Self.switchAnimation) {
            dragOffset = 0
            displayIndex = index + 1
            selectedAccountId = accounts[index].accountId
        }
    }

    /// Springs the strip back to rest without changing the selected account.
    private func settle() {
        withAnimation(Self.switchAnimation) { dragOffset = 0 }
    }
}

/// Bridges macOS precision-scroll events into the carousel's interactive drag.
/// SwiftUI's `DragGesture` covers click-and-drag, but a two-finger trackpad
/// swipe arrives as `NSEvent.EventType.scrollWheel` and otherwise goes only to
/// the surrounding vertical `ScrollView`.
private struct TrackpadSwipeMonitor: NSViewRepresentable {
    let onChanged: (CGFloat) -> Void
    let onEnded: (CGFloat) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(onChanged: onChanged, onEnded: onEnded)
    }

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        context.coordinator.attach(to: view)
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        context.coordinator.onChanged = onChanged
        context.coordinator.onEnded = onEnded
    }

    static func dismantleNSView(_ nsView: NSView, coordinator: Coordinator) {
        coordinator.detach()
    }

    @MainActor final class Coordinator {
        enum Axis {
            case undecided
            case horizontal
            case vertical
        }

        var onChanged: (CGFloat) -> Void
        var onEnded: (CGFloat) -> Void

        private weak var view: NSView?
        private var monitor: Any?
        private var axis = Axis.undecided
        private var pendingX: CGFloat = 0
        private var pendingY: CGFloat = 0
        private var translation: CGFloat = 0
        private var delayedEnd: DispatchWorkItem?

        init(onChanged: @escaping (CGFloat) -> Void, onEnded: @escaping (CGFloat) -> Void) {
            self.onChanged = onChanged
            self.onEnded = onEnded
        }

        func attach(to view: NSView) {
            self.view = view
            monitor = NSEvent.addLocalMonitorForEvents(matching: .scrollWheel) { [weak self] event in
                guard let self else { return event }
                return self.handle(event) ? nil : event
            }
        }

        func detach() {
            delayedEnd?.cancel()
            if let monitor { NSEvent.removeMonitor(monitor) }
            monitor = nil
        }

        /// Returns true only for an established horizontal gesture. Vertical
        /// events continue untouched to the summary's enclosing ScrollView.
        private func handle(_ event: NSEvent) -> Bool {
            guard event.hasPreciseScrollingDeltas,
                  event.momentumPhase.isEmpty,
                  let view,
                  let window = view.window,
                  event.window === window,
                  view.bounds.contains(view.convert(event.locationInWindow, from: nil)) else {
                return false
            }

            if event.phase.contains(.began) {
                reset()
            }

            pendingX += event.scrollingDeltaX
            pendingY += event.scrollingDeltaY

            if axis == .undecided,
               max(abs(pendingX), abs(pendingY)) >= 3 {
                axis = abs(pendingX) > abs(pendingY) ? .horizontal : .vertical
            }

            if axis == .horizontal {
                // Reversed scrolling: the strip moves the same way as the
                // scroll delta rather than tracking under the user's fingers.
                translation = pendingX
                onChanged(translation)
            }

            if event.phase.contains(.ended) || event.phase.contains(.cancelled) {
                finish(cancelled: event.phase.contains(.cancelled))
            } else if event.phase.isEmpty {
                // Some precision devices omit gesture phases. Treat a short
                // pause as the end while retaining the same axis locking.
                scheduleEnd()
            }

            return axis == .horizontal
        }

        private func scheduleEnd() {
            delayedEnd?.cancel()
            let work = DispatchWorkItem { [weak self] in self?.finish(cancelled: false) }
            delayedEnd = work
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.12, execute: work)
        }

        private func finish(cancelled: Bool) {
            delayedEnd?.cancel()
            if axis == .horizontal {
                onEnded(cancelled ? 0 : translation)
            }
            reset()
        }

        private func reset() {
            delayedEnd?.cancel()
            delayedEnd = nil
            axis = .undecided
            pendingX = 0
            pendingY = 0
            translation = 0
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
                    if countSummaryText != nil || resetExpiryText != nil {
                        HStack(spacing: Theme.Spacing.xs) {
                            if let resetExpiryText {
                                Text(resetExpiryText)
                                    .fontWeight(.medium)
                            }
                            Spacer(minLength: Theme.Spacing.sm)
                            if let countSummaryText {
                                Text(countSummaryText)
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

    /// Trailing count cluster — reset credits sit immediately to the left of the
    /// limits count, e.g. "3 resets · 2 limits". Either part is dropped when it
    /// has nothing to report.
    private var countSummaryText: String? {
        var parts: [String] = []
        if let resetCountText { parts.append(resetCountText) }
        if limitWindows.count > 1 { parts.append(limitCountText) }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private var resetCountText: String? {
        guard let resetWindow else { return nil }
        let value = resetWindow.value.replacingOccurrences(of: " available", with: "")
        let noun = value == "1" ? "reset" : "resets"
        return "\(value) \(noun)"
    }

    /// The soonest reset credit's relative deadline ("expires in 2 days"), shown
    /// on the leading side; the count itself lives in `countSummaryText`.
    private var resetExpiryText: String? {
        guard resetWindow != nil, let expiresAt = provider.resetCredits.first?.expiresAt else { return nil }
        let relative = DateFormats.resetRelativeString(for: expiresAt)
        let verb = expiresAt <= Date() ? "expired" : "expires"
        return "\(verb) \(relative)"
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
