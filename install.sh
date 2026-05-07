#!/usr/bin/env bash
# rdc installer: detects platform/arch, downloads the right binary from the
# latest GitHub release, installs to ~/.local/bin/rdc.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/mrtnzlml/rossum-deployment-manager-experiment/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/mrtnzlml/rossum-deployment-manager-experiment/main/install.sh | sh -s -- v0.0.1
#
# Environment overrides:
#   RDC_INSTALL_DIR  Install directory (default: $HOME/.local/bin)
#   RDC_REPO         GitHub repo (default: mrtnzlml/rossum-deployment-manager-experiment)

set -euo pipefail

VERSION="${1:-latest}"
REPO="${RDC_REPO:-mrtnzlml/rossum-deployment-manager-experiment}"
INSTALL_DIR="${RDC_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux)  os="unknown-linux-gnu" ;;
    *)
        echo "rdc installer: unsupported OS: $(uname -s)" >&2
        echo "Supported: Darwin, Linux" >&2
        exit 1
        ;;
esac

case "$(uname -m)" in
    x86_64|amd64)   arch="x86_64" ;;
    aarch64|arm64)  arch="aarch64" ;;
    *)
        echo "rdc installer: unsupported arch: $(uname -m)" >&2
        echo "Supported: x86_64, aarch64" >&2
        exit 1
        ;;
esac

if [ "$os" = "unknown-linux-gnu" ] && [ "$arch" = "aarch64" ]; then
    echo "rdc installer: linux-aarch64 is not yet built." >&2
    echo "Build from source with cargo: cargo install --git https://github.com/${REPO}" >&2
    exit 1
fi

target="${arch}-${os}"

if [ "$VERSION" = "latest" ]; then
    url="https://github.com/${REPO}/releases/latest/download/rdc-${target}.tar.gz"
else
    url="https://github.com/${REPO}/releases/download/${VERSION}/rdc-${target}.tar.gz"
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO- "$1"; }
else
    echo "rdc installer: neither curl nor wget found" >&2
    exit 1
fi

mkdir -p "$INSTALL_DIR"
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

echo "Downloading rdc-${target}.tar.gz from $VERSION release…"
fetch "$url" | tar xz -C "$tmpdir"

if [ ! -f "$tmpdir/rdc" ]; then
    echo "rdc installer: extraction failed (no 'rdc' binary in tarball)" >&2
    exit 1
fi

mv "$tmpdir/rdc" "$INSTALL_DIR/rdc"
chmod +x "$INSTALL_DIR/rdc"

echo "Installed to $INSTALL_DIR/rdc"

case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        ;;
    *)
        echo
        echo "Note: $INSTALL_DIR is not on your PATH."
        echo "Add this line to your shell profile (~/.zshrc, ~/.bashrc, etc.):"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

echo
echo "Verify: rdc --version"
