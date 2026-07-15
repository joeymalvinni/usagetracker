import SwiftUI

struct WindowRow: View {
    let window: WindowVM
    /// Compact mode: used by `ProviderRow` summary tiles. Drops the percent
    /// number (the progress bar communicates it). Renders without a nested
    /// inset inside the parent card, so surfaces never stack.
    var compact: Bool = false
    /// When false, the inline "Resets …" line is dropped and the reset time is
    /// only surfaced via `.help`.
    var showsReset: Bool = true
    /// When true, the reset time renders as a tappable dropdown: a relative
    /// countdown collapsed, the fully explicit date revealed on tap. Used by
    /// the detail-page Limits section.
    var resetExpandable: Bool = false
    /// When provided, a right-click "Hide this metric" action is attached to the
    /// row. Supplied only by the detail page; compact summary tiles omit it.
    var onHide: (() -> Void)? = nil

    @State private var resetExpanded = false

    var body: some View {
        VStack(spacing: 4) {
            HStack(alignment: .firstTextBaseline) {
                Text(window.label).lineLimit(1)
                Spacer()
                if compact, !window.reset.isEmpty {
                    Text(relativeReset)
                        .font(Theme.Typography.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
                if !compact {
                    if let absolute = window.absolute {
                        Text(absolute)
                            .font(Theme.Typography.caption.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                    Text(window.value)
                        .foregroundStyle(window.status.needsAttention ? AnyShapeStyle(window.status.tint) : AnyShapeStyle(.primary))
                        .monospacedDigit()
                }
            }
            .font(Theme.Typography.caption)

            if showsInlineReset {
                HStack(spacing: Theme.Spacing.xs) {
                    Image(systemName: "clock")
                    Text(window.reset)
                        .monospacedDigit()
                    Spacer()
                }
                .font(Theme.Typography.micro)
                .foregroundStyle(.secondary)
            }

            if let p = window.percent {
                ProgressBar(
                    percent: p,
                    status: window.status,
                    providerId: window.providerId,
                    forecastPercent: window.forecast?.projectedPercentRemaining
                )
                    .animation(.spring(duration: 0.4), value: p)
                    .animation(
                        .spring(duration: 0.4),
                        value: window.forecast?.projectedPercentRemaining
                    )
            }

            if showsResetDropdown {
                resetDropdown
            }
        }
        // Compact rows are naked HStacks inside the parent card. Full rows
        // (used on the detail page) keep their own inset surface.
        .modifier(CompactOrInset(compact: compact))
        .help(resetHelp)
        .modifier(HideMenu(onHide: onHide))
    }

    /// Static reset line (Credits, and any full row that isn't a dropdown).
    private var showsInlineReset: Bool {
        !window.reset.isEmpty && !compact && showsReset && !resetExpandable
    }

    private var showsResetDropdown: Bool {
        !compact && showsReset && resetExpandable && window.resetAt != nil
    }

    @ViewBuilder
    private var resetDropdown: some View {
        VStack(spacing: 4) {
            Button {
                withAnimation(.spring(duration: 0.28)) { resetExpanded.toggle() }
            } label: {
                HStack(spacing: Theme.Spacing.xs) {
                    Image(systemName: "clock")
                    Text(relativeReset)
                        .monospacedDigit()
                    if let forecast = window.forecast {
                        Text("·").foregroundStyle(.tertiary)
                        Text(forecast.summary)
                    }
                    Spacer()
                    Image(systemName: "chevron.down")
                        .font(Theme.Typography.micro.weight(.semibold))
                        .foregroundStyle(.tertiary)
                        .rotationEffect(.degrees(resetExpanded ? 0 : -90))
                }
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
                .font(Theme.Typography.micro)
                .foregroundStyle(.secondary)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if resetExpanded {
                VStack(alignment: .leading, spacing: 3) {
                    Text(explicitReset)
                        .foregroundStyle(.tertiary)
                    if let forecast = window.forecast {
                        Text(forecast.detail)
                            .foregroundStyle(.secondary)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .font(Theme.Typography.micro)
                .fixedSize(horizontal: false, vertical: true)
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
    }

    private var relativeReset: String {
        guard let date = window.resetAt else { return window.reset }
        let relative = DateFormats.resetRelativeString(for: date)
        return "Resets \(relative)"
    }

    private var explicitReset: String {
        guard let date = window.resetAt else { return window.reset }
        return "Resets on \(DateFormats.explicit.string(from: date))"
    }

    private var resetHelp: String {
        let prefix = "\(window.providerName) · \(window.label)"
        let reset = window.reset.isEmpty ? prefix : "\(prefix) · \(window.reset)"
        return window.forecast.map { "\(reset) · \($0.summary)" } ?? reset
    }
}

/// Attaches a "Hide this metric" right-click menu only when a hide action is
/// supplied, so rows without one keep their normal right-click behaviour.
private struct HideMenu: ViewModifier {
    let onHide: (() -> Void)?
    func body(content: Content) -> some View {
        if let onHide {
            content.contextMenu {
                Button("Hide this metric", systemImage: "eye.slash", action: onHide)
            }
        } else {
            content
        }
    }
}

private struct CompactOrInset: ViewModifier {
    let compact: Bool
    func body(content: Content) -> some View {
        if compact {
            content
        } else {
            content
                .surfaceInset()
        }
    }
}
