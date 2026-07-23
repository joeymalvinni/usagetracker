import AppKit
import SwiftUI

/// Hosts the confirm-on-open sheet in its own window: the Settings popover is
/// transient and would close when NSOpenPanel takes key focus.
@MainActor
final class OpenSessionWindow: NSObject, NSWindowDelegate {
    static let shared = OpenSessionWindow()
    private var window: NSWindow?
    private weak var state: AppState?

    func present(state: AppState, model: OpenSessionModel) {
        close()
        self.state = state
        let hosting = NSHostingController(
            rootView: OpenSessionSheet(state: state, model: model) { [weak self] in
                self?.close()
            }
        )
        let window = NSWindow(contentViewController: hosting)
        window.title = "Open Claude Session"
        window.styleMask = [.titled, .closable]
        window.level = .floating
        window.isReleasedWhenClosed = false
        window.delegate = self
        window.center()
        self.window = window
        NSApp.activate()
        window.makeKeyAndOrderFront(nil)
    }

    func close() {
        window?.delegate = nil
        window?.close()
        window = nil
    }

    // The red close button is a third dismissal path — it must clear the
    // published sheet state like Cancel does, or openSession goes stale.
    func windowWillClose(_ notification: Notification) {
        state?.openSession = nil
        window = nil
    }
}

struct OpenSessionSheet: View {
    @ObservedObject var state: AppState
    @State private var model: OpenSessionModel
    @State private var isOpening = false
    let dismiss: () -> Void

    init(state: AppState, model: OpenSessionModel, dismiss: @escaping () -> Void) {
        self.state = state
        _model = State(initialValue: model)
        self.dismiss = dismiss
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Open Claude session for \(model.accountTitle)")
                .font(.headline)

            VStack(alignment: .leading, spacing: 4) {
                Text("Working directory").font(.subheadline)
                HStack {
                    TextField("Optional — opens in the home folder", text: $model.workingDirectory)
                        .textFieldStyle(.roundedBorder)
                    Button("Choose…") { chooseFolder() }
                }
            }

            VStack(alignment: .leading, spacing: 4) {
                Text("Launch flags").font(.subheadline)
                TextField("Model (optional, e.g. fable)", text: $model.model)
                    .textFieldStyle(.roundedBorder)
                Picker("Effort", selection: $model.effort) {
                    ForEach(EffortChoice.allCases) { choice in
                        Text(choice.label).tag(choice)
                    }
                }
                Toggle("Skip permission prompts (dangerous)", isOn: $model.dangerouslySkipPermissions)
                    .onChange(of: model.dangerouslySkipPermissions) { _, isOn in
                        if !isOn { model.rememberDangerous = false }
                    }
                if model.dangerouslySkipPermissions {
                    Toggle("Remember for this account", isOn: $model.rememberDangerous)
                        .padding(.leading, 20)
                        .help("Off means the dangerous flag applies to this session only.")
                }
            }

            if let error = state.actionError {
                Text(error).font(.caption).foregroundStyle(.red)
            }

            HStack {
                Spacer()
                Button("Cancel") {
                    state.openSession = nil
                    dismiss()
                }
                .keyboardShortcut(.cancelAction)
                .disabled(isOpening)
                Button("Open") {
                    isOpening = true
                    Task {
                        await state.confirmOpenSession(model)
                        isOpening = false
                        if state.openSession == nil { dismiss() }
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(isOpening)
            }
        }
        .padding(20)
        .frame(width: 440)
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        let current = model.trimmedWorkingDirectory
        if !current.isEmpty {
            panel.directoryURL = URL(
                fileURLWithPath: (current as NSString).expandingTildeInPath
            )
        }
        if panel.runModal() == .OK, let url = panel.url {
            model.workingDirectory = url.path
        }
    }
}
