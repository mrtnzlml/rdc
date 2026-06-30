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

struct ThrowingEncodeCodec: BookmarkCodec {
    struct EncodeFailed: Error {}
    func encode(_ url: URL) throws -> Data { throw EncodeFailed() }
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

    func testParentClearOnNil() {
        let store = makeStore()
        store.parent = URL(fileURLWithPath: "/tmp/Rossum")
        store.parent = nil
        XCTAssertNil(store.parent)
    }

    func testEncodeFailurePreservesExistingParentAndReportsFailure() {
        let defaults = UserDefaults(suiteName: "bm-\(UUID().uuidString)")!
        // Seed a working parent via the path codec...
        BookmarkStore(defaults: defaults, codec: FakePathCodec()).parent = URL(fileURLWithPath: "/tmp/A")
        // ...then a store whose encode throws must NOT delete it on a failed set,
        // and addExternal must report false.
        let throwing = BookmarkStore(defaults: defaults, codec: ThrowingEncodeCodec())
        throwing.parent = URL(fileURLWithPath: "/tmp/B")
        XCTAssertEqual(throwing.parent?.path, "/tmp/A")
        XCTAssertFalse(throwing.addExternal(URL(fileURLWithPath: "/tmp/C")))
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
