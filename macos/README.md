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

## Running the app (maintainer)

The model layer + UI compile under `xcodebuild`, but the GUI, the sandbox folder
grant, security-scoped bookmarks, and a live sync can only be exercised by running
the signed app:

1. `../rdc-ffi/build-xcframework.sh` (if not already built)
2. `xcodegen generate`
3. `open RossumLocal.xcodeproj`, select your team (free Personal Team is fine), ⌘R.

### Manual verification checklist
- [ ] First launch shows the "Choose a folder" empty state; picking a folder (e.g. `~/Documents/Rossum`) persists and the sidebar appears.
- [ ] **New Connection ⌘N** → fill the form (token or username/password) → the connection appears in the sidebar.
- [ ] Selecting a connection shows its details; **Sync** shows a progress glyph, then a completion notification + the Dock badge clears.
- [ ] **Open Existing rdc Project ⌘O** → pick an existing `rdc` project folder → it appears (attached, not copied).
- [ ] **Edit Credentials…** flips token↔password and persists.
- [ ] **Remove** a managed connection → moves its folder to Trash; **Detach** an external one → leaves the folder in place.
- [ ] **Reveal in Finder** opens the connection's folder.
- [ ] Interop: run `rdc sync main` in a connection's folder from Terminal — the CLI and app agree on the same files.
