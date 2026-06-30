import SwiftUI

struct ContentView: View {
    @Environment(ConnectionStore.self) private var store
    @State private var selectedID: ConnectionSummary.ID?
    @State private var pendingRemoval: ConnectionSummary?
    @State private var editTarget: ConnectionSummary?

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
        .onAppear { store.reload() }
    }
}
