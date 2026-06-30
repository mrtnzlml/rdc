# Native macOS App — Phase 2b: SwiftUI UI + native touches

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the runnable SwiftUI UI for "Rossum Local" on top of the merged Phase-2a model layer — sidebar + detail, add/edit sheets, open-existing/remove/reveal, and native touches (menu bar, Dock badge, notifications) — so the app is a clickable, signed-run-ready desktop tool.

**Architecture:** SwiftUI views bind to the `@Observable @MainActor` `ConnectionStore` and `SyncCoordinator` from Phase 2a (injected via the Observation `.environment`). `NavigationSplitView` gives a native vibrant sidebar + detail. Folder access goes through `NSOpenPanel` → `ConnectionStore.setParentFolder`/`attachExisting` (security-scoped bookmarks). Menu commands bridge to the active view via `NotificationCenter`; the Dock badge mirrors `SyncCoordinator.activeCount`; completion notifications fire from a model terminal-phase hook.

**Tech Stack:** SwiftUI + AppKit interop (NSOpenPanel, NSApp.dockTile, NSWorkspace) + UserNotifications, macOS 15, Swift 5 language mode, the Phase-2a model layer, XCTest, `xcodebuild`.

## Global Constraints

- **Min macOS:** `15.0`. **`SWIFT_VERSION: "5.0"`** (Swift 5 language mode). Swift module is `RossumLocal`.
- **Bundle identifier:** `ai.rossum.local`. **Product name:** `Rossum Local`.
- **No new rdc logic:** the UI only calls the existing Phase-2a model layer (`ConnectionStore`, `SyncCoordinator`), which calls the FFI. Never reimplement rdc behavior or parse on-disk formats in a view.
- **Single env:** `main` (the model/FFI owns this).
- **Sandbox:** unchanged entitlements (app-sandbox, network.client, files.user-selected.read-write). Folder access is via `NSOpenPanel` + the existing `BookmarkStore`.
- **Customer confidentiality:** no customer names/identifiers anywhere — source, tests, docs, commit messages. Neutral placeholders only.
- **Verification:** per-task gate is `xcodebuild build` (compile) + `xcodebuild test` (the model + any new XCTest), `CODE_SIGNING_ALLOWED=NO`, run in the FOREGROUND with a Bash timeout up to 600000. Views are compile-gated; the running GUI + sandbox runtime (NSOpenPanel, bookmarks, live sync) are MAINTAINER-verified by running the signed app (final task ships a checklist).
- **Build prerequisite (unchanged from 2a):** `rdc-ffi/build-xcframework.sh` must have produced `rdc_ffi.xcframework` + `rdc-ffi/generated/rdc_ffi.swift`; then `cd macos && xcodegen generate`. New files under `macos/RossumLocal/**` are auto-globbed; re-run `xcodegen generate` after adding files.
- **Do not commit generated artifacts:** `macos/RossumLocal.xcodeproj`, the xcframework, `rdc-ffi/generated/` stay gitignored.

## Phase-2a model surface this UI binds to (verbatim, merged on `main`)

```swift
@Observable @MainActor final class ConnectionStore {
    private(set) var connections: [ConnectionSummary]
    var lastError: String?
    init(bridge: RdcBridging, bookmarks: BookmarkStore)
    func addConnection(_ input: AddConnectionInput) -> Bool      // false on failure, sets lastError
    func editCredentials(folder: URL, _ input: EditCredentialsInput) -> Bool
    func attachExisting(_ path: URL) -> Bool
    func isExternal(_ summary: ConnectionSummary) -> Bool
    func remove(_ summary: ConnectionSummary) -> Bool
    func reveal(_ summary: ConnectionSummary)
    func reload()
}
@Observable @MainActor final class SyncCoordinator {
    private(set) var phases: [String: SyncPhase]                  // keyed by ConnectionSummary.id
    private(set) var activeCount: Int
    init(runner: SyncRunner = BridgeSyncRunner())
    func sync(_ summary: ConnectionSummary)
}
struct RdcBridge: RdcBridging { }                                 // concrete bridge to inject
final class BookmarkStore { init(defaults:codec:) }              // default init usable
func lastSyncText(_ unix: Int64?, now: Date) -> String
// FFI: ConnectionSummary{id,name,apiBase,orgId,folder,authKind,lastSyncUnix,fileCount},
// AddConnectionInput, EditCredentialsInput, AuthKind{token,password},
// SyncPhase{started,done(fileCount),error(message)}, ffiVersion()
```

Two model additions Phase 2a deliberately left for the UI (Task 1): a public way to set/read the granted parent folder, and a terminal-phase hook for notifications.

---

## File Structure

| File | Responsibility |
|---|---|
| `macos/RossumLocal/Model/ConnectionStore.swift` (modify) | add `parentFolder` getter + `setParentFolder(_:)` |
| `macos/RossumLocal/Model/SyncCoordinator.swift` (modify) | add `onTerminal` hook fired on done/error |
| `macos/RossumLocal/Support/Identifiable+FFI.swift` (create) | `extension ConnectionSummary: Identifiable {}` |
| `macos/RossumLocal/Support/FolderPicker.swift` (create) | `NSOpenPanel` wrappers (choose folder) |
| `macos/RossumLocal/Support/DockBadge.swift` (create) | set/clear Dock-tile badge |
| `macos/RossumLocal/Support/Notifications.swift` (create) | `UNUserNotificationCenter` auth + post; `Notification.Name` menu signals |
| `macos/RossumLocal/RossumLocalApp.swift` (modify) | composition root, `.environment`, `.commands`, dock/notif wiring |
| `macos/RossumLocal/Views/ContentView.swift` (create) | `NavigationSplitView` + sheet/dialog state + menu `.onReceive` |
| `macos/RossumLocal/Views/SidebarView.swift` (create) | connection `List` + selection + status glyph + context menu |
| `macos/RossumLocal/Views/DetailView.swift` (create) | selected-connection fields + action buttons + sync state |
| `macos/RossumLocal/Views/EmptyStateView.swift` (create) | no-folder / no-connections states |
| `macos/RossumLocal/Views/AddConnectionSheet.swift` (create) | add-connection form |
| `macos/RossumLocal/Views/EditCredentialsSheet.swift` (create) | edit-credentials form |
| `macos/RossumLocalTests/StoreUIBindingTests.swift` (create) | XCTest for the Task-1 model additions |

---

## Task 1: Model additions — parent-folder entry point + terminal-phase hook

**Files:**
- Modify: `macos/RossumLocal/Model/ConnectionStore.swift`
- Modify: `macos/RossumLocal/Model/SyncCoordinator.swift`
- Create: `macos/RossumLocalTests/StoreUIBindingTests.swift`

**Interfaces:**
- Produces:
  - `ConnectionStore.parentFolder: URL?` (reads `bookmarks.parent`)
  - `ConnectionStore.setParentFolder(_ url: URL)` (sets `bookmarks.parent`, then `reload()`)
  - `SyncCoordinator.onTerminal: ((String, SyncPhase) -> Void)?` — invoked on the main actor when a connection's sync reaches `.done`/`.error` (id, phase).

- [ ] **Step 1: Expose the parent folder on `ConnectionStore`**

In `ConnectionStore.swift`, add (after `reveal`):
```swift
    /// The granted parent folder for managed connections, if one has been chosen.
    var parentFolder: URL? { bookmarks.parent }

    /// Grant (or change) the parent folder, then refresh the list.
    func setParentFolder(_ url: URL) {
        bookmarks.parent = url
        reload()
    }
```

- [ ] **Step 2: Add the terminal-phase hook to `SyncCoordinator`**

In `SyncCoordinator.swift`, add a stored property and fire it from `apply`:
```swift
    /// Called on the main actor when a sync reaches a terminal phase (.done/.error).
    var onTerminal: ((String, SyncPhase) -> Void)?
```
and change `apply(id:phase:)` to:
```swift
    private func apply(id: String, phase: SyncPhase) {
        let wasActive = isActive(phases[id])
        phases[id] = phase
        if wasActive, !isActive(phase) {
            activeCount = max(0, activeCount - 1)
            onTerminal?(id, phase)
        }
    }
```

- [ ] **Step 3: Write failing tests**

Create `macos/RossumLocalTests/StoreUIBindingTests.swift`:
```swift
import XCTest
@testable import RossumLocal

@MainActor
final class StoreUIBindingTests: XCTestCase {
    func testSetParentFolderExposesItAndReloads() {
        let bm = BookmarkStore(defaults: UserDefaults(suiteName: "ui-\(UUID().uuidString)")!,
                               codec: FakePathCodec())
        let store = ConnectionStore(bridge: FakeBridge(), bookmarks: bm)
        XCTAssertNil(store.parentFolder)
        store.setParentFolder(URL(fileURLWithPath: "/tmp/Rossum"))
        XCTAssertEqual(store.parentFolder?.path, "/tmp/Rossum")
    }

    func testOnTerminalFiresOnDone() async {
        let coord = SyncCoordinator(runner: FakeRunner(fileCount: 3))
        var terminal: (String, SyncPhase)?
        coord.onTerminal = { id, phase in terminal = (id, phase) }
        let s = ConnectionSummary(id: "acme", name: "acme", apiBase: "https://example.test/api/v1",
                                  orgId: 1, folder: "/p/acme", authKind: .token,
                                  lastSyncUnix: nil, fileCount: 0)
        coord.sync(s)
        for _ in 0..<200 where terminal == nil { try? await Task.sleep(nanoseconds: 10_000_000) }
        XCTAssertEqual(terminal?.0, "acme")
        if case .done(let n)? = terminal?.1 { XCTAssertEqual(n, 3) } else { XCTFail("not .done") }
    }
}
```
(`FakeBridge`, `FakePathCodec`, `FakeRunner` are the existing top-level test helpers from Phase 2a.)

- [ ] **Step 4: Run red→green**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO test`
Expected: `** TEST SUCCEEDED **` — the two new tests pass, all 14 prior tests stay green.

- [ ] **Step 5: Commit**

```bash
git add macos/RossumLocal/Model/ConnectionStore.swift macos/RossumLocal/Model/SyncCoordinator.swift macos/RossumLocalTests/StoreUIBindingTests.swift
git commit -m "feat(macos): expose parentFolder/setParentFolder + sync onTerminal hook

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Composition root + ContentView shell + EmptyStateView (runnable app)

**Files:**
- Create: `macos/RossumLocal/Support/Identifiable+FFI.swift`
- Modify: `macos/RossumLocal/RossumLocalApp.swift`
- Create: `macos/RossumLocal/Views/ContentView.swift`
- Create: `macos/RossumLocal/Views/EmptyStateView.swift`

**Interfaces:**
- Consumes: `ConnectionStore`, `SyncCoordinator`, `RdcBridge`, `BookmarkStore`.
- Produces: a buildable app whose `ContentView` is a `NavigationSplitView`; `ConnectionSummary: Identifiable`; the stores injected via `.environment`.

- [ ] **Step 1: Make `ConnectionSummary` Identifiable**

Create `macos/RossumLocal/Support/Identifiable+FFI.swift`:
```swift
import Foundation

// ConnectionSummary already has `id: String`; declare the conformance so it
// can drive SwiftUI Lists and selection.
extension ConnectionSummary: Identifiable {}
```

- [ ] **Step 2: Composition root**

Replace `macos/RossumLocal/RossumLocalApp.swift` with:
```swift
import SwiftUI

@main
struct RossumLocalApp: App {
    @State private var store = ConnectionStore(bridge: RdcBridge(), bookmarks: BookmarkStore())
    @State private var sync = SyncCoordinator()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environment(store)
                .environment(sync)
                .frame(minWidth: 720, minHeight: 460)
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
```

- [ ] **Step 3: ContentView shell**

Create `macos/RossumLocal/Views/ContentView.swift`:
```swift
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
```

- [ ] **Step 4: EmptyStateView**

Create `macos/RossumLocal/Views/EmptyStateView.swift`:
```swift
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
```

- [ ] **Step 5: Declare the menu `Notification.Name`s (needed for the app to compile)**

Create `macos/RossumLocal/Support/Notifications.swift` (the UNUserNotificationCenter parts are filled in Task 8; the names are needed now):
```swift
import Foundation

extension Notification.Name {
    static let newConnection = Notification.Name("ai.rossum.local.newConnection")
    static let openExisting = Notification.Name("ai.rossum.local.openExisting")
}
```

- [ ] **Step 6: Build (compile gate) + tests stay green**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO build`
Expected: `** BUILD SUCCEEDED **`.
Run the test action too: `... CODE_SIGNING_ALLOWED=NO test` → `** TEST SUCCEEDED **` (16 tests; nothing regressed).

- [ ] **Step 7: Commit**

```bash
git add macos/RossumLocal/RossumLocalApp.swift macos/RossumLocal/Views/ContentView.swift macos/RossumLocal/Views/EmptyStateView.swift macos/RossumLocal/Support/Identifiable+FFI.swift macos/RossumLocal/Support/Notifications.swift
git commit -m "feat(macos): app composition root + NavigationSplitView shell + empty state

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: SidebarView (connection list + selection + status + context menu)

**Files:**
- Create: `macos/RossumLocal/Views/SidebarView.swift`
- Modify: `macos/RossumLocal/Views/ContentView.swift` (use `SidebarView`, bind selection)

**Interfaces:**
- Consumes: `store.connections`, `sync.phases`, `store.isExternal`, `store.reveal`, `store.remove`, `sync.sync`.
- Produces: `SidebarView(selectedID:)` taking a `Binding<ConnectionSummary.ID?>`.

- [ ] **Step 1: SidebarView**

Create `macos/RossumLocal/Views/SidebarView.swift`:
```swift
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
```

- [ ] **Step 2: Wire SidebarView into ContentView**

Replace the sidebar branch + add removal state in `ContentView.swift`:
```swift
import SwiftUI

struct ContentView: View {
    @Environment(ConnectionStore.self) private var store
    @State private var selectedID: ConnectionSummary.ID?
    @State private var pendingRemoval: ConnectionSummary?

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
            Text("Detail")   // replaced in Task 4
        }
        .onAppear { store.reload() }
    }
}
```

- [ ] **Step 3: Build + tests**

Run: `cd macos && xcodegen generate && xcodebuild ... CODE_SIGNING_ALLOWED=NO build` → `** BUILD SUCCEEDED **`, then `... test` → `** TEST SUCCEEDED **`.

- [ ] **Step 4: Commit**

```bash
git add macos/RossumLocal/Views/SidebarView.swift macos/RossumLocal/Views/ContentView.swift
git commit -m "feat(macos): sidebar connection list with status glyph + context menu

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: DetailView (fields + actions + sync state)

**Files:**
- Create: `macos/RossumLocal/Views/DetailView.swift`
- Modify: `macos/RossumLocal/Views/ContentView.swift` (use `DetailView`; hold edit-sheet state)

**Interfaces:**
- Consumes: the selected `ConnectionSummary`, `sync.phases`, `sync.sync`, `store.reveal`, `lastSyncText`.
- Produces: `DetailView(connection:, editTarget:)`.

- [ ] **Step 1: DetailView**

Create `macos/RossumLocal/Views/DetailView.swift`:
```swift
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
```

- [ ] **Step 2: Wire DetailView + edit-sheet state into ContentView**

Update `ContentView.swift` body's `detail:` branch and add `@State private var editTarget: ConnectionSummary?`:
```swift
        } detail: {
            if let selected {
                DetailView(connection: selected, editTarget: $editTarget, pendingRemoval: $pendingRemoval)
            } else {
                EmptyStateView(needsFolder: false)
            }
        }
```
(Add `@State private var editTarget: ConnectionSummary?` next to the other `@State` properties.)

- [ ] **Step 3: Build + tests**

Run the build + test commands → `** BUILD SUCCEEDED **` then `** TEST SUCCEEDED **`.

- [ ] **Step 4: Commit**

```bash
git add macos/RossumLocal/Views/DetailView.swift macos/RossumLocal/Views/ContentView.swift
git commit -m "feat(macos): detail pane with fields, actions, and sync status

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: AddConnectionSheet

**Files:**
- Create: `macos/RossumLocal/Views/AddConnectionSheet.swift`
- Modify: `macos/RossumLocal/Views/ContentView.swift` (present on a `showAdd` flag)

**Interfaces:**
- Consumes: `store.addConnection`, `store.lastError`, `AddConnectionInput`, `AuthKind`.
- Produces: `AddConnectionSheet(isPresented:)`.

- [ ] **Step 1: AddConnectionSheet**

Create `macos/RossumLocal/Views/AddConnectionSheet.swift`:
```swift
import SwiftUI

struct AddConnectionSheet: View {
    @Environment(ConnectionStore.self) private var store
    @Environment(\.dismiss) private var dismiss

    @State private var name = ""
    @State private var apiBase = ""
    @State private var orgId = ""
    @State private var authKind: AuthKind = .token
    @State private var token = ""
    @State private var username = ""
    @State private var password = ""
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("New Connection").font(.headline).padding()
            Form {
                TextField("Name", text: $name)
                TextField("API base", text: $apiBase, prompt: Text("https://example.test/api/v1"))
                TextField("Org ID", text: $orgId)
                Picker("Auth", selection: $authKind) {
                    Text("Token").tag(AuthKind.token)
                    Text("Username / password").tag(AuthKind.password)
                }.pickerStyle(.segmented)
                if authKind == .token {
                    SecureField("API token", text: $token)
                } else {
                    TextField("Username", text: $username)
                    SecureField("Password", text: $password)
                }
            }.formStyle(.grouped)
            if let error { Text(error).foregroundStyle(.red).padding(.horizontal) }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Add") { add() }.keyboardShortcut(.defaultAction).disabled(!isValid)
            }.padding()
        }
        .frame(width: 420)
    }

    private var isValid: Bool {
        guard !name.isEmpty, !apiBase.isEmpty, UInt64(orgId) != nil else { return false }
        return authKind == .token ? !token.isEmpty : (!username.isEmpty && !password.isEmpty)
    }

    private func add() {
        guard let org = UInt64(orgId) else { error = "Org ID must be a number."; return }
        let input = AddConnectionInput(
            name: name, apiBase: apiBase, orgId: org, authKind: authKind,
            token: authKind == .token ? token : nil,
            username: authKind == .password ? username : nil,
            password: authKind == .password ? password : nil)
        if store.addConnection(input) { dismiss() } else { error = store.lastError }
    }
}
```

- [ ] **Step 2: Present from ContentView**

Add `@State private var showAdd = false` to `ContentView`, attach a sheet, and trigger it from the New-Connection menu signal:
```swift
        .sheet(isPresented: $showAdd) { AddConnectionSheet() }
        .onReceive(NotificationCenter.default.publisher(for: .newConnection)) { _ in showAdd = true }
```
(Place both modifiers on the `NavigationSplitView`.)

- [ ] **Step 3: Build + tests** → `** BUILD SUCCEEDED **`, `** TEST SUCCEEDED **`.

- [ ] **Step 4: Commit**

```bash
git add macos/RossumLocal/Views/AddConnectionSheet.swift macos/RossumLocal/Views/ContentView.swift
git commit -m "feat(macos): Add Connection sheet wired to the New Connection command

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: EditCredentialsSheet

**Files:**
- Create: `macos/RossumLocal/Views/EditCredentialsSheet.swift`
- Modify: `macos/RossumLocal/Views/ContentView.swift` (present on `editTarget`)

**Interfaces:**
- Consumes: `store.editCredentials`, `store.lastError`, `EditCredentialsInput`, `AuthKind`, the target `ConnectionSummary`.
- Produces: `EditCredentialsSheet(connection:)`.

- [ ] **Step 1: EditCredentialsSheet**

Create `macos/RossumLocal/Views/EditCredentialsSheet.swift`:
```swift
import SwiftUI

struct EditCredentialsSheet: View {
    @Environment(ConnectionStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    let connection: ConnectionSummary

    @State private var authKind: AuthKind = .token
    @State private var token = ""
    @State private var username = ""
    @State private var password = ""
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Edit Credentials — \(connection.name)").font(.headline).padding()
            Form {
                Picker("Auth", selection: $authKind) {
                    Text("Token").tag(AuthKind.token)
                    Text("Username / password").tag(AuthKind.password)
                }.pickerStyle(.segmented)
                if authKind == .token {
                    SecureField("API token", text: $token)
                } else {
                    TextField("Username", text: $username)
                    SecureField("Password", text: $password)
                }
            }.formStyle(.grouped)
            if let error { Text(error).foregroundStyle(.red).padding(.horizontal) }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Save") { save() }.keyboardShortcut(.defaultAction).disabled(!isValid)
            }.padding()
        }
        .frame(width: 420)
        .onAppear { authKind = connection.authKind }
    }

    private var isValid: Bool {
        authKind == .token ? !token.isEmpty : (!username.isEmpty && !password.isEmpty)
    }

    private func save() {
        let input = EditCredentialsInput(
            authKind: authKind,
            token: authKind == .token ? token : nil,
            username: authKind == .password ? username : nil,
            password: authKind == .password ? password : nil)
        if store.editCredentials(folder: URL(fileURLWithPath: connection.folder), input) { dismiss() }
        else { error = store.lastError }
    }
}
```

- [ ] **Step 2: Present from ContentView**

Attach to the `NavigationSplitView` (uses the `editTarget` added in Task 4):
```swift
        .sheet(item: $editTarget) { conn in EditCredentialsSheet(connection: conn) }
```
(`item:` requires `ConnectionSummary: Identifiable` — added in Task 2.)

- [ ] **Step 3: Build + tests** → `** BUILD SUCCEEDED **`, `** TEST SUCCEEDED **`.

- [ ] **Step 4: Commit**

```bash
git add macos/RossumLocal/Views/EditCredentialsSheet.swift macos/RossumLocal/Views/ContentView.swift
git commit -m "feat(macos): Edit Credentials sheet

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Folder grant + Open Existing (NSOpenPanel)

**Files:**
- Create: `macos/RossumLocal/Support/FolderPicker.swift`
- Modify: `macos/RossumLocal/Views/EmptyStateView.swift` (a "Choose Folder…" button)
- Modify: `macos/RossumLocal/Views/ContentView.swift` (open-existing menu signal; folder-change toolbar)

**Interfaces:**
- Produces: `FolderPicker.chooseFolder(prompt:) -> URL?` (modal `NSOpenPanel`, directories only).
- Consumes: `store.setParentFolder`, `store.attachExisting`, `store.lastError`.

- [ ] **Step 1: FolderPicker**

Create `macos/RossumLocal/Support/FolderPicker.swift`:
```swift
import AppKit

enum FolderPicker {
    /// Modal directory chooser. Returns the user-selected folder (security-scoped
    /// access is active for the returned URL) or nil if cancelled.
    @MainActor
    static func chooseFolder(prompt: String, directoryHint: URL? = nil) -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = prompt
        if let directoryHint { panel.directoryURL = directoryHint }
        return panel.runModal() == .OK ? panel.url : nil
    }

    /// Default starting location for the connections parent folder.
    static var documentsRossum: URL? {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first?
            .appendingPathComponent("Rossum")
    }
}
```

- [ ] **Step 2: "Choose Folder…" in the empty state**

Add to `EmptyStateView` (inside the `if needsFolder` block, after the explanatory text):
```swift
                Button("Choose Folder…") {
                    if let url = FolderPicker.chooseFolder(prompt: "Choose", directoryHint: FolderPicker.documentsRossum) {
                        store.setParentFolder(url)
                    }
                }
                .keyboardShortcut(.defaultAction)
```

- [ ] **Step 3: Open-existing + change-folder wiring in ContentView**

Add a toolbar with a "Change Folder…" control and handle the open-existing menu signal. On the `NavigationSplitView`:
```swift
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button { showAdd = true } label: { Image(systemName: "plus") }
                    .help("New Connection")
                    .disabled(store.parentFolder == nil)
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: .openExisting)) { _ in
            if let url = FolderPicker.chooseFolder(prompt: "Open") {
                if !store.attachExisting(url) { /* lastError shown via the alert in Task 8 */ }
            }
        }
```

- [ ] **Step 4: Build + tests** → `** BUILD SUCCEEDED **`, `** TEST SUCCEEDED **`.

- [ ] **Step 5: Commit**

```bash
git add macos/RossumLocal/Support/FolderPicker.swift macos/RossumLocal/Views/EmptyStateView.swift macos/RossumLocal/Views/ContentView.swift
git commit -m "feat(macos): NSOpenPanel folder grant + open-existing project

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Native touches — Dock badge, notifications, remove confirm, error alert

**Files:**
- Modify: `macos/RossumLocal/Support/DockBadge.swift` (create), `Notifications.swift`
- Modify: `macos/RossumLocal/RossumLocalApp.swift` (wire onTerminal → badge + notification)
- Modify: `macos/RossumLocal/Views/ContentView.swift` (remove-confirm dialog + error alert)

**Interfaces:**
- Consumes: `sync.activeCount`, `sync.onTerminal`, `store.remove`, `store.lastError`.

- [ ] **Step 1: DockBadge**

Create `macos/RossumLocal/Support/DockBadge.swift`:
```swift
import AppKit

enum DockBadge {
    @MainActor static func set(_ count: Int) {
        NSApp.dockTile.badgeLabel = count > 0 ? String(count) : nil
    }
}
```

- [ ] **Step 2: Notifications (UNUserNotificationCenter)**

Add to `macos/RossumLocal/Support/Notifications.swift`:
```swift
import UserNotifications

enum SyncNotifications {
    static func requestAuthorization() {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }
    @MainActor static func post(title: String, body: String) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        let req = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(req)
    }
}
```
(`UUID()` here is in app code, not a workflow script — allowed.)

- [ ] **Step 3: Wire onTerminal + activeCount in the composition root**

In `RossumLocalApp.swift`, wire the hooks in an `.onAppear` on `ContentView` and observe `activeCount`:
```swift
            ContentView()
                .environment(store)
                .environment(sync)
                .frame(minWidth: 720, minHeight: 460)
                .onAppear {
                    SyncNotifications.requestAuthorization()
                    sync.onTerminal = { [weak store] _, phase in
                        switch phase {
                        case .done(let n): SyncNotifications.post(title: "Sync complete", body: "\(n) files")
                        case .error(let m): SyncNotifications.post(title: "Sync failed", body: m)
                        case .started: break
                        }
                        _ = store // keep store alive for the closure's lifetime; reload already happens in the model
                    }
                }
                .onChange(of: sync.activeCount) { _, newValue in DockBadge.set(newValue) }
```
(If the `[weak store]`/`_ = store` line trips an unused-capture warning, drop the capture list and the `_ = store` line — the closure doesn't actually need `store`. Keep it only if the implementer adds a post-sync `store.reload()`; the model already reloads via the FFI's on-disk writes + the next list refresh, so a reload here is optional.)

- [ ] **Step 4: Remove-confirm dialog + error alert in ContentView**

On the `NavigationSplitView`, add (uses `pendingRemoval` from Task 3 and `store.lastError`):
```swift
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
                 ? "“\(conn.name)” will be detached. Its folder stays where it is."
                 : "“\(conn.name)” will be moved to the Trash.")
        }
        .alert("Error", isPresented: Binding(
            get: { store.lastError != nil },
            set: { if !$0 { store.lastError = nil } })
        ) {
            Button("OK") { store.lastError = nil }
        } message: {
            Text(store.lastError ?? "")
        }
```

- [ ] **Step 5: Build + tests** → `** BUILD SUCCEEDED **`, `** TEST SUCCEEDED **`. Report any concurrency/unused warnings.

- [ ] **Step 6: Commit**

```bash
git add macos/RossumLocal/Support/DockBadge.swift macos/RossumLocal/Support/Notifications.swift macos/RossumLocal/RossumLocalApp.swift macos/RossumLocal/Views/ContentView.swift
git commit -m "feat(macos): Dock badge, sync notifications, remove confirm, error alert

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Final clean build, docs, and the maintainer GUI-verification checklist

**Files:**
- Modify: `macos/README.md`

**Interfaces:** none new.

- [ ] **Step 1: Final clean build + test**

Run: `cd macos && xcodegen generate && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO clean build test`
Expected: `** BUILD SUCCEEDED **` then `** TEST SUCCEEDED **` (16 tests).

- [ ] **Step 2: Update README with the UI + the maintainer checklist**

Append to `macos/README.md`:
```markdown

## Running the app (maintainer)

The model layer + UI compile under `xcodebuild`, but the GUI, the sandbox folder
grant, security-scoped bookmarks, and a live sync can only be exercised by running
the signed app:

1. `../rdc-ffi/build-xcframework.sh` (if not already built)
2. `xcodegen generate`
3. `open RossumLocal.xcodeproj`, select your team (free Personal Team is fine), ⌘R.

### Manual verification checklist
- [ ] First launch shows the "Choose a folder" empty state; picking a folder (e.g. `~/Documents/Rossum`) persists and the sidebar appears.
- [ ] **New Connection ⌘N** → fill the form (token or username/password) → the connection appears in the sidebar.
- [ ] Selecting a connection shows its details; **Sync** shows a progress glyph, then a completion notification + the Dock badge clears.
- [ ] **Open Existing rdc Project ⌘O** → pick an existing `rdc` project folder → it appears (attached, not copied).
- [ ] **Edit Credentials…** flips token↔password and persists.
- [ ] **Remove** a managed connection → moves its folder to Trash; **Detach** an external one → leaves the folder in place.
- [ ] **Reveal in Finder** opens the connection's folder.
- [ ] Interop: run `rdc sync main` in a connection's folder from Terminal — the CLI and app agree on the same files.
```

- [ ] **Step 3: Commit**

```bash
git add macos/README.md
git commit -m "docs(macos): Phase 2b UI build + maintainer GUI-verification checklist

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage** (against `2026-06-30-native-macos-app-phase2-swiftui-design.md` §6 + native touches):
- NavigationSplitView sidebar+detail: Tasks 2–4. ✓ (vibrancy is automatic for the SplitView sidebar.)
- SidebarView (list + selection + status + context menu): Task 3. ✓
- DetailView (fields + Sync/Edit/Reveal/Remove + sync state): Task 4. ✓
- AddConnectionSheet / EditCredentialsSheet (token vs password Picker): Tasks 5–6. ✓
- RemoveConfirmation (`.confirmationDialog`): Task 8. ✓
- EmptyStateView: Task 2 (+ Choose-Folder in Task 7). ✓
- Open-existing via NSOpenPanel + bookmark; folder grant: Task 7. ✓
- Menu bar (New ⌘N / Open ⌘O): Task 2 (commands) + Tasks 5/7 (handlers). ✓
- Dock badge / notifications: Task 8. ✓
- Single-instance: macOS default (no code). ✓ (noted in spec)
- The model entry point the UI needed (parent folder) + the notification hook: Task 1. ✓

**Placeholder scan:** No TBD/TODO. Every step has complete Swift. The only conditional guidance is Task 8 Step 3's optional `[weak store]` capture (with an explicit instruction on when to drop it) — bounded, not a placeholder.

**Type consistency:** `ConnectionSummary: Identifiable` (Task 2) is what lets `List(selection:)`/`.sheet(item:)` work (Tasks 3, 6). `selectedID`/`pendingRemoval`/`editTarget`/`showAdd` are `@State` on `ContentView` and passed as `@Binding` to children consistently. `AddConnectionInput`/`EditCredentialsInput`/`AuthKind`/`SyncPhase` used exactly as the FFI declares. Store/coordinator method names (`addConnection`/`editCredentials`/`attachExisting`/`remove`/`reveal`/`reload`/`setParentFolder`/`parentFolder`; `sync`/`phases`/`activeCount`/`onTerminal`) match Phase 2a + Task 1.

**Verification reality note:** every task is `xcodebuild`-compile-gated (the strongest check available without a human); only the running GUI + sandbox runtime are maintainer-verified (Task 9 checklist). SwiftUI authored blind may need small compile fixes at the gate — the implementer fixes to green before committing, as in Phase 2a.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-30-native-macos-app-phase2b-swiftui-ui.md`.
