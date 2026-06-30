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

    /// Returns true on success; on failure sets `lastError` and returns false.
    func addConnection(_ input: AddConnectionInput) -> Bool {
        guard let parent = bookmarks.parent else {
            lastError = "Choose a folder for your connections first."
            return false
        }
        do { _ = try bridge.add(parent: parent, input: input); reload(); return true }
        catch { lastError = message(from: error); return false }
    }

    func editCredentials(folder: URL, _ input: EditCredentialsInput) -> Bool {
        do { try bridge.edit(folder: folder, input: input); reload(); return true }
        catch { lastError = message(from: error); return false }
    }

    func attachExisting(_ path: URL) -> Bool {
        do {
            _ = try bridge.validate(path: path)
            guard bookmarks.addExternal(path) else {
                lastError = "Couldn't save access to that folder."
                return false
            }
            reload()
            return true
        } catch {
            lastError = message(from: error)
            return false
        }
    }

    /// External = the connection's folder is not inside the granted parent folder.
    func isExternal(_ summary: ConnectionSummary) -> Bool {
        guard let parent = bookmarks.parent?.standardizedFileURL.path else { return true }
        let folderParent = URL(fileURLWithPath: summary.folder).deletingLastPathComponent()
            .standardizedFileURL.path
        return folderParent != parent
    }

    func remove(_ summary: ConnectionSummary) -> Bool {
        let folder = URL(fileURLWithPath: summary.folder)
        if isExternal(summary) {
            bookmarks.removeExternal(folder)   // never trash the user's external folder
            reload(); return true
        }
        do { try FileActions.trash(folder); reload(); return true }
        catch { lastError = message(from: error); return false }
    }

    func reveal(_ summary: ConnectionSummary) {
        FileActions.reveal(URL(fileURLWithPath: summary.folder))
    }

    /// The granted parent folder for managed connections, if one has been chosen.
    var parentFolder: URL? { bookmarks.parent }

    /// Grant (or change) the parent folder, then refresh the list.
    func setParentFolder(_ url: URL) {
        bookmarks.parent = url
        reload()
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
