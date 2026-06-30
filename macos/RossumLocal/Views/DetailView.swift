import SwiftUI

struct DetailView: View {
    @Environment(ConnectionStore.self) private var store
    @Environment(SyncCoordinator.self) private var sync
    let connection: ConnectionSummary
    @Binding var editTarget: ConnectionSummary?
    @Binding var pendingRemoval: ConnectionSummary?

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(connection.name).font(.largeTitle)
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 6) {
                row("API base", connection.apiBase)
                row("Org ID", String(connection.orgId))
                row("Auth", connection.authKind == .token ? "Token" : "Username / password")
                row("Last sync", lastSyncText(connection.lastSyncUnix, now: Date()))
                row("Files", String(connection.fileCount))
            }
            syncStatus
            Spacer()
            HStack {
                Button { sync.sync(connection) } label: { Label("Sync", systemImage: "arrow.triangle.2.circlepath") }
                    .disabled(isSyncing)
                Button("Edit Credentials…") { editTarget = connection }
                Button("Reveal in Finder") { store.reveal(connection) }
                Spacer()
                Button(store.isExternal(connection) ? "Detach" : "Remove…", role: .destructive) {
                    pendingRemoval = connection
                }
            }
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    private var isSyncing: Bool {
        if case .started = sync.phases[connection.id] { return true }; return false
    }

    @ViewBuilder private var syncStatus: some View {
        switch sync.phases[connection.id] {
        case .started: HStack { ProgressView().controlSize(.small); Text("Syncing…") }
        case .done(let n): Label("Synced · \(n) files", systemImage: "checkmark.circle").foregroundStyle(.green)
        case .error(let m): Label(m, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
        case .none: EmptyView()
        }
    }

    @ViewBuilder private func row(_ label: String, _ value: String) -> some View {
        GridRow {
            Text(label).foregroundStyle(.secondary)
            Text(value).textSelection(.enabled)
        }
    }
}
