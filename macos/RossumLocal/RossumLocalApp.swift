import SwiftUI

@main
struct RossumLocalApp: App {
    var body: some Scene {
        WindowGroup {
            VStack(spacing: 8) {
                Text("Rossum Local").font(.title2)
                // Calling ffiVersion() proves the xcframework links and the
                // generated bindings are callable. Real UI arrives in Phase 2b.
                Text("rdc core \(ffiVersion() ?? "unavailable")")
                    .foregroundStyle(.secondary)
            }
            .frame(minWidth: 640, minHeight: 420)
            .padding()
        }
    }
}
