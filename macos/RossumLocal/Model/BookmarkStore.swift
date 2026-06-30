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
}
