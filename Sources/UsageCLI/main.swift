import Foundation
import UsageCore
import UsageProviders
import UsageStore

@main
struct UsageCommand {
    static func main() async {
        do {
            let command = CommandLineParser.parse(Array(CommandLine.arguments.dropFirst()))
            try await run(command: command)
        } catch let error as UsageCLIError {
            FileHandle.standardError.writeLine(error.description)
            Foundation.exit(1)
        } catch {
            FileHandle.standardError.writeLine("usage: \(error)")
            Foundation.exit(1)
        }
    }

    private static func run(command: CLICommand) async throws {
        switch command.action {
        case .help:
            print(helpText)
        case .doctor:
            try await doctor()
        case .refresh:
            let diagnostics = try await refresh()
            print("Refreshed usage data.")
            printDiagnostics(diagnostics)
        case .daemon:
            try await daemon(intervalSeconds: command.intervalSeconds)
        case .selfTest:
            try selfTest()
            print("Self-test passed.")
        case .dbPath:
            print(try paths().database.path)
        case .configExample:
            print(configExample)
        case .show:
            let appPaths = try paths()
            let database = try UsageDatabase(path: appPaths.database.path)
            var diagnostics: [ProviderDiagnostic] = []
            if command.refresh {
                diagnostics = try await refresh(database: database, appPaths: appPaths)
            }

            let since = Calendar.current.date(byAdding: .day, value: -45, to: Date())
            let events = try database.events(since: since)
                .filter { Self.visibleService($0.service) }
            let windows = try database.windows()
                .filter { Self.visibleService($0.service) }
            let cards = UsageSnapshotBuilder.buildCards(events: events, windows: windows, now: Date())

            switch command.output {
            case .cards:
                print(CardRenderer.render(cards))
            case .table:
                print(TableRenderer.render(cards))
            case .json:
                let encoder = JSONEncoder()
                encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
                encoder.dateEncodingStrategy = .iso8601
                let data = try encoder.encode(cards)
                print(String(data: data, encoding: .utf8) ?? "[]")
            }

            if command.showDiagnostics {
                printDiagnostics(diagnostics)
            }
        }
    }

    private static func refresh() async throws -> [ProviderDiagnostic] {
        let appPaths = try paths()
        let database = try UsageDatabase(path: appPaths.database.path)
        return try await refresh(database: database, appPaths: appPaths)
    }

    private static func refresh(database: UsageDatabase, appPaths: AppPaths) async throws -> [ProviderDiagnostic] {
        let context = ProviderContext(homeDirectory: appPaths.home, now: Date())
        let providers = ProviderRegistry.defaultProviders()
        var allDiagnostics: [ProviderDiagnostic] = []

        for provider in providers {
            let collection = await provider.collect(context: context)
            if provider.id == "codex-direct", !collection.windows.isEmpty {
                try database.deleteWindows(sourceKinds: [.openAIWeb, .claudeWeb])
            }
            try database.upsert(events: collection.events)
            try database.upsert(windows: collection.windows)
            allDiagnostics.append(contentsOf: collection.diagnostics)
        }

        let config = try loadConfig(path: appPaths.config)
        try database.applyConfiguredWindows(config: config, now: Date())
        return allDiagnostics
    }

    private static func daemon(intervalSeconds: Int) async throws {
        let interval = max(intervalSeconds, 30)
        while true {
            let diagnostics = try await refresh()
            let timestamp = ISO8601DateFormatter().string(from: Date())
            FileHandle.standardOutput.writeLine("[\(timestamp)] refreshed")
            printDiagnostics(diagnostics)
            try await Task.sleep(nanoseconds: UInt64(interval) * 1_000_000_000)
        }
    }

    private static func selfTest() throws {
        let start = Date(timeIntervalSince1970: 0)
        let now = Date(timeIntervalSince1970: 50)
        let reset = Date(timeIntervalSince1970: 100)
        let window = QuotaWindow(
            id: "self-test",
            service: .codex,
            sourceKind: .configured,
            kind: .session,
            startedAt: start,
            resetAt: reset,
            usedUnits: 25,
            limitUnits: 100,
            unit: .tokens,
            observedAt: now
        )
        let projection = PaceEngine.project(window: window, now: now)
        guard projection.status == .onTrack,
              projection.leftFraction == 0.75,
              projection.burnIndex == 0.5
        else {
            throw UsageCLIError.message("pacing self-test failed")
        }

        let rendered = CardRenderer.render([
            UsageCard(title: "Codex · direct", rows: [
                UsageRow(label: "Weekly", value: "80% left", bar: "██████████░░", detail: "resets in 5d")
            ])
        ])
        guard rendered.contains("Codex · direct"),
              rendered.contains("Weekly"),
              rendered.contains("██████████░░")
        else {
            throw UsageCLIError.message("renderer self-test failed")
        }
    }


    private static func doctor() async throws {
        let appPaths = try paths()
        try FileManager.default.createDirectory(at: appPaths.supportDirectory, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: appPaths.config.deletingLastPathComponent(), withIntermediateDirectories: true)

        let database = try UsageDatabase(path: appPaths.database.path)
        let eventCount = try database.eventCount()
        let configExists = FileManager.default.fileExists(atPath: appPaths.config.path)

        print("Usage Tracker")
        print("  database  \(appPaths.database.path)")
        print("  config    \(appPaths.config.path)\(configExists ? "" : " (missing)")")
        print("  events    \(eventCount)")
        print("")

        let context = ProviderContext(homeDirectory: appPaths.home, now: Date())
        for provider in ProviderRegistry.defaultProviders() {
            let collection = await provider.collect(context: context)
            let status = collection.diagnostics.contains(where: { $0.severity == .error })
                ? "error"
                : (collection.events.isEmpty && collection.windows.isEmpty ? "no data" : "ok")
            print("  \(provider.id.padding(toLength: 22, withPad: " ", startingAt: 0)) \(status)")
            for diagnostic in collection.diagnostics where diagnostic.severity != .info {
                print("    \(diagnostic.severity.rawValue): \(diagnostic.message)")
            }
        }

        if !configExists {
            print("")
            print("Run `usage config-example > \(appPaths.config.path)` to start configuring limits.")
        }
    }

    private static func paths() throws -> AppPaths {
        let environment = ProcessInfo.processInfo.environment
        let home = FileManager.default.homeDirectoryForCurrentUser
        let support = home
            .appendingPathComponent("Library")
            .appendingPathComponent("Application Support")
            .appendingPathComponent("UsageTracker")
        let config = environment["USAGETRACKER_CONFIG"].map(URL.init(fileURLWithPath:))
            ?? home
                .appendingPathComponent(".usagetracker")
                .appendingPathComponent("config.json")
        let database = environment["USAGETRACKER_DB"].map(URL.init(fileURLWithPath:))
            ?? support.appendingPathComponent("usage.sqlite")

        try FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: database.deletingLastPathComponent(), withIntermediateDirectories: true)
        return AppPaths(
            home: home,
            supportDirectory: support,
            database: database,
            config: config
        )
    }

    private static func loadConfig(path: URL) throws -> AppConfig {
        guard FileManager.default.fileExists(atPath: path.path) else {
            return AppConfig()
        }
        let data = try Data(contentsOf: path)
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return try decoder.decode(AppConfig.self, from: data)
    }

    private static func printDiagnostics(_ diagnostics: [ProviderDiagnostic]) {
        let visible = diagnostics.filter { $0.severity != .info }
        guard !visible.isEmpty else {
            return
        }
        FileHandle.standardError.writeLine("")
        for diagnostic in visible {
            FileHandle.standardError.writeLine("\(diagnostic.providerID): \(diagnostic.severity.rawValue): \(diagnostic.message)")
        }
    }

    private static func visibleService(_ service: UsageService) -> Bool {
        switch service {
        case .codex, .openAI:
            true
        case .claude, .anthropic:
            false
        }
    }
}

private struct AppPaths {
    var home: URL
    var supportDirectory: URL
    var database: URL
    var config: URL
}

private enum OutputMode {
    case cards
    case table
    case json
}

private enum Action {
    case show
    case refresh
    case daemon
    case selfTest
    case doctor
    case dbPath
    case configExample
    case help
}

private struct CLICommand {
    var action: Action = .show
    var output: OutputMode = .cards
    var refresh: Bool = true
    var showDiagnostics: Bool = false
    var intervalSeconds: Int = 300
}

private enum CommandLineParser {
    static func parse(_ args: [String]) -> CLICommand {
        var command = CLICommand()

        for arg in args {
            switch arg {
            case "refresh":
                command.action = .refresh
            case "daemon":
                command.action = .daemon
            case "self-test":
                command.action = .selfTest
            case "doctor":
                command.action = .doctor
            case "db-path":
                command.action = .dbPath
            case "config-example":
                command.action = .configExample
            case "-h", "--help", "help":
                command.action = .help
            case "--cards":
                command.output = .cards
            case "--table":
                command.output = .table
            case "--json":
                command.output = .json
            case "--no-refresh":
                command.refresh = false
            case "--diagnostics":
                command.showDiagnostics = true
            default:
                if arg.hasPrefix("--interval="), let value = Int(arg.dropFirst("--interval=".count)) {
                    command.intervalSeconds = value
                }
                break
            }
        }

        return command
    }
}

private enum UsageCLIError: Error, CustomStringConvertible {
    case message(String)

    var description: String {
        switch self {
        case let .message(message):
            message
        }
    }
}

private extension FileHandle {
    func writeLine(_ line: String) {
        if let data = (line + "\n").data(using: .utf8) {
            write(data)
        }
    }
}

private let helpText = """
Usage Tracker

Commands:
  usage                 Refresh sources and show cards
  usage --table         Show compact table
  usage --json          Emit JSON cards
  usage --no-refresh    Read the existing SQLite snapshot only
  usage refresh         Refresh all available sources
  usage daemon          Refresh SQLite continuously
  usage daemon --interval=60
  usage self-test       Run built-in smoke assertions
  usage doctor          Inspect sources and paths
  usage db-path         Print SQLite database path
  usage config-example  Print a starter limits config

Environment:
  CODEX_AUTH_FILE       Overrides ~/.codex/auth.json for direct Codex usage
  OPENAI_ADMIN_KEY      Enables OpenAI organization usage/cost polling
  USAGETRACKER_DB       Overrides the SQLite database path
  USAGETRACKER_CONFIG   Overrides the limits config path
"""

private let configExample = """
{
  "accounts": [
    {
      "id": "codex-direct",
      "service": "codex",
      "title": "Codex · direct",
      "accountLabel": "joey@example.com",
      "windows": [
        {
          "kind": "daily",
          "limit": 100000000,
          "unit": "tokens",
          "period": "day",
          "anchorHour": 0
        },
        {
          "kind": "weekly",
          "limit": 500000000,
          "unit": "tokens",
          "period": "week",
          "anchorHour": 0
        }
      ]
    }
  ]
}
"""
