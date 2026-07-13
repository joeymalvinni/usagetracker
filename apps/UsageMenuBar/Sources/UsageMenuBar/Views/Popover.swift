import SwiftUI

struct Popover: View {
    @EnvironmentObject var state: AppState
    @ObservedObject var navigation: PopoverNavigation

    init(navigation: PopoverNavigation) {
        self.navigation = navigation
    }

    var body: some View {
        Group {
            if !state.ui.onboardingCompleted {
                Onboarding()
            } else {
                mainContent
            }
        }
        .frame(width: Theme.Popover.width, height: Theme.Popover.height)
        .preferredColorScheme(state.ui.darkModeEnabled ? .dark : .light)
    }

    private var mainContent: some View {
        HStack(spacing: 0) {
            Rail(selection: $navigation.selection)
            Rectangle()
                .fill(Color(nsColor: .separatorColor))
                .frame(width: 0.5)
            Group {
                switch navigation.selection {
                case .summary: Summary(updater: state.updater, selection: $navigation.selection)
                case .provider(let id, let accountId): Detail(providerId: id, initialAccountId: accountId)
                case .settings: Settings()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .focusable()
        .focusEffectDisabled()
        .onKeyPress(.upArrow) { moveSelection(-1); return .handled }
        .onKeyPress(.downArrow) { moveSelection(+1); return .handled }
        .onKeyPress(phases: .down) { keyPress in
            guard keyPress.modifiers == .command else { return .ignored }
            switch keyPress.key {
            case "1": navigation.selection = .summary
            case "2": navigation.selection = .settings
            case "3":
                navigation.selection = state.providers.first.map { .provider($0.id, accountId: nil) } ?? .summary
            default: return .ignored
            }
            return .handled
        }
    }

    private func moveSelection(_ delta: Int) {
        let entries = railEntries()
        guard !entries.isEmpty else { return }
        if let index = entries.firstIndex(where: { $0.matches(navigation.selection) }) {
            let next = min(max(index + delta, 0), entries.count - 1)
            navigation.selection = entries[next].selection
        } else {
            navigation.selection = entries[0].selection
        }
    }

    private func railEntries() -> [RailEntry] {
        var entries: [RailEntry] = [.init(selection: .summary)]
        entries.append(contentsOf: state.providers.map { RailEntry(selection: .provider($0.id, accountId: nil)) })
        entries.append(.init(selection: .settings))
        return entries
    }

}

@MainActor final class PopoverNavigation: ObservableObject {
    @Published var selection: Selection

    init() {
        switch ProcessInfo.processInfo.environment["USAGE_DEBUG_PAGE"] {
        case "settings": selection = .settings
        case let page? where page.hasPrefix("provider:"):
            selection = .provider(String(page.dropFirst("provider:".count)), accountId: nil)
        default: selection = .summary
        }
    }

    init(selection: Selection) {
        self.selection = selection
    }
}

enum Selection: Hashable {
    case summary
    case provider(String, accountId: String?)
    case settings
}

private struct RailEntry {
    let selection: Selection
    func matches(_ other: Selection) -> Bool {
        switch (selection, other) {
        case (.summary, .summary), (.settings, .settings): true
        case (.provider(let a, _), .provider(let b, _)): a == b
        default: false
        }
    }
}
