import SwiftUI

struct PricingCoverageNotice: View {
    let onDismiss: () -> Void

    var body: some View {
        HStack(spacing: Theme.Spacing.sm) {
            Image(systemName: "info.circle")
                .foregroundStyle(.secondary)
            Text("Some model prices are unavailable. Cost totals may be incomplete.")
                .font(Theme.Typography.caption)
                .foregroundStyle(.secondary)
            Spacer(minLength: Theme.Spacing.sm)
            Button(action: onDismiss) {
                Image(systemName: "xmark")
                    .font(Theme.Typography.micro.weight(.bold))
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
            .help("Hide")
        }
        .surfaceInset()
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
