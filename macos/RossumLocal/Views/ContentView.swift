import SwiftUI

struct ContentView: View {
    @Environment(ConnectionStore.self) private var store
    @State private var selectedID: ConnectionSummary.ID?

    var body: some View {
        NavigationSplitView {
            if store.parentFolder == nil {
                EmptyStateView(needsFolder: true)
            } else {
                Text("Sidebar")   // replaced by SidebarView in Task 3
            }
        } detail: {
            Text("Detail")        // replaced by DetailView in Task 4
        }
        .onAppear { store.reload() }
    }
}
