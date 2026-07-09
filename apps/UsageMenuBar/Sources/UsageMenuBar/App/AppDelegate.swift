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

@MainActor final class AppDelegate: NSObject, NSApplicationDelegate {
    private let state = AppState()
    private let popover = NSPopover()
    private var item: NSStatusItem!
    private var bag = Set<AnyCancellable>()

    func applicationDidFinishLaunching(_ note: Notification) {
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        configureStatusButton()

        popover.behavior = .transient
        popover.contentSize = NSSize(width: Theme.Popover.width, height: Theme.Popover.height)
        popover.contentViewController = GlassPopoverHostingController(rootView: Popover().environmentObject(state))

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
        item.button?.action = #selector(toggle)
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

    @objc private func toggle() {
        guard let button = item.button else { return }
        if popover.isShown { popover.performClose(nil) } else {
            popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
            configurePopoverWindow()
            Task { await state.refreshForPopoverOpen() }
        }
    }

    private func configurePopoverWindow() {
        guard let window = popover.contentViewController?.view.window else { return }
        window.isOpaque = false
        window.backgroundColor = .clear
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
