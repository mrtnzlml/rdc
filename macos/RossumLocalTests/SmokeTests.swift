import XCTest
@testable import RossumLocal

final class SmokeTests: XCTestCase {
    func testFfiVersionIsCallable() {
        // Proves the test target links the xcframework and the bindings load.
        XCTAssertNotNil(ffiVersion())
    }
}
