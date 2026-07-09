import Foundation
import Observation

protocol SyncRunner: Sendable {
    func run(folder: URL, scope: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult
}

/// Real runner: delegates to RdcBridge. Sendable because RdcBridge is stateless.
struct BridgeSyncRunner: SyncRunner {
    func run(folder: URL, scope: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult {
        try RdcBridge().sync(folder: folder, scope: scope, apiBase: apiBase, orgId: orgId, progress: progress)
    }
}

@Observable
@MainActor
final class SyncCoordinator {
    private(set) var phases: [String: SyncPhase] = [:]
    private(set) var activeCount: Int = 0

    /// Called on the main actor when a sync reaches a terminal phase (.done/.error).
    var onTerminal: ((String, SyncPhase) -> Void)?

    private let runner: SyncRunner
    private let bookmarks: BookmarkStore?
    init(runner: SyncRunner = BridgeSyncRunner(), bookmarks: BookmarkStore? = nil) {
        self.runner = runner
        self.bookmarks = bookmarks
    }

    func sync(_ summary: ConnectionSummary) {
        let id = summary.id
        guard phases[id] == nil || !isActive(phases[id]) else { return } // no double-run
        phases[id] = .started
        activeCount += 1

        let folder = URL(fileURLWithPath: summary.folder)
        // The security-scoped URL that actually authorizes writing under `folder`:
        // the granted parent for an in-parent connection, or the connection's own
        // external bookmark. Falls back to `folder` (a no-op scope) only when no
        // grant is known — outside the sandbox that still works.
        let scope = bookmarks?.scope(forConnectionAt: folder) ?? folder
        let apiBase = summary.apiBase
        let orgId = summary.orgId
        let runner = self.runner
        let bridge = SyncProgressBridge { [weak self] phase in
            // Already on the main actor (the bridge hops there).
            self?.apply(id: id, phase: phase)
        }

        Task.detached { [bridge] in
            // Blocking FFI call off the main thread. Errors surface as a final phase.
            do { _ = try runner.run(folder: folder, scope: scope, apiBase: apiBase, orgId: orgId, progress: bridge) }
            catch { bridge.onPhase(phase: .error(message: message(from: error))) }
        }
    }

    private func isActive(_ phase: SyncPhase?) -> Bool {
        if case .started = phase { return true }; return false
    }

    private func apply(id: String, phase: SyncPhase) {
        let wasActive = isActive(phases[id])
        phases[id] = phase
        if wasActive, !isActive(phase) {
            activeCount = max(0, activeCount - 1)
            onTerminal?(id, phase)
        }
    }
}

/// Adapts the FFI callback (fired on a background thread) onto the main actor.
private final class SyncProgressBridge: SyncProgress {
    private let onMain: @MainActor (SyncPhase) -> Void
    init(onMain: @escaping @MainActor (SyncPhase) -> Void) { self.onMain = onMain }
    func onPhase(phase: SyncPhase) {
        Task { @MainActor in onMain(phase) }
    }
}
