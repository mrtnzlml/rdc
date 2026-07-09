import SwiftUI

/// A dismissible, selectable error/warning strip shown at the top of the window.
/// The message text is selectable and offers a right-click "Copy"; the × dismisses.
struct ErrorBanner: View {
    let message: String
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            Text(message)
                .textSelection(.enabled)
                .contextMenu { Button("Copy") { Pasteboard.copy(message) } }
                .frame(maxWidth: .infinity, alignment: .leading)
            Button(action: onDismiss) {
                Image(systemName: "xmark").imageScale(.small)
            }
            .buttonStyle(.plain)
            .help("Dismiss")
        }
        .padding(10)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(.orange.opacity(0.35)))
        .padding(.horizontal, 12)
        .padding(.top, 8)
    }
}
