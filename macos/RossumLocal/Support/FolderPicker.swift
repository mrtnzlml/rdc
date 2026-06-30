import AppKit

enum FolderPicker {
    /// Modal directory chooser. Returns the user-selected folder (security-scoped
    /// access is active for the returned URL) or nil if cancelled.
    @MainActor
    static func chooseFolder(prompt: String, directoryHint: URL? = nil) -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = prompt
        if let directoryHint { panel.directoryURL = directoryHint }
        return panel.runModal() == .OK ? panel.url : nil
    }

    /// Default starting location for the connections parent folder.
    static var documentsRossum: URL? {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first?
            .appendingPathComponent("Rossum")
    }
}
