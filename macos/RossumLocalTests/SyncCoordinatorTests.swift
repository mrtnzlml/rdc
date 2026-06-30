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
