# rdc-ffi

In-process FFI bridge from the native macOS app ("Rossum Local") to the `rdc`
core, generated with [UniFFI](https://mozilla.github.io/uniffi-rs/).

Every operation re-uses rdc's own functions — no credential or sync logic is
reimplemented here, and the on-disk project format (`rdc.toml`,
`secrets/main.secrets.json`, `envs/main`, `.rdc/state`) is identical to the
CLI's, so the app and `rdc` interoperate on the same folders.

## Exported surface

| Function | Purpose |
|---|---|
| `list_connections(parent)` | Scan a folder for single-env (`main`) rdc projects |
| `add_connection(parent, input)` | Write `rdc.toml` + secrets under a unique slug |
| `edit_credentials(folder, input)` | Wipe + rewrite a connection's secrets |
| `validate_existing_project(path)` | Validate a folder is a `main` rdc project |
| `sync_connection(folder, api_base, org_id, progress)` | Pull-only sync with a phase callback |
| `ffi_version()` | rdc package version (About box) |

File operations the sandboxed app does natively (Trash, reveal in Finder,
security-scoped bookmarks) are intentionally **not** in this crate.

## Building the artifact

```sh
./build-xcframework.sh
```

Produces `generated/*.swift` (the Swift bindings) and `rdc_ffi.xcframework`
(a universal arm64 + x86_64 static lib). The Xcode project links both. The
final `xcodebuild -create-xcframework` step needs Xcode Command Line Tools.

## Tests

```sh
cargo test -p rdc-ffi             # offline unit tests
RDC_FFI_LIVE_DIR=/path/to/project \
  cargo test -p rdc-ffi -- --ignored live_sync   # live API (opt-in)
```
