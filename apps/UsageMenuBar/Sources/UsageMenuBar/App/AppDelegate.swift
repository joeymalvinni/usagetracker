import AppKit
import Combine
import SwiftUI

@main enum UsageMenuBar {
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.accessory)
        app.run()
    }
}

@MainActor final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private let state = AppState()
    private let popover = NSPopover()
    private var item: NSStatusItem!
    private var contextMenu: NSMenu?
    private var bag = Set<AnyCancellable>()
    private let menuIconSize = NSSize(width: 16, height: 16)

    func applicationDidFinishLaunching(_ note: Notification) {
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        configureStatusButton()

        popover.behavior = .transient
        popover.contentSize = NSSize(width: Theme.Popover.width, height: Theme.Popover.height)
        popover.contentViewController = makePopoverController(selection: .summary)

        state.$menuPreview.receive(on: RunLoop.main).sink { [weak self] preview in self?.item.button?.toolTip = preview.isEmpty ? "Usage" : preview }.store(in: &bag)
        Publishers.CombineLatest(state.$menuStatus, state.$menuBars)
            .receive(on: RunLoop.main)
            .sink { [weak self] status, bars in self?.updateMenuIcon(for: status, bars: bars) }
            .store(in: &bag)
        Task { await state.bootstrap(); await state.pollLoop() }

        if ProcessInfo.processInfo.environment["USAGE_POPOVER_DEBUG"] == "1" { showDebugWindow() }
    }

    private func configureStatusButton() {
        item.button?.imagePosition = .imageOnly
        item.button?.target = self
        item.button?.action = #selector(statusItemClicked(_:))
        item.button?.sendAction(on: [.leftMouseUp, .rightMouseUp])
        item.button?.toolTip = "Usage"
        item.button?.title = ""
        item.button?.attributedTitle = NSAttributedString(string: "")
        updateMenuIcon(for: .stale, bars: [])
    }

    private func updateMenuIcon(for status: DisplayStatus, bars: [MenuBarProviderVM]) {
        guard let button = item.button else { return }
        item.length = MenuBarProgressIcon.statusItemLength(for: bars)
        button.image = MenuBarProgressIcon.image(for: bars, status: status)
        button.contentTintColor = nil
    }

    @objc private func statusItemClicked(_ sender: NSStatusBarButton) {
        if isContextClick(NSApp.currentEvent) {
            showContextMenu()
        } else {
            togglePopover()
        }
    }

    private func isContextClick(_ event: NSEvent?) -> Bool {
        guard let event else { return false }
        return event.type == .rightMouseUp || event.modifierFlags.contains(.control)
    }

    private func togglePopover() {
        guard let button = item.button else { return }
        if popover.isShown { popover.performClose(nil) } else { showPopover(selection: .summary, relativeTo: button) }
    }

    private func showPopover(selection: Selection, relativeTo button: NSStatusBarButton? = nil) {
        guard let button = button ?? item.button else { return }
        popover.contentViewController = makePopoverController(selection: selection)
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        configurePopoverWindow()
        Task { await state.refreshForPopoverOpen() }
    }

    private func makePopoverController(selection: Selection) -> NSViewController {
        GlassPopoverHostingController(rootView: Popover(initialSelection: selection).environmentObject(state))
    }

    private func configurePopoverWindow() {
        guard let window = popover.contentViewController?.view.window else { return }
        window.isOpaque = false
        window.backgroundColor = .clear
    }

    private func showContextMenu() {
        popover.performClose(nil)
        let menu = makeStatusMenu()
        menu.delegate = self
        contextMenu = menu
        item.menu = menu
        item.button?.performClick(nil)
    }

    func menuDidClose(_ menu: NSMenu) {
        guard menu === contextMenu else { return }
        item.menu = nil
        contextMenu = nil
    }

    private func makeStatusMenu() -> NSMenu {
        let menu = NSMenu()
        menu.autoenablesItems = false

        let title = NSMenuItem(title: "UsageTracker-beta", action: nil, keyEquivalent: "")
        title.isEnabled = false
        title.image = menuIcon(symbolImage("chart.bar.fill"))
        menu.addItem(title)

        let status = NSMenuItem(title: statusSummary, action: nil, keyEquivalent: "")
        status.isEnabled = false
        menu.addItem(status)
        menu.addItem(.separator())

        let providers = state.providers.filter(\.enabled)
        if providers.isEmpty {
            let empty = NSMenuItem(title: "Waiting for usage data", action: nil, keyEquivalent: "")
            empty.isEnabled = false
            menu.addItem(empty)
        } else {
            for provider in providers {
                let item = NSMenuItem(title: providerMenuTitle(provider), action: #selector(openProviderFromMenu(_:)), keyEquivalent: "")
                item.target = self
                item.representedObject = provider.id
                item.image = menuIcon(ProviderBrand.image(provider.id) ?? symbolImage(provider.symbol))
                item.toolTip = provider.detail
                menu.addItem(item)
            }
        }

        menu.addItem(.separator())
        let summary = NSMenuItem(title: "Open Summary", action: #selector(openSummaryFromMenu), keyEquivalent: "")
        summary.target = self
        summary.image = menuIcon(symbolImage("rectangle.grid.1x2"))
        menu.addItem(summary)

        let settings = NSMenuItem(title: "Settings", action: #selector(openSettingsFromMenu), keyEquivalent: ",")
        settings.target = self
        settings.image = menuIcon(symbolImage("gearshape"))
        menu.addItem(settings)
        menu.addItem(menuBarSettingsItem())
        menu.addItem(.separator())

        let refresh = NSMenuItem(title: state.refreshing ? "Refreshing..." : "Refresh", action: #selector(refreshFromMenu), keyEquivalent: "r")
        refresh.target = self
        refresh.isEnabled = !state.refreshing
        refresh.image = menuIcon(symbolImage("arrow.clockwise"))
        menu.addItem(refresh)

        let quit = NSMenuItem(title: "Quit UsageTracker", action: #selector(quitFromMenu), keyEquivalent: "q")
        quit.target = self
        quit.image = menuIcon(symbolImage("power"))
        menu.addItem(quit)

        return menu
    }

    private var statusSummary: String {
        switch state.daemon {
        case .online: "Daemon online"
        case .offline: "Daemon offline"
        case .unknown: "Checking daemon"
        }
    }

    private func providerMenuTitle(_ provider: ProviderVM) -> String {
        let value: String
        if let percent = provider.percent {
            let displayed = max(0, min(100, state.ui.menuMetric == .used ? 100 - percent : percent))
            value = "\(Int(displayed.rounded()))\(state.ui.menuMetric == .used ? "% used" : "% left")"
        } else {
            value = provider.primary
        }
        return "\(provider.name): \(value) · \(provider.status.label)"
    }

    private func menuBarSettingsItem() -> NSMenuItem {
        let item = NSMenuItem(title: "Menu Bar", action: nil, keyEquivalent: "")
        item.image = menuIcon(symbolImage("menubar.rectangle"))

        let submenu = NSMenu(title: "Menu Bar")
        submenu.autoenablesItems = false
        submenu.addItem(toggleItem(
            title: "Show Provider Labels",
            state: state.ui.showProviderLabels,
            action: #selector(toggleProviderLabelsFromMenu)
        ))
        submenu.addItem(.separator())
        submenu.addItem(metricItem(title: "% Left", metric: .remaining))
        submenu.addItem(metricItem(title: "% Used", metric: .used))
        submenu.addItem(.separator())
        submenu.addItem(maxProvidersItem(count: 1))
        submenu.addItem(maxProvidersItem(count: 2))
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
        let item = NSMenuItem(title: "Show \(count) Provider\(count == 1 ? "" : "s")", action: #selector(setMaxProvidersFromMenu(_:)), keyEquivalent: "")
        item.target = self
        item.representedObject = count
        item.state = state.ui.maxMenuProviders == count ? .on : .off
        return item
    }

    private func symbolImage(_ name: String) -> NSImage? {
        NSImage(systemSymbolName: name, accessibilityDescription: nil)
    }

    private func menuIcon(_ image: NSImage?) -> NSImage? {
        guard let image else { return nil }
        let copy = image.copy() as? NSImage ?? image
        copy.size = menuIconSize
        copy.isTemplate = image.isTemplate
        return copy
    }

    @objc private func openSummaryFromMenu() {
        showPopover(selection: .summary)
    }

    @objc private func openSettingsFromMenu() {
        showPopover(selection: .settings)
    }

    @objc private func openProviderFromMenu(_ sender: NSMenuItem) {
        guard let id = sender.representedObject as? String else { return }
        showPopover(selection: .provider(id))
    }

    @objc private func refreshFromMenu() {
        Task { await state.refreshAll() }
    }

    @objc private func toggleProviderLabelsFromMenu() {
        state.ui.showProviderLabels.toggle()
    }

    @objc private func setMetricFromMenu(_ sender: NSMenuItem) {
        guard let raw = sender.representedObject as? String,
              let metric = UIConfig.MenuMetric(rawValue: raw)
        else { return }
        state.ui.menuMetric = metric
    }

    @objc private func setMaxProvidersFromMenu(_ sender: NSMenuItem) {
        guard let count = sender.representedObject as? Int else { return }
        state.ui.maxMenuProviders = count
    }

    @objc private func quitFromMenu() {
        NSApp.terminate(nil)
    }

    // Renders the popover content in a floating window so the UI can be
    // inspected/screenshotted without clicking the status item.
    private var debugWindow: NSWindow?
    private func showDebugWindow() {
        let size = NSSize(width: Theme.Popover.width, height: Theme.Popover.height)
        let window = NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless], backing: .buffered, defer: false)
        window.isOpaque = false
        window.backgroundColor = .clear
        window.level = .floating
        window.contentViewController = GlassPopoverHostingController(rootView: Popover().environmentObject(state))
        if let screen = NSScreen.main {
            window.setFrameTopLeftPoint(NSPoint(x: 60, y: screen.frame.maxY - 60))
        }
        window.orderFrontRegardless()
        debugWindow = window
    }
}

/// Hosts the SwiftUI popover content on the app's single translucent shell:
/// Liquid Glass (`NSGlassEffectView`) on macOS 26, vibrancy
/// (`NSVisualEffectView`) on 14/15. All interior surfaces are flat fills, so
/// the two renderings read as the same app.
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

        if #available(macOS 26, *) {
            let glass = NSGlassEffectView(frame: frame)
            glass.cornerRadius = Theme.Radius.xl
            glass.clipsToBounds = true
            // Enough body that text stays readable over arbitrary desktops,
            // low enough that the glass still reads as glass.
            glass.tintColor = NSColor.windowBackgroundColor.withAlphaComponent(0.55)
            glass.contentView = host
            view = glass
        } else {
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
}
