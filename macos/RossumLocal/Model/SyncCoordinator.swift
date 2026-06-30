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

        Task.detached { [bridge] in
            // Blocking FFI call off the main thread. Errors surface as a final phase.
            do { _ = try runner.run(folder: folder, apiBase: apiBase, orgId: orgId, progress: bridge) }
            catch { bridge.onPhase(phase: .error(message: message(from: error))) }
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
private final class SyncProgressBridge: SyncProgress {
    private let onMain: @MainActor (SyncPhase) -> Void
    init(onMain: @escaping @MainActor (SyncPhase) -> Void) { self.onMain = onMain }
    func onPhase(phase: SyncPhase) {
        Task { @MainActor in onMain(phase) }
    }
}
