import Foundation

public enum CardRenderer {
    public static func render(_ cards: [UsageCard]) -> String {
        if cards.isEmpty {
            return "No usage data yet. Run `usage doctor` to see available sources."
        }

        return "AI Usage\n\n" + cards.map(renderCard).joined(separator: "\n\n")
    }

    private static func renderCard(_ card: UsageCard) -> String {
        let labelWidth = max(8, card.rows.map { $0.label.count }.max() ?? 0)
        let rowStrings = card.rows.map { row in
            if card.title == "Overview", let detail = row.detail {
                return row.label.padding(toLength: 17, withPad: " ", startingAt: 0)
                    + row.value.padding(toLength: 11, withPad: " ", startingAt: 0)
                    + detail
            }

            if card.title.hasPrefix("Activity ·"), let bar = row.bar {
                return row.label.padding(toLength: 8, withPad: " ", startingAt: 0)
                    + " "
                    + leftPad(row.value, toLength: 4)
                    + "  "
                    + bar
            }

            var line = "\(row.label.padding(toLength: labelWidth, withPad: " ", startingAt: 0)) \(row.value)"
            if let bar = row.bar {
                line += "  \(bar)"
            }
            if let detail = row.detail {
                line += "  \(detail)"
            }
            return line
        }

        let contentWidth = max(([card.title.count + 4] + rowStrings.map(\.count)).max() ?? 0, 54)
        let top = "┌─ \(card.title) " + String(repeating: "─", count: max(contentWidth - card.title.count - 1, 0)) + "┐"
        let body = rowStrings.map { row in
            "│ " + row.padding(toLength: contentWidth, withPad: " ", startingAt: 0) + " │"
        }
        let bottom = "└" + String(repeating: "─", count: contentWidth + 2) + "┘"
        return ([top] + body + [bottom]).joined(separator: "\n")
    }

    private static func leftPad(_ value: String, toLength length: Int) -> String {
        if value.count >= length {
            return value
        }
        return String(repeating: " ", count: length - value.count) + value
    }
}

public enum TableRenderer {
    public static func render(_ cards: [UsageCard]) -> String {
        if cards.isEmpty {
            return "No usage data yet. Run `usage doctor` to see available sources."
        }

        var lines = ["SERVICE   WINDOW      VALUE             USAGE          DETAIL"]
        for card in cards {
            let service = card.title.components(separatedBy: " · ").first ?? card.title
            for row in card.rows where row.label != "Account" {
                let bar = row.bar ?? ""
                let detail = row.detail ?? ""
                lines.append(
                    service.padding(toLength: 8, withPad: " ", startingAt: 0)
                        + "  "
                        + row.label.padding(toLength: 10, withPad: " ", startingAt: 0)
                        + "  "
                        + row.value.padding(toLength: 16, withPad: " ", startingAt: 0)
                        + "  "
                        + bar.padding(toLength: 13, withPad: " ", startingAt: 0)
                        + "  "
                        + detail
                )
            }
        }
        return lines.joined(separator: "\n")
    }
}
