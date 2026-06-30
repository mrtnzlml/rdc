# Native macOS App — Phase 2a: app foundation + model layer

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the XcodeGen-generated `macos/RossumLocal` app that links the `rdc_ffi.xcframework` and builds, plus the full testable model layer (bridge, connection store, bookmark store, discovery merge, sync coordinator) — leaving a green `xcodebuild test` suite the SwiftUI UI (Phase 2b) binds to.

**Architecture:** A SwiftUI app target (macOS 15, sandboxed) generated from a committed `macos/project.yml`. It links the Phase-1 `rdc_ffi.xcframework` and includes the generated `rdc_ffi.swift`. The model layer wraps the FFI behind protocols for testability: `RdcBridge` (sync FFI calls + security-scope bracketing), `BookmarkStore` (security-scoped bookmark persistence with an injectable codec), `ConnectionStore` (`@Observable` UI state + the parent-scan ∪ external-bookmark discovery merge), and `SyncCoordinator` (the blocking `syncConnection` on a background task with the `SyncProgress`→`@MainActor` hop).

**Tech Stack:** Swift + SwiftUI (macOS 15), XcodeGen, the Phase-1 `rdc-ffi` UniFFI bindings, XCTest, `xcodebuild`.

## Global Constraints

- **Min macOS:** `15.0` (deployment target on every target).
- **Bundle identifier:** `ai.rossum.local`. **Product name:** `Rossum Local`.
- **Sandbox-first:** app target entitlements = `com.apple.security.app-sandbox`, `com.apple.security.network.client`, `com.apple.security.files.user-selected.read-write`. The **test target is NOT sandboxed** (so it can read/write temp dirs and call the FFI freely).
- **Single source of truth:** all sync/credential/scaffold/discovery work goes through the generated FFI functions (`listConnections`, `addConnection`, `editCredentials`, `validateExistingProject`, `syncConnection`, `ffiVersion`). Never reimplement rdc logic or parse `rdc.toml`/`secrets/*.secrets.json` by hand in Swift.
- **Single env:** every project uses env name `main` (the FFI hardcodes it). Not a customer identifier.
- **On-disk format untouched:** the app and the `rdc` CLI interoperate on the same folders.
- **Build prerequisite:** `rdc-ffi/rdc_ffi.xcframework` + `rdc-ffi/generated/rdc_ffi.swift` must exist (produced by `rdc-ffi/build-xcframework.sh`) before generating/building the Xcode project. They are gitignored.
- **Customer confidentiality:** no customer names or customer-specific identifiers anywhere — source, tests, docs, commit messages. Neutral placeholders only (`example.test`, `acme`, `main`).
- **Signing:** build verification uses `CODE_SIGNING_ALLOWED=NO`; the maintainer's interactive run uses free Personal Team automatic signing.
- **Do not commit generated artifacts:** `macos/RossumLocal.xcodeproj`, `rdc-ffi/rdc_ffi.xcframework`, `rdc-ffi/generated/` are gitignored.

## The generated FFI surface this plan consumes (verbatim, from `rdc-ffi/generated/rdc_ffi.swift`)

```swift
func listConnections(parent: String) -> [ConnectionSummary]
func addConnection(parent: String, input: AddConnectionInput) throws -> ConnectionSummary
func editCredentials(folder: String, input: EditCredentialsInput) throws
func validateExistingProject(path: String) throws -> ConnectionSummary
func syncConnection(folder: String, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult
func ffiVersion() -> String?
struct ConnectionSummary { var id, name, apiBase: String; var orgId: UInt64; var folder: String; var authKind: AuthKind; var lastSyncUnix: Int64?; var fileCount: UInt64 }
struct AddConnectionInput { var name, apiBase: String; var orgId: UInt64; var authKind: AuthKind; var token, username, password: String? }
struct EditCredentialsInput { var authKind: AuthKind; var token, username, password: String? }
struct SyncResult { var fileCount: UInt64 }
enum AuthKind { case token, password }
enum FfiError: Error { case Operation(message: String) }
enum SyncPhase { case started; case done(fileCount: UInt64); case error(message: String) }
protocol SyncProgress: AnyObject { func onPhase(phase: SyncPhase) }
```

---

## File Structure

| File | Responsibility |
|---|---|
| `macos/project.yml` (create) | XcodeGen config: app + test targets, links xcframework, includes bindings |
| `macos/.gitignore` (create) | Ignore generated `RossumLocal.xcodeproj`, build/, xcuserdata |
| `macos/RossumLocal/RossumLocalApp.swift` (create→grow) | `@main` App; minimal window in 2a (full UI in 2b) |
| `macos/RossumLocal/RossumLocal.entitlements` (create) | Sandbox + network.client + user-selected files |
| `macos/RossumLocal/Model/AppError.swift` (create) | `FfiError`→message mapping |
| `macos/RossumLocal/Model/RdcBridge.swift` (create) | `RdcBridging` protocol + real impl over the FFI, scope bracketing |
| `macos/RossumLocal/Model/BookmarkStore.swift` (create) | Security-scoped bookmark persistence (injectable codec) |
| `macos/RossumLocal/Model/ConnectionStore.swift` (create) | `@Observable` UI state + discovery merge |
| `macos/RossumLocal/Model/SyncCoordinator.swift` (create) | Blocking sync on background task + `SyncProgress`→`@MainActor` |
| `macos/RossumLocal/Support/Formatting.swift` (create) | Relative last-sync date formatting |
| `macos/RossumLocalTests/*.swift` (create) | XCTest: bridge (temp-dir), bookmark store, store merge, sync coordinator, formatting |

---

## Task 1: XcodeGen project that links the FFI and builds

**Files:**
- Create: `macos/project.yml`, `macos/.gitignore`, `macos/RossumLocal/RossumLocalApp.swift`, `macos/RossumLocal/RossumLocal.entitlements`
- Create: `macos/RossumLocalTests/SmokeTests.swift`

**Interfaces:**
- Consumes: the generated FFI (`ffiVersion()`), the `rdc_ffi.xcframework`.
- Produces: a buildable `RossumLocal` app target + `RossumLocalTests` test target; the project layout every later task extends.

> This is THE integration gate: it proves the xcframework links, the generated `rdc_ffi.swift` compiles into the app, and the FFI is callable from Swift (`ffiVersion()`). If linking fails, iterate on `project.yml` (framework path, the `rdc_ffiFFI` module from the xcframework's modulemap) until `xcodebuild build` succeeds — do not proceed until it does.

- [ ] **Step 1: Ensure the FFI artifact + bindings exist**

Run: `test -d rdc-ffi/rdc_ffi.xcframework && test -f rdc-ffi/generated/rdc_ffi.swift && echo PRESENT || (cd rdc-ffi && ./build-xcframework.sh)`
Expected: `PRESENT` (they were built during Phase 1). If missing, the script rebuilds them (slow, release+LTO) — run it in the FOREGROUND with a Bash timeout of 600000.

- [ ] **Step 2: Install XcodeGen if needed**

Run: `command -v xcodegen >/dev/null && echo HAVE || brew install xcodegen`
Expected: `HAVE`, or a successful brew install.

- [ ] **Step 3: Create `macos/.gitignore`**

```
RossumLocal.xcodeproj/
build/
*.xcuserdatad
.DS_Store
```

- [ ] **Step 4: Create `macos/project.yml`**

```yaml
name: RossumLocal
options:
  bundleIdPrefix: ai.rossum
  createIntermediateGroups: true
  deploymentTarget:
    macOS: "15.0"
settings:
  base:
    MARKETING_VERSION: "0.1.0"
    CURRENT_PROJECT_VERSION: "1"
    SWIFT_VERSION: "5.0"   # Swift 5 language mode (strict-concurrency = warnings); tighten to 6.0 later
targets:
  RossumLocal:
    type: application
    platform: macOS
    deploymentTarget: "15.0"
    sources:
      - path: RossumLocal
      - path: ../rdc-ffi/generated/rdc_ffi.swift
        group: Generated
    settings:
      base:
        PRODUCT_BUNDLE_IDENTIFIER: ai.rossum.local
        PRODUCT_NAME: "Rossum Local"
        GENERATE_INFOPLIST_FILE: YES
        INFOPLIST_KEY_CFBundleDisplayName: "Rossum Local"
        INFOPLIST_KEY_LSApplicationCategoryType: "public.app-category.developer-tools"
        INFOPLIST_KEY_NSHumanReadableCopyright: ""
        CODE_SIGN_ENTITLEMENTS: RossumLocal/RossumLocal.entitlements
        CODE_SIGN_STYLE: Automatic
        ENABLE_HARDENED_RUNTIME: YES
        SWIFT_EMIT_LOC_STRINGS: NO
    dependencies:
      - framework: ../rdc-ffi/rdc_ffi.xcframework
        embed: false
  RossumLocalTests:
    type: bundle.unit-test
    platform: macOS
    deploymentTarget: "15.0"
    sources:
      - path: RossumLocalTests
    dependencies:
      - target: RossumLocal
    settings:
      base:
        # Unit tests are not sandboxed: free temp-dir + FFI access.
        ENABLE_APP_SANDBOX: NO
```

> Note on linking: `embed: false` because the xcframework wraps a **static** lib (`librdc_ffi.a`) — static libs are linked, not embedded. XcodeGen adds it to "Link Binary With Libraries" and sets the framework search path; the xcframework's bundled `module.modulemap` exposes the `rdc_ffiFFI` C module that `rdc_ffi.swift` imports.

- [ ] **Step 5: Create `macos/RossumLocal/RossumLocal.entitlements`**

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.app-sandbox</key>
    <true/>
    <key>com.apple.security.network.client</key>
    <true/>
    <key>com.apple.security.files.user-selected.read-write</key>
    <true/>
</dict>
</plist>
```

- [ ] **Step 6: Create the minimal app that calls the FFI**

`macos/RossumLocal/RossumLocalApp.swift`:
```swift
import SwiftUI

@main
struct RossumLocalApp: App {
    var body: some Scene {
        WindowGroup {
            VStack(spacing: 8) {
                Text("Rossum Local").font(.title2)
                // Calling ffiVersion() proves the xcframework links and the
                // generated bindings are callable. Real UI arrives in Phase 2b.
                Text("rdc core \(ffiVersion() ?? "unavailable")")
                    .foregroundStyle(.secondary)
            }
            .frame(minWidth: 640, minHeight: 420)
            .padding()
        }
    }
}
```

- [ ] **Step 7: Create a smoke test that calls the FFI**

`macos/RossumLocalTests/SmokeTests.swift`:
```swift
import XCTest
@testable import RossumLocal

final class SmokeTests: XCTestCase {
    func testFfiVersionIsCallable() {
        // Proves the test target links the xcframework and the bindings load.
        XCTAssertNotNil(ffiVersion())
    }
}
```

- [ ] **Step 8: Generate the project and build (the integration gate)**

```bash
cd macos
xcodegen generate
xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal \
  -destination 'platform=macOS' -derivedDataPath build \
  CODE_SIGNING_ALLOWED=NO build
```
Expected: `** BUILD SUCCEEDED **`. If the `rdc_ffiFFI` module is not found, add `FRAMEWORK_SEARCH_PATHS` for `../rdc-ffi` or verify the xcframework dependency in the generated project, then re-run. Do not proceed until the build succeeds.

- [ ] **Step 9: Run the smoke test**

```bash
cd macos
xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal \
  -destination 'platform=macOS' -derivedDataPath build \
  CODE_SIGNING_ALLOWED=NO test
```
Expected: `** TEST SUCCEEDED **`, `testFfiVersionIsCallable` passes.

- [ ] **Step 10: Commit**

```bash
git add macos/project.yml macos/.gitignore macos/RossumLocal macos/RossumLocalTests
git commit -m "feat(macos): XcodeGen skeleton linking rdc_ffi.xcframework, FFI callable

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: AppError + RdcBridge read path (`listConnections`)

**Files:**
- Create: `macos/RossumLocal/Model/AppError.swift`
- Create: `macos/RossumLocal/Model/RdcBridge.swift`
- Create: `macos/RossumLocalTests/RdcBridgeTests.swift`

**Interfaces:**
- Consumes: FFI `listConnections`, `ConnectionSummary`, `FfiError`.
- Produces:
  - `func message(from error: Error) -> String` (in AppError.swift) — `FfiError.Operation(message)` → `message`, else `localizedDescription`.
  - `protocol RdcBridging` with `func list(parent: URL) -> [ConnectionSummary]` (more methods added in later tasks).
  - `struct RdcBridge: RdcBridging` — the real impl; `list` brackets `parent` in a security scope and calls `listConnections(parent.path)`.

- [ ] **Step 1: Create the error mapper**

`macos/RossumLocal/Model/AppError.swift`:
```swift
import Foundation

/// Flatten any error from the FFI into a user-facing string. The FFI throws
/// `FfiError.Operation(message:)`; everything else falls back to its description.
func message(from error: Error) -> String {
    if case let FfiError.Operation(message) = error {
        return message
    }
    return (error as NSError).localizedDescription
}
```

- [ ] **Step 2: Create the bridge protocol + real `list`**

`macos/RossumLocal/Model/RdcBridge.swift`:
```swift
import Foundation

/// Boundary over the generated FFI. A protocol so the store can be tested with
/// a fake; the real impl below calls the bindings and brackets file access in a
/// security scope (a no-op outside the sandbox, e.g. in unit tests).
protocol RdcBridging {
    func list(parent: URL) -> [ConnectionSummary]
}

struct RdcBridge: RdcBridging {
    func list(parent: URL) -> [ConnectionSummary] {
        withScope(parent) { listConnections(parent: parent.path) }
    }
}

/// Run `body` with `url`'s security scope active, balancing start/stop. Returns
/// `false`-start gracefully: if access can't be started (e.g. non-sandboxed
/// tests, where it's unnecessary), the body still runs.
@discardableResult
func withScope<T>(_ url: URL, _ body: () -> T) -> T {
    let started = url.startAccessingSecurityScopedResource()
    defer { if started { url.stopAccessingSecurityScopedResource() } }
    return body()
}
```

- [ ] **Step 3: Write the failing test (list against a temp dir)**

`macos/RossumLocalTests/RdcBridgeTests.swift`:
```swift
import XCTest
@testable import RossumLocal

final class RdcBridgeTests: XCTestCase {
    /// Create a temp dir containing one rdc project folder (rdc.toml + [envs.main]).
    private func seedProject(named name: String, orgId: UInt64) throws -> URL {
        let parent = FileManager.default.temporaryDirectory
            .appendingPathComponent("rdc-test-\(UUID().uuidString)")
        let folder = parent.appendingPathComponent(name)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let toml = "[envs.main]\napi_base = \"https://example.test/api/v1\"\norg_id = \(orgId)\n"
        try toml.write(to: folder.appendingPathComponent("rdc.toml"), atomically: true, encoding: .utf8)
        return parent
    }

    func testListReturnsSeededConnection() throws {
        let parent = try seedProject(named: "acme", orgId: 5)
        defer { try? FileManager.default.removeItem(at: parent) }
        let conns = RdcBridge().list(parent: parent)
        XCTAssertEqual(conns.count, 1)
        XCTAssertEqual(conns.first?.name, "acme")
        XCTAssertEqual(conns.first?.orgId, 5)
        XCTAssertEqual(conns.first?.authKind, .token)
    }
}
```

- [ ] **Step 4: Run to verify it fails (red), then passes (green)**

Run: `cd macos && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO test`
Expected RED before Steps 1–2 exist (compile error: `RdcBridge`/`message` undefined). After Steps 1–2: `** TEST SUCCEEDED **`, `testListReturnsSeededConnection` passes.

- [ ] **Step 5: Commit**

```bash
git add macos/RossumLocal/Model/AppError.swift macos/RossumLocal/Model/RdcBridge.swift macos/RossumLocalTests/RdcBridgeTests.swift
git commit -m "feat(macos): RdcBridge.list + FfiError message mapping

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: BookmarkStore (security-scoped bookmark persistence)

**Files:**
- Create: `macos/RossumLocal/Model/BookmarkStore.swift`
- Create: `macos/RossumLocalTests/BookmarkStoreTests.swift`

**Interfaces:**
- Produces:
  - `protocol BookmarkCodec { func encode(_ url: URL) throws -> Data; func decode(_ data: Data) throws -> (url: URL, isStale: Bool) }`
  - `struct SecurityScopedBookmarkCodec: BookmarkCodec` — real `.withSecurityScope` impl.
  - `final class BookmarkStore` with: `var parent: URL?` (get/set persists), `func externalProjects() -> [URL]`, `func addExternal(_ url: URL)`, `func removeExternal(_ url: URL)`. Persists bookmark *data* in an injected `UserDefaults`.

> The tracking/persistence logic is tested with a fake in-memory codec (a non-sandboxed test process can't create real `.withSecurityScope` bookmarks). The real codec is thin and validated at runtime in the maintainer's signed run.

- [ ] **Step 1: Create the store + codec**

`macos/RossumLocal/Model/BookmarkStore.swift`:
```swift
import Foundation

protocol BookmarkCodec {
    func encode(_ url: URL) throws -> Data
    func decode(_ data: Data) throws -> (url: URL, isStale: Bool)
}

struct SecurityScopedBookmarkCodec: BookmarkCodec {
    func encode(_ url: URL) throws -> Data {
        try url.bookmarkData(options: .withSecurityScope,
                             includingResourceValuesForKeys: nil, relativeTo: nil)
    }
    func decode(_ data: Data) throws -> (url: URL, isStale: Bool) {
        var stale = false
        let url = try URL(resolvingBookmarkData: data, options: .withSecurityScope,
                          relativeTo: nil, bookmarkDataIsStale: &stale)
        return (url, stale)
    }
}

/// Persists access grants (the parent folder + each externally-attached project)
/// as security-scoped bookmark data in UserDefaults. Holds NO connection metadata.
final class BookmarkStore {
    private let defaults: UserDefaults
    private let codec: BookmarkCodec
    private let parentKey = "bookmark.parent"
    private let externalsKey = "bookmark.externals"

    init(defaults: UserDefaults = .standard, codec: BookmarkCodec = SecurityScopedBookmarkCodec()) {
        self.defaults = defaults
        self.codec = codec
    }

    var parent: URL? {
        get {
            guard let data = defaults.data(forKey: parentKey),
                  let resolved = try? codec.decode(data) else { return nil }
            return resolved.url
        }
        set {
            guard let url = newValue, let data = try? codec.encode(url) else {
                defaults.removeObject(forKey: parentKey); return
            }
            defaults.set(data, forKey: parentKey)
        }
    }

    func externalProjects() -> [URL] {
        let blobs = defaults.array(forKey: externalsKey) as? [Data] ?? []
        return blobs.compactMap { try? codec.decode($0).url }
    }

    func addExternal(_ url: URL) {
        guard let data = try? codec.encode(url) else { return }
        var blobs = defaults.array(forKey: externalsKey) as? [Data] ?? []
        // De-dupe by resolved path.
        blobs.removeAll { (try? codec.decode($0).url.path) == url.path }
        blobs.append(data)
        defaults.set(blobs, forKey: externalsKey)
    }

    func removeExternal(_ url: URL) {
        let blobs = (defaults.array(forKey: externalsKey) as? [Data] ?? [])
            .filter { (try? codec.decode($0).url.path) != url.path }
        defaults.set(blobs, forKey: externalsKey)
    }
}
```

- [ ] **Step 2: Write the failing test (fake codec)**

`macos/RossumLocalTests/BookmarkStoreTests.swift`:
```swift
import XCTest
@testable import RossumLocal

/// In-memory codec: encodes a URL's path as UTF-8 bytes, decodes it back. Lets
/// us test tracking/persistence without real security-scoped bookmarks.
struct FakePathCodec: BookmarkCodec {
    func encode(_ url: URL) throws -> Data { Data(url.path.utf8) }
    func decode(_ data: Data) throws -> (url: URL, isStale: Bool) {
        (URL(fileURLWithPath: String(decoding: data, as: UTF8.self)), false)
    }
}

final class BookmarkStoreTests: XCTestCase {
    private func makeStore() -> BookmarkStore {
        let defaults = UserDefaults(suiteName: "test-\(UUID().uuidString)")!
        return BookmarkStore(defaults: defaults, codec: FakePathCodec())
    }

    func testParentRoundTrips() {
        let store = makeStore()
        XCTAssertNil(store.parent)
        store.parent = URL(fileURLWithPath: "/tmp/Rossum")
        XCTAssertEqual(store.parent?.path, "/tmp/Rossum")
    }

    func testExternalsAddDedupeRemove() {
        let store = makeStore()
        let a = URL(fileURLWithPath: "/ext/a")
        let b = URL(fileURLWithPath: "/ext/b")
        store.addExternal(a)
        store.addExternal(b)
        store.addExternal(a) // dupe
        XCTAssertEqual(Set(store.externalProjects().map(\.path)), ["/ext/a", "/ext/b"])
        store.removeExternal(a)
        XCTAssertEqual(store.externalProjects().map(\.path), ["/ext/b"])
    }
}
```

- [ ] **Step 3: Run red→green**

Run: `cd macos && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO test`
Expected: after Step 1, `** TEST SUCCEEDED **` — `testParentRoundTrips` and `testExternalsAddDedupeRemove` pass.

- [ ] **Step 4: Commit**

```bash
git add macos/RossumLocal/Model/BookmarkStore.swift macos/RossumLocalTests/BookmarkStoreTests.swift
git commit -m "feat(macos): BookmarkStore with injectable codec for security-scoped bookmarks

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: ConnectionStore (`@Observable`) + discovery merge

**Files:**
- Modify: `macos/RossumLocal/Model/RdcBridge.swift` (add `validate(path:)` to the protocol + impl)
- Create: `macos/RossumLocal/Model/ConnectionStore.swift`
- Create: `macos/RossumLocalTests/ConnectionStoreTests.swift`

**Interfaces:**
- Consumes: `RdcBridging.list`, FFI `validateExistingProject`, `BookmarkStore`.
- Produces:
  - `RdcBridging.validate(path: URL) throws -> ConnectionSummary` (+ real impl).
  - `@Observable final class ConnectionStore` with `var connections: [ConnectionSummary]`, `var lastError: String?`, `init(bridge:bookmarks:)`, `func reload()`. `reload()` = `bridge.list(parent)` (if parent set) ∪ `bookmarks.externalProjects()` each through `bridge.validate` (skipping ones that throw), de-duped by `folder`.

- [ ] **Step 1: Add `validate` to the bridge**

In `RdcBridge.swift`, add to the protocol and impl:
```swift
// in protocol RdcBridging:
    func validate(path: URL) throws -> ConnectionSummary

// in struct RdcBridge:
    func validate(path: URL) throws -> ConnectionSummary {
        try withScope(path) { try validateExistingProject(path: path.path) }
    }
```
(`withScope` returns the throwing body's result — update its signature to rethrow:)
```swift
@discardableResult
func withScope<T>(_ url: URL, _ body: () throws -> T) rethrows -> T {
    let started = url.startAccessingSecurityScopedResource()
    defer { if started { url.stopAccessingSecurityScopedResource() } }
    return try body()
}
```

- [ ] **Step 2: Create the store**

`macos/RossumLocal/Model/ConnectionStore.swift`:
```swift
import Foundation
import Observation

@Observable
@MainActor
final class ConnectionStore {
    private(set) var connections: [ConnectionSummary] = []
    var lastError: String?

    private let bridge: RdcBridging
    private let bookmarks: BookmarkStore

    init(bridge: RdcBridging, bookmarks: BookmarkStore) {
        self.bridge = bridge
        self.bookmarks = bookmarks
    }

    /// Rebuild the list: managed connections under the granted parent, plus each
    /// externally-attached project (skipping any that no longer validate),
    /// de-duped by folder path.
    func reload() {
        var result: [ConnectionSummary] = []
        if let parent = bookmarks.parent {
            result.append(contentsOf: bridge.list(parent: parent))
        }
        for ext in bookmarks.externalProjects() {
            if let summary = try? bridge.validate(path: ext) {
                result.append(summary)
            }
        }
        var seen = Set<String>()
        connections = result.filter { seen.insert($0.folder).inserted }
            .sorted { $0.name < $1.name }
    }
}
```

- [ ] **Step 3: Write the failing test (fake bridge)**

`macos/RossumLocalTests/ConnectionStoreTests.swift`:
```swift
import XCTest
@testable import RossumLocal

private func summary(name: String, folder: String, org: UInt64 = 1) -> ConnectionSummary {
    ConnectionSummary(id: name, name: name, apiBase: "https://example.test/api/v1",
                      orgId: org, folder: folder, authKind: .token,
                      lastSyncUnix: nil, fileCount: 0)
}

private struct FakeBridge: RdcBridging {
    var parentList: [ConnectionSummary] = []
    var validateMap: [String: ConnectionSummary] = [:]   // path -> summary
    func list(parent: URL) -> [ConnectionSummary] { parentList }
    func validate(path: URL) throws -> ConnectionSummary {
        guard let s = validateMap[path.path] else { throw FfiError.Operation(message: "not a project") }
        return s
    }
}

@MainActor
final class ConnectionStoreTests: XCTestCase {
    private func bookmarks(parent: URL?, externals: [URL]) -> BookmarkStore {
        let store = BookmarkStore(defaults: UserDefaults(suiteName: "cs-\(UUID().uuidString)")!,
                                  codec: FakePathCodec())
        store.parent = parent
        externals.forEach { store.addExternal($0) }
        return store
    }

    func testMergesParentAndExternalsSortedDeduped() {
        let bridge = FakeBridge(
            parentList: [summary(name: "beta", folder: "/p/beta"),
                         summary(name: "alpha", folder: "/p/alpha")],
            validateMap: ["/ext/gamma": summary(name: "gamma", folder: "/ext/gamma")]
        )
        let store = ConnectionStore(
            bridge: bridge,
            bookmarks: bookmarks(parent: URL(fileURLWithPath: "/p"),
                                 externals: [URL(fileURLWithPath: "/ext/gamma")]))
        store.reload()
        XCTAssertEqual(store.connections.map(\.name), ["alpha", "beta", "gamma"])
    }

    func testInvalidExternalIsSkipped() {
        let bridge = FakeBridge(parentList: [], validateMap: [:]) // validate always throws
        let store = ConnectionStore(
            bridge: bridge,
            bookmarks: bookmarks(parent: nil, externals: [URL(fileURLWithPath: "/ext/gone")]))
        store.reload()
        XCTAssertTrue(store.connections.isEmpty)
    }
}
```

- [ ] **Step 4: Run red→green**

Run: `cd macos && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO test`
Expected: `** TEST SUCCEEDED **` — both store tests pass.

- [ ] **Step 5: Commit**

```bash
git add macos/RossumLocal/Model/RdcBridge.swift macos/RossumLocal/Model/ConnectionStore.swift macos/RossumLocalTests/ConnectionStoreTests.swift
git commit -m "feat(macos): ConnectionStore with parent-scan + external-bookmark merge

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: RdcBridge write paths (add / edit) + store mutations

**Files:**
- Modify: `macos/RossumLocal/Model/RdcBridge.swift` (add `add`, `edit`)
- Modify: `macos/RossumLocal/Model/ConnectionStore.swift` (add `addConnection`, `editCredentials`)
- Modify: `macos/RossumLocalTests/RdcBridgeTests.swift` (add temp-dir add/edit test)

**Interfaces:**
- Produces:
  - `RdcBridging.add(parent: URL, input: AddConnectionInput) throws -> ConnectionSummary`
  - `RdcBridging.edit(folder: URL, input: EditCredentialsInput) throws`
  - `ConnectionStore.addConnection(_ input:) -> Bool` and `editCredentials(folder:_ input:) -> Bool` — call the bridge, set `lastError` + return false on throw, `reload()` + return true on success.

- [ ] **Step 1: Add bridge methods**

In `RdcBridge.swift` protocol + impl:
```swift
// protocol:
    func add(parent: URL, input: AddConnectionInput) throws -> ConnectionSummary
    func edit(folder: URL, input: EditCredentialsInput) throws

// impl:
    func add(parent: URL, input: AddConnectionInput) throws -> ConnectionSummary {
        try withScope(parent) { try addConnection(parent: parent.path, input: input) }
    }
    func edit(folder: URL, input: EditCredentialsInput) throws {
        try withScope(folder) { try editCredentials(folder: folder.path, input: input) }
    }
```

- [ ] **Step 2: Add store mutations**

In `ConnectionStore.swift`:
```swift
    /// Returns true on success; on failure sets `lastError` and returns false.
    func addConnection(_ input: AddConnectionInput) -> Bool {
        guard let parent = bookmarks.parent else {
            lastError = "Choose a folder for your connections first."
            return false
        }
        do { _ = try bridge.add(parent: parent, input: input); reload(); return true }
        catch { lastError = message(from: error); return false }
    }

    func editCredentials(folder: URL, _ input: EditCredentialsInput) -> Bool {
        do { try bridge.edit(folder: folder, input: input); reload(); return true }
        catch { lastError = message(from: error); return false }
    }
```

- [ ] **Step 3: Add the failing temp-dir test (real bridge add → list reflects it; edit flips auth)**

Add to `RdcBridgeTests.swift`:
```swift
    func testAddThenListReflectsConnectionAndEditFlipsAuth() throws {
        let parent = FileManager.default.temporaryDirectory
            .appendingPathComponent("rdc-test-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: parent) }

        let bridge = RdcBridge()
        let added = try bridge.add(parent: parent, input: AddConnectionInput(
            name: "Acme Prod", apiBase: "https://example.test/api/v1", orgId: 7,
            authKind: .token, token: "tok-123", username: nil, password: nil))
        XCTAssertEqual(added.orgId, 7)
        XCTAssertEqual(added.authKind, .token)
        XCTAssertEqual(bridge.list(parent: parent).count, 1)

        let folder = parent.appendingPathComponent(added.id)
        try bridge.edit(folder: folder, input: EditCredentialsInput(
            authKind: .password, token: nil, username: "user", password: "pass"))
        XCTAssertEqual(bridge.list(parent: parent).first?.authKind, .password)
    }
```

- [ ] **Step 4: Run red→green**

Run: `cd macos && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO test`
Expected: `** TEST SUCCEEDED **`, `testAddThenListReflectsConnectionAndEditFlipsAuth` passes.

- [ ] **Step 5: Commit**

```bash
git add macos/RossumLocal/Model macos/RossumLocalTests/RdcBridgeTests.swift
git commit -m "feat(macos): bridge+store add_connection / edit_credentials

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Open-existing (validate + bookmark) and Remove (trash vs. drop) + reveal

**Files:**
- Modify: `macos/RossumLocal/Model/ConnectionStore.swift` (add `attachExisting`, `remove`, `reveal`)
- Create: `macos/RossumLocal/Support/FileActions.swift`
- Create: `macos/RossumLocalTests/RemoveLogicTests.swift`

**Interfaces:**
- Produces:
  - `FileActions` (in Support): `static func trash(_ url: URL) throws` (`FileManager.trashItem`), `static func reveal(_ url: URL)` (`NSWorkspace.activateFileViewerSelecting`).
  - `ConnectionStore.attachExisting(_ path: URL) -> Bool` — `bridge.validate`, on success `bookmarks.addExternal` + `reload`.
  - `ConnectionStore.isExternal(_ summary:) -> Bool` — true iff the summary's folder is NOT under `bookmarks.parent`.
  - `ConnectionStore.remove(_ summary:) -> Bool` — if external: `bookmarks.removeExternal` (never trash); else `FileActions.trash(folder)`. Then `reload`.
  - `ConnectionStore.reveal(_ summary:)`.

- [ ] **Step 1: Create FileActions**

`macos/RossumLocal/Support/FileActions.swift`:
```swift
import AppKit

enum FileActions {
    static func trash(_ url: URL) throws {
        var resulting: NSURL?
        try FileManager.default.trashItem(at: url, resultingItemURL: &resulting)
    }
    static func reveal(_ url: URL) {
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }
}
```

- [ ] **Step 2: Add store methods**

In `ConnectionStore.swift`:
```swift
    func attachExisting(_ path: URL) -> Bool {
        do { _ = try bridge.validate(path: path); bookmarks.addExternal(path); reload(); return true }
        catch { lastError = message(from: error); return false }
    }

    /// External = the connection's folder is not inside the granted parent folder.
    func isExternal(_ summary: ConnectionSummary) -> Bool {
        guard let parent = bookmarks.parent?.standardizedFileURL.path else { return true }
        let folderParent = URL(fileURLWithPath: summary.folder).deletingLastPathComponent()
            .standardizedFileURL.path
        return folderParent != parent
    }

    func remove(_ summary: ConnectionSummary) -> Bool {
        let folder = URL(fileURLWithPath: summary.folder)
        if isExternal(summary) {
            bookmarks.removeExternal(folder)   // never trash the user's external folder
            reload(); return true
        }
        do { try FileActions.trash(folder); reload(); return true }
        catch { lastError = message(from: error); return false }
    }

    func reveal(_ summary: ConnectionSummary) {
        FileActions.reveal(URL(fileURLWithPath: summary.folder))
    }
```

- [ ] **Step 3: Write the failing test (remove decision: external drops bookmark, managed trashes)**

`macos/RossumLocalTests/RemoveLogicTests.swift`:
```swift
import XCTest
@testable import RossumLocal

@MainActor
final class RemoveLogicTests: XCTestCase {
    func testIsExternalDistinguishesParentVsOutside() {
        let bm = BookmarkStore(defaults: UserDefaults(suiteName: "rm-\(UUID().uuidString)")!,
                               codec: FakePathCodec())
        bm.parent = URL(fileURLWithPath: "/p")
        let store = ConnectionStore(bridge: FakeBridge(), bookmarks: bm)

        let managed = summary(name: "m", folder: "/p/m")
        let external = summary(name: "e", folder: "/elsewhere/e")
        XCTAssertFalse(store.isExternal(managed))
        XCTAssertTrue(store.isExternal(external))
    }

    func testRemoveExternalDropsBookmarkWithoutTrashing() {
        let bm = BookmarkStore(defaults: UserDefaults(suiteName: "rm-\(UUID().uuidString)")!,
                               codec: FakePathCodec())
        bm.parent = URL(fileURLWithPath: "/p")
        let ext = URL(fileURLWithPath: "/elsewhere/e")
        bm.addExternal(ext)
        // FakeBridge.validate throws for unknown paths, so reload() will drop it
        // from the list once the bookmark is gone — we assert the bookmark removal.
        let store = ConnectionStore(bridge: FakeBridge(), bookmarks: bm)
        _ = store.remove(summary(name: "e", folder: "/elsewhere/e"))
        XCTAssertTrue(bm.externalProjects().isEmpty)
    }
}
```

- [ ] **Step 4: Run red→green**

Run: `cd macos && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO test`
Expected: `** TEST SUCCEEDED **`, both remove-logic tests pass. (The `NSWorkspace`/`trashItem` syscalls aren't exercised here — `reveal` and the managed-trash path are verified in the maintainer's signed run.)

- [ ] **Step 5: Commit**

```bash
git add macos/RossumLocal/Model/ConnectionStore.swift macos/RossumLocal/Support/FileActions.swift macos/RossumLocalTests/RemoveLogicTests.swift
git commit -m "feat(macos): open-existing (bookmark), remove (trash vs drop), reveal

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: SyncCoordinator + SyncProgressBridge (threading + `@MainActor` hop)

**Files:**
- Create: `macos/RossumLocal/Model/SyncCoordinator.swift`
- Modify: `macos/RossumLocal/Model/RdcBridge.swift` (add `sync`)
- Create: `macos/RossumLocalTests/SyncCoordinatorTests.swift`

**Interfaces:**
- Produces:
  - `RdcBridging.sync(folder: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult`.
  - `protocol SyncRunner { func run(folder: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult }` (the real one delegates to the bridge; tests inject a fake that drives `progress`).
  - `@Observable @MainActor final class SyncCoordinator` with `var phases: [String: SyncPhase]` (keyed by connection id) and `var activeCount: Int`; `func sync(_ summary: ConnectionSummary)` runs the blocking runner on `Task.detached` and updates `phases`/`activeCount` on the main actor via a `SyncProgressBridge`.

> Threading is the crux: the FFI call blocks and `onPhase` fires on the calling (background) thread; `SyncProgressBridge.onPhase` MUST hop to `@MainActor` before touching `@Observable` state. This task isolates that hop in one place and tests the resulting state transitions with a fake runner.

- [ ] **Step 1: Add `sync` to the bridge**

In `RdcBridge.swift`:
```swift
// protocol:
    func sync(folder: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult
// impl:
    func sync(folder: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult {
        try withScope(folder) {
            try syncConnection(folder: folder.path, apiBase: apiBase, orgId: orgId, progress: progress)
        }
    }
```

- [ ] **Step 2: Create the coordinator**

`macos/RossumLocal/Model/SyncCoordinator.swift`:
```swift
import Foundation
import Observation

protocol SyncRunner: Sendable {
    func run(folder: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult
}

/// Real runner: delegates to RdcBridge. Sendable because RdcBridge is stateless.
struct BridgeSyncRunner: SyncRunner {
    func run(folder: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult {
        try RdcBridge().sync(folder: folder, apiBase: apiBase, orgId: orgId, progress: progress)
    }
}

@Observable
@MainActor
final class SyncCoordinator {
    private(set) var phases: [String: SyncPhase] = [:]
    private(set) var activeCount: Int = 0

    private let runner: SyncRunner
    init(runner: SyncRunner = BridgeSyncRunner()) { self.runner = runner }

    func sync(_ summary: ConnectionSummary) {
        let id = summary.id
        guard phases[id] == nil || !isActive(phases[id]) else { return } // no double-run
        phases[id] = .started
        activeCount += 1

        let folder = URL(fileURLWithPath: summary.folder)
        let apiBase = summary.apiBase
        let orgId = summary.orgId
        let runner = self.runner
        let bridge = SyncProgressBridge { [weak self] phase in
            // Already on the main actor (the bridge hops there).
            self?.apply(id: id, phase: phase)
        }

        Task.detached {
            // Blocking FFI call off the main thread. Errors surface as a final phase.
            do { _ = try runner.run(folder: folder, apiBase: apiBase, orgId: orgId, progress: bridge) }
            catch {
                await MainActor.run { self.apply(id: id, phase: .error(message: message(from: error))) }
            }
        }
    }

    private func isActive(_ phase: SyncPhase?) -> Bool {
        if case .started = phase { return true }; return false
    }

    private func apply(id: String, phase: SyncPhase) {
        let wasActive = isActive(phases[id])
        phases[id] = phase
        if wasActive, !isActive(phase) { activeCount = max(0, activeCount - 1) }
    }
}

/// Adapts the FFI callback (fired on a background thread) onto the main actor.
final class SyncProgressBridge: SyncProgress {
    private let onMain: @MainActor (SyncPhase) -> Void
    init(onMain: @escaping @MainActor (SyncPhase) -> Void) { self.onMain = onMain }
    func onPhase(phase: SyncPhase) {
        Task { @MainActor in onMain(phase) }
    }
}
```

- [ ] **Step 3: Write the failing test (fake runner drives phases)**

`macos/RossumLocalTests/SyncCoordinatorTests.swift`:
```swift
import XCTest
@testable import RossumLocal

/// Fake runner: emits started then done synchronously through the progress callback.
private struct FakeRunner: SyncRunner {
    let fileCount: UInt64
    func run(folder: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult {
        progress.onPhase(phase: .started)
        progress.onPhase(phase: .done(fileCount: fileCount))
        return SyncResult(fileCount: fileCount)
    }
}

@MainActor
final class SyncCoordinatorTests: XCTestCase {
    func testSyncTransitionsToDoneAndClearsActiveCount() async {
        let coord = SyncCoordinator(runner: FakeRunner(fileCount: 12))
        let s = ConnectionSummary(id: "acme", name: "acme", apiBase: "https://example.test/api/v1",
                                  orgId: 1, folder: "/p/acme", authKind: .token,
                                  lastSyncUnix: nil, fileCount: 0)
        coord.sync(s)
        // The progress hops are dispatched onto the main actor via Task { @MainActor }.
        // Yield until the terminal phase lands.
        try? await waitUntil { if case .done = coord.phases["acme"] { return true }; return false }
        if case .done(let n) = coord.phases["acme"] { XCTAssertEqual(n, 12) } else { XCTFail("not done") }
        XCTAssertEqual(coord.activeCount, 0)
    }

    /// Poll the main actor up to ~2s for `cond`.
    private func waitUntil(_ cond: @MainActor () -> Bool) async throws {
        for _ in 0..<200 {
            if cond() { return }
            try await Task.sleep(nanoseconds: 10_000_000) // 10ms
        }
        XCTFail("condition not met in time")
    }
}
```

- [ ] **Step 4: Run red→green**

Run: `cd macos && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO test`
Expected: `** TEST SUCCEEDED **`, `testSyncTransitionsToDoneAndClearsActiveCount` passes.

- [ ] **Step 5: Commit**

```bash
git add macos/RossumLocal/Model/SyncCoordinator.swift macos/RossumLocal/Model/RdcBridge.swift macos/RossumLocalTests/SyncCoordinatorTests.swift
git commit -m "feat(macos): SyncCoordinator with background sync + MainActor progress hop

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Last-sync formatting + final 2a verification + dev docs

**Files:**
- Create: `macos/RossumLocal/Support/Formatting.swift`
- Create: `macos/RossumLocalTests/FormattingTests.swift`
- Create: `macos/README.md`

**Interfaces:**
- Produces: `func lastSyncText(_ unix: Int64?, now: Date) -> String` — `nil` → "Never synced"; else a relative string ("2 hours ago").

- [ ] **Step 1: Create the formatter**

`macos/RossumLocal/Support/Formatting.swift`:
```swift
import Foundation

/// Human last-sync label. `now` is injectable for deterministic tests.
func lastSyncText(_ unix: Int64?, now: Date = Date()) -> String {
    guard let unix else { return "Never synced" }
    let date = Date(timeIntervalSince1970: TimeInterval(unix))
    let fmt = RelativeDateTimeFormatter()
    fmt.unitsStyle = .full
    return fmt.localizedString(for: date, relativeTo: now)
}
```

- [ ] **Step 2: Write the failing test**

`macos/RossumLocalTests/FormattingTests.swift`:
```swift
import XCTest
@testable import RossumLocal

final class FormattingTests: XCTestCase {
    func testNilIsNeverSynced() {
        XCTAssertEqual(lastSyncText(nil), "Never synced")
    }
    func testRelativePast() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        let twoHoursEarlier = Int64(1_000_000 - 7200)
        let text = lastSyncText(twoHoursEarlier, now: now)
        XCTAssertTrue(text.contains("hour"), "expected an hours-ago string, got: \(text)")
    }
}
```

- [ ] **Step 3: Run red→green**

Run: `cd macos && xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal -destination 'platform=macOS' -derivedDataPath build CODE_SIGNING_ALLOWED=NO test`
Expected: `** TEST SUCCEEDED **`, both formatting tests pass.

- [ ] **Step 4: Write `macos/README.md`**

```markdown
# Rossum Local (macOS app)

Native SwiftUI front-end for the rdc core, bridging in-process via the
`rdc-ffi` UniFFI bindings. Phase 2a is the app foundation + tested model layer;
the full UI is Phase 2b.

## Build & run

\`\`\`sh
# 1. Build the FFI artifact (once, and after any rdc/rdc-ffi change):
../rdc-ffi/build-xcframework.sh

# 2. Generate the Xcode project:
brew install xcodegen          # first time only
xcodegen generate

# 3. Open and run (free Personal Team signing — no Developer ID needed):
open RossumLocal.xcodeproj     # then ⌘R
\`\`\`

`RossumLocal.xcodeproj`, `../rdc-ffi/rdc_ffi.xcframework`, and
`../rdc-ffi/generated/` are generated artifacts (gitignored).

## Test (no Xcode GUI required)

\`\`\`sh
xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal \
  -destination 'platform=macOS' -derivedDataPath build \
  CODE_SIGNING_ALLOWED=NO test
\`\`\`

Model-layer logic (bridge, bookmark store, connection-list merge, sync state,
formatting) is covered by `RossumLocalTests`. The sandbox runtime (folder
grant, bookmarks) and a live sync are verified by running the signed app.
```

- [ ] **Step 5: Final full build + test of the target**

```bash
cd macos
xcodegen generate
xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal \
  -destination 'platform=macOS' -derivedDataPath build \
  CODE_SIGNING_ALLOWED=NO clean build test
```
Expected: `** BUILD SUCCEEDED **` then `** TEST SUCCEEDED **` (all model + smoke tests pass).

- [ ] **Step 6: Commit**

```bash
git add macos/RossumLocal/Support/Formatting.swift macos/RossumLocalTests/FormattingTests.swift macos/README.md
git commit -m "feat(macos): last-sync formatting + Phase 2a docs

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage** (against `2026-06-30-native-macos-app-phase2-swiftui-design.md`):
- §Decisions: XcodeGen project, macOS 15, identity, sandbox entitlements, build prerequisite → Task 1. ✓
- §Architecture units: `RdcBridge` (T2,4,5,7), `BookmarkStore` (T3), `ConnectionStore` + merge (T4), `SyncCoordinator` + `@MainActor` hop (T7), `Formatting` (T8). ✓
- §Discovery model (parent ∪ external, remove trash-vs-drop, open-existing bookmark): T4, T6. ✓
- §Sandbox & BookmarkStore: T1 (entitlements), T3 (store). ✓
- §Threading & security scope (`withScope` bracket, background sync, MainActor hop): T2 (`withScope`), T7. ✓
- §Testing (xcodebuild build/test; bookmark, merge, error mapping, sync transitions, FFI-against-temp-dir): T1–T8. ✓
- §Views + native touches (menu, Dock badge, notifications, the full SwiftUI UI): **deferred to Phase 2b** (this plan only stands up a minimal window + the tested model layer). Stated up front.

**Placeholder scan:** No TBD/TODO. Every code step has complete Swift. The only "iterate if needed" guidance is Task 1 Step 8 (framework search path) — concrete and bounded to the one fragile linking gate.

**Type consistency:** `RdcBridging` grows monotonically (list→validate→add/edit→sync) with consistent signatures; `ConnectionStore`/`SyncCoordinator` are `@MainActor @Observable`; test helpers `summary(...)`, `FakeBridge`, `FakePathCodec` are defined once and reused; FFI types (`ConnectionSummary`, `AddConnectionInput`, `EditCredentialsInput`, `SyncResult`, `AuthKind`, `SyncPhase`, `FfiError`, `SyncProgress`) used exactly as the generated bindings declare them.

**Note on blind-authored Swift:** the per-task `xcodebuild build`/`test` gate is the real correctness check — if any snippet has a compile error (e.g. a Swift 6 concurrency annotation), the implementer fixes it to green before committing, exactly as the Phase-1 cargo gate worked.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-30-native-macos-app-phase2a-foundation.md`.
