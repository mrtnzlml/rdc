#!/usr/bin/env bash
# Render the placeholder master icon and fill the macOS AppIcon.appiconset.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SET="$HERE/../RossumLocal/Assets.xcassets/AppIcon.appiconset"
mkdir -p "$SET"
MASTER="$HERE/icon-1024.png"

swift "$HERE/make-app-icon.swift" "$MASTER"

# macOS app-icon sizes: (px, filename)
emit() { sips -z "$1" "$1" "$MASTER" --out "$SET/$2" >/dev/null; }
emit 16   icon_16.png
emit 32   icon_16@2x.png
emit 32   icon_32.png
emit 64   icon_32@2x.png
emit 128  icon_128.png
emit 256  icon_128@2x.png
emit 256  icon_256.png
emit 512  icon_256@2x.png
emit 512  icon_512.png
cp "$MASTER" "$SET/icon_512@2x.png"   # 1024

cat > "$SET/Contents.json" <<'JSON'
{
  "images" : [
    { "size":"16x16","idiom":"mac","filename":"icon_16.png","scale":"1x" },
    { "size":"16x16","idiom":"mac","filename":"icon_16@2x.png","scale":"2x" },
    { "size":"32x32","idiom":"mac","filename":"icon_32.png","scale":"1x" },
    { "size":"32x32","idiom":"mac","filename":"icon_32@2x.png","scale":"2x" },
    { "size":"128x128","idiom":"mac","filename":"icon_128.png","scale":"1x" },
    { "size":"128x128","idiom":"mac","filename":"icon_128@2x.png","scale":"2x" },
    { "size":"256x256","idiom":"mac","filename":"icon_256.png","scale":"1x" },
    { "size":"256x256","idiom":"mac","filename":"icon_256@2x.png","scale":"2x" },
    { "size":"512x512","idiom":"mac","filename":"icon_512.png","scale":"1x" },
    { "size":"512x512","idiom":"mac","filename":"icon_512@2x.png","scale":"2x" }
  ],
  "info" : { "author":"xcode","version":1 }
}
JSON
echo "AppIcon.appiconset written to $SET"
