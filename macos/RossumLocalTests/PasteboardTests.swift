import XCTest
import AppKit
@testable import RossumLocal

@MainActor
final class PasteboardTests: XCTestCase {
    func testCopyWritesStringToGeneralPasteboard() {
        let message = "Sync failed: host example.test unreachable"
        Pasteboard.copy(message)
        XCTAssertEqual(NSPasteboard.general.string(forType: .string), message)
    }
}
