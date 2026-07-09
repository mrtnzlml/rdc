import SwiftUI

@main
struct RossumLocalApp: App {
    @State private var store: ConnectionStore
    @State private var sync: SyncCoordinator

    init() {
        // One grant store shared by both: ConnectionStore records/reads folder
        // access; SyncCoordinator resolves the security scope to bracket a sync
        // with (see SyncCoordinator.sync / BookmarkStore.scope). Without the
        // shared parent grant, a sandboxed sync of an in-parent connection would
        // bracket the plain child folder — a no-op scope — and the OS would deny
        // the scaffold write (EPERM).
        let bookmarks = BookmarkStore()
        _store = State(initialValue: ConnectionStore(bridge: RdcBridge(), bookmarks: bookmarks))
        _sync = State(initialValue: SyncCoordinator(bookmarks: bookmarks))
    }

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environment(store)
                .environment(sync)
                .frame(minWidth: 720, minHeight: 460)
                .onAppear {
                    SyncNotifications.requestAuthorization()
                    sync.onTerminal = { _, phase in
                        switch phase {
                        case .done(let n): SyncNotifications.post(title: "Sync complete", body: "\(n) files")
                        case .error(let m): SyncNotifications.post(title: "Sync failed", body: m)
                        case .started: break
                        }
                    }
                }
                .onChange(of: sync.activeCount) { _, newValue in DockBadge.set(newValue) }
        }
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("New Connection…") { NotificationCenter.default.post(name: .newConnection, object: nil) }
                    .keyboardShortcut("n", modifiers: .command)
                Button("Open Existing rdc Project…") { NotificationCenter.default.post(name: .openExisting, object: nil) }
                    .keyboardShortcut("o", modifiers: .command)
            }
        }
    }
}
