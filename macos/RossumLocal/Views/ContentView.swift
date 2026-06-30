import SwiftUI

struct ContentView: View {
    @Environment(ConnectionStore.self) private var store
    @State private var selectedID: ConnectionSummary.ID?
    @State private var pendingRemoval: ConnectionSummary?
    @State private var editTarget: ConnectionSummary?
    @State private var showAdd = false

    private var selected: ConnectionSummary? {
        store.connections.first { $0.id == selectedID }
    }

    var body: some View {
        NavigationSplitView {
            if store.parentFolder == nil {
                EmptyStateView(needsFolder: true)
            } else {
                SidebarView(selectedID: $selectedID, pendingRemoval: $pendingRemoval)
            }
        } detail: {
            if let selected {
                DetailView(connection: selected, editTarget: $editTarget, pendingRemoval: $pendingRemoval)
            } else {
                EmptyStateView(needsFolder: false)
            }
        }
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button { showAdd = true } label: { Image(systemName: "plus") }
                    .help("New Connection")
                    .disabled(store.parentFolder == nil)
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: .openExisting)) { _ in
            if let url = FolderPicker.chooseFolder(prompt: "Open") {
                if !store.attachExisting(url) { /* lastError shown via the alert below */ }
            }
        }
        .onAppear { store.reload() }
        .sheet(isPresented: $showAdd) { AddConnectionSheet() }
        .sheet(item: $editTarget) { conn in EditCredentialsSheet(connection: conn) }
        .onReceive(NotificationCenter.default.publisher(for: .newConnection)) { _ in showAdd = true }
        .confirmationDialog(
            "Remove this connection?",
            isPresented: Binding(get: { pendingRemoval != nil }, set: { if !$0 { pendingRemoval = nil } }),
            presenting: pendingRemoval
        ) { conn in
            Button(store.isExternal(conn) ? "Detach" : "Move to Trash", role: .destructive) {
                _ = store.remove(conn)
                if selectedID == conn.id { selectedID = nil }
                pendingRemoval = nil
            }
            Button("Cancel", role: .cancel) { pendingRemoval = nil }
        } message: { conn in
            Text(store.isExternal(conn)
                 ? "\u{201C}\(conn.name)\u{201D} will be detached. Its folder stays where it is."
                 : "\u{201C}\(conn.name)\u{201D} will be moved to the Trash.")
        }
        .alert("Error", isPresented: Binding(
            get: { store.lastError != nil },
            set: { if !$0 { store.lastError = nil } })
        ) {
            Button("OK") { store.lastError = nil }
        } message: {
            Text(store.lastError ?? "")
        }
    }
}
