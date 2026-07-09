import XCTest
@testable import RossumLocal

func summary(name: String, folder: String, org: UInt64 = 1) -> ConnectionSummary {
    ConnectionSummary(id: name, name: name, apiBase: "https://example.test/api/v1",
                      orgId: org, folder: folder, authKind: .token,
                      lastSyncUnix: nil, fileCount: 0)
}

struct FakeBridge: RdcBridging {
    var parentList: [ConnectionSummary] = []
    var validateMap: [String: ConnectionSummary] = [:]   // path -> summary
    func list(parent: URL) -> [ConnectionSummary] { parentList }
    func validate(path: URL) throws -> ConnectionSummary {
        guard let s = validateMap[path.path] else { throw FfiError.Operation(message: "not a project") }
        return s
    }
    func add(parent: URL, input: AddConnectionInput) throws -> ConnectionSummary {
        throw FfiError.Operation(message: "not implemented in FakeBridge")
    }
    func edit(folder: URL, input: EditCredentialsInput) throws {
        throw FfiError.Operation(message: "not implemented in FakeBridge")
    }
    func sync(folder: URL, scope: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult {
        throw FfiError.Operation(message: "not implemented in FakeBridge")
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
