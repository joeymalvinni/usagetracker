import SwiftUI

/// Capsule progress bar tinted by provider identity. Charts use the matching
/// flat color; progress bars use the richer gradient from the same palette.
struct ProgressBar: View {
    @Environment(\.colorScheme) private var colorScheme

    let percent: Double
    let status: DisplayStatus
    let providerId: String
    var forecastPercent: Double? = nil

    var body: some View {
        GeometryReader { geo in
            let trackHeight = 5.0
            let fill = max(0, min(1, percent / 100))
            let fillWidth = fill <= 0 ? 0 : max(trackHeight, geo.size.width * fill)
            ZStack(alignment: .leading) {
                Capsule()
                    .fill(.quaternary.opacity(0.5))
                    .frame(height: trackHeight)
                Capsule()
                    .fill(fillStyle)
                    .frame(width: fillWidth, height: trackHeight)
                if let markerOffset = markerOffset(in: geo.size.width) {
                    // Use an explicit opaque color instead of the semantic
                    // `.primary` style, which macOS may render translucently.
                    // The line extends beyond the track so it reads as a
                    // forecast marker rather than part of the current fill.
                    Capsule()
                        .fill(markerColor)
                        .frame(width: 3, height: 11)
                        .offset(x: markerOffset)
                        .accessibilityHidden(true)
                }
            }
        }
        .frame(height: 11)
        .accessibilityLabel("Percent remaining")
        .accessibilityValue(accessibilityValue)
        .help(forecastHelp)
    }

    private func markerOffset(in width: CGFloat) -> CGFloat? {
        guard let forecastPercent, forecastPercent.isFinite else { return nil }
        let fraction = CGFloat(max(0, min(1, forecastPercent / 100)))
        return min(max(0, width * fraction - 1.5), max(0, width - 3))
    }

    private var accessibilityValue: String {
        let current = "\(Int(percent.rounded()))%"
        guard let forecastPercent, forecastPercent.isFinite else { return current }
        return "\(current), forecast \(Int(max(0, min(100, forecastPercent)).rounded()))% at reset"
    }

    private var forecastHelp: String {
        let current = "\(Int(percent.rounded()))% remaining"
        guard let forecastPercent, forecastPercent.isFinite else { return current }
        let forecast = Int(max(0, min(100, forecastPercent)).rounded())
        return "Projected remaining at reset: \(forecast)% · Current: \(Int(percent.rounded()))%"
    }

    /// Black in light mode and white in dark mode gives the forecast marker a
    /// fully opaque, high-contrast treatment over both the track and fill.
    private var markerColor: Color {
        colorScheme == .dark ? .white : .black
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
