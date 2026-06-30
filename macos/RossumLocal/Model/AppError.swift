import Foundation

/// Flatten any error from the FFI into a user-facing string. The FFI throws
/// `FfiError.Operation(message:)`; everything else falls back to its description.
func message(from error: Error) -> String {
    if case let FfiError.Operation(message) = error {
        return message
    }
    return (error as NSError).localizedDescription
}
