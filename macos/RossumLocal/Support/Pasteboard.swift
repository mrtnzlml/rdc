import AppKit

/// Writes a string to the general pasteboard. Used by the error banner and the
/// sync-error context menu so users can copy a message to paste elsewhere.
enum Pasteboard {
    @MainActor static func copy(_ string: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(string, forType: .string)
    }
}
