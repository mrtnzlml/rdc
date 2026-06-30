import AppKit

enum DockBadge {
    @MainActor static func set(_ count: Int) {
        NSApp.dockTile.badgeLabel = count > 0 ? String(count) : nil
    }
}
