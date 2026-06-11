import Foundation

public struct PaceProjection: Equatable, Sendable {
    public enum Status: String, Sendable {
        case noLimit
        case full
        case onTrack
        case warm
        case hot
        case empty
    }

    public var usedFraction: Double?
    public var leftFraction: Double?
    public var elapsedFraction: Double?
    public var burnIndex: Double?
    public var projectedEndFraction: Double?
    public var projectedEmptyAt: Date?
    public var status: Status
    public var summary: String
}

public enum PaceEngine {
    public static func project(window: QuotaWindow, now: Date) -> PaceProjection {
        guard let limit = window.limitUnits, limit > 0 else {
            return PaceProjection(
                usedFraction: nil,
                leftFraction: nil,
                elapsedFraction: nil,
                burnIndex: nil,
                projectedEndFraction: nil,
                projectedEmptyAt: nil,
                status: .noLimit,
                summary: "\(Format.shortNumber(window.usedUnits)) \(window.unit.displayName) observed"
            )
        }

        let usedFraction = clamp(window.usedUnits / limit, lower: 0, upper: 10)
        let leftFraction = clamp(1 - usedFraction, lower: 0, upper: 1)

        guard let resetAt = window.resetAt, resetAt > window.startedAt else {
            let status: PaceProjection.Status = usedFraction >= 1 ? .empty : (usedFraction == 0 ? .full : .onTrack)
            return PaceProjection(
                usedFraction: usedFraction,
                leftFraction: leftFraction,
                elapsedFraction: nil,
                burnIndex: nil,
                projectedEndFraction: nil,
                projectedEmptyAt: nil,
                status: status,
                summary: status == .empty ? "depleted" : "\(Format.percent(leftFraction)) left"
            )
        }

        let totalDuration = resetAt.timeIntervalSince(window.startedAt)
        let elapsed = clamp(now.timeIntervalSince(window.startedAt) / totalDuration, lower: 0.0001, upper: 1)
        let burnIndex = usedFraction / elapsed
        let projectedEndFraction = burnIndex
        let remaining = max(limit - window.usedUnits, 0)
        let elapsedSeconds = max(now.timeIntervalSince(window.startedAt), 1)
        let rate = max(window.usedUnits / elapsedSeconds, 0)
        let projectedEmptyAt = rate > 0 && remaining > 0 ? now.addingTimeInterval(remaining / rate) : nil

        let status: PaceProjection.Status
        let summary: String
        if usedFraction >= 1 {
            status = .empty
            summary = "depleted"
        } else if usedFraction == 0 {
            status = .full
            summary = paceSummary(usedFraction: usedFraction, elapsedFraction: elapsed)
        } else if projectedEndFraction <= 0.85 {
            status = .onTrack
            summary = paceSummary(usedFraction: usedFraction, elapsedFraction: elapsed)
        } else if projectedEndFraction <= 1.0 {
            status = .onTrack
            summary = paceSummary(usedFraction: usedFraction, elapsedFraction: elapsed)
        } else if projectedEndFraction <= 1.25 {
            status = .warm
            summary = "\(Format.percent(projectedEndFraction - 1)) over pace"
        } else {
            status = .hot
            if let projectedEmptyAt {
                summary = "\(Format.multiplier(burnIndex)) pace, empty \(Format.relativeTime(until: projectedEmptyAt, from: now))"
            } else {
                summary = "\(Format.multiplier(burnIndex)) pace"
            }
        }

        return PaceProjection(
            usedFraction: usedFraction,
            leftFraction: leftFraction,
            elapsedFraction: elapsed,
            burnIndex: burnIndex,
            projectedEndFraction: projectedEndFraction,
            projectedEmptyAt: projectedEmptyAt,
            status: status,
            summary: summary
        )
    }

    private static func clamp(_ value: Double, lower: Double, upper: Double) -> Double {
        min(max(value, lower), upper)
    }

    private static func paceSummary(usedFraction: Double, elapsedFraction: Double) -> String {
        let reserve = max(elapsedFraction - usedFraction, 0)
        return "\(Format.percent(reserve)) in reserve | Expected \(Format.percent(elapsedFraction)) used | Lasts until reset"
    }
}
