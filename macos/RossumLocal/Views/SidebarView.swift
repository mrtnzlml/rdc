import SwiftUI

struct SidebarView: View {
    @Environment(ConnectionStore.self) private var store
    @Environment(SyncCoordinator.self) private var sync
    @Binding var selectedID: ConnectionSummary.ID?
    /// Set by the row's context menu / detail to request removal confirmation.
    @Binding var pendingRemoval: ConnectionSummary?

    var body: some View {
        List(store.connections, selection: $selectedID) { conn in
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text(conn.name)
                    Text(conn.apiBase).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer()
                statusGlyph(for: conn)
            }
            .tag(conn.id)
            .contextMenu {
                Button("Sync") { sync.sync(conn) }
                Button("Reveal in Finder") { store.reveal(conn) }
                Divider()
                Button(store.isExternal(conn) ? "Detach" : "Remove…", role: .destructive) {
                    pendingRemoval = conn
                }
            }
        }
        .navigationTitle("Connections")
    }

    @ViewBuilder
    private func statusGlyph(for conn: ConnectionSummary) -> some View {
        switch sync.phases[conn.id] {
        case .started:
            ProgressView().controlSize(.small)
        case .error:
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
        case .done, .none:
            EmptyView()
        }
    }
}
