import Foundation
import UsageCore

public struct CodexLocalProvider: UsageProvider {
    public let id = "codex-local"
    public let displayName = "Codex local sessions"

    public init() {}

    public func collect(context: ProviderContext) async -> ProviderCollection {
        let logDirectory = context.homeDirectory
            .appendingPathComponent(".codex")
            .appendingPathComponent("log")

        guard let files = try? FileManager.default.contentsOfDirectory(
            at: logDirectory,
            includingPropertiesForKeys: [.contentModificationDateKey],
            options: [.skipsHiddenFiles]
        ) else {
            return ProviderCollection(diagnostics: [
                ProviderDiagnostic(providerID: id, severity: .info, message: "No ~/.codex/log directory found")
            ])
        }

        let jsonlFiles = files.filter { $0.pathExtension == "jsonl" }
        var events: [UsageEvent] = []
        var diagnostics: [ProviderDiagnostic] = []

        for file in jsonlFiles {
            guard let lines = try? String(contentsOf: file, encoding: .utf8)
                .split(separator: "\n", omittingEmptySubsequences: true)
                .map(String.init)
            else {
                continue
            }

            guard let first = lines.first, let firstObject = JSONHelpers.object(from: first) else {
                continue
            }

            let kind = JSONHelpers.string(firstObject, "kind")
            guard kind == "session_start" else {
                continue
            }

            let start = JSONHelpers.string(firstObject, "ts").flatMap(JSONHelpers.parseISODate)
                ?? modificationDate(file)
                ?? context.now
            let end = lastTimestamp(in: lines) ?? start
            let model = JSONHelpers.string(firstObject, "model")
            let providerName = JSONHelpers.string(firstObject, "model_provider_name")
            let cwd = JSONHelpers.string(firstObject, "cwd")

            events.append(
                UsageEvent(
                    id: "codex-local:session:\(file.deletingPathExtension().lastPathComponent)",
                    service: .codex,
                    sourceKind: .codexLocal,
                    model: model,
                    startedAt: start,
                    endedAt: max(end, start),
                    requests: 1,
                    metadata: [
                        "source": "session-jsonl",
                        "provider": providerName ?? "",
                        "cwd": cwd ?? ""
                    ].filter { !$0.value.isEmpty }
                )
            )
        }

        if events.isEmpty {
            diagnostics.append(
                ProviderDiagnostic(providerID: id, severity: .warning, message: "No Codex session_start records found")
            )
        }

        return ProviderCollection(events: events, diagnostics: diagnostics)
    }

    private func lastTimestamp(in lines: [String]) -> Date? {
        for line in lines.reversed() {
            guard
                let object = JSONHelpers.object(from: line),
                let timestamp = JSONHelpers.string(object, "ts"),
                let date = JSONHelpers.parseISODate(timestamp)
            else {
                continue
            }
            return date
        }
        return nil
    }

    private func modificationDate(_ url: URL) -> Date? {
        guard
            let values = try? url.resourceValues(forKeys: [.contentModificationDateKey])
        else {
            return nil
        }
        return values.contentModificationDate
    }
}
