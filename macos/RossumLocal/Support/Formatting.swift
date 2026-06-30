import Foundation

/// Human last-sync label. `now` is injectable for deterministic tests.
func lastSyncText(_ unix: Int64?, now: Date = Date()) -> String {
    guard let unix else { return "Never synced" }
    let date = Date(timeIntervalSince1970: TimeInterval(unix))
    let fmt = RelativeDateTimeFormatter()
    fmt.unitsStyle = .full
    return fmt.localizedString(for: date, relativeTo: now)
}
