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
            let fillWidth = fill <= 0 ? 0 : max(geo.size.height, geo.size.width * fill)
            ZStack(alignment: .leading) {
                Capsule().fill(.quaternary.opacity(0.5))
                Capsule()
                    .fill(fillStyle)
                    .frame(width: fillWidth)
            }
        }
        .frame(height: 5)
    }

    private var fillStyle: AnyShapeStyle {
        if percent < 10 {
            return AnyShapeStyle(Color.red)
        }
        if percent < 25 {
            return AnyShapeStyle(Color.orange)
        }
        return AnyShapeStyle(ProviderBrand.palette(providerId).progress)
    }
}
