import SwiftUI

struct Popover: View {
    @EnvironmentObject var state: AppState
    @State private var selection: Selection

    init(initialSelection: Selection? = nil) {
        _selection = State(initialValue: initialSelection ?? Self.debugSelection())
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
    }

    private var mainContent: some View {
        HStack(spacing: 0) {
            Rail(selection: $selection)
            Rectangle()
                .fill(Color(nsColor: .separatorColor))
                .frame(width: 0.5)
            Group {
                switch selection {
                case .summary: Summary(selection: $selection)
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
            case "1": selection = .summary
            case "2": selection = .settings
            case "3":
                selection = state.providers.first.map { .provider($0.id, accountId: nil) } ?? .summary
            default: return .ignored
            }
            return .handled
        }
    }

    private func moveSelection(_ delta: Int) {
        let entries = railEntries()
        guard !entries.isEmpty else { return }
        if let index = entries.firstIndex(where: { $0.matches(selection) }) {
            let next = min(max(index + delta, 0), entries.count - 1)
            selection = entries[next].selection
        } else {
            selection = entries[0].selection
        }
    }

    private func railEntries() -> [RailEntry] {
        var entries: [RailEntry] = [.init(selection: .summary)]
        entries.append(contentsOf: state.providers.map { RailEntry(selection: .provider($0.id, accountId: nil)) })
        entries.append(.init(selection: .settings))
        return entries
    }

    private static func debugSelection() -> Selection {
        switch ProcessInfo.processInfo.environment["USAGE_DEBUG_PAGE"] {
        case "settings": .settings
        case let page? where page.hasPrefix("provider:"): .provider(String(page.dropFirst("provider:".count)), accountId: nil)
        default: .summary
        }
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
