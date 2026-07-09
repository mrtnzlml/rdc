import Foundation

protocol BookmarkCodec {
    func encode(_ url: URL) throws -> Data
    func decode(_ data: Data) throws -> (url: URL, isStale: Bool)
}

struct SecurityScopedBookmarkCodec: BookmarkCodec {
    func encode(_ url: URL) throws -> Data {
        try url.bookmarkData(options: .withSecurityScope,
                             includingResourceValuesForKeys: nil, relativeTo: nil)
    }
    func decode(_ data: Data) throws -> (url: URL, isStale: Bool) {
        var stale = false
        let url = try URL(resolvingBookmarkData: data, options: .withSecurityScope,
                          relativeTo: nil, bookmarkDataIsStale: &stale)
        return (url, stale)
    }
}

/// Persists access grants (the parent folder + each externally-attached project)
/// as security-scoped bookmark data in UserDefaults. Holds NO connection metadata.
final class BookmarkStore {
    private let defaults: UserDefaults
    private let codec: BookmarkCodec
    private let parentKey = "bookmark.parent"
    private let externalsKey = "bookmark.externals"

    init(defaults: UserDefaults = .standard, codec: BookmarkCodec = SecurityScopedBookmarkCodec()) {
        self.defaults = defaults
        self.codec = codec
    }

    var parent: URL? {
        get {
            guard let data = defaults.data(forKey: parentKey),
                  let resolved = try? codec.decode(data) else { return nil }
            return resolved.url
        }
        set {
            guard let url = newValue else {
                defaults.removeObject(forKey: parentKey)
                return
            }
            // Encode failure must NOT clear an existing grant — leave it intact.
            if let data = try? codec.encode(url) {
                defaults.set(data, forKey: parentKey)
            }
        }
    }

    func externalProjects() -> [URL] {
        let blobs = defaults.array(forKey: externalsKey) as? [Data] ?? []
        return blobs.compactMap { try? codec.decode($0).url }
    }

    @discardableResult
    func addExternal(_ url: URL) -> Bool {
        guard let data = try? codec.encode(url) else { return false }
        var blobs = defaults.array(forKey: externalsKey) as? [Data] ?? []
        // De-dupe by resolved path.
        blobs.removeAll { (try? codec.decode($0).url.path) == url.path }
        blobs.append(data)
        defaults.set(blobs, forKey: externalsKey)
        return true
    }

    func removeExternal(_ url: URL) {
        let blobs = (defaults.array(forKey: externalsKey) as? [Data] ?? [])
            .filter { (try? codec.decode($0).url.path) != url.path }
        defaults.set(blobs, forKey: externalsKey)
    }

    /// The security-scoped URL that authorizes file access to a connection at
    /// `folder`: the granted parent (when `folder` is inside it), otherwise the
    /// external bookmark whose resolved URL is `folder` or an ancestor of it.
    /// Returns nil when no stored grant covers `folder`.
    ///
    /// The returned URL is a freshly bookmark-resolved URL — bracket it directly
    /// with `startAccessingSecurityScopedResource`; do not transform it (via
    /// `standardizedFileURL`, `appendingPathComponent`, …) or the scope is lost.
    func scope(forConnectionAt folder: URL) -> URL? {
        let target = folder.standardizedFileURL.path
        if let parent, Self.grant(parent, covers: target) { return parent }
        return externalProjects().first { Self.grant($0, covers: target) }
    }

    /// True when `grant` is `targetPath` itself or a directory ancestor of it.
    /// Compares standardized copies but never mutates the caller's bookmark URL.
    private static func grant(_ grant: URL, covers targetPath: String) -> Bool {
        let g = grant.standardizedFileURL.path
        return targetPath == g || targetPath.hasPrefix(g + "/")
    }
}
