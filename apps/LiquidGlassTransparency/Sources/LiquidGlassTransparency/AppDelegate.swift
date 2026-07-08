import AppKit
import Combine
import SwiftUI

@main enum LiquidGlassTransparencyApp {
    @MainActor
    private static var appDelegate: AppDelegate?

    @MainActor static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        appDelegate = delegate
        app.delegate = delegate
        app.setActivationPolicy(.accessory)
        app.run()
    }
}

@MainActor final class AppDelegate: NSObject, NSApplicationDelegate {
    private let settings = GlassSettings()
    private let popover = NSPopover()
    private var item: NSStatusItem!

    func applicationDidFinishLaunching(_ notification: Notification) {
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        item.button?.image = NSImage(
            systemSymbolName: "circle.lefthalf.filled",
            accessibilityDescription: "Liquid Glass Transparency"
        )
        item.button?.target = self
        item.button?.action = #selector(togglePopover)
        item.button?.toolTip = "Liquid Glass Transparency"

        popover.behavior = .transient
        popover.contentSize = NSSize(width: 280, height: 76)
        popover.contentViewController = GlassPopoverController(settings: settings)
    }

    @objc private func togglePopover() {
        guard let button = item.button else { return }

        if popover.isShown {
            popover.performClose(nil)
        } else {
            popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
            configurePopoverWindow()
        }
    }

    private func configurePopoverWindow() {
        guard let window = popover.contentViewController?.view.window else { return }
        window.isOpaque = false
        window.backgroundColor = .clear
    }
}

@MainActor final class GlassSettings: ObservableObject {
    @Published var transparency = 0.45
}

private struct TransparencySlider: View {
    @ObservedObject var settings: GlassSettings

    var body: some View {
        Slider(value: $settings.transparency, in: 0...1)
            .controlSize(.large)
            .tint(.white.opacity(0.85))
            .frame(width: 224)
            .padding(.horizontal, 28)
            .padding(.vertical, 22)
            .accessibilityLabel("Liquid Glass transparency")
    }
}

private final class GlassPopoverController: NSViewController {
    private let settings: GlassSettings
    private let glassView = NSGlassEffectView()
    private var cancellable: AnyCancellable?

    init(settings: GlassSettings) {
        self.settings = settings
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func loadView() {
        let size = NSSize(width: 280, height: 76)
        glassView.frame = NSRect(origin: .zero, size: size)
        glassView.cornerRadius = 22
        glassView.clipsToBounds = true

        let host = NSHostingView(rootView: TransparencySlider(settings: settings).background(Color.clear))
        host.frame = glassView.bounds
        host.autoresizingMask = [.width, .height]
        host.wantsLayer = true
        host.layer?.backgroundColor = NSColor.clear.cgColor

        glassView.contentView = host
        view = glassView
        updateGlass(transparency: settings.transparency)
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        cancellable = settings.$transparency
            .receive(on: RunLoop.main)
            .sink { [weak self] transparency in
                self?.updateGlass(transparency: transparency)
            }
    }

    private func updateGlass(transparency: Double) {
        let clamped = min(max(transparency, 0), 1)
        let tintAlpha = CGFloat(1 - clamped)

        glassView.style = clamped > 0.92 ? .clear : .regular
        glassView.tintColor = NSColor.windowBackgroundColor.withAlphaComponent(tintAlpha)
    }
}
