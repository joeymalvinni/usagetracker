import Foundation
import SQLite3
import UsageCore

public final class UsageDatabase {
    private let handle: OpaquePointer
    public let path: String

    public init(path: String) throws {
        self.path = path
        var db: OpaquePointer?
        if sqlite3_open(path, &db) != SQLITE_OK {
            throw DatabaseError.openFailed(String(cString: sqlite3_errmsg(db)))
        }
        guard let db else {
            throw DatabaseError.openFailed("sqlite3_open returned nil")
        }
        handle = db
        try execute("PRAGMA busy_timeout=5000")
        try execute("PRAGMA journal_mode=WAL")
        try execute("PRAGMA foreign_keys=ON")
        try migrate()
    }

    deinit {
        sqlite3_close(handle)
    }

    public func upsert(events: [UsageEvent]) throws {
        guard !events.isEmpty else {
            return
        }
        try execute("BEGIN IMMEDIATE")
        do {
            let sql = """
            INSERT INTO usage_events (
                id, service, source_kind, account_label, model, started_at, ended_at,
                input_tokens, output_tokens, cached_input_tokens, requests,
                cost_amount, cost_currency, metadata_json
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                service = excluded.service,
                source_kind = excluded.source_kind,
                account_label = excluded.account_label,
                model = excluded.model,
                started_at = excluded.started_at,
                ended_at = excluded.ended_at,
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cached_input_tokens = excluded.cached_input_tokens,
                requests = excluded.requests,
                cost_amount = excluded.cost_amount,
                cost_currency = excluded.cost_currency,
                metadata_json = excluded.metadata_json
            """
            let statement = try Statement(database: handle, sql: sql)
            defer { statement.finalize() }

            for event in events {
                statement.reset()
                try statement.bind(event.id, at: 1)
                try statement.bind(event.service.rawValue, at: 2)
                try statement.bind(event.sourceKind.rawValue, at: 3)
                try statement.bind(event.accountLabel, at: 4)
                try statement.bind(event.model, at: 5)
                try statement.bind(event.startedAt.timeIntervalSince1970, at: 6)
                try statement.bind(event.endedAt.timeIntervalSince1970, at: 7)
                try statement.bind(Int64(event.inputTokens), at: 8)
                try statement.bind(Int64(event.outputTokens), at: 9)
                try statement.bind(Int64(event.cachedInputTokens), at: 10)
                try statement.bind(Int64(event.requests), at: 11)
                try statement.bind(event.costAmount.map { NSDecimalNumber(decimal: $0).doubleValue }, at: 12)
                try statement.bind(event.costCurrency, at: 13)
                try statement.bind(jsonString(event.metadata), at: 14)
                try statement.stepDone()
            }
            try execute("COMMIT")
        } catch {
            try? execute("ROLLBACK")
            throw error
        }
    }

    public func upsert(windows: [QuotaWindow]) throws {
        guard !windows.isEmpty else {
            return
        }
        try execute("BEGIN IMMEDIATE")
        do {
            let sql = """
            INSERT INTO quota_windows (
                id, service, source_kind, account_label, kind, started_at, reset_at,
                used_units, limit_units, unit, observed_at, metadata_json
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                service = excluded.service,
                source_kind = excluded.source_kind,
                account_label = excluded.account_label,
                kind = excluded.kind,
                started_at = excluded.started_at,
                reset_at = excluded.reset_at,
                used_units = excluded.used_units,
                limit_units = excluded.limit_units,
                unit = excluded.unit,
                observed_at = excluded.observed_at,
                metadata_json = excluded.metadata_json
            """
            let statement = try Statement(database: handle, sql: sql)
            defer { statement.finalize() }

            for window in windows {
                statement.reset()
                try statement.bind(window.id, at: 1)
                try statement.bind(window.service.rawValue, at: 2)
                try statement.bind(window.sourceKind.rawValue, at: 3)
                try statement.bind(window.accountLabel, at: 4)
                try statement.bind(window.kind.rawValue, at: 5)
                try statement.bind(window.startedAt.timeIntervalSince1970, at: 6)
                try statement.bind(window.resetAt?.timeIntervalSince1970, at: 7)
                try statement.bind(window.usedUnits, at: 8)
                try statement.bind(window.limitUnits, at: 9)
                try statement.bind(window.unit.rawValue, at: 10)
                try statement.bind(window.observedAt.timeIntervalSince1970, at: 11)
                try statement.bind(jsonString(window.metadata), at: 12)
                try statement.stepDone()
            }
            try execute("COMMIT")
        } catch {
            try? execute("ROLLBACK")
            throw error
        }
    }

    public func deleteWindows(sourceKinds: [SourceKind]) throws {
        guard !sourceKinds.isEmpty else {
            return
        }
        let placeholders = Array(repeating: "?", count: sourceKinds.count).joined(separator: ", ")
        let statement = try Statement(
            database: handle,
            sql: "DELETE FROM quota_windows WHERE source_kind IN (\(placeholders))"
        )
        defer { statement.finalize() }

        for (index, sourceKind) in sourceKinds.enumerated() {
            try statement.bind(sourceKind.rawValue, at: Int32(index + 1))
        }
        try statement.stepDone()
    }

    public func events(since: Date? = nil) throws -> [UsageEvent] {
        var sql = "SELECT * FROM usage_events"
        if since != nil {
            sql += " WHERE ended_at >= ?"
        }
        sql += " ORDER BY started_at ASC"

        let statement = try Statement(database: handle, sql: sql)
        defer { statement.finalize() }
        if let since {
            try statement.bind(since.timeIntervalSince1970, at: 1)
        }

        var events: [UsageEvent] = []
        while try statement.stepRow() {
            events.append(
                UsageEvent(
                    id: statement.string("id") ?? "",
                    service: UsageService(rawValue: statement.string("service") ?? "") ?? .codex,
                    sourceKind: SourceKind(rawValue: statement.string("source_kind") ?? "") ?? .configured,
                    accountLabel: statement.string("account_label"),
                    model: statement.string("model"),
                    startedAt: Date(timeIntervalSince1970: statement.double("started_at") ?? 0),
                    endedAt: Date(timeIntervalSince1970: statement.double("ended_at") ?? 0),
                    inputTokens: Int(statement.int("input_tokens") ?? 0),
                    outputTokens: Int(statement.int("output_tokens") ?? 0),
                    cachedInputTokens: Int(statement.int("cached_input_tokens") ?? 0),
                    requests: Int(statement.int("requests") ?? 0),
                    costAmount: statement.double("cost_amount").map { Decimal($0) },
                    costCurrency: statement.string("cost_currency"),
                    metadata: parseStringMap(statement.string("metadata_json"))
                )
            )
        }
        return events
    }

    public func windows() throws -> [QuotaWindow] {
        let statement = try Statement(
            database: handle,
            sql: "SELECT * FROM quota_windows ORDER BY service ASC, kind ASC, observed_at DESC"
        )
        defer { statement.finalize() }

        var windows: [QuotaWindow] = []
        while try statement.stepRow() {
            windows.append(
                QuotaWindow(
                    id: statement.string("id") ?? "",
                    service: UsageService(rawValue: statement.string("service") ?? "") ?? .codex,
                    sourceKind: SourceKind(rawValue: statement.string("source_kind") ?? "") ?? .configured,
                    accountLabel: statement.string("account_label"),
                    kind: QuotaWindowKind(rawValue: statement.string("kind") ?? "") ?? .observed,
                    startedAt: Date(timeIntervalSince1970: statement.double("started_at") ?? 0),
                    resetAt: statement.double("reset_at").map { Date(timeIntervalSince1970: $0) },
                    usedUnits: statement.double("used_units") ?? 0,
                    limitUnits: statement.double("limit_units"),
                    unit: UsageUnit(rawValue: statement.string("unit") ?? "") ?? .tokens,
                    observedAt: Date(timeIntervalSince1970: statement.double("observed_at") ?? 0),
                    metadata: parseStringMap(statement.string("metadata_json"))
                )
            )
        }
        return windows
    }

    public func applyConfiguredWindows(config: AppConfig, now: Date) throws {
        let allEvents = try events()
        var windows: [QuotaWindow] = []

        for account in config.accounts {
            for window in account.windows {
                let interval = intervalFor(window: window, now: now)
                let matchingEvents = allEvents.filter {
                    $0.service == account.service
                        && $0.startedAt >= interval.start
                        && $0.startedAt < interval.end
                }
                let used = aggregate(events: matchingEvents, unit: window.unit)
                var metadata = ["account_id": account.id]
                if let title = account.title {
                    metadata["title"] = title
                }
                windows.append(
                    QuotaWindow(
                        id: "configured:\(account.id):\(window.kind.rawValue)",
                        service: account.service,
                        sourceKind: .configured,
                        accountLabel: account.accountLabel,
                        kind: window.kind,
                        startedAt: interval.start,
                        resetAt: interval.end,
                        usedUnits: used,
                        limitUnits: window.limit,
                        unit: window.unit,
                        observedAt: now,
                        metadata: metadata
                    )
                )
            }
        }

        try upsert(windows: windows)
    }

    public func eventCount() throws -> Int {
        let statement = try Statement(database: handle, sql: "SELECT COUNT(*) AS count FROM usage_events")
        defer { statement.finalize() }
        guard try statement.stepRow() else {
            return 0
        }
        return Int(statement.int("count") ?? 0)
    }

    private func migrate() throws {
        try execute(
            """
            CREATE TABLE IF NOT EXISTS usage_events (
                id TEXT PRIMARY KEY,
                service TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                account_label TEXT,
                model TEXT,
                started_at REAL NOT NULL,
                ended_at REAL NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cached_input_tokens INTEGER NOT NULL DEFAULT 0,
                requests INTEGER NOT NULL DEFAULT 0,
                cost_amount REAL,
                cost_currency TEXT,
                metadata_json TEXT NOT NULL DEFAULT '{}'
            )
            """
        )
        try execute("CREATE INDEX IF NOT EXISTS idx_usage_events_service_time ON usage_events(service, started_at)")
        try execute(
            """
            CREATE TABLE IF NOT EXISTS quota_windows (
                id TEXT PRIMARY KEY,
                service TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                account_label TEXT,
                kind TEXT NOT NULL,
                started_at REAL NOT NULL,
                reset_at REAL,
                used_units REAL NOT NULL,
                limit_units REAL,
                unit TEXT NOT NULL,
                observed_at REAL NOT NULL,
                metadata_json TEXT NOT NULL DEFAULT '{}'
            )
            """
        )
        try execute("CREATE INDEX IF NOT EXISTS idx_quota_windows_service_kind ON quota_windows(service, kind)")
    }

    private func execute(_ sql: String) throws {
        if sqlite3_exec(handle, sql, nil, nil, nil) != SQLITE_OK {
            throw DatabaseError.queryFailed(String(cString: sqlite3_errmsg(handle)))
        }
    }

    private func aggregate(events: [UsageEvent], unit: UsageUnit) -> Double {
        switch unit {
        case .tokens:
            Double(events.reduce(0) { $0 + $1.totalTokens })
        case .usd:
            events.reduce(0) { partial, event in
                partial + (event.costAmount.map { NSDecimalNumber(decimal: $0).doubleValue } ?? 0)
            }
        case .credits:
            events.reduce(0) { partial, event in
                partial + (Double(event.metadata["credits"] ?? "") ?? 0)
            }
        case .requests:
            Double(events.reduce(0) { $0 + $1.requests })
        case .sessions:
            Double(events.count)
        case .messages:
            Double(events.reduce(0) { $0 + Int($1.metadata["messages"] ?? "0", default: 0) })
        case .percent:
            0
        }
    }

    private func intervalFor(window: ConfiguredWindow, now: Date) -> DateInterval {
        if let startsAt = window.startsAt, let resetAt = window.resetAt, resetAt > startsAt {
            return DateInterval(start: startsAt, end: resetAt)
        }

        var calendar = Calendar.current
        calendar.timeZone = .current
        let anchorHour = window.anchorHour ?? 0

        switch window.period {
        case .day:
            let start = anchoredStart(of: .day, now: now, anchorHour: anchorHour, calendar: calendar)
            return DateInterval(start: start, end: calendar.date(byAdding: .day, value: 1, to: start) ?? now)
        case .week:
            let base = calendar.dateInterval(of: .weekOfYear, for: now)?.start ?? calendar.startOfDay(for: now)
            let start = calendar.date(byAdding: .hour, value: anchorHour, to: base) ?? base
            return DateInterval(start: start, end: calendar.date(byAdding: .weekOfYear, value: 1, to: start) ?? now)
        case .month:
            let base = calendar.dateInterval(of: .month, for: now)?.start ?? calendar.startOfDay(for: now)
            let start = calendar.date(byAdding: .hour, value: anchorHour, to: base) ?? base
            return DateInterval(start: start, end: calendar.date(byAdding: .month, value: 1, to: start) ?? now)
        case .rollingHours:
            let hours = window.rollingHours ?? 5
            let start = now.addingTimeInterval(-hours * 3_600)
            return DateInterval(start: start, end: now.addingTimeInterval(hours * 3_600))
        }
    }

    private func anchoredStart(
        of component: Calendar.Component,
        now: Date,
        anchorHour: Int,
        calendar: Calendar
    ) -> Date {
        let base = calendar.dateInterval(of: component, for: now)?.start ?? calendar.startOfDay(for: now)
        let anchored = calendar.date(byAdding: .hour, value: anchorHour, to: base) ?? base
        if anchored <= now {
            return anchored
        }
        return calendar.date(byAdding: component, value: -1, to: anchored) ?? anchored
    }
}

public enum DatabaseError: Error, CustomStringConvertible {
    case openFailed(String)
    case queryFailed(String)
    case prepareFailed(String)
    case bindFailed(String)
    case stepFailed(String)

    public var description: String {
        switch self {
        case let .openFailed(message):
            "open failed: \(message)"
        case let .queryFailed(message):
            "query failed: \(message)"
        case let .prepareFailed(message):
            "prepare failed: \(message)"
        case let .bindFailed(message):
            "bind failed: \(message)"
        case let .stepFailed(message):
            "step failed: \(message)"
        }
    }
}

private final class Statement {
    private let database: OpaquePointer
    private var statement: OpaquePointer?
    private var columnMap: [String: Int32] = [:]

    init(database: OpaquePointer, sql: String) throws {
        self.database = database
        if sqlite3_prepare_v2(database, sql, -1, &statement, nil) != SQLITE_OK {
            throw DatabaseError.prepareFailed(String(cString: sqlite3_errmsg(database)))
        }
    }

    func finalize() {
        sqlite3_finalize(statement)
    }

    func reset() {
        sqlite3_reset(statement)
        sqlite3_clear_bindings(statement)
    }

    func bind(_ value: String?, at index: Int32) throws {
        guard let value else {
            sqlite3_bind_null(statement, index)
            return
        }
        if sqlite3_bind_text(statement, index, value, -1, transientDestructor) != SQLITE_OK {
            throw DatabaseError.bindFailed(String(cString: sqlite3_errmsg(database)))
        }
    }

    func bind(_ value: Double?, at index: Int32) throws {
        guard let value else {
            sqlite3_bind_null(statement, index)
            return
        }
        if sqlite3_bind_double(statement, index, value) != SQLITE_OK {
            throw DatabaseError.bindFailed(String(cString: sqlite3_errmsg(database)))
        }
    }

    func bind(_ value: Int64, at index: Int32) throws {
        if sqlite3_bind_int64(statement, index, value) != SQLITE_OK {
            throw DatabaseError.bindFailed(String(cString: sqlite3_errmsg(database)))
        }
    }

    func stepDone() throws {
        let result = sqlite3_step(statement)
        guard result == SQLITE_DONE else {
            throw DatabaseError.stepFailed(String(cString: sqlite3_errmsg(database)))
        }
    }

    func stepRow() throws -> Bool {
        let result = sqlite3_step(statement)
        if result == SQLITE_ROW {
            if columnMap.isEmpty {
                buildColumnMap()
            }
            return true
        }
        if result == SQLITE_DONE {
            return false
        }
        throw DatabaseError.stepFailed(String(cString: sqlite3_errmsg(database)))
    }

    func string(_ column: String) -> String? {
        guard let index = columnMap[column], sqlite3_column_type(statement, index) != SQLITE_NULL else {
            return nil
        }
        guard let text = sqlite3_column_text(statement, index) else {
            return nil
        }
        return String(cString: text)
    }

    func double(_ column: String) -> Double? {
        guard let index = columnMap[column], sqlite3_column_type(statement, index) != SQLITE_NULL else {
            return nil
        }
        return sqlite3_column_double(statement, index)
    }

    func int(_ column: String) -> Int64? {
        guard let index = columnMap[column], sqlite3_column_type(statement, index) != SQLITE_NULL else {
            return nil
        }
        return sqlite3_column_int64(statement, index)
    }

    private func buildColumnMap() {
        let count = sqlite3_column_count(statement)
        for index in 0..<count {
            guard let name = sqlite3_column_name(statement, index) else {
                continue
            }
            columnMap[String(cString: name)] = index
        }
    }
}

private let transientDestructor = unsafeBitCast(-1, to: sqlite3_destructor_type.self)

private func jsonString(_ value: [String: String]) -> String {
    guard let data = try? JSONEncoder().encode(value), let string = String(data: data, encoding: .utf8) else {
        return "{}"
    }
    return string
}

private func parseStringMap(_ value: String?) -> [String: String] {
    guard let value, let data = value.data(using: .utf8) else {
        return [:]
    }
    return (try? JSONDecoder().decode([String: String].self, from: data)) ?? [:]
}

private extension Int {
    init(_ value: String, default defaultValue: Int) {
        self = Int(value) ?? defaultValue
    }
}
