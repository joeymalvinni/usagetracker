import AppKit

enum MenuBarProgressIcon {
    private static let maxRows = 2
    private static let height: CGFloat = 18
    private static let horizontalInset: CGFloat = 2

    static func statusItemLength(for rows: [MenuBarProviderVM]) -> CGFloat {
        imageSize(for: rows).width + 8
    }

    static func image(for rows: [MenuBarProviderVM], status: DisplayStatus) -> NSImage {
        let size = imageSize(for: rows)
        let image = NSImage(size: size, flipped: false) { rect in
            draw(in: rect, rows: Array(rows.prefix(maxRows)), status: status)
            return true
        }
        image.isTemplate = false
        image.accessibilityDescription = "Usage"
        return image
    }

    private static func imageSize(for rows: [MenuBarProviderVM]) -> NSSize {
        let visibleCount = max(1, min(rows.count, maxRows))
        return NSSize(width: visibleCount == 1 ? 28 : 34, height: height)
    }

    private static func draw(in rect: NSRect, rows: [MenuBarProviderVM], status: DisplayStatus) {
        let visibleRows = rows.isEmpty ? [placeholderRow(for: status)] : rows
        let rowCount = visibleRows.count
        let rowHeight: CGFloat = rowCount == 2 ? 4 : 6
        let gap: CGFloat = 3
        let totalHeight = CGFloat(rowCount) * rowHeight + CGFloat(max(0, rowCount - 1)) * gap
        let trackWidth = rect.width - horizontalInset * 2
        let bottom = rect.midY - totalHeight / 2

        for (index, row) in visibleRows.enumerated() {
            let y = bottom + CGFloat(rowCount - index - 1) * (rowHeight + gap)
            let track = NSRect(x: horizontalInset, y: y, width: trackWidth, height: rowHeight)
            drawPill(track, color: NSColor.labelColor.withAlphaComponent(0.16))

            if let percent = row.percent {
                let fill = max(0, min(1, percent / 100))
                if fill > 0 {
                    let fillWidth = max(rowHeight, track.width * CGFloat(fill))
                    let fillRect = NSRect(x: track.minX, y: track.minY, width: min(track.width, fillWidth), height: track.height)
                    drawPill(fillRect, color: fillColor(for: row))
                }
            }
        }
    }

    private static func drawPill(_ rect: NSRect, color: NSColor) {
        color.setFill()
        NSBezierPath(roundedRect: rect, xRadius: rect.height / 2, yRadius: rect.height / 2).fill()
    }

    private static func fillColor(for row: MenuBarProviderVM) -> NSColor {
        switch row.status {
        case .warning:
            return .systemOrange
        case .critical, .error, .offline:
            return .systemRed
        case .stale, .disabled:
            return .secondaryLabelColor
        case .normal:
            return NSColor(ProviderBrand.palette(row.providerId).chart)
        }
    }

    private static func placeholderRow(for status: DisplayStatus) -> MenuBarProviderVM {
        let percent: Double? = switch status {
        case .offline, .error, .warning, .critical:
            100
        case .stale, .disabled, .normal:
            nil
        }
        return MenuBarProviderVM(id: "usage", providerId: "usage", short: "", percent: percent, status: status)
    }
}
