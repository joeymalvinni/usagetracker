import SwiftUI

struct Header: View {
    @EnvironmentObject var state: AppState
    let title: String
    var subtitleStyle: HeaderSubtitleStyle = .custom("")
    var showsRefresh = true
    var updateAction: HeaderUpdateAction?
    var refreshAction: (() -> Void)?

    var body: some View {
        HStack(spacing: Theme.Spacing.md) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(Theme.Typography.title)
                statusPill
            }
            Spacer()
            if let updateAction {
                Button(action: updateAction.perform) {
                    HStack(spacing: Theme.Spacing.xs) {
                        if updateAction.isInstalling {
                            ProgressView()
                                .controlSize(.small)
                                .tint(.white)
                        }
                        Text(updateAction.isInstalling ? "Updating…" : "Update")
                    }
                }
                .buttonStyle(HeaderUpdateButtonStyle())
                .disabled(updateAction.isInstalling)
                .help("Install UsageTracker \(updateAction.version)")
            }
            if showsRefresh {
                RefreshRing(refreshing: state.refreshing) {
                    if let refreshAction {
                        refreshAction()
                    } else {
                        Task { await state.refreshAll() }
                    }
                }
            }
        }
    }

    @ViewBuilder
    private var statusPill: some View {
        switch subtitleStyle {
        case .online: StatusPill(online: true)
        case .offline: StatusPill(online: false, detail: state.message)
        case .custom(let text):
            if !text.isEmpty {
                Text(text)
                    .font(Theme.Typography.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
    }
}

private struct HeaderUpdateButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.Typography.caption.bold())
            .foregroundStyle(.white)
            .padding(.horizontal, Theme.Spacing.sm + 2)
            .padding(.vertical, Theme.Spacing.xs + 1)
            .background(
                Capsule()
                    .fill(Color(
                        .displayP3,
                        red: 99.0 / 255.0,
                        green: 130.0 / 255.0,
                        blue: 240.0 / 255.0,
                        opacity: 1
                    ))
            )
            .contentShape(Capsule())
            .opacity(isEnabled ? (configuration.isPressed ? 0.8 : 1) : 0.55)
            .animation(.easeOut(duration: 0.1), value: configuration.isPressed)
    }
}

struct HeaderUpdateAction {
    let version: String
    let isInstalling: Bool
    let perform: () -> Void
}

enum HeaderSubtitleStyle {
    case online, offline, custom(String)
}

/// Daemon connection state as plain labeled text. Offline is the exceptional
/// state, so it alone carries color.
struct StatusPill: View {
    let online: Bool
    var detail: String? = nil

    var body: some View {
        HStack(spacing: 5) {
            Text(online ? "online" : "offline")
                .font(Theme.Typography.micro)
                .foregroundStyle(online ? AnyShapeStyle(.secondary) : AnyShapeStyle(Color.red))
            if let detail, !detail.isEmpty {
                Text("· \(detail)")
                    .font(Theme.Typography.micro)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
        }
    }
}

/// Custom circular progress ring driven by the refreshing state.
struct RefreshRing: View {
    let refreshing: Bool
    let action: () -> Void

    @State private var rotation: Double = 0

    var body: some View {
        Button(action: action) {
            ZStack {
                if refreshing {
                    Circle()
                        .stroke(.tertiary.opacity(0.4), lineWidth: 2)
                    Circle()
                        .trim(from: 0, to: 0.7)
                        .stroke(.primary, style: StrokeStyle(lineWidth: 2, lineCap: .round))
                        .rotationEffect(.degrees(rotation))
                } else {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(.secondary)
                }
            }
            .frame(width: 28, height: 28)
        }
        .buttonStyle(.plain)
        .help("Refresh")
        .onChange(of: refreshing) {
            if refreshing {
                withAnimation(.linear(duration: 0.9).repeatForever(autoreverses: false)) { rotation = 360 }
            } else {
                rotation = 0
            }
        }
    }
}
