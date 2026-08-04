# rdc (desktop app)

Cross-platform desktop front-end for the rdc core — **macOS, Windows, and
Linux** — built with [Flutter](https://flutter.dev) and
[flutter_rust_bridge](https://cjycode.com/flutter_rust_bridge/) (FRB).

It manages "connections" (each an rdc project folder) and pulls Rossum orgs into
them. All file/credential/sync work is done by the `rdc` core through the
`rdc_bridge` crate — the UI adds no new logic. The on-disk format (`rdc.toml`,
`secrets/`, `envs/`, `.rdc/state`) is identical to the CLI's, so the CLI and the
app operate on the same folders.

## Layout

```
desktop/
  lib/                 Dart UI (main.dart, src/)
  lib/src/rust/        FRB-generated Dart bindings (committed)
  rust/                Rust bridge crate `rdc_bridge` (its own cargo workspace)
    src/api/rdc.rs     the FRB-exposed surface (7 ops, delegates to `rdc`)
    src/discover.rs    connection discovery (ported from the retired rdc-ffi)
    src/frb_generated.rs  FRB-generated glue (committed)
  macos/ windows/ linux/   per-platform runners
  flutter_rust_bridge.yaml  FRB config (rust_input: crate::api)
```

The bridge crate is **deliberately its own workspace** (not a member of the
parent rdc workspace) because the parent sets `panic = "abort"`, and FRB needs
unwinding to turn Rust panics into Dart exceptions. It depends on `rdc` via a
path dependency (`../..`).

## Promote

For projects with 2+ environments, the Project view (select the project row,
not one of its envs) shows a **Promote** panel below the Environments table
for moving config from one env to another (e.g. `dev` → `prod`). It's a
2-phase flow:

- **Prepare** — runs `migrate` offline (writes the target env's local
  snapshot only; no network) and then captures a dry-run push preview against
  the target org (this step *contacts* the target — it needs the target
  token — but never writes to it), so nothing is pushed to the remote yet.
- **Push** — a gated `sync --no-pull` against the target, with an explicit
  conflict policy (which side wins when the same item changed on both ends)
  and an opt-in "allow deletes" toggle.

This is deliberately the only place the app writes to an environment other
than the one you're looking at: the per-env **Sync** action elsewhere in the
app stays pull-only, so cross-environment writes always go through this
explicit, previewed flow.

**Known limitation:** Prepare unconditionally overwrites the target's local
snapshot (`envs/<tgt>/`) with `<src>`'s — any un-synced local edits to the
target are replaced, with no diff or drift check first (the panel shows a
caption warning about this before every Prepare). Full drift-detection
(warn only when the target actually has un-synced changes) is deferred.

## Prerequisites

- [Flutter](https://docs.flutter.dev/get-started/install) 3.44+ (stable), with
  desktop enabled: `flutter config --enable-macos-desktop`
  (`--enable-windows-desktop` / `--enable-linux-desktop`).
- A Rust toolchain (`rustup`). Cargokit compiles `rdc_bridge` during the build.
- macOS: Xcode + CocoaPods (`brew install cocoapods`).
- Linux: `ninja-build libgtk-3-dev clang cmake pkg-config liblzma-dev`.

## Build & run

```sh
cd desktop
flutter pub get
flutter run -d macos      # or: -d windows / -d linux
```

To produce a release bundle:

```sh
flutter build macos       # build/macos/Build/Products/Release/rdc.app
flutter build windows     # build/windows/x64/runner/Release/
flutter build linux       # build/linux/x64/release/bundle/
```

## Regenerating the bridge bindings

Only needed after changing the Rust API (`rust/src/api/`):

```sh
cargo install flutter_rust_bridge_codegen --version 2.12.0   # once
cd desktop && flutter_rust_bridge_codegen generate
```

The generated Dart (`lib/src/rust/`) and Rust glue (`rust/src/frb_generated.rs`)
are committed so CI and fresh checkouts build without the codegen toolchain.

## Tests

```sh
# Rust bridge logic (offline):
cd desktop/rust && cargo test

# Bridge behavior against the real native library (runs on the host OS):
cd desktop && flutter test integration_test -d macos    # or -d windows / -d linux
```

## Releases

Tagging `desktop-v*` triggers `.github/workflows/desktop-release.yml`, which
builds on macOS/Windows/Linux runners and publishes a notarized DMG, a Windows
zip, and a Linux tarball. macOS signing reuses the repo's existing Developer ID
secrets.
