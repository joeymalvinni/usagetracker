import SwiftUI

/// Capsule progress bar tinted by provider identity. Charts use the matching
/// flat color; progress bars use the richer gradient from the same palette.
struct ProgressBar: View {
    let percent: Double
    let status: DisplayStatus
    let providerId: String

    var body: some View {
        GeometryReader { geo in
            let fill = max(0, min(1, percent / 100))
            ZStack(alignment: .leading) {
                Capsule().fill(.quaternary.opacity(0.5))
                Capsule()
                    .fill(ProviderBrand.palette(providerId).progress)
                    .overlay {
                        if status.needsAttention {
                            Capsule()
                                .fill(status.tint.opacity(0.20))
                        }
                    }
                    .frame(width: max(geo.size.height, geo.size.width * fill))
            }
        }
        .frame(height: 5)
    }
}
