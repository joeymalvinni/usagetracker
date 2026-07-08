import SwiftUI

/// Empty / error state with an SF Symbol illustration, explanatory text, and
/// (optionally) a retry button — replaces the bare `EmptyState(text:)` label.
struct EmptyState: View {
    enum Kind {
        case empty(text: String)
        case error(text: String)
    }

    let kind: Kind
    var retry: (() -> Void)? = nil

    init(text: String, retry: (() -> Void)? = nil, isError: Bool = false) {
        self.kind = isError ? .error(text: text) : .empty(text: text)
        self.retry = retry
    }

    var body: some View {
        VStack(spacing: Theme.Spacing.md) {
            Image(systemName: symbol)
                .font(.system(size: 48, weight: .regular))
                .foregroundStyle(.tertiary)
                .symbolRenderingMode(.hierarchical)
            Text(title)
                .font(Theme.Typography.headline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            if let retry {
                Button("Retry", action: retry)
                    .buttonStyle(.borderedProminent)
                    .controlSize(.regular)
            }
        }
        .padding(Theme.Spacing.lg)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var symbol: String {
        switch kind {
        case .empty: "tray"
        case .error: "exclamationmark.triangle"
        }
    }
    private var title: String {
        switch kind {
        case .empty(let text): text
        case .error(let text): text
        }
    }
}