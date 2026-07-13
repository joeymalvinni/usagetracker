import Foundation

extension AppState {
    nonisolated static func staleProviderIDs(
        config: ConfigResponse?,
        accounts: [Account],
        snapshots: [UsageSnapshot],
        now: Date
    ) -> [String] {
        guard let config else { return [] }
        let staleAfter = TimeInterval(config.pollIntervalSeconds * 2)
        return config.providers.compactMap { providerID, toggle in
            guard toggle.enabled else { return nil }
            let allAccounts = accounts.filter { $0.providerId == providerID }
            let visibleAccounts = allAccounts.filter { !$0.hidden }
            if !allAccounts.isEmpty && visibleAccounts.isEmpty { return nil }
            let enabledAccounts = visibleAccounts.filter(\.collectionEnabled)
            if !visibleAccounts.isEmpty && enabledAccounts.isEmpty { return nil }

            let accountIDs = Set(enabledAccounts.map(\.id))
            let relevant = snapshots.filter {
                $0.providerId == providerID && (accountIDs.isEmpty || accountIDs.contains($0.accountId))
            }
            let latestByAccount = Dictionary(grouping: relevant, by: \.accountId)
                .mapValues { $0.map(\.collectedAt).max() }
            if !enabledAccounts.isEmpty {
                return enabledAccounts.contains { account in
                    latestByAccount[account.id].flatMap { $0 }
                        .isNoneOrOlder(than: staleAfter, relativeTo: now)
                } ? providerID : nil
            }
            guard !relevant.isEmpty else { return providerID }
            return latestByAccount.values.contains {
                $0.flatMap { $0 }.isNoneOrOlder(than: staleAfter, relativeTo: now)
            } ? providerID : nil
        }.sorted()
    }
}

private extension Optional where Wrapped == Date {
    func isNoneOrOlder(than interval: TimeInterval, relativeTo now: Date) -> Bool {
        guard let self else { return true }
        return now.timeIntervalSince(self) > interval
    }
}
