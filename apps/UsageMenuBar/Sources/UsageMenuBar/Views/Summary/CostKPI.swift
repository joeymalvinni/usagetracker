import SwiftUI

struct CostKPI: View {
    let title: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(title).font(Theme.Typography.micro).foregroundStyle(.secondary)
            NumText(value: value)
                .lineLimit(1)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 2)
        .padding(.vertical, 1)
    }
}