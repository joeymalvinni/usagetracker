import AppKit
import Combine
import SwiftUI
import UserNotifications

@main enum UsageMenuBar {
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.accessory)
        app.run()
    }
}

@MainActor final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate, UNUserNotificationCenterDelegate {
    private struct MenuIconPresentation: Equatable {
        let status: DisplayStatus
        let bars: [MenuBarProviderVM]
    }

    private struct ProviderMenuSelection {
        let providerId: String
        let accountId: String?
    }

    private let state = AppState()
    private let popover = NSPopover()
    private let navigation = PopoverNavigation()
    private lazy var popoverController = makePopoverController()
    private var item: NSStatusItem!
    private let contextMenu = NSMenu()
    private var statusMenuItem: NSMenuItem!
    private var refreshMenuItem: NSMenuItem!
    private var providerMenuItems = [NSMenuItem]()
    private var providerLabelsMenuItem: NSMenuItem!
    private var remainingMetricMenuItem: NSMenuItem!
    private var usedMetricMenuItem: NSMenuItem!
    private var providerCountMenuItems = [Int: NSMenuItem]()
    private var iconCache = [String: NSImage]()
    private var providerMenuSignature = ""
    private var renderedMenuIcon: MenuIconPresentation?
    private var bag = Set<AnyCancellable>()
    private let menuIconSize = NSSize(width: 16, height: 16)

    func applicationDidFinishLaunching(_ note: Notification) {
        UNUserNotificationCenter.current().delegate = self
        state.$ui
            .map(\.darkModeEnabled)
            .removeDuplicates()
            .receive(on: RunLoop.main)
            .sink { [weak self] enabled in
                self?.applyAppearance(darkModeEnabled: enabled)
            }
            .store(in: &bag)
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        configureStatusButton()

        popover.behavior = .transient
        popover.contentSize = NSSize(width: Theme.Popover.width, height: Theme.Popover.height)
        popover.contentViewController = popoverController
        makeStatusMenu()

        state.$derived.map(\.menuPreview).removeDuplicates().sink { [weak self] preview in self?.item.button?.toolTip = preview.isEmpty ? "Usage" : preview }.store(in: &bag)
        state.$derived.map { MenuIconPresentation(status: $0.menuStatus, bars: $0.menuBars) }
            .removeDuplicates()
            .sink { [weak self] value in self?.updateMenuIcon(for: value.status, bars: value.bars) }
            .store(in: &bag)
        NSWorkspace.shared.notificationCenter.publisher(for: NSWorkspace.didWakeNotification)
            .sink { [weak self] _ in
                Task { await self?.state.refreshAfterWake() }
            }
            .store(in: &bag)
        Task { await state.bootstrap(); await state.pollLoop() }
        Task { await state.updater.checkForUpdates() }

        // Force the retained SwiftUI/AppKit tree to load and lay itself out after
        // launch, before the user's first click.
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            _ = self.popoverController.view
            self.popoverController.view.layoutSubtreeIfNeeded()
        }

        if ProcessInfo.processInfo.environment["USAGE_POPOVER_DEBUG"] == "1" { showDebugWindow() }
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([.banner, .sound])
    }

    private func configureStatusButton() {
        item.button?.imagePosition = .imageOnly
        item.button?.target = self
        item.button?.action = #selector(statusItemClicked(_:))
        item.button?.sendAction(on: [.leftMouseDown, .rightMouseDown])
        item.button?.toolTip = "Usage"
        item.button?.title = ""
        item.button?.attributedTitle = NSAttributedString(string: "")
        updateMenuIcon(for: .stale, bars: [])
    }

    private func updateMenuIcon(for status: DisplayStatus, bars: [MenuBarProviderVM]) {
        let presentation = MenuIconPresentation(status: status, bars: bars)
        guard presentation != renderedMenuIcon else { return }
        renderedMenuIcon = presentation
        guard let button = item.button else { return }
        item.length = MenuBarProgressIcon.statusItemLength(for: bars)
        button.image = MenuBarProgressIcon.image(for: bars, status: status)
        button.contentTintColor = nil
    }

    @objc private func statusItemClicked(_ sender: NSStatusBarButton) {
        if let event = NSApp.currentEvent, isContextClick(event) {
            showContextMenu(with: event, relativeTo: sender)
        } else {
            togglePopover()
        }
    }

    private func isContextClick(_ event: NSEvent?) -> Bool {
        guard let event else { return false }
        return event.type == .rightMouseDown || event.modifierFlags.contains(.control)
    }

    private func togglePopover() {
        guard let button = item.button else { return }
        if popover.isShown { popover.performClose(nil) } else { showPopover(selection: .summary, relativeTo: button) }
    }

    private func showPopover(selection: Selection, relativeTo button: NSStatusBarButton? = nil) {
        guard let button = button ?? item.button else { return }
        navigation.selection = selection
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        configurePopoverWindow()
        Task { await state.refreshForPopoverOpen() }
        Task { await state.updater.checkForUpdates() }
    }

    private func makePopoverController() -> NSViewController {
        NSHostingController(rootView: Popover(navigation: navigation).environmentObject(state))
    }

    private func configurePopoverWindow() {
        guard let window = popover.contentViewController?.view.window else { return }
        window.appearance = appearance(darkModeEnabled: state.ui.darkModeEnabled)
        window.isOpaque = false
        window.backgroundColor = .clear
    }

    private func applyAppearance(darkModeEnabled: Bool) {
        let appearance = appearance(darkModeEnabled: darkModeEnabled)
        NSApp.appearance = appearance
        popover.appearance = appearance
        popover.contentViewController?.view.appearance = appearance
        popover.contentViewController?.view.window?.appearance = appearance
    }

    private func appearance(darkModeEnabled: Bool) -> NSAppearance? {
        NSAppearance(named: darkModeEnabled ? .darkAqua : .aqua)
    }

    private func showContextMenu(with event: NSEvent, relativeTo button: NSStatusBarButton) {
        popover.performClose(nil)
        updateStatusMenu()
        NSMenu.popUpContextMenu(contextMenu, with: event, for: button)
    }

    private func makeStatusMenu() {
        let menu = contextMenu
        menu.autoenablesItems = false

        let title = NSMenuItem(title: "UsageTracker-beta", action: nil, keyEquivalent: "")
        title.isEnabled = false
        title.image = menuIcon(symbolImage("chart.bar.fill"), cacheKey: "title")
        menu.addItem(title)

        statusMenuItem = NSMenuItem(title: statusSummary, action: nil, keyEquivalent: "")
        statusMenuItem.isEnabled = false
        menu.addItem(statusMenuItem)
        menu.addItem(.separator())
        menu.addItem(.separator())
        let summary = NSMenuItem(title: "Open Summary", action: #selector(openSummaryFromMenu), keyEquivalent: "")
        summary.target = self
        summary.image = menuIcon(symbolImage("rectangle.grid.1x2"), cacheKey: "summary")
        menu.addItem(summary)

        let settings = NSMenuItem(title: "Settings", action: #selector(openSettingsFromMenu), keyEquivalent: ",")
        settings.target = self
        settings.image = menuIcon(symbolImage("gearshape"), cacheKey: "settings")
        menu.addItem(settings)
        menu.addItem(menuBarSettingsItem())
        menu.addItem(.separator())

        refreshMenuItem = NSMenuItem(title: "Refresh", action: #selector(refreshFromMenu), keyEquivalent: "r")
        refreshMenuItem.target = self
        refreshMenuItem.image = menuIcon(symbolImage("arrow.clockwise"), cacheKey: "refresh")
        menu.addItem(refreshMenuItem)

        let quit = NSMenuItem(title: "Quit UsageTracker", action: #selector(quitFromMenu), keyEquivalent: "q")
        quit.target = self
        quit.image = menuIcon(symbolImage("power"), cacheKey: "quit")
        menu.addItem(quit)
        updateStatusMenu()
    }

    private func updateStatusMenu() {
        guard statusMenuItem != nil else { return }
        statusMenuItem.title = statusSummary
        refreshMenuItem.title = state.refreshing ? "Refreshing..." : "Refresh"
        refreshMenuItem.isEnabled = !state.refreshing
        updateMenuBarSettingsStates()

        let providers = contextMenuProviders()
        let signature = makeProviderMenuSignature(providers)
        guard signature != providerMenuSignature else { return }
        providerMenuSignature = signature

        providerMenuItems.forEach(menuRemoveItem)
        if providers.isEmpty {
            let empty = NSMenuItem(title: "No connected providers or usage data", action: nil, keyEquivalent: "")
            empty.isEnabled = false
            providerMenuItems = [empty]
        } else {
            providerMenuItems = providers.map(providerMenuItem)
        }
        for (offset, providerItem) in providerMenuItems.enumerated() {
            contextMenu.insertItem(providerItem, at: 3 + offset)
        }
    }

    private func updateMenuBarSettingsStates() {
        providerLabelsMenuItem.state = state.ui.showProviderLabels ? .on : .off
        remainingMetricMenuItem.state = state.ui.menuMetric == .remaining ? .on : .off
        usedMetricMenuItem.state = state.ui.menuMetric == .used ? .on : .off
        for (count, item) in providerCountMenuItems {
            item.state = state.menuProviderCount == count ? .on : .off
        }
    }

    private func contextMenuProviders() -> [ProviderVM] {
        let eligibleProviderIDs = AppState.providerIDsWithDataOrConnection(
            accounts: state.accounts,
            snapshots: state.snapshots
        )
        return StatusMenuProviderSelection.select(
            from: state.providers,
            eligibleProviderIDs: eligibleProviderIDs
        )
    }

    private func makeProviderMenuSignature(_ providers: [ProviderVM]) -> String {
        var rows = [String]()
        for provider in providers {
            for item in [provider] + (provider.subAccounts ?? []) {
                let percent = item.percent.map { String($0) } ?? ""
                rows.append([
                    item.id, item.name, item.primary, percent, item.status.code,
                    item.detail, item.errorDetail ?? "",
                ].joined(separator: "\u{1F}"))
            }
        }
        rows.append(state.ui.menuMetric.rawValue)
        return rows.joined(separator: "\u{1E}")
    }

    private func menuRemoveItem(_ menuItem: NSMenuItem) {
        contextMenu.removeItem(menuItem)
    }

    private var statusSummary: String {
        switch state.daemon {
        case .online: "Daemon online"
        case .offline: "Daemon offline"
        case .unknown: "Checking daemon"
        }
    }

    private func providerMenuTitle(_ provider: ProviderVM, name: String? = nil) -> String {
        let value: String
        if let percent = provider.percent {
            let displayed = max(0, min(100, state.ui.menuMetric == .used ? 100 - percent : percent))
            value = "\(Int(displayed.rounded()))\(state.ui.menuMetric == .used ? "% used" : "% left")"
        } else {
            value = provider.primary
        }
        return "\(name ?? provider.name): \(value) · \(provider.status.label)"
    }

    private func providerMenuItem(_ provider: ProviderVM) -> NSMenuItem {
        let item = NSMenuItem(title: providerMenuTitle(provider), action: nil, keyEquivalent: "")
        item.image = menuIcon(ProviderBrand.image(provider.providerId) ?? symbolImage(provider.symbol), cacheKey: "provider:\(provider.providerId)")
        item.toolTip = provider.detail

        guard let accounts = provider.subAccounts, accounts.count > 1 else {
            item.action = #selector(openProviderFromMenu(_:))
            item.target = self
            item.representedObject = ProviderMenuSelection(providerId: provider.id, accountId: nil)
            return item
        }

        let submenu = NSMenu(title: provider.name)
        submenu.autoenablesItems = false
        for account in accounts {
            let accountItem = NSMenuItem(
                title: providerMenuTitle(account, name: accountMenuName(account, among: accounts)),
                action: #selector(openProviderFromMenu(_:)),
                keyEquivalent: ""
            )
            accountItem.target = self
            accountItem.representedObject = ProviderMenuSelection(
                providerId: provider.id,
                accountId: account.accountId
            )
            accountItem.image = menuIcon(symbolImage("person.crop.circle"), cacheKey: "account")
            accountItem.toolTip = account.errorDetail ?? account.detail
            submenu.addItem(accountItem)
        }
        item.submenu = submenu
        return item
    }

    private func accountMenuName(_ account: ProviderVM, among accounts: [ProviderVM]) -> String {
        let duplicates = accounts.filter {
            $0.name.localizedCaseInsensitiveCompare(account.name) == .orderedSame
        }
        guard duplicates.count > 1 else { return account.name }

        if let email = account.accountEmail,
           !email.isEmpty,
           duplicates.filter({ $0.accountEmail == email }).count == 1 {
            return "\(account.name) (\(email))"
        }
        if let accountId = account.accountId {
            return "\(account.name) (\(accountId.suffix(6)))"
        }
        return account.name
    }

    private func menuBarSettingsItem() -> NSMenuItem {
        let item = NSMenuItem(title: "Menu Bar Icon", action: nil, keyEquivalent: "")
        item.image = menuIcon(symbolImage("menubar.rectangle"), cacheKey: "menubar")

        let submenu = NSMenu(title: "Menu Bar Icon")
        submenu.autoenablesItems = false
        for count in UIConfig.menuProviderCountRange {
            let countItem = maxProvidersItem(count: count)
            providerCountMenuItems[count] = countItem
            submenu.addItem(countItem)
        }
        submenu.addItem(.separator())
        remainingMetricMenuItem = metricItem(title: "Show Remaining", metric: .remaining)
        usedMetricMenuItem = metricItem(title: "Show Used", metric: .used)
        submenu.addItem(remainingMetricMenuItem)
        submenu.addItem(usedMetricMenuItem)
        submenu.addItem(.separator())
        providerLabelsMenuItem = toggleItem(
            title: "Show Provider Names in Tooltip",
            state: state.ui.showProviderLabels,
            action: #selector(toggleProviderLabelsFromMenu)
        )
        submenu.addItem(providerLabelsMenuItem)
        item.submenu = submenu
        return item
    }

    private func toggleItem(title: String, state: Bool, action: Selector) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
        item.target = self
        item.state = state ? .on : .off
        return item
    }

    private func metricItem(title: String, metric: UIConfig.MenuMetric) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: #selector(setMetricFromMenu(_:)), keyEquivalent: "")
        item.target = self
        item.representedObject = metric.rawValue
        item.state = state.ui.menuMetric == metric ? .on : .off
        return item
    }

    private func maxProvidersItem(count: Int) -> NSMenuItem {
        let title = count == 1 ? "One Provider" : "Two Providers"
        let item = NSMenuItem(title: title, action: #selector(setMaxProvidersFromMenu(_:)), keyEquivalent: "")
        item.target = self
        item.representedObject = count
        item.state = state.menuProviderCount == count ? .on : .off
        item.toolTip = "Uses provider order from Settings."
        return item
    }

    private func symbolImage(_ name: String) -> NSImage? {
        NSImage(systemSymbolName: name, accessibilityDescription: nil)
    }

    private func menuIcon(_ image: NSImage?, cacheKey: String) -> NSImage? {
        if let cached = iconCache[cacheKey] { return cached }
        guard let image else { return nil }
        let sourceSize = image.size
        guard sourceSize.width > 0, sourceSize.height > 0 else { return image }

        let scale = min(menuIconSize.width / sourceSize.width, menuIconSize.height / sourceSize.height)
        let drawSize = NSSize(width: sourceSize.width * scale, height: sourceSize.height * scale)
        let drawRect = NSRect(
            x: (menuIconSize.width - drawSize.width) / 2,
            y: (menuIconSize.height - drawSize.height) / 2,
            width: drawSize.width,
            height: drawSize.height
        )

        let icon = NSImage(size: menuIconSize)
        icon.lockFocus()
        image.draw(in: drawRect, from: .zero, operation: .sourceOver, fraction: 1)
        icon.unlockFocus()
        icon.isTemplate = image.isTemplate
        iconCache[cacheKey] = icon
        return icon
    }

    @objc private func openSummaryFromMenu() {
        showPopover(selection: .summary)
    }

    @objc private func openSettingsFromMenu() {
        showPopover(selection: .settings)
    }

    @objc private func openProviderFromMenu(_ sender: NSMenuItem) {
        guard let selection = sender.representedObject as? ProviderMenuSelection else { return }
        showPopover(selection: .provider(selection.providerId, accountId: selection.accountId))
    }

    @objc private func refreshFromMenu() {
        Task { await state.refreshAll() }
    }

    @objc private func toggleProviderLabelsFromMenu() {
        state.ui.showProviderLabels.toggle()
        updateMenuIconPreferences()
    }

    @objc private func setMetricFromMenu(_ sender: NSMenuItem) {
        guard let raw = sender.representedObject as? String,
              let metric = UIConfig.MenuMetric(rawValue: raw)
        else { return }
        state.ui.menuMetric = metric
        updateMenuIconPreferences()
    }

    @objc private func setMaxProvidersFromMenu(_ sender: NSMenuItem) {
        guard let count = sender.representedObject as? Int else { return }
        state.ui.maxMenuProviders = count
        updateMenuIconPreferences()
    }

    private func updateMenuIconPreferences() {
        updateMenuBarSettingsStates()
        let preview = state.menuPreview.isEmpty ? "Usage" : state.menuPreview
        item.button?.toolTip = preview
        updateMenuIcon(for: state.menuStatus, bars: state.menuBars)
    }

    @objc private func quitFromMenu() {
        NSApp.terminate(nil)
    }

    // Renders the popover content in a floating window so the UI can be
    // inspected/screenshotted without clicking the status item.
    private var debugWindow: NSWindow?
    private func showDebugWindow() {
        let size = NSSize(width: Theme.Popover.width, height: Theme.Popover.height)
        let window = PopoverDebugWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless], backing: .buffered, defer: false)
        window.isOpaque = false
        window.backgroundColor = .clear
        window.level = .floating
        window.contentViewController = GlassPopoverHostingController(rootView: Popover(navigation: navigation).environmentObject(state))
        if let screen = NSScreen.main {
            window.setFrameTopLeftPoint(NSPoint(x: 60, y: screen.frame.maxY - 60))
        }
        debugWindow = window
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }
}

/// Borderless windows cannot become key by default. The debug window needs to
/// accept focus so its controls remain interactive and its glass stays active.
private final class PopoverDebugWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}

enum StatusMenuProviderSelection {
    static let maximumCount = 5

    static func select(
        from providers: [ProviderVM],
        eligibleProviderIDs: Set<String>
    ) -> [ProviderVM] {
        Array(
            providers.lazy
                .filter { $0.enabled && eligibleProviderIDs.contains($0.providerId) }
                .prefix(maximumCount)
        )
    }
}

/// Gives the standalone debug window its own translucent shell. The live
/// `NSPopover` deliberately does not use this controller because AppKit owns
/// that popover's body, arrow, material, and corner geometry.
private final class GlassPopoverHostingController<Content: View>: NSViewController {
    private let rootView: Content

    init(rootView: Content) {
        self.rootView = rootView
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func loadView() {
        let size = NSSize(width: Theme.Popover.width, height: Theme.Popover.height)
        let frame = NSRect(origin: .zero, size: size)

        let host = NSHostingView(rootView: AnyView(rootView.background(Color.clear)))
        host.frame = frame
        host.autoresizingMask = [.width, .height]
        host.wantsLayer = true
        host.layer?.backgroundColor = NSColor.clear.cgColor

        #if compiler(>=6.2)
        if #available(macOS 26, *) {
            let glass = NSGlassEffectView(frame: frame)
            glass.cornerRadius = Theme.Radius.xl
            glass.clipsToBounds = true
            // Enough body that text stays readable over arbitrary desktops,
            // low enough that the glass still reads as glass. Dark mode carries
            // a heavier tint: a bright wallpaper bleeds through the lighter
            // light-mode value and washes the dark UI out to a muddy grey.
            glass.tintColor = NSColor(name: "GlassShellTint") { appearance in
                let isDark = appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                return NSColor.windowBackgroundColor.withAlphaComponent(isDark ? 0.72 : 0.55)
            }
            glass.contentView = host
            view = glass
            return
        }
        #endif

        let effect = NSVisualEffectView(frame: frame)
        effect.material = .popover
        effect.state = .active
        effect.blendingMode = .behindWindow
        effect.wantsLayer = true
        effect.layer?.cornerRadius = Theme.Radius.xl
        effect.layer?.masksToBounds = true
        effect.addSubview(host)
        view = effect
    }
}
