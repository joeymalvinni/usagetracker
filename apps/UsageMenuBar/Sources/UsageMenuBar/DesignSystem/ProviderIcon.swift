import SwiftUI

/// Brand logo rendered as a tintable template, falling back to the provider's SF Symbol.
struct ProviderIcon: View {
    let id: String
    let symbol: String
    var size: CGFloat = 15

    var body: some View {
        if let image = ProviderBrand.image(id) {
            Image(nsImage: image)
                .resizable()
                .renderingMode(.template)
                .scaledToFit()
                .frame(width: size, height: size)
        } else {
            Image(systemName: symbol)
        }
    }
}