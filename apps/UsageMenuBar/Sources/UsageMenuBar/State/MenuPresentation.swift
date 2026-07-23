extension AppState {
    nonisolated static func menuContent(
        providers: [ProviderVM],
        daemon: DaemonState,
        connectivity: ConnectivityStatus = .unknown,
        ui: UIConfig,
        eligibleProviderIDs: Set<String>
    ) -> (preview: String, status: DisplayStatus, bars: [MenuBarProviderVM]) {
        guard daemon != .offline else { return ("Usage offline", .offline, []) }
        let eligible = providers.filter {
            $0.enabled && $0.visibleInMenu && eligibleProviderIDs.contains($0.providerId)
        }
        let shown = Array(eligible.prefix(ui.resolvedMenuProviderCount(automaticCount: eligible.count)))
        let preview = shown.map { provider in
            let value = provider.percent.map {
                let displayed = max(0, min(100, ui.menuMetric == .used ? 100 - $0 : $0))
                return "\(Int(displayed.rounded()))%"
            } ?? provider.primary
            return ui.showProviderLabels ? "\(provider.short) \(value)" : value
        }.joined(separator: "  ")
        let bars = shown.map { provider in
            let displayed = provider.percent.map {
                max(0, min(100, ui.menuMetric == .used ? 100 - $0 : $0))
            }
            return MenuBarProviderVM(
                id: provider.id,
                providerId: provider.providerId,
                short: provider.short,
                percent: displayed,
                status: provider.status,
                isMuted: connectivity == .offline
            )
        }
        guard !preview.isEmpty else { return ("Usage", .stale, []) }
        if connectivity == .offline {
            return ("Offline · showing last known usage", .offline, bars)
        }
        return (preview, shown.map(\.status).max { $0.severity < $1.severity } ?? .stale, bars)
    }

    nonisolated static func providerIDsWithDataOrConnection(
        accounts: [Account],
        snapshots: [UsageSnapshot]
    ) -> Set<String> {
        var providerIDs = Set(snapshots.map(\.providerId))
        providerIDs.formUnion(accounts.lazy.filter { !$0.hidden }.map(\.providerId))
        return providerIDs
    }

}
