import SwiftUI

/// Numeric text helper: monospaced digits + an animated content transition
/// so values roll when data refreshes.
struct NumText: View {
    let value: String
    var font: Font = .subheadline.weight(.semibold)
    var animate: Bool = true

    var body: some View {
        Text(value)
            .font(font)
            .monospacedDigit()
            .contentTransition(animate ? .numericText() : .identity)
            .animation(.spring(duration: 0.35), value: value)
    }
}
