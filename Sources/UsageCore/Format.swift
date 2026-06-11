import Foundation

public enum Format {
    public static func percent(_ fraction: Double) -> String {
        "\(Int((fraction * 100).rounded()))%"
    }

    public static func multiplier(_ value: Double) -> String {
        String(format: "%.1fx", value)
    }

    public static func shortNumber(_ value: Double) -> String {
        let absolute = abs(value)
        if absolute >= 1_000_000_000 {
            return String(format: "%.1fB", value / 1_000_000_000)
        }
        if absolute >= 1_000_000 {
            return String(format: "%.1fM", value / 1_000_000)
        }
        if absolute >= 1_000 {
            return String(format: "%.1fk", value / 1_000)
        }
        if value.rounded() == value {
            return String(Int(value))
        }
        return String(format: "%.1f", value)
    }

    public static func activityNumber(_ value: Double) -> String {
        let absolute = abs(value)
        if absolute >= 1_000_000_000 {
            return "\(Int((value / 1_000_000_000).rounded()))B"
        }
        if absolute >= 1_000_000 {
            return "\(Int((value / 1_000_000).rounded()))M"
        }
        if absolute >= 1_000 {
            return "\(Int((value / 1_000).rounded()))k"
        }
        return "\(Int(value.rounded()))"
    }

    public static func duration(seconds: Int) -> String {
        let minutes = seconds / 60
        let remainingSeconds = seconds % 60
        if minutes >= 60 {
            let hours = minutes / 60
            let remainingMinutes = minutes % 60
            return remainingMinutes > 0 ? "\(hours)h \(remainingMinutes)m" : "\(hours)h"
        }
        if minutes > 0 {
            return "\(minutes)m \(remainingSeconds)s"
        }
        return "\(remainingSeconds)s"
    }

    public static func bar(leftFraction: Double?, width: Int = 12) -> String {
        guard let leftFraction else {
            return String(repeating: "·", count: width)
        }
        let filled = min(max(Int((leftFraction * Double(width)).rounded()), 0), width)
        return String(repeating: "█", count: filled) + String(repeating: "░", count: width - filled)
    }

    public static func resetText(resetAt: Date?, now: Date) -> String? {
        guard let resetAt else {
            return nil
        }
        if resetAt > now, !Calendar.current.isDate(resetAt, inSameDayAs: now) {
            return "resets \(relativeTime(until: resetAt, from: now))"
        }
        let formatter = DateFormatter()
        formatter.timeStyle = .short
        formatter.dateStyle = .none
        return "resets \(formatter.string(from: resetAt))"
    }

    public static func relativeTime(until target: Date, from now: Date) -> String {
        let seconds = Int(target.timeIntervalSince(now).rounded())
        if seconds <= 0 {
            return "now"
        }
        let days = seconds / 86_400
        let hours = (seconds % 86_400) / 3_600
        let minutes = (seconds % 3_600) / 60

        if days > 0 {
            return hours > 0 ? "in \(days)d \(hours)h" : "in \(days)d"
        }
        if hours > 0 {
            return minutes > 0 ? "in \(hours)h \(minutes)m" : "in \(hours)h"
        }
        return "in \(max(minutes, 1))m"
    }
}
