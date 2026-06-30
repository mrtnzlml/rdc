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
