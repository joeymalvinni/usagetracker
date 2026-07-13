import SwiftUI

struct ReleaseNotesCard: View {
    let notes: ReleaseNotes
    let dismiss: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.md) {
            HStack(alignment: .firstTextBaseline, spacing: Theme.Spacing.sm) {
                Text("What’s new in \(notes.version)")
                    .font(Theme.Typography.headline)
                Spacer()
                Button(action: dismiss) {
                    Image(systemName: "xmark")
                        .font(Theme.Typography.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                        .padding(Theme.Spacing.xs)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Dismiss release notes")
                .accessibilityLabel("Dismiss release notes")
            }

            markdownText(notes.summary)
                .font(Theme.Typography.caption)
                .foregroundStyle(.secondary)

            VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
                ForEach(Array(notes.highlights.prefix(ReleaseNotesParser.maximumHighlights).enumerated()), id: \.offset) { _, highlight in
                    HStack(alignment: .firstTextBaseline, spacing: Theme.Spacing.sm) {
                        Image(systemName: "circle.fill")
                            .font(.system(size: 4))
                            .foregroundStyle(.tertiary)
                        markdownText(highlight)
                            .font(Theme.Typography.caption)
                            .foregroundStyle(.primary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }

            HStack {
                Spacer()
                Button("Got it", action: dismiss)
                    .buttonStyle(.chip)
            }
        }
        .surfaceCard()
    }

    private func markdownText(_ value: String) -> Text {
        if let attributed = try? AttributedString(markdown: value) {
            return Text(attributed)
        }
        return Text(value)
    }
}
