import Foundation
import Observation

@Observable
@MainActor
final class ConnectionStore {
    private(set) var connections: [ConnectionSummary] = []
    var lastError: String?

    private let bridge: RdcBridging
    private let bookmarks: BookmarkStore

    init(bridge: RdcBridging, bookmarks: BookmarkStore) {
        self.bridge = bridge
        self.bookmarks = bookmarks
    }

    /// Rebuild the list: managed connections under the granted parent, plus each
    /// externally-attached project (skipping any that no longer validate),
    /// de-duped by folder path.
    func reload() {
        var result: [ConnectionSummary] = []
        if let parent = bookmarks.parent {
            result.append(contentsOf: bridge.list(parent: parent))
        }
        for ext in bookmarks.externalProjects() {
            if let summary = try? bridge.validate(path: ext) {
                result.append(summary)
            }
        }
        var seen = Set<String>()
        connections = result.filter { seen.insert($0.folder).inserted }
            .sorted { $0.name < $1.name }
    }
}
