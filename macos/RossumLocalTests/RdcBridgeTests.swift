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
