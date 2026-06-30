import Foundation

// ConnectionSummary already has `id: String`; declare the conformance so it
// can drive SwiftUI Lists and selection.
extension ConnectionSummary: Identifiable {}
