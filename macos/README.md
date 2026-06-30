# Rossum Local (macOS app)

Native SwiftUI front-end for the rdc core, bridging in-process via the
`rdc-ffi` UniFFI bindings. Phase 2a is the app foundation + tested model layer;
the full UI is Phase 2b.

## Build & run

```sh
# 1. Build the FFI artifact (once, and after any rdc/rdc-ffi change):
../rdc-ffi/build-xcframework.sh

# 2. Generate the Xcode project:
brew install xcodegen          # first time only
xcodegen generate

# 3. Open and run (free Personal Team signing — no Developer ID needed):
open RossumLocal.xcodeproj     # then ⌘R
```

`RossumLocal.xcodeproj`, `../rdc-ffi/rdc_ffi.xcframework`, and
`../rdc-ffi/generated/` are generated artifacts (gitignored).

## Test (no Xcode GUI required)

```sh
xcodebuild -project RossumLocal.xcodeproj -scheme RossumLocal \
  -destination 'platform=macOS' -derivedDataPath build \
  CODE_SIGNING_ALLOWED=NO test
```

Model-layer logic (bridge, bookmark store, connection-list merge, sync state,
formatting) is covered by `RossumLocalTests`. The sandbox runtime (folder
grant, bookmarks) and a live sync are verified by running the signed app.
