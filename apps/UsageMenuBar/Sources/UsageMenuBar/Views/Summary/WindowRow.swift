import SwiftUI

struct WindowRow: View {
    let window: WindowVM
    /// Compact mode: used by `ProviderRow` summary tiles. Drops the percent
    /// number (the progress bar communicates it) and the reset time string
    /// (surfaced via `.help`). Renders as a plain `HStack` inside the parent
    /// card rather than a nested inset, so surfaces never stack.
    var compact: Bool = false

    var body: some View {
        VStack(spacing: 4) {
            HStack {
                Text(window.label).lineLimit(1)
                Spacer()
                if let absolute = window.absolute, compact {
                    // "12M / 50M"-style: show the absolute used count as the
                    // primary figure in compact mode. Percent number is
                    // redundant next to the progress bar.
                    Text(absolute)
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
                if !window.reset.isEmpty, !compact {
                    Text(window.reset).foregroundStyle(.secondary)
                }
            }
            .font(Theme.Typography.caption)

            if let p = window.percent {
                ProgressBar(percent: p, status: window.status, providerId: window.providerId)
                    .animation(.spring(duration: 0.4), value: p)
            }
        }
        // Compact rows are naked HStacks inside the parent card. Full rows
        // (used on the detail page) keep their own inset surface.
        .modifier(CompactOrInset(compact: compact))
        .help(resetHelp)
    }

    private var resetHelp: String {
        let prefix = "\(window.providerName) · \(window.label)"
        return window.reset.isEmpty ? prefix : "\(prefix) · \(window.reset)"
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
