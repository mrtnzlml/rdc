#!/usr/bin/env bash
# Build a universal (arm64 + x86_64) static lib for rdc-ffi, generate the
# Swift bindings, and assemble an .xcframework for the SwiftUI app to link.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
TARGET_DIR="$ROOT/target"
OUT="$HERE/generated"
LIB="librdc_ffi.a"
MODULE="rdc_ffi"

echo "==> Adding Apple targets"
rustup target add aarch64-apple-darwin x86_64-apple-darwin

echo "==> Building release static libs"
cargo build --release -p rdc-ffi --target aarch64-apple-darwin
cargo build --release -p rdc-ffi --target x86_64-apple-darwin

echo "==> Generating Swift bindings (library mode)"
rm -rf "$OUT"; mkdir -p "$OUT"
cargo run --release --features uniffi/cli --bin uniffi-bindgen -- \
  generate \
  --library "$TARGET_DIR/aarch64-apple-darwin/release/$LIB" \
  --language swift \
  --out-dir "$OUT"
echo "Generated files:"; ls -1 "$OUT"

echo "==> Creating universal static lib"
mkdir -p "$TARGET_DIR/universal-apple-darwin/release"
lipo -create \
  "$TARGET_DIR/aarch64-apple-darwin/release/$LIB" \
  "$TARGET_DIR/x86_64-apple-darwin/release/$LIB" \
  -output "$TARGET_DIR/universal-apple-darwin/release/$LIB"

echo "==> Assembling headers + modulemap"
HDRS="$OUT/include"; mkdir -p "$HDRS"
# UniFFI emits <Module>FFI.h and a modulemap; names can vary by version.
# Move whatever .h / .modulemap were produced into the include dir and
# normalize the modulemap filename to module.modulemap.
find "$OUT" -maxdepth 1 -name "*.h" -exec mv {} "$HDRS/" \;
find "$OUT" -maxdepth 1 -name "*.modulemap" -exec mv {} "$HDRS/module.modulemap" \;

# Fail loudly if uniffi-bindgen emitted no header/modulemap (e.g. after a
# toolchain upgrade) rather than letting xcodebuild fail later with an
# opaque error.
[ -f "$HDRS/module.modulemap" ] || { echo "ERROR: uniffi-bindgen emitted no .modulemap" >&2; exit 1; }
ls "$HDRS"/*.h >/dev/null 2>&1 || { echo "ERROR: uniffi-bindgen emitted no .h header" >&2; exit 1; }

echo "==> Creating xcframework"
rm -rf "$HERE/$MODULE.xcframework"
xcodebuild -create-xcframework \
  -library "$TARGET_DIR/universal-apple-darwin/release/$LIB" \
  -headers "$HDRS" \
  -output "$HERE/$MODULE.xcframework"

echo "==> Done"
echo "    Swift sources: $OUT/*.swift"
echo "    XCFramework:   $HERE/$MODULE.xcframework"
