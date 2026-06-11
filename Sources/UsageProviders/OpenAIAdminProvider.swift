import Foundation
import UsageCore

public struct OpenAIAdminProvider: UsageProvider {
    public let id = "openai-admin-api"
    public let displayName = "OpenAI Admin API"

    private let apiKey: String?
    private let session: URLSession

    public init(apiKey: String?, session: URLSession = .shared) {
        self.apiKey = apiKey
        self.session = session
    }

    public func collect(context: ProviderContext) async -> ProviderCollection {
        guard let apiKey, !apiKey.isEmpty else {
            return ProviderCollection(diagnostics: [
                ProviderDiagnostic(providerID: id, severity: .info, message: "OPENAI_ADMIN_KEY is not set")
            ])
        }

        var events: [UsageEvent] = []
        var diagnostics: [ProviderDiagnostic] = []
        let start = context.now.addingTimeInterval(-31 * 86_400)
        let startSeconds = Int(start.timeIntervalSince1970)
        let endSeconds = Int(context.now.timeIntervalSince1970)

        do {
            let usageURL = try makeURL(
                path: "/v1/organization/usage/completions",
                query: [
                    URLQueryItem(name: "start_time", value: String(startSeconds)),
                    URLQueryItem(name: "end_time", value: String(endSeconds)),
                    URLQueryItem(name: "bucket_width", value: "1d"),
                    URLQueryItem(name: "limit", value: "31"),
                    URLQueryItem(name: "group_by", value: "model")
                ]
            )
            let root = try await getJSON(url: usageURL, apiKey: apiKey)
            events.append(contentsOf: parseUsage(root))
        } catch {
            diagnostics.append(
                ProviderDiagnostic(providerID: id, severity: .warning, message: "OpenAI usage request failed: \(error)")
            )
        }

        do {
            let costsURL = try makeURL(
                path: "/v1/organization/costs",
                query: [
                    URLQueryItem(name: "start_time", value: String(startSeconds)),
                    URLQueryItem(name: "end_time", value: String(endSeconds)),
                    URLQueryItem(name: "bucket_width", value: "1d"),
                    URLQueryItem(name: "limit", value: "31")
                ]
            )
            let root = try await getJSON(url: costsURL, apiKey: apiKey)
            events.append(contentsOf: parseCosts(root))
        } catch {
            diagnostics.append(
                ProviderDiagnostic(providerID: id, severity: .warning, message: "OpenAI costs request failed: \(error)")
            )
        }

        return ProviderCollection(events: events, diagnostics: diagnostics)
    }

    private func parseUsage(_ root: [String: Any]) -> [UsageEvent] {
        guard let buckets = root["data"] as? [[String: Any]] else {
            return []
        }

        var events: [UsageEvent] = []
        for bucket in buckets {
            let start = Date(timeIntervalSince1970: JSONHelpers.double(bucket, "start_time") ?? 0)
            let end = Date(timeIntervalSince1970: JSONHelpers.double(bucket, "end_time") ?? start.timeIntervalSince1970)
            let results = bucket["results"] as? [[String: Any]] ?? []
            for result in results {
                let model = JSONHelpers.string(result, "model") ?? "unknown"
                let input = JSONHelpers.int(result, "input_tokens")
                let output = JSONHelpers.int(result, "output_tokens")
                let cached = JSONHelpers.int(result, "input_cached_tokens")
                let requests = JSONHelpers.int(result, "num_model_requests")
                guard input + output + cached + requests > 0 else {
                    continue
                }
                events.append(
                    UsageEvent(
                        id: "openai-admin:usage:\(Int(start.timeIntervalSince1970)):\(model)",
                        service: .codex,
                        sourceKind: .openAIAdminAPI,
                        model: model,
                        startedAt: start,
                        endedAt: end,
                        inputTokens: input,
                        outputTokens: output,
                        cachedInputTokens: cached,
                        requests: requests,
                        metadata: ["source": "organization/usage/completions"]
                    )
                )
            }
        }
        return events
    }

    private func parseCosts(_ root: [String: Any]) -> [UsageEvent] {
        guard let buckets = root["data"] as? [[String: Any]] else {
            return []
        }

        var events: [UsageEvent] = []
        for bucket in buckets {
            let start = Date(timeIntervalSince1970: JSONHelpers.double(bucket, "start_time") ?? 0)
            let end = Date(timeIntervalSince1970: JSONHelpers.double(bucket, "end_time") ?? start.timeIntervalSince1970)
            let results = bucket["results"] as? [[String: Any]] ?? []
            var total = Decimal(0)
            var currency = "usd"
            for result in results {
                guard let amount = result["amount"] as? [String: Any] else {
                    continue
                }
                if let value = JSONHelpers.double(amount, "value") {
                    total += Decimal(value)
                }
                currency = JSONHelpers.string(amount, "currency") ?? currency
            }
            guard total > 0 else {
                continue
            }
            events.append(
                UsageEvent(
                    id: "openai-admin:cost:\(Int(start.timeIntervalSince1970))",
                    service: .codex,
                    sourceKind: .openAIAdminAPI,
                    startedAt: start,
                    endedAt: end,
                    costAmount: total,
                    costCurrency: currency,
                    metadata: ["source": "organization/costs"]
                )
            )
        }
        return events
    }

    private func getJSON(url: URL, apiKey: String) async throws -> [String: Any] {
        var request = URLRequest(url: url)
        request.setValue("Bearer \(apiKey)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        let (data, response) = try await session.data(for: request)
        if let http = response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw ProviderHTTPError.status(http.statusCode, body: String(data: data, encoding: .utf8) ?? "")
        }
        return try JSONHelpers.object(from: data)
    }

    private func makeURL(path: String, query: [URLQueryItem]) throws -> URL {
        var components = URLComponents()
        components.scheme = "https"
        components.host = "api.openai.com"
        components.path = path
        components.queryItems = query
        guard let url = components.url else {
            throw ProviderHTTPError.invalidURL(path)
        }
        return url
    }
}
