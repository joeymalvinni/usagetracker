import SwiftUI

enum Theme {
    enum Spacing {
        static let xs: CGFloat = 4
        static let sm: CGFloat = 8
        static let md: CGFloat = 12
        static let lg: CGFloat = 16
        static let xl: CGFloat = 24
        static let xxl: CGFloat = 32
    }
    enum Radius {
        static let sm: CGFloat = 6
        static let md: CGFloat = 10
        static let lg: CGFloat = 14
        static let xl: CGFloat = 24
    }
    enum Popover {
        static let width: CGFloat = 440
        static let height: CGFloat = 620
    }
    enum Typography {
        /// Primary metric in a provider row. Semibold title3 — prominent
        /// without the display-size shouting of the old `.largeTitle` hero.
        static let metric = Font.title3.weight(.semibold)
        static let title = Font.title3.bold()
        static let headline = Font.headline
        static let body = Font.body
        static let caption = Font.caption
        static let micro = Font.caption2
    }

    /// Flat provider series color. The matching progress gradient lives in
    /// `ProviderBrand.palette(_:)` so charts and progress bars stay in sync.
    static func chartColor(_ providerId: String) -> Color {
        ProviderBrand.palette(providerId).chart
    }
}
