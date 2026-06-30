import SwiftUI

struct EmptyStateView: View {
    @Environment(ConnectionStore.self) private var store
    let needsFolder: Bool

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: needsFolder ? "folder.badge.questionmark" : "tray")
                .font(.system(size: 40))
                .foregroundStyle(.secondary)
            Text(needsFolder ? "Choose a folder for your connections" : "No connections yet")
                .font(.headline)
            if needsFolder {
                Text("Pick a folder (e.g. ~/Documents/Rossum) where Rossum Local keeps each connection.")
                    .font(.callout).foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
        }
        .padding(40)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
