import AppKit
import SwiftUI

struct ProviderPalette {
    let chart: Color
    let progressStart: Color
    let progressEnd: Color

    var progress: LinearGradient {
        LinearGradient(
            colors: [progressEnd, chart, progressStart],
            startPoint: .leading,
            endPoint: .trailing
        )
    }
}

enum ProviderBrand {
    @MainActor private static var cache = [String: NSImage?]()

    private static let fallbackPalette = ProviderPalette(
        chart: Color(red: 0.45, green: 0.52, blue: 0.78),
        progressStart: Color(red: 0.58, green: 0.64, blue: 0.90),
        progressEnd: Color(red: 0.35, green: 0.42, blue: 0.68)
    )

    private static let palettes: [String: ProviderPalette] = [
        "claude": ProviderPalette(
            chart: Color(red: 0.85, green: 0.47, blue: 0.34),
            progressStart: Color(red: 0.96, green: 0.61, blue: 0.45),
            progressEnd: Color(red: 0.70, green: 0.34, blue: 0.24)
        ),
        "codex": ProviderPalette(
            chart: Color(red: 0.28, green: 0.62, blue: 0.60),
            progressStart: Color(red: 0.38, green: 0.76, blue: 0.74),
            progressEnd: Color(red: 0.20, green: 0.47, blue: 0.50)
        ),
    ]

    @MainActor static func image(_ id: String) -> NSImage? {
        if let cached = cache[id] { return cached }
        let name: String? = switch id {
        case "codex": "chatgpt"
        case "claude": "claude"
        default: nil
        }
        var image: NSImage?
        if let name,
           let url = Bundle.module.url(forResource: name, withExtension: "svg", subdirectory: "Resources"),
           let loaded = NSImage(contentsOf: url) {
            loaded.isTemplate = true
            image = loaded
        }
        cache[id] = image
        return image
    }

    static func palette(_ id: String) -> ProviderPalette {
        palettes[id] ?? fallbackPalette
    }
}
