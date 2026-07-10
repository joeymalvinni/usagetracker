import Foundation

enum ProviderOrdering {
    /// Resolves a partial saved preference against the providers available now.
    /// This keeps existing choices stable while appending newly introduced providers
    /// deterministically.
    static func resolve(_ providerIds: [String], preferred: [String]) -> [String] {
        let available = Set(providerIds)
        var seen = Set<String>()
        var result = preferred.filter {
            available.contains($0) && seen.insert($0).inserted
        }
        result.append(contentsOf: providerIds.sorted().filter { seen.insert($0).inserted })
        return result
    }

    /// Dropping onto a later provider places the dragged provider after it; dropping
    /// onto an earlier provider places it before it. This matches native list moves
    /// without needing separate, tiny insertion targets in the compact rail.
    static func moving(_ providerId: String, over targetProviderId: String, in order: [String]) -> [String] {
        guard providerId != targetProviderId,
              let source = order.firstIndex(of: providerId),
              let target = order.firstIndex(of: targetProviderId)
        else { return order }

        var result = order
        let provider = result.remove(at: source)
        result.insert(provider, at: target)
        return result
    }

    /// Places the dragged provider directly before or after the provider occupying its preview
    /// destination. Hidden providers keep their relative positions because the target index is
    /// resolved after the dragged provider is removed.
    static func moving(_ providerId: String, relativeTo targetProviderId: String, after: Bool, in order: [String]) -> [String] {
        guard providerId != targetProviderId,
              order.contains(providerId),
              order.contains(targetProviderId)
        else { return order }

        var result = order
        result.removeAll { $0 == providerId }
        guard let target = result.firstIndex(of: targetProviderId) else { return order }
        result.insert(providerId, at: after ? target + 1 : target)
        return result
    }
}
