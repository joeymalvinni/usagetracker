import SwiftUI

struct ProviderCollectionNotice: View {
    @EnvironmentObject var state: AppState
    let provider: ProviderVM
    let issue: ProviderCollectionIssue

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Spacing.sm) {
            HStack(alignment: .firstTextBaseline, spacing: Theme.Spacing.sm) {
                Label(issue.summary, systemImage: "info.circle")
                    .font(Theme.Typography.caption.weight(.medium))
                Spacer(minLength: 0)
                if state.isProviderSignInActive(provider.providerId) {
                    Button("Cancel sign-in") { state.cancelProviderSignIn(provider.providerId) }
                        .buttonStyle(.chip)
                } else if let action = issue.recoveryAction,
                          state.canRecoverProvider(provider.providerId, action: action) {
                    Button(action.label) {
                        Task {
                            await state.recoverProvider(
                                provider.providerId, accountId: provider.accountId, action: action
                            )
                        }
                    }
                    .buttonStyle(.chip)
                    .disabled(state.daemon != .online || state.providerRecoveryIsBusy(provider.providerId))
                }
            }
            Text(issue.explanation)
                .font(Theme.Typography.caption)
                .fixedSize(horizontal: false, vertical: true)
            if let detail = provider.errorDetail, !detail.isEmpty {
                DisclosureGroup("Details") {
                    Text(detail)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.top, Theme.Spacing.xs)
                }
                .font(Theme.Typography.micro)
            }
        }
        .foregroundStyle(.secondary)
        .surfaceInset()
    }
}
