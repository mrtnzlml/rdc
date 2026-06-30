import XCTest
@testable import RossumLocal

final class FormattingTests: XCTestCase {
    func testNilIsNeverSynced() {
        XCTAssertEqual(lastSyncText(nil), "Never synced")
    }
    func testRelativePast() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        let twoHoursEarlier = Int64(1_000_000 - 7200)
        let text = lastSyncText(twoHoursEarlier, now: now)
        XCTAssertTrue(text.contains("hour"), "expected an hours-ago string, got: \(text)")
    }
}
