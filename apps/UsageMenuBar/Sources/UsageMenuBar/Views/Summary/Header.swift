import SwiftUI

struct Header: View {
    @EnvironmentObject var state: AppState
    let title: String
    var subtitleStyle: HeaderSubtitleStyle = .custom("")
    var showsRefresh = true
    var refreshAction: (() -> Void)?

    var body: some View {
        HStack(spacing: Theme.Spacing.md) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(Theme.Typography.title)
                statusPill
            }
            Spacer()
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
