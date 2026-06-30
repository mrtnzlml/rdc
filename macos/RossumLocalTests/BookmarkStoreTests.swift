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
