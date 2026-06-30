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
