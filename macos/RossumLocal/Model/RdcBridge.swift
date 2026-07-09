import Foundation

/// Boundary over the generated FFI. A protocol so the store can be tested with
/// a fake; the real impl below calls the bindings and brackets file access in a
/// security scope (a no-op outside the sandbox, e.g. in unit tests).
protocol RdcBridging {
    func list(parent: URL) -> [ConnectionSummary]
    func validate(path: URL) throws -> ConnectionSummary
    func add(parent: URL, input: AddConnectionInput) throws -> ConnectionSummary
    func edit(folder: URL, input: EditCredentialsInput) throws
    func sync(folder: URL, scope: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult
}

struct RdcBridge: RdcBridging {
    func list(parent: URL) -> [ConnectionSummary] {
        withScope(parent) { listConnections(parent: parent.path) }
    }

    func validate(path: URL) throws -> ConnectionSummary {
        try withScope(path) { try validateExistingProject(path: path.path) }
    }

    func add(parent: URL, input: AddConnectionInput) throws -> ConnectionSummary {
        try withScope(parent) { try addConnection(parent: parent.path, input: input) }
    }

    func edit(folder: URL, input: EditCredentialsInput) throws {
        try withScope(folder) { try editCredentials(folder: folder.path, input: input) }
    }

    // `scope` is the security-scoped URL that authorizes access (the granted
    // parent, or the connection's own external bookmark). `folder` is the plain
    // path handed to the FFI. They differ for a connection inside the parent
    // grant: the child folder isn't itself a security-scoped URL, so bracketing
    // it would be a no-op and the sandbox would deny the write.
    func sync(folder: URL, scope: URL, apiBase: String, orgId: UInt64, progress: SyncProgress) throws -> SyncResult {
        try withScope(scope) {
            try syncConnection(folder: folder.path, apiBase: apiBase, orgId: orgId, progress: progress)
        }
    }
}

/// Run `body` with `url`'s security scope active, balancing start/stop. Returns
/// `false`-start gracefully: if access can't be started (e.g. non-sandboxed
/// tests, where it's unnecessary), the body still runs.
@discardableResult
func withScope<T>(_ url: URL, _ body: () throws -> T) rethrows -> T {
    let started = url.startAccessingSecurityScopedResource()
    defer { if started { url.stopAccessingSecurityScopedResource() } }
    return try body()
}
