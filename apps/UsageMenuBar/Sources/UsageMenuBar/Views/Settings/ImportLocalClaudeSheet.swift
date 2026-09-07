import AppKit
import SwiftUI

/// Hosts the import-from-local-Claude sheet in its own window: the Settings
/// popover is transient and would close if nested panels took key focus.
@MainActor
final class ImportLocalClaudeWindow: NSObject, NSWindowDelegate {
    static let shared = ImportLocalClaudeWindow()
    private var window: NSWindow?
    private weak var state: AppState?

    func present(state: AppState, model: ImportLocalClaudeModel) {
        close()
        self.state = state
        let hosting = NSHostingController(
            rootView: ImportLocalClaudeSheet(state: state, model: model) { [weak self] in
                self?.close()
            }
        )
        let window = NSWindow(contentViewController: hosting)
        window.title = "Import from Local Claude"
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

    func windowWillClose(_ notification: Notification) {
        state?.importLocalClaude = nil
        window = nil
    }
}

struct ImportLocalClaudeSheet: View {
    @ObservedObject var state: AppState
    @State private var model: ImportLocalClaudeModel
    @State private var isImporting = false
    let dismiss: () -> Void

    init(state: AppState, model: ImportLocalClaudeModel, dismiss: @escaping () -> Void) {
        self.state = state
        _model = State(initialValue: model)
        self.dismiss = dismiss
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Import from local Claude for \(model.accountTitle)")
                .font(.headline)

            VStack(alignment: .leading, spacing: 4) {
                labeledPath("Source (read-only)", model.sourceHome)
                labeledPath("Project trust source", model.sourceClaudeJson)
                labeledPath("Destination", model.destination)
                if let identity = model.sourceIdentity {
                    labeledPath("Source identity", identity)
                }
            }
            .font(.caption)

            if let error = model.managedProfileError {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
            }

            Picker("Mode", selection: $model.mode) {
                Text("Preferences only").tag(ImportMode.prefsOnly)
                Text("Replace imported paths").tag(ImportMode.replace)
            }
            .pickerStyle(.segmented)
            .disabled(!model.canImport || isImporting)

            VStack(alignment: .leading, spacing: 8) {
                Text("Import items").font(.subheadline)
                ForEach(Array(model.toggles.enumerated()), id: \.element.id) { index, toggle in
                    toggleRow(index: index, toggle: toggle)
                }
            }

            Text(
                "Local Claude is read-only. Close any Claude session using this managed profile before replacing imported paths."
            )
            .font(.caption)
            .foregroundStyle(.secondary)

            if let error = state.actionError {
                Text(error).font(.caption).foregroundStyle(.red)
            }

            HStack {
                Spacer()
                Button("Cancel") {
                    state.importLocalClaude = nil
                    dismiss()
                }
                .keyboardShortcut(.cancelAction)
                .disabled(isImporting)
                Button("Import") {
                    isImporting = true
                    Task {
                        await state.confirmImportLocalClaude(model)
                        isImporting = false
                        if state.importLocalClaude == nil { dismiss() }
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(isImporting || !model.canImport)
            }
        }
        .padding(20)
        .frame(width: 480)
    }

    @ViewBuilder
    private func toggleRow(index: Int, toggle: ImportToggleRow) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Toggle(isOn: toggleBinding(at: index, supported: toggle.supported)) {
                HStack(spacing: 6) {
                    Text(toggle.label)
                    if let size = toggle.sizeLabel {
                        Text(size)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .disabled(!toggle.supported || !model.canImport || isImporting)
            if let note = toggle.note {
                Text(note)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .padding(.leading, 20)
            }
        }
    }

    private func toggleBinding(at index: Int, supported: Bool) -> Binding<Bool> {
        Binding(
            get: { model.toggles[index].enabled },
            set: { newValue in
                guard supported else { return }
                model.toggles[index].enabled = newValue
            }
        )
    }

    private func labeledPath(_ title: String, _ path: String) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(title).foregroundStyle(.secondary)
            Text(path).textSelection(.enabled).lineLimit(2)
        }
    }
}
