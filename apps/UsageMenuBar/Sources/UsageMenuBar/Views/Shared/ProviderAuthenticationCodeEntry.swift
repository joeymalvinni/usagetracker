import SwiftUI

struct ProviderAuthenticationCodeEntry: View {
    @EnvironmentObject private var state: AppState
    let providerId: String
    @State private var authenticationCode = ""

    private var isSubmitting: Bool {
        state.pendingAuthenticationCodeProviders.contains(providerId)
    }

    private var canSubmit: Bool {
        !authenticationCode.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !isSubmitting
    }

    var body: some View {
        DisclosureGroup("Enter a code (if shown)") {
            HStack(spacing: Theme.Spacing.sm) {
                SecureField("Authentication code", text: $authenticationCode)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit { submit() }
                Button("Submit") { submit() }
                    .buttonStyle(.chipProminent)
                    .disabled(!canSubmit)
                if isSubmitting {
                    ProgressView().controlSize(.small)
                }
            }
        }
        .font(Theme.Typography.caption)
    }

    private func submit() {
        guard canSubmit else { return }
        Task {
            if await state.submitProviderAuthenticationCode(
                authenticationCode,
                providerId: providerId
            ) {
                authenticationCode = ""
            }
        }
    }
}
