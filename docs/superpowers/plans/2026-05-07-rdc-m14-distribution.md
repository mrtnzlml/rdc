# rdc M14 — Distribution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rdc` installable without a Rust toolchain. Tag push → GitHub Actions cross-compiles binaries → uploads to GitHub Releases → users `curl | sh` the installer to get a binary on PATH.

**Architecture:** A single `.github/workflows/release.yaml` runs on tag push (`v*`). It cross-compiles for darwin-x86_64, darwin-aarch64, and linux-x86_64 (no Linux ARM or Windows in v0.0.1). Each target builds in its native runner (macOS for darwin, Ubuntu for linux). Binaries are tarred and uploaded as release assets. A small `install.sh` script detects platform/arch, downloads the right tarball, and installs to `~/.local/bin/rdc`. Homebrew tap and self-update deferred.

**Tech Stack:** GitHub Actions, Bash, existing Rust binary (no new code in src/).

**Scope:**
- ✅ GitHub Actions workflow that builds + releases on tag push
- ✅ Targets: darwin-x86_64, darwin-aarch64, linux-x86_64
- ✅ `install.sh` shell installer (curl | sh compatible)
- ✅ README installation section
- ✅ Tag and push v0.0.1 to trigger first release
- ❌ NOT Linux ARM (aarch64-unknown-linux-gnu)
- ❌ NOT Windows
- ❌ NOT Homebrew tap (would need a separate repo)
- ❌ NOT `rdc update` self-update command
- ❌ NOT signing / notarization

**End state of M14:**

```sh
$ curl -fsSL https://raw.githubusercontent.com/mrtnzlml/rossum-deployment-manager-experiment/main/install.sh | sh
Downloading rdc-aarch64-apple-darwin.tar.gz from latest release…
Installed to /Users/you/.local/bin/rdc
Make sure /Users/you/.local/bin is in your PATH

$ rdc --version
rdc 0.0.1
```

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `.github/workflows/release.yaml` | Create | Build + publish on tag push |
| `install.sh` | Create | Platform-detecting installer (curl \| sh) |
| `README.md` | Modify | Installation section + Status |

---

## Task 1: GitHub Actions release workflow

**Files:**
- Create: `.github/workflows/release.yaml`

- [ ] **Step 1: Create the workflow**

```yaml
name: Release

on:
  push:
    tags:
      - 'v*'

permissions:
  contents: write

jobs:
  build:
    name: Build ${{ matrix.target }}
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - target: x86_64-unknown-linux-gnu
            os: ubuntu-latest
          - target: x86_64-apple-darwin
            os: macos-latest
          - target: aarch64-apple-darwin
            os: macos-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - name: Cache cargo registry
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: cargo-${{ matrix.target }}-${{ hashFiles('Cargo.lock') }}

      - name: Build
        run: cargo build --release --target ${{ matrix.target }} --locked

      - name: Package
        shell: bash
        run: |
          cd target/${{ matrix.target }}/release
          tar czf ../../../rdc-${{ matrix.target }}.tar.gz rdc

      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: rdc-${{ matrix.target }}
          path: rdc-${{ matrix.target }}.tar.gz
          retention-days: 1

  release:
    name: Publish release
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Download artifacts
        uses: actions/download-artifact@v4
        with:
          path: artifacts
          merge-multiple: true

      - name: List artifacts
        run: ls -la artifacts/

      - name: Create release
        uses: softprops/action-gh-release@v2
        with:
          files: artifacts/*.tar.gz
          generate_release_notes: true
          fail_on_unmatched_files: true
```

- [ ] **Step 2: Verify the workflow YAML is valid**

Run: `cat .github/workflows/release.yaml | python3 -c "import sys, yaml; yaml.safe_load(sys.stdin)"`

Expected: no error.

If `python3` or `yaml` is unavailable, alternative validation:

```bash
ruby -ryaml -e "YAML.load(File.read('.github/workflows/release.yaml'))"
```

Either way, just confirm the YAML parses. Actual workflow execution can only be verified on tag push.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yaml
git commit -m "ci: GitHub Actions workflow for cross-compiled releases on tag push"
```

---

## Task 2: `install.sh` installer script

**Files:**
- Create: `install.sh`

- [ ] **Step 1: Create `install.sh`**

```bash
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

# Detect OS
case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux)  os="unknown-linux-gnu" ;;
    *)
        echo "rdc installer: unsupported OS: $(uname -s)" >&2
        echo "Supported: Darwin, Linux" >&2
        exit 1
        ;;
esac

# Detect arch
case "$(uname -m)" in
    x86_64|amd64)   arch="x86_64" ;;
    aarch64|arm64)  arch="aarch64" ;;
    *)
        echo "rdc installer: unsupported arch: $(uname -m)" >&2
        echo "Supported: x86_64, aarch64" >&2
        exit 1
        ;;
esac

# Linux aarch64 not yet built — fail with a clear message.
if [ "$os" = "unknown-linux-gnu" ] && [ "$arch" = "aarch64" ]; then
    echo "rdc installer: linux-aarch64 is not yet built." >&2
    echo "Build from source with cargo: cargo install --git https://github.com/${REPO}" >&2
    exit 1
fi

target="${arch}-${os}"

# Build the download URL.
if [ "$VERSION" = "latest" ]; then
    url="https://github.com/${REPO}/releases/latest/download/rdc-${target}.tar.gz"
else
    url="https://github.com/${REPO}/releases/download/${VERSION}/rdc-${target}.tar.gz"
fi

# Sanity: which curl/wget?
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO- "$1"; }
else
    echo "rdc installer: neither curl nor wget found" >&2
    exit 1
fi

# Download + extract.
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

# Friendly PATH check.
case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        # Already on PATH — nothing more to say.
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
```

- [ ] **Step 2: Make executable + sanity-check syntax**

```bash
chmod +x install.sh
bash -n install.sh
```

Expected: bash syntax check passes silently.

- [ ] **Step 3: Commit**

```bash
git add install.sh
git commit -m "feat(install): curl|sh installer script"
```

---

## Task 3: README installation section

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update Status + add Install section**

Replace the Status line:
```
**Status:** M14 — distributable. Pull all kinds; push + deploy for hooks/rules/labels. Install via `curl | sh` or `cargo install`.
```

Insert a new "Install" section right under the title, before "Quick start":

```
## Install

Quickest path (macOS + Linux x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/mrtnzlml/rossum-deployment-manager-experiment/main/install.sh | sh
```

This downloads the right pre-built binary from the latest GitHub release and
installs it to `~/.local/bin/rdc`. Add that directory to your `PATH` if it
isn't already.

To install a specific version:

```sh
curl -fsSL https://raw.githubusercontent.com/mrtnzlml/rossum-deployment-manager-experiment/main/install.sh | sh -s -- v0.0.1
```

Or build from source with Rust:

```sh
cargo install --git https://github.com/mrtnzlml/rossum-deployment-manager-experiment
```

Or clone the repo and `cargo install --path .`.

**Supported platforms (pre-built):** macOS (Intel + Apple Silicon), Linux x86_64.
For Linux aarch64, Windows, or other platforms, build from source.
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: M14 — installation instructions"
```

---

## Task 4: Tag v0.0.1 and trigger first release

**Files:**
- (no file changes — only git tag + push)

- [ ] **Step 1: Verify the tree is clean**

```bash
git status
```

Expected: `nothing to commit, working tree clean`. If anything is dirty, commit first.

- [ ] **Step 2: Verify version matches the tag we're about to create**

```bash
grep '^version' Cargo.toml
```

Expected: `version = "0.0.1"`. If not 0.0.1, bump it first (and commit) — the tag should match the Cargo.toml version.

- [ ] **Step 3: Create and push the tag**

```bash
git tag -a v0.0.1 -m "v0.0.1: first distributable release"
git push origin v0.0.1
```

This pushes the tag to GitHub. The release workflow runs automatically; check progress at:
`https://github.com/mrtnzlml/rossum-deployment-manager-experiment/actions`

When complete, the release will be at:
`https://github.com/mrtnzlml/rossum-deployment-manager-experiment/releases/tag/v0.0.1`

- [ ] **Step 4: After release lands, verify the installer**

```bash
RDC_INSTALL_DIR=/tmp/rdc-test bash install.sh
/tmp/rdc-test/rdc --version
```

Expected: `rdc 0.0.1`. If the install fails because the GHA workflow hasn't finished yet, wait and retry.

---

## Self-Review

**Spec coverage:**
- §15 Distribution — partial: GitHub Releases + curl|sh installer covered. Homebrew tap, `rdc update`, and Windows/Linux-ARM deferred.

**Placeholder scan:** No "TBD"/"TODO" patterns.

**Type consistency:** N/A (no Rust types in this milestone).

**Scope check:** 4 tasks. Task 1 + 2 are deliverables; Task 3 is documentation; Task 4 is the actual ship action that triggers the workflow.

---

## After M14

The tool ships. Future work (in any order, as time permits):

- **Push for remaining kinds** — queues, schemas (with formula combined hash for outbound), inboxes, engines, engine_fields, workflows, workflow_steps, email_templates, MDH. ~10 mechanical milestones similar to M13.
- **Auxiliary commands** — `rdc status` (auth + drift summary), `rdc diff <env>` (local vs remote), `rdc auth <env>` (token rotation), `rdc repair --rebuild-lock <env>` (recover from corrupted lockfile).
- **Pull-side overlay stripping** — per spec §9.3.
- **Cross-ref indexer** — "hook X attached to queues Y, Z" in `_index.md`.
- **Homebrew tap** + `rdc update` — extend distribution.
- **Linux aarch64 + Windows builds** — broader platform support.
