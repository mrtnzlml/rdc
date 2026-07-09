import XCTest
@testable import RossumLocal

/// Fake runner: emits started then done synchronously through the progress callback.
struct FakeRunner: SyncRunner {
    let fileCount: UInt64
    func run(folder: URL, scope: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult {
        progress.onPhase(phase: .started)
        progress.onPhase(phase: .done(fileCount: fileCount))
        return SyncResult(fileCount: fileCount)
    }
}

/// Records the `scope` URL it was handed, then completes synchronously. Used to
/// assert the coordinator brackets the security-scoped bookmark URL (the granted
/// parent / external), not the plain child folder path.
/// `@unchecked Sendable`: `recordedScope` is written inside the detached run and
/// read only after the terminal phase lands on the main actor (happens-after).
final class RecordingRunner: SyncRunner, @unchecked Sendable {
    private(set) var recordedScope: URL?
    func run(folder: URL, scope: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult {
        recordedScope = scope
        progress.onPhase(phase: .done(fileCount: 0))
        return SyncResult(fileCount: 0)
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

    /// Regression: a connection inside the granted parent must be synced with the
    /// PARENT's security-scoped bookmark active — not the plain child folder URL
    /// (which isn't security-scoped, so the sandbox would deny the scaffold write).
    func testSyncBracketsGrantedParentScopeNotPlainChildFolder() async {
        let bookmarks = BookmarkStore(defaults: UserDefaults(suiteName: "sc-\(UUID().uuidString)")!,
                                      codec: FakePathCodec())
        bookmarks.parent = URL(fileURLWithPath: "/granted/Rossum")
        let runner = RecordingRunner()
        let coord = SyncCoordinator(runner: runner, bookmarks: bookmarks)
        let s = ConnectionSummary(id: "test", name: "test", apiBase: "https://example.test/api/v1",
                                  orgId: 1, folder: "/granted/Rossum/test", authKind: .token,
                                  lastSyncUnix: nil, fileCount: 0)
        coord.sync(s)
        try? await waitUntil { if case .done = coord.phases["test"] { return true }; return false }
        XCTAssertEqual(runner.recordedScope?.path, "/granted/Rossum",
                       "sync must bracket the granted parent bookmark, not the plain child folder")
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
