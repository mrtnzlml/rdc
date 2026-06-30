import Foundation

/// Boundary over the generated FFI. A protocol so the store can be tested with
/// a fake; the real impl below calls the bindings and brackets file access in a
/// security scope (a no-op outside the sandbox, e.g. in unit tests).
protocol RdcBridging {
    func list(parent: URL) -> [ConnectionSummary]
}

struct RdcBridge: RdcBridging {
    func list(parent: URL) -> [ConnectionSummary] {
        withScope(parent) { listConnections(parent: parent.path) }
    }
}

/// Run `body` with `url`'s security scope active, balancing start/stop. Returns
/// `false`-start gracefully: if access can't be started (e.g. non-sandboxed
/// tests, where it's unnecessary), the body still runs.
@discardableResult
func withScope<T>(_ url: URL, _ body: () -> T) -> T {
    let started = url.startAccessingSecurityScopedResource()
    defer { if started { url.stopAccessingSecurityScopedResource() } }
    return body()
}
