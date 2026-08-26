# Automated weekly release — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A scheduled GitHub Actions workflow that cuts an rdc release once a week — but only when something shippable landed — verifying the tree before it writes a single byte.

**Architecture:** One new workflow (`weekly-release.yaml`) decides, gates, bumps, commits and tags; it then calls the existing `release.yaml` **directly** via `workflow_call` rather than pushing a tag and hoping the tag trigger fires. It cannot: a tag pushed with the default `GITHUB_TOKEN` does not start `on: push: tags` workflows. The decision logic and the four-file version bump live in two POSIX `sh` scripts under `.github/scripts/`, so both are unit-testable from Rust without a repository, a tag, or a network.

**Tech Stack:** GitHub Actions (`schedule`, `workflow_dispatch`, `workflow_call`), POSIX `sh` + `awk`, Rust integration tests (`tests/`, `assert_cmd`-free — plain `std::process::Command`), `actionlint` for workflow linting.

**Spec:** `docs/superpowers/specs/2026-08-25-automated-weekly-release-design.md`

## Global Constraints

- **Never leak customer names or customer data** — no customer names, org/division/region codes, real environment names, queue/engine/hook slugs, hostnames, URLs, or file paths in source, tests, docs, fixtures, **or git commit messages**. Neutral placeholders only (`acme`, `main`, `invoices`, `dev`/`test`/`prod`).
- **Never `git push`.** Commit to local `main` only. The maintainer publishes.
- **Work on local `main`**, not on a `fix/` or `work/` branch.
- **Never run repo-wide `cargo fmt`.** This tree is not fmt-clean under the local rustfmt and never has been; that is pre-existing, not a regression. `cargo fmt` is deliberately absent from the CI gate for the same reason.
- **`src/api/retry.rs` may be modified in the working tree by someone else** (shared checkout). Never stage it, never revert it.
- **`templates/gitlab-ci.yml` is embedded in the binary** via `include_str!` (`src/cli/init.rs:919`). Edit the template, never a copy.
- **The template's committed `RDC_VERSION` must stay a pinned tag** naming a real, current release. The deploy job it scaffolds runs `rdc sync --allow-deletes --yes` unattended.
- **Line 52 of `templates/gitlab-ci.yml`** — `# the pre-0.7 unversioned names, so this pipeline works against old and new` — is a *historical* statement about pre-0.7 releases. It must **never** be bumped.
- Bump rule, on a 0.x crate: any `feat`, any `type!:`, or any `BREAKING CHANGE:` → **minor**; otherwise **patch**.
- Ship-path rule: `src/`, `templates/`, `desktop/`, root `Cargo.toml`, root `Cargo.lock`.
- Every version-bump edit must change **exactly one line** per file, asserted before the write lands.

## Verified baseline (re-confirm if the tree has moved)

| Fact | Value |
| --- | --- |
| current crate version | `0.7.0` (`Cargo.toml:3`) |
| `^version = "` in `Cargo.toml` | exactly 1 |
| `^name = "rdc"$` in `Cargo.lock` / `desktop/rust/Cargo.lock` | exactly 1 each (lines 1596 / 1636; version on the next line) |
| `^  RDC_VERSION: "` in `templates/gitlab-ci.yml` | exactly 1 (line 48) |
| `github.ref_name` in `release.yaml` | lines 50, 79, 130, 198 |
| `actions/checkout@v7` in `release.yaml` | lines 28 (this repo), 90 (this repo), **121 (the `mrtnzlml/homebrew-tap` repo — must NOT get a `ref:`)** |
| `cargo clippy --all-targets --locked -- -D warnings` | **clean** as of 2026-08-26 |
| `cargo test --locked --lib cli::gitlab_ci` | 16 tests, passes, ~0.01s |
| `tests/live.rs` | `#[ignore]` by default — a plain `cargo test` never hits the network |
| `actionlint 1.7.12` on `.github/workflows/` | clean |
| GitHub expression semantics | *"If you attempt to dereference a nonexistent property, it will evaluate to an empty string."* → `inputs.tag \|\| github.ref_name` is safe on the tag-push path |

---

### Task 1: Make the template's version literal unique, and pin it with a test

Today the version appears on seven lines of `templates/gitlab-ci.yml`. A mechanical bumper cannot tell the pin from the prose, and one of those lines is a historical statement that must never move. Rewrite the comments so the pin is the only line carrying a full version, then add the test that proves template and crate never drift apart.

**Files:**
- Modify: `templates/gitlab-ci.yml:43-44`, `:51-52`, `:68-69`
- Test: `src/cli/gitlab_ci.rs` (add to the existing `mod tests`, beside `committed_template_regions_match_the_renderer`)

**Interfaces:**
- Consumes: nothing.
- Produces: the invariant `GITLAB_CI_TEMPLATE`'s `RDC_VERSION` == `v` + `CARGO_PKG_VERSION`, and the guarantee that `^  RDC_VERSION: "` is the single line Task 3's bumper must rewrite.

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `src/cli/gitlab_ci.rs`, after `committed_template_regions_match_the_renderer`:

```rust
    /// The template ships a pinned tag, and CLAUDE.md requires that pin to name
    /// a real, current release -- the deploy job it scaffolds runs
    /// `rdc sync --allow-deletes --yes` unattended. Both files move in the same
    /// release commit, so this holds on every committed state; it fails only on
    /// the intermediate CI state where `Cargo.toml` has been bumped and the
    /// template has not, which is exactly the half-finished bump it exists to
    /// catch.
    #[test]
    fn committed_template_pins_this_crates_version() {
        let template = crate::cli::init::GITLAB_CI_TEMPLATE;
        let pin = template
            .lines()
            .find_map(|line| line.trim().strip_prefix("RDC_VERSION: "))
            .expect("templates/gitlab-ci.yml has an RDC_VERSION line")
            .trim_matches('"');
        assert_eq!(
            pin,
            format!("v{}", env!("CARGO_PKG_VERSION")),
            "templates/gitlab-ci.yml's RDC_VERSION pin and Cargo.toml's version have drifted"
        );
    }

    /// The bumper in `.github/scripts/bump-version.sh` rewrites the ONE line
    /// matching `^  RDC_VERSION: "`. If a second line ever carries the same
    /// prefix, or the pin loses its two-space indent, the bumper's 1-line-diff
    /// assertion turns a release red for a reason nobody will guess.
    #[test]
    fn the_version_pin_is_the_only_line_a_bumper_could_match() {
        let template = crate::cli::init::GITLAB_CI_TEMPLATE;
        let pins = template.lines().filter(|l| l.starts_with("  RDC_VERSION: \"")).count();
        assert_eq!(pins, 1, "expected exactly one bumpable RDC_VERSION line");
        let literals = template.matches(env!("CARGO_PKG_VERSION")).count();
        assert_eq!(
            literals, 1,
            "the full version literal must appear exactly once, so comment prose \
             can never be silently falsified by a version bump"
        );
    }
```

- [ ] **Step 2: Run the tests to verify the second one fails**

Run: `cargo test --locked --lib cli::gitlab_ci`
Expected: `committed_template_pins_this_crates_version` PASSES (the pin is already `v0.7.0`), and `the_version_pin_is_the_only_line_a_bumper_could_match` FAILS with `expected the full version literal ... left: 4, right: 1` — the literal `0.7.0` is on lines 43, 48, 51 and 68.

- [ ] **Step 3: Rewrite the template comments**

In `templates/gitlab-ci.yml`, replace lines 43-44:

```yaml
  # Exact tag (v0.7.0) is reproducible and is what this file ships with.
  # "v0.7" tracks the newest patch in that line; "latest" tracks the newest
```

with:

```yaml
  # An exact tag is reproducible and is what this file ships with. A series
  # prefix tracks the newest patch in that line; "latest" tracks the newest
```

Replace lines 50-53:

```yaml
  # Matched against the END of the asset name, because release assets carry the
  # version (rdc-0.7.0-x86_64-unknown-linux-gnu.tar.gz). A suffix also matches
  # the pre-0.7 unversioned names, so this pipeline works against old and new
  # releases alike. Swap the platform here to install a different build.
```

with:

```yaml
  # Matched against the END of the asset name, because release assets carry the
  # version (rdc-<version>-x86_64-unknown-linux-gnu.tar.gz). A suffix also
  # matches the pre-0.7 unversioned names, so this pipeline works against old
  # and new releases alike. Swap the platform here to install a different build.
```

Note `pre-0.7` survives verbatim — it is a fact about releases that already happened, and `0.7` is not the full literal `0.7.0`, so it never matches the bumper or the uniqueness test.

Replace lines 68-69:

```yaml
  #   v0.7.0  exact tag           reproducible; what this file ships with
  #   v0.7    newest patch in 0.7 picks up fixes, never a new feature
```

with:

```yaml
  #   vX.Y.Z  exact tag           reproducible; what this file ships with
  #   vX.Y    newest patch in X.Y picks up fixes, never a new feature
```

Leave `RDC_VERSION: "v0.7.0"` (line 48) exactly as it is. Do not touch anything between the `# >>> rdc:…` / `# <<< rdc:…` markers.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --locked --lib cli::gitlab_ci`
Expected: PASS, 18 tests. `committed_template_regions_match_the_renderer` must still pass — `regions::splice` compares only the marker regions, and every line edited here lives outside them.

Also confirm the literal really is unique:

Run: `grep -c '0\.7\.0' templates/gitlab-ci.yml`
Expected: `1`

- [ ] **Step 5: Confirm the scaffolder still round-trips**

Run: `cargo test --locked --test cli_init`
Expected: PASS. `rdc init` writes this template byte-for-byte on a fresh project; a stray edit inside a region would show up here.

- [ ] **Step 6: Commit**

```bash
git add templates/gitlab-ci.yml src/cli/gitlab_ci.rs
git commit -m "$(cat <<'MSG'
ci: make the CI template's version literal unique and pin it to the crate

The version appeared on seven lines of templates/gitlab-ci.yml, one of them
a historical statement about pre-0.7 releases that a mechanical bumper would
silently falsify. Reword the prose so the full literal appears exactly once,
on the RDC_VERSION line, and add two tests: the pin must equal
CARGO_PKG_VERSION, and it must be the only line a bumper could match.

Nothing tested this before, so a half-finished bump shipped.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 2: `.github/scripts/release-plan.sh` — decide whether, and how big

A pure decision script: two input files in, `key=value` lines out. No git, no network, no working-tree writes — so the release decision is testable without a repository.

**Files:**
- Create: `.github/scripts/release-plan.sh`
- Test: `tests/release_plan.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `release-plan.sh <current-version> <changed-paths-file> <log-file>`, printing either `release=false` or four lines `release=true` / `bump=<minor|patch>` / `version=<x.y.z>` / `tag=v<x.y.z>`. Task 6 pipes that straight into `$GITHUB_OUTPUT` and reads `steps.plan.outputs.{release,version,tag}`.

- [ ] **Step 1: Write the failing test**

Create `tests/release_plan.rs`:

```rust
//! Drives `.github/scripts/release-plan.sh` with synthetic `git diff` / `git log`
//! output. The script is pure -- two input files in, key=value lines out -- so
//! the weekly release decision is testable without a repository, a tag, or a
//! network.
#![cfg(unix)]

use std::fs;
use std::process::Command;

use tempfile::TempDir;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.github/scripts/release-plan.sh");

/// Runs the script over the given changed-paths and commit-log text.
/// Returns `(exited zero, stdout)`.
fn plan(version: &str, paths: &str, log: &str) -> (bool, String) {
    let dir = TempDir::new().unwrap();
    let paths_file = dir.path().join("paths");
    let log_file = dir.path().join("log");
    fs::write(&paths_file, paths).unwrap();
    fs::write(&log_file, log).unwrap();
    let out = Command::new("sh")
        .arg(SCRIPT)
        .arg(version)
        .arg(&paths_file)
        .arg(&log_file)
        .output()
        .expect("sh is available");
    (out.status.success(), String::from_utf8(out.stdout).unwrap())
}

#[test]
fn a_docs_only_week_releases_nothing() {
    let (ok, out) = plan(
        "0.7.0",
        "docs/superpowers/specs/a-design.md\nREADME.md\ntests/cli_sync.rs\n",
        "docs: explain the thing\ntest: cover the thing\n",
    );
    assert!(ok);
    assert_eq!(out, "release=false\n");
}

#[test]
fn an_empty_range_releases_nothing() {
    let (ok, out) = plan("0.7.0", "", "");
    assert!(ok);
    assert_eq!(out, "release=false\n");
}

#[test]
fn a_feat_touching_src_cuts_a_minor() {
    let (ok, out) = plan("0.7.0", "src/cli/sync.rs\n", "feat(sync): add a flag\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=minor\nversion=0.8.0\ntag=v0.8.0\n");
}

#[test]
fn a_fix_only_week_cuts_a_patch() {
    let (ok, out) = plan(
        "0.7.0",
        "src/api/mod.rs\n",
        "fix(api): retry on 502\nchore: tidy an import\ndocs: note the retry\n",
    );
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.7.1\ntag=v0.7.1\n");
}

#[test]
fn a_desktop_only_week_still_ships() {
    // The desktop app is a release asset. Without `desktop/` in the ship gate
    // these weeks read as "nothing to release" and desktop fixes never ship.
    let (ok, out) = plan("0.7.0", "desktop/lib/main.dart\n", "fix(desktop): sidebar focus\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.7.1\ntag=v0.7.1\n");
}

#[test]
fn the_desktop_lockfile_ships() {
    let (ok, out) = plan("0.7.0", "desktop/rust/Cargo.lock\n", "chore(deps): bump a crate\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.7.1\ntag=v0.7.1\n");
}

#[test]
fn the_embedded_template_ships() {
    let (ok, out) = plan("0.7.0", "templates/gitlab-ci.yml\n", "fix(ci): correct the pin\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.7.1\ntag=v0.7.1\n");
}

#[test]
fn a_breaking_marker_in_a_body_forces_a_minor() {
    // On a 0.x crate a breaking change is a minor bump, so `feat`, `feat!` and
    // `BREAKING CHANGE:` all collapse to the same rule.
    let (ok, out) = plan(
        "0.7.0",
        "src/lib.rs\n",
        "fix(codec): drop the legacy field\n\nBREAKING CHANGE: snapshots written by 0.6 no longer load.\n",
    );
    assert!(ok);
    assert_eq!(out, "release=true\nbump=minor\nversion=0.8.0\ntag=v0.8.0\n");
}

#[test]
fn a_bang_suffixed_type_forces_a_minor() {
    let (ok, out) = plan("0.7.0", "src/lib.rs\n", "fix(cli)!: rename --force\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=minor\nversion=0.8.0\ntag=v0.8.0\n");
}

#[test]
fn a_nested_cargo_manifest_does_not_trip_the_gate() {
    // Cargo.toml / Cargo.lock are anchored at the repo root: a fixture manifest
    // under tests/ ships nothing.
    let (ok, out) = plan(
        "0.7.0",
        "tests/fixtures/Cargo.toml\ndocs/notes/x.md\n",
        "test: add a fixture\n",
    );
    assert!(ok);
    assert_eq!(out, "release=false\n");
}

#[test]
fn a_minor_bump_resets_the_patch_component() {
    let (ok, out) = plan("0.7.3", "src/lib.rs\n", "feat: something new\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=minor\nversion=0.8.0\ntag=v0.8.0\n");
}

#[test]
fn a_non_semver_current_version_is_an_error() {
    let (ok, out) = plan("0.7", "src/lib.rs\n", "feat: x\n");
    assert!(!ok, "an unparseable version must fail loudly, not guess");
    assert_eq!(out, "");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --test release_plan`
Expected: every test FAILS — `sh` reports `.github/scripts/release-plan.sh: No such file or directory`.

- [ ] **Step 3: Write the script**

Create `.github/scripts/release-plan.sh`:

```sh
#!/bin/sh
# Decide whether the commits since the last release are worth releasing, and at
# what level. Pure by construction: it reads two files, writes key=value lines
# to stdout, and touches neither git nor the working tree -- which is what makes
# `tests/release_plan.rs` able to drive it with synthetic input.
#
# Usage: release-plan.sh <current-version> <changed-paths-file> <log-file>
#   current-version     e.g. 0.7.0, read out of Cargo.toml by the caller
#   changed-paths-file  `git diff --name-only "$prev"..HEAD`, one path per line
#   log-file            `git log --format='%s%n%b' "$prev"..HEAD`
#
# Output, ready for `>> "$GITHUB_OUTPUT"`:
#   release=false                                  nothing shippable landed
#   release=true / bump=… / version=… / tag=…      cut this release
set -eu

usage='usage: release-plan.sh <current-version> <changed-paths-file> <log-file>'
current=${1:?$usage}
paths=${2:?$usage}
log=${3:?$usage}

[ -f "$paths" ] || { echo "release-plan: no such file: $paths" >&2; exit 1; }
[ -f "$log" ] || { echo "release-plan: no such file: $log" >&2; exit 1; }

# Refuse to guess at a version we cannot parse -- silently mis-bumping is worse
# than a red run.
printf '%s\n' "$current" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' || {
  echo "release-plan: '$current' is not an x.y.z version" >&2
  exit 1
}

# Ship gate. These are the paths that end up inside a release asset: the CLI
# source, the template embedded in the binary, the desktop app (which ships its
# own .dmg/.tar.gz/.zip), and the two files that pin the root dependency graph.
# Docs, tests and CI config deliberately do NOT ship, so a docs-only week costs
# nothing and releases nothing.
#
# Paths answer *whether* the artifact changed and cannot be fooled by a
# mislabelled commit; the commit types below answer only *how big* the change
# was. The two questions deliberately use different signals.
if ! grep -qE '^(src|templates|desktop)/|^Cargo\.(toml|lock)$' "$paths"; then
  echo "release=false"
  exit 0
fi

# Bump level. On a 0.x crate a breaking change is a minor bump, so `feat`,
# `feat!`, any other `type!:` and a `BREAKING CHANGE:` trailer all collapse into
# the same rule.
if grep -qE '^feat(\([^)]*\))?!?:|^[a-z]+(\([^)]*\))?!:|^BREAKING CHANGE:' "$log"; then
  bump=minor
else
  bump=patch
fi

major=${current%%.*}
rest=${current#*.}
minor=${rest%%.*}
patch=${rest#*.}

if [ "$bump" = minor ]; then
  minor=$((minor + 1))
  patch=0
else
  patch=$((patch + 1))
fi

printf 'release=true\nbump=%s\nversion=%s.%s.%s\ntag=v%s.%s.%s\n' \
  "$bump" "$major" "$minor" "$patch" "$major" "$minor" "$patch"
```

Then make it executable:

```bash
chmod +x .github/scripts/release-plan.sh
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --test release_plan`
Expected: PASS, 12 tests.

- [ ] **Step 5: Check it against the real repository**

Run:

```bash
git diff --name-only v0.7.0..HEAD > /tmp/rdc-paths
git log --format='%s%n%b' v0.7.0..HEAD > /tmp/rdc-log
.github/scripts/release-plan.sh 0.7.0 /tmp/rdc-paths /tmp/rdc-log
```

Expected (as of 2026-08-26, with 24 commits and 8 `feat:` subjects behind `v0.7.0`):

```
release=true
bump=minor
version=0.8.0
tag=v0.8.0
```

- [ ] **Step 6: Commit**

```bash
git add .github/scripts/release-plan.sh tests/release_plan.rs
git commit -m "$(cat <<'MSG'
ci: add release-plan.sh, the "is this week worth releasing" decision

Pure script: changed-paths file and commit-log file in, key=value lines out.
Ship gate is path-based (src/, templates/, desktop/, root Cargo.toml/lock) so
a mislabelled commit cannot hide a real change; bump level is derived from
commit types, so fix-only weeks stay patches and the vX.Y series pin the CI
template documents keeps its promise.

desktop/ is in the gate because the desktop app ships as a release asset --
49 of the last 200 commits touch only desktop/.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 3: `.github/scripts/bump-version.sh` — rewrite the four files, one line each

A release commit touches exactly four files. Every edit here is asserted to change **exactly one line**, and the guard runs before a byte is written — a silent multi-line rewrite of a lockfile is how you ship a release whose dependency graph nobody reviewed.

**Files:**
- Create: `.github/scripts/bump-version.sh`
- Test: `tests/bump_version.rs`

**Interfaces:**
- Consumes: Task 1's guarantee that `^  RDC_VERSION: "` matches exactly one line of `templates/gitlab-ci.yml`.
- Produces: `bump-version.sh <new-version> [repo-root]`, exiting non-zero and leaving no `.tmp` files behind on any surprise. Task 6 calls it with `steps.plan.outputs.version`.

- [ ] **Step 1: Write the failing test**

Create `tests/bump_version.rs`:

```rust
//! Drives `.github/scripts/bump-version.sh` over a synthetic four-file tree.
//! The script refuses any edit that is not exactly one line, which is the guard
//! that keeps an unattended release from quietly rewriting a lockfile.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.github/scripts/bump-version.sh");

const CARGO_TOML: &str = r#"[package]
name = "rdc"
version = "0.7.0"
edition.workspace = true

[dependencies]
anyhow = "1"

[workspace.package]
edition = "2024"
"#;

const ROOT_LOCK: &str = r#"version = 4

[[package]]
name = "anyhow"
version = "1.0.100"

[[package]]
name = "rdc"
version = "0.7.0"
dependencies = [
 "anyhow",
]
"#;

const DESKTOP_LOCK: &str = r#"version = 4

[[package]]
name = "rdc"
version = "0.7.0"
dependencies = [
 "anyhow",
]

[[package]]
name = "rdc_bridge"
version = "0.1.0"
"#;

const TEMPLATE: &str = r#"variables:
  # An exact tag is reproducible and is what this file ships with.
  RDC_VERSION: "v0.7.0"
  RDC_REPO: "mrtnzlml/rdc"
  # A suffix also matches the pre-0.7 unversioned names.
  RDC_ASSET_SUFFIX: "-x86_64-unknown-linux-gnu.tar.gz"
"#;

fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("Cargo.toml"), CARGO_TOML).unwrap();
    fs::write(dir.path().join("Cargo.lock"), ROOT_LOCK).unwrap();
    fs::create_dir_all(dir.path().join("desktop/rust")).unwrap();
    fs::write(dir.path().join("desktop/rust/Cargo.lock"), DESKTOP_LOCK).unwrap();
    fs::create_dir_all(dir.path().join("templates")).unwrap();
    fs::write(dir.path().join("templates/gitlab-ci.yml"), TEMPLATE).unwrap();
    dir
}

fn bump(root: &Path, version: &str) -> Output {
    Command::new("sh")
        .arg(SCRIPT)
        .arg(version)
        .arg(root)
        .output()
        .expect("sh is available")
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn bumps_all_four_files() {
    let t = tree();
    let out = bump(t.path(), "0.8.0");
    assert!(out.status.success(), "{}", stderr(&out));

    assert!(read(t.path(), "Cargo.toml").contains("\nversion = \"0.8.0\"\n"));
    assert!(read(t.path(), "Cargo.lock").contains("name = \"rdc\"\nversion = \"0.8.0\"\n"));
    assert!(read(t.path(), "desktop/rust/Cargo.lock").contains("name = \"rdc\"\nversion = \"0.8.0\"\n"));
    assert!(read(t.path(), "templates/gitlab-ci.yml").contains("  RDC_VERSION: \"v0.8.0\"\n"));
}

#[test]
fn leaves_every_other_version_line_alone() {
    let t = tree();
    assert!(bump(t.path(), "0.8.0").status.success());

    // A dependency's pin, the lockfile format version, the sibling crate's own
    // version and the historical "pre-0.7" comment must all survive untouched.
    let root_lock = read(t.path(), "Cargo.lock");
    assert!(root_lock.starts_with("version = 4\n"));
    assert!(root_lock.contains("name = \"anyhow\"\nversion = \"1.0.100\""));
    assert!(read(t.path(), "desktop/rust/Cargo.lock").contains("name = \"rdc_bridge\"\nversion = \"0.1.0\""));
    assert!(read(t.path(), "templates/gitlab-ci.yml").contains("pre-0.7 unversioned names"));
    assert!(read(t.path(), "Cargo.toml").contains("anyhow = \"1\""));
}

#[test]
fn refuses_a_tree_where_the_crate_version_is_ambiguous() {
    let t = tree();
    // Two `version = "..."` lines at column 0 -- the script cannot know which is
    // the crate's, so it must refuse rather than pick one.
    fs::write(
        t.path().join("Cargo.toml"),
        format!("{CARGO_TOML}\n[some-other-table]\nversion = \"9.9.9\"\n"),
    )
    .unwrap();

    let out = bump(t.path(), "0.8.0");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("expected 1 line matching"), "{}", stderr(&out));
    // Cargo.toml is edited first, so nothing else was written either.
    assert!(read(t.path(), "Cargo.lock").contains("version = \"0.7.0\""));
}

#[test]
fn refuses_a_missing_file() {
    let t = tree();
    fs::remove_file(t.path().join("desktop/rust/Cargo.lock")).unwrap();
    let out = bump(t.path(), "0.8.0");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("missing"), "{}", stderr(&out));
}

#[test]
fn refuses_a_non_semver_version() {
    let t = tree();
    // A leading "v" is the tag's business, not the crate version's.
    let out = bump(t.path(), "v0.8.0");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("is not an x.y.z version"), "{}", stderr(&out));
    assert!(read(t.path(), "Cargo.toml").contains("version = \"0.7.0\""));
}

#[test]
fn refuses_a_tree_already_at_the_target_version() {
    // A no-op edit means the tree was not in the state the caller believed it
    // was, which is worth a red run rather than a silent success.
    let t = tree();
    let out = bump(t.path(), "0.7.0");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("expected a 1-line change, got 0"), "{}", stderr(&out));
}

#[test]
fn leaves_no_temp_files_behind() {
    let t = tree();
    assert!(bump(t.path(), "0.8.0").status.success());
    for rel in ["Cargo.toml.tmp", "Cargo.lock.tmp", "desktop/rust/Cargo.lock.tmp", "templates/gitlab-ci.yml.tmp"] {
        assert!(!t.path().join(rel).exists(), "{rel} was left behind");
    }
}

#[test]
fn bumps_the_real_repository_tree_in_a_copy() {
    // Guards the four real files against a rename or a reshuffle: copy them out
    // of the checkout and bump the copy. Never touches the working tree.
    let t = TempDir::new().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::create_dir_all(t.path().join("desktop/rust")).unwrap();
    fs::create_dir_all(t.path().join("templates")).unwrap();
    for rel in ["Cargo.toml", "Cargo.lock", "desktop/rust/Cargo.lock", "templates/gitlab-ci.yml"] {
        fs::copy(repo.join(rel), t.path().join(rel)).unwrap();
    }

    let current = env!("CARGO_PKG_VERSION");
    let out = bump(t.path(), "99.99.99");
    assert!(out.status.success(), "{}", stderr(&out));

    assert!(read(t.path(), "Cargo.toml").contains("\nversion = \"99.99.99\"\n"));
    assert!(read(t.path(), "Cargo.lock").contains("name = \"rdc\"\nversion = \"99.99.99\"\n"));
    assert!(read(t.path(), "desktop/rust/Cargo.lock").contains("name = \"rdc\"\nversion = \"99.99.99\"\n"));
    assert!(read(t.path(), "templates/gitlab-ci.yml").contains("  RDC_VERSION: \"v99.99.99\"\n"));
    // And the old version is gone from every one of them.
    for rel in ["Cargo.toml", "templates/gitlab-ci.yml"] {
        assert!(!read(t.path(), rel).contains(current), "{rel} still names {current}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --test bump_version`
Expected: every test FAILS — `sh` reports `.github/scripts/bump-version.sh: No such file or directory`.

- [ ] **Step 3: Write the script**

Create `.github/scripts/bump-version.sh`:

```sh
#!/bin/sh
# Rewrite the crate version in the four files a release commit touches:
# Cargo.toml, Cargo.lock, desktop/rust/Cargo.lock and templates/gitlab-ci.yml.
#
# Every edit is guarded twice -- the target pattern must match exactly one line
# BEFORE the edit, and the result must differ on exactly one line AFTER it --
# because an unattended release that quietly rewrites a lockfile is a release
# whose dependency graph nobody reviewed.
#
# Usage: bump-version.sh <new-version> [repo-root]
set -eu

new=${1:?'usage: bump-version.sh <new-version> [repo-root]'}
root=${2:-.}

printf '%s\n' "$new" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' || {
  echo "bump-version: '$new' is not an x.y.z version (no leading v)" >&2
  exit 1
}

# Apply an awk program to a file, refusing anything but a clean 1-line change.
#   edit <file> <guard-regex> <awk-program>
# `diff | grep -c '^>'` counts the lines present in the new file and absent from
# the old; for an in-place substitution that is exactly 1, for a no-op it is 0.
edit() {
  file=$1
  guard=$2
  program=$3

  [ -f "$file" ] || { echo "bump-version: missing $file" >&2; exit 1; }

  matches=$(grep -cE "$guard" "$file" || true)
  if [ "$matches" != 1 ]; then
    echo "bump-version: $file: expected 1 line matching /$guard/, found $matches" >&2
    exit 1
  fi

  awk -v new="$new" "$program" "$file" > "$file.tmp"
  changed=$(diff "$file" "$file.tmp" | grep -c '^>' || true)
  if [ "$changed" != 1 ]; then
    rm -f "$file.tmp"
    echo "bump-version: $file: expected a 1-line change, got $changed" >&2
    exit 1
  fi

  mv "$file.tmp" "$file"
  echo "bumped $file"
}

# Cargo.toml: the single `version = "…"` line at column 0, under [package].
# [workspace.package] carries only `edition` and `license`, so there is exactly
# one -- and the guard above proves it rather than assuming it.
edit "$root/Cargo.toml" '^version = "' '
  { if (!done && $0 ~ /^version = "/) { $0 = "version = \"" new "\""; done = 1 } print }
'

# The two lockfiles: the `version` key inside the `[[package]] name = "rdc"`
# block, identified by the line before it. Preferred over `cargo metadata`,
# which needs a warm registry cache or a network fetch and can rewrite unrelated
# entries when the lock is stale. `^name = "rdc"$` is anchored, so the sibling
# crate `rdc_bridge` is not a match.
lock_program='
  {
    if (!done && prev == "name = \"rdc\"" && $0 ~ /^version = "/) {
      $0 = "version = \"" new "\""
      done = 1
    }
    print
    prev = $0
  }
'
edit "$root/Cargo.lock" '^name = "rdc"$' "$lock_program"
edit "$root/desktop/rust/Cargo.lock" '^name = "rdc"$' "$lock_program"

# templates/gitlab-ci.yml: the RDC_VERSION pin, which CLAUDE.md requires to name
# a real, current tag -- the deploy job it scaffolds runs
# `rdc sync --allow-deletes --yes` unattended. This is the one line in the file
# carrying the full version literal; `committed_template_pins_this_crates_version`
# in src/cli/gitlab_ci.rs keeps it that way.
edit "$root/templates/gitlab-ci.yml" '^  RDC_VERSION: "' '
  { if (!done && $0 ~ /^  RDC_VERSION: "/) { $0 = "  RDC_VERSION: \"v" new "\""; done = 1 } print }
'
```

Then:

```bash
chmod +x .github/scripts/bump-version.sh
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --test bump_version`
Expected: PASS, 8 tests.

- [ ] **Step 5: Confirm the working tree is untouched**

Run: `git status --porcelain -- Cargo.toml Cargo.lock desktop/rust/Cargo.lock templates/gitlab-ci.yml`
Expected: only `templates/gitlab-ci.yml` from Task 1, already committed — i.e. **no output**. `bumps_the_real_repository_tree_in_a_copy` copies before it edits, so a dirty tree here means a real bug.

- [ ] **Step 6: Commit**

```bash
git add .github/scripts/bump-version.sh tests/bump_version.rs
git commit -m "$(cat <<'MSG'
ci: add bump-version.sh, a 1-line-diff-asserted version bumper

Rewrites the four files a release commit touches. Each edit is guarded twice:
the target pattern must match exactly one line before the edit, and the result
must differ on exactly one line after it. A surgical lockfile edit is preferred
over `cargo metadata`, which needs a warm registry or a network fetch and can
rewrite unrelated entries when the lock is stale.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 4: Make `release.yaml` callable

A tag pushed with the default `GITHUB_TOKEN` does **not** start `on: push: tags` workflows — GitHub suppresses those events to prevent recursion. So the weekly job cannot tag and walk away; it has to call the release. `workflow_call` is not a trigger, so the restriction never applies. This mirrors how `release.yaml` already consumes `desktop-build.yml`.

**Files:**
- Modify: `.github/workflows/release.yaml:2-9` (the `on:` block), `:28`, `:50`, `:79`, `:90`

**Interfaces:**
- Consumes: nothing.
- Produces: `release.yaml` accepts `workflow_call` with a required string input `tag` (e.g. `v0.8.0`). Task 6 calls it as `uses: ./.github/workflows/release.yaml` with `secrets: inherit`.

- [ ] **Step 1: Add the `workflow_call` trigger**

Replace `.github/workflows/release.yaml` lines 3-6:

```yaml
on:
  push:
    tags:
      - 'v*'
```

with:

```yaml
on:
  push:
    tags:
      - 'v*'
  # Called directly by weekly-release.yaml. A tag pushed with the default
  # GITHUB_TOKEN does NOT start the `push` trigger above -- GitHub suppresses
  # events raised by that token to prevent recursion -- so the scheduled
  # release has to call this workflow rather than rely on the tag it just
  # wrote. `workflow_call` is not a trigger, so the restriction never applies.
  workflow_call:
    inputs:
      tag:
        description: >-
          The release tag to build and publish, e.g. "v0.8.0". Set only by a
          caller; on the tag-push path it is absent and every use below falls
          back to github.ref_name.
        required: true
        type: string

# Serialised against weekly-release.yaml's `prepare` job, which holds the same
# group while it decides and tags. No cancel-in-progress: a release must never
# be killed halfway through publishing.
concurrency:
  group: rdc-release
```

- [ ] **Step 2: Fall back on every `github.ref_name` use**

There are four. GitHub's expression docs are explicit that *"if you attempt to dereference a nonexistent property, it will evaluate to an empty string"*, so on the tag-push path `inputs.tag` is `''` and `||` yields `github.ref_name` — byte-identical behaviour to today.

Replace, at lines 50, 79, 130 and 198 respectively:

```yaml
          TAG: ${{ github.ref_name }}
```
```yaml
      version_label: ${{ github.ref_name }}
```
```yaml
          TAG: ${{ github.ref_name }}
```
```yaml
          TAG: ${{ github.ref_name }}
```

with:

```yaml
          TAG: ${{ inputs.tag || github.ref_name }}
```
```yaml
      version_label: ${{ inputs.tag || github.ref_name }}
```
```yaml
          TAG: ${{ inputs.tag || github.ref_name }}
```
```yaml
          TAG: ${{ inputs.tag || github.ref_name }}
```

- [ ] **Step 3: Pin the two checkouts of *this* repo to the tag**

On the call path the workflow runs against the caller's ref, which is `main` — so without a `ref:` the matrix would build whatever `main` happens to be, not the tag. Line 28 (the build matrix) and line 90 (the publish job) both check out this repo:

```yaml
      - uses: actions/checkout@v7
```

becomes, in **both** places:

```yaml
      - uses: actions/checkout@v7
        with:
          # On the call path the run's ref is the caller's (main); pin to the
          # tag so the matrix builds what the tag names. On the tag-push path
          # github.ref is already refs/tags/vX.Y.Z, so this is a no-op.
          ref: ${{ inputs.tag || github.ref }}
```

**Do not touch the checkout at line 121.** That one clones `mrtnzlml/homebrew-tap`, a different repository; an rdc tag does not exist there and `ref:` would break it.

- [ ] **Step 4: Lint**

Run: `actionlint`
Expected: no output (exit 0). This also type-checks the `inputs` context in every position it is used.

- [ ] **Step 5: Confirm the tag-push path is unchanged**

Run: `git diff .github/workflows/release.yaml`
Expected: the only semantic changes are the added `workflow_call` block, the added `concurrency` block, four `github.ref_name` → `inputs.tag || github.ref_name`, and two added `ref:` checkouts. No job name, `needs:`, `if:`, or artifact path changed.

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/release.yaml
git commit -m "$(cat <<'MSG'
ci: make release.yaml callable via workflow_call

A tag pushed with the default GITHUB_TOKEN does not start `on: push: tags`
workflows, so a scheduled job cannot tag and walk away -- it has to call the
release. Add a workflow_call trigger with a `tag` input, fall back to
github.ref_name in all four places so the manual `git push origin vX.Y.Z`
path is byte-identical, and pin the two checkouts of this repo to the tag so
a called run builds the tag rather than the caller's main.

The tap checkout is deliberately left alone: it clones a different repository.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 5: Fix the Homebrew tap job

The last four release runs all ended `failure`, and on every one of them the only failing job was `Bump Homebrew tap formula`. It fetches release assets from `https://github.com/…/releases/download/<tag>/<asset>` with no credentials; the repo is private and that URL returns **404**. A weekly schedule would turn that into a weekly failure email for a job that has failed identically four releases running — which trains the maintainer to ignore exactly the signal this whole design exists to send.

**Files:**
- Modify: `.github/workflows/release.yaml` — the `homebrew` job's `Regenerate formula` step (lines ~127-138 as of Task 4)

**Interfaces:**
- Consumes: Task 4's `TAG` fallback.
- Produces: nothing downstream.

- [ ] **Step 1: Give the job a token for this repo**

The job currently has only `HOMEBREW_TAP_TOKEN`, which addresses the *tap* repo. Add the run token for *this* repo to the `Regenerate formula` step's `env:` and switch the download:

Replace:

```yaml
      - name: Regenerate formula
        shell: bash
        env:
          TAG: ${{ inputs.tag || github.ref_name }}
        run: |
          set -euo pipefail
          version="${TAG#v}"
          base="https://github.com/mrtnzlml/rdc/releases/download/${TAG}"
          sha_of() { curl -fsSL "$1" | sha256sum | awk '{ print $1 }'; }
          darwin_arm_sha=$(sha_of "$base/rdc-${version}-aarch64-apple-darwin.tar.gz")
          darwin_x86_sha=$(sha_of "$base/rdc-${version}-x86_64-apple-darwin.tar.gz")
          linux_x86_sha=$(sha_of "$base/rdc-${version}-x86_64-unknown-linux-gnu.tar.gz")
```

with:

```yaml
      - name: Regenerate formula
        shell: bash
        env:
          TAG: ${{ inputs.tag || github.ref_name }}
          # Two different tokens address two different repos and must not be
          # confused: GH_TOKEN reads assets from THIS repo, HOMEBREW_TAP_TOKEN
          # (used by the checkout step above) writes to the tap repo.
          GH_TOKEN: ${{ github.token }}
        run: |
          set -euo pipefail
          version="${TAG#v}"
          base="https://github.com/mrtnzlml/rdc/releases/download/${TAG}"
          # This repo is private, so the releases/download/... browser URL 404s
          # even with a token -- that is what failed the last four releases.
          # `gh release download` resolves the numeric asset id through the API,
          # which is the only way in.
          mkdir -p assets
          gh release download "$TAG" --repo mrtnzlml/rdc --dir assets --clobber \
            --pattern 'rdc-*-aarch64-apple-darwin.tar.gz' \
            --pattern 'rdc-*-x86_64-apple-darwin.tar.gz' \
            --pattern 'rdc-*-x86_64-unknown-linux-gnu.tar.gz'
          sha_of() { sha256sum "assets/$1" | awk '{ print $1 }'; }
          darwin_arm_sha=$(sha_of "rdc-${version}-aarch64-apple-darwin.tar.gz")
          darwin_x86_sha=$(sha_of "rdc-${version}-x86_64-apple-darwin.tar.gz")
          linux_x86_sha=$(sha_of "rdc-${version}-x86_64-unknown-linux-gnu.tar.gz")
```

`base` is still used, unchanged, for the `url` fields written into the formula — see the next step for why that is deliberate.

- [ ] **Step 2: Record honestly what this does not fix**

Immediately above `name: Bump Homebrew tap formula`, extend the existing comment block with:

```yaml
    # NOTE: this makes the JOB green and the formula accurate; it does NOT make
    # `brew install mrtnzlml/tap/rdc` work while this repo is private. Homebrew
    # fetches the `url` fields below with no token, and that URL 404s (verified
    # directly). Fixing the install needs either a public repo or dropping
    # Homebrew -- a separate decision. The urls are left pointing at the browser
    # path because that is the only form Homebrew understands.
```

- [ ] **Step 3: Lint**

Run: `actionlint`
Expected: no output.

- [ ] **Step 4: Verify the download works against the real, existing release**

This is checkable locally with the maintainer's own `gh` auth — it exercises the exact API path the job will use:

```bash
gh release download v0.7.0 --repo mrtnzlml/rdc --dir /tmp/rdc-assets --clobber \
  --pattern 'rdc-*-x86_64-unknown-linux-gnu.tar.gz'
ls -la /tmp/rdc-assets
```

Expected: `rdc-0.7.0-x86_64-unknown-linux-gnu.tar.gz` present and non-empty. Contrast with the unauthenticated browser URL, which returns 404:

```bash
curl -o /dev/null -sw '%{http_code}\n' -L \
  https://github.com/mrtnzlml/rdc/releases/download/v0.7.0/rdc-0.7.0-x86_64-unknown-linux-gnu.tar.gz
```

Expected: `404`.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/release.yaml
git commit -m "$(cat <<'MSG'
ci: download release assets through the API in the tap job

The last four releases all ended `failure`, every time with the tap job as the
only red job: it fetched assets from the releases/download/... browser URL,
which 404s on a private repo even with a token. Switch to `gh release
download`, which resolves the numeric asset id through the API.

This makes the job green and the formula accurate. It does NOT make
`brew install` work while the repo is private -- Homebrew fetches the same
browser URL with no token -- and the comment now says so.

Left unfixed, a weekly schedule turns this into a weekly failure email for a
job that has failed identically four releases running.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 6: `.github/workflows/weekly-release.yaml`

The workflow itself. Order is load-bearing throughout: **decide → verify → bump → re-verify → commit → tag → publish.**

**Files:**
- Create: `.github/workflows/weekly-release.yaml`

**Interfaces:**
- Consumes: `.github/scripts/release-plan.sh` (Task 2), `.github/scripts/bump-version.sh` (Task 3), `release.yaml`'s `workflow_call` + `tag` input (Task 4).
- Produces: a `chore(release): vX.Y.Z` commit on `main` and an annotated tag `vX.Y.Z` with body `rdc vX.Y.Z`, matching every release the repo already has.

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/weekly-release.yaml`:

```yaml
name: Weekly release

# Cuts a release once a week, but only when something shippable landed since the
# last tag. Verifies the tree BEFORE writing a single byte, then bumps, commits,
# tags, and calls release.yaml directly.
#
# It calls release.yaml rather than pushing a tag and walking away because a tag
# pushed with the default GITHUB_TOKEN does not start `on: push: tags`
# workflows. `workflow_call` is not a trigger, so that restriction never
# applies.
#
# The decision and the bump live in .github/scripts/, unit-tested by
# tests/release_plan.rs and tests/bump_version.rs -- neither needs a repository,
# a tag, or a network.

on:
  schedule:
    # Monday morning UTC, deliberately off the top of the hour: GitHub warns
    # that scheduled runs are delayed or dropped under load, and that "high load
    # times include the start of every hour".
    - cron: "17 6 * * 1"
  # So a release can be forced without waiting for Monday.
  workflow_dispatch:

permissions:
  # Push the bump commit and the tag.
  contents: write

jobs:
  prepare:
    name: Gate and tag
    runs-on: ubuntu-latest
    # Shared with release.yaml, so a scheduled run and a manual
    # `git push origin vX.Y.Z` can never interleave. Held only for as long as
    # this job runs; by the time `release` calls release.yaml (which declares
    # the same group) this job has finished and released it, so there is no
    # self-deadlock. No cancel-in-progress -- never kill a release halfway.
    concurrency:
      group: rdc-release
    outputs:
      released: ${{ steps.plan.outputs.release }}
      version: ${{ steps.plan.outputs.version }}
      tag: ${{ steps.plan.outputs.tag }}
    steps:
      - uses: actions/checkout@v7
        with:
          # Every decision below is a range against the previous tag, which a
          # depth-1 checkout cannot see.
          fetch-depth: 0

      - name: Decide whether to release
        id: plan
        run: |
          set -euo pipefail
          prev=$(git describe --tags --abbrev=0 --match 'v*')
          count=$(git rev-list --count "$prev"..HEAD)
          echo "Previous release: $prev ($count commits since)"

          git diff --name-only "$prev"..HEAD > "$RUNNER_TEMP/changed-paths"
          git log --format='%s%n%b' "$prev"..HEAD > "$RUNNER_TEMP/log"

          # Read the version from Cargo.toml rather than from the tag, so a
          # hand-edited version can never be silently reverted.
          current=$(awk '/^version = "/ { gsub(/"/, "", $3); print $3; exit }' Cargo.toml)
          echo "Current version: $current"

          .github/scripts/release-plan.sh \
            "$current" "$RUNNER_TEMP/changed-paths" "$RUNNER_TEMP/log" \
            | tee -a "$GITHUB_OUTPUT"

          echo "COMMIT_COUNT=$count" >> "$GITHUB_ENV"

      - name: Nothing to release
        if: steps.plan.outputs.release != 'true'
        # A quiet week is a success, not a failure. It must not page anyone.
        run: |
          echo "::notice::${COMMIT_COUNT} commit(s) since the last tag, none of them shippable. No release this week."

      # ---- Everything below runs only when there is something to ship. ----

      - name: Install Rust
        if: steps.plan.outputs.release == 'true'
        uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy

      - name: Cache cargo
        if: steps.plan.outputs.release == 'true'
        uses: actions/cache@v6
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: cargo-weekly-${{ hashFiles('Cargo.lock') }}
          restore-keys: cargo-weekly-

      - name: Test
        if: steps.plan.outputs.release == 'true'
        # tests/live.rs is #[ignore] by default, so this never touches the API.
        run: cargo test --locked

      - name: Clippy
        if: steps.plan.outputs.release == 'true'
        # v0.6.0 shipped with six clippy errors on main because the tag went out
        # while clippy was still running.
        #
        # `cargo fmt` is deliberately absent: this tree is not fmt-clean under
        # the local rustfmt and never has been, so adding it would make every
        # week red for a reason unrelated to the release.
        run: cargo clippy --all-targets --locked -- -D warnings

      - name: Bump version
        if: steps.plan.outputs.release == 'true'
        run: .github/scripts/bump-version.sh "${{ steps.plan.outputs.version }}"

      - name: Verify the bump
        if: steps.plan.outputs.release == 'true'
        run: |
          set -euo pipefail
          # --locked fails if the lockfile no longer matches the manifest, so a
          # green release build proves the tag cannot be created against a tree
          # that will not produce a release binary. The profile is lto = "fat",
          # codegen-units = 1, so budget for it: ~$0.06 of Linux time.
          cargo build --release --locked
          # Cargokit does not build the desktop bridge with --locked, so a bad
          # edit to desktop/rust/Cargo.lock would otherwise surface as a dirty
          # tree on some future build instead of as a failure here.
          cargo metadata --locked --format-version 1 \
            --manifest-path desktop/rust/Cargo.toml > /dev/null
          # Re-run AFTER the bump: a pre-bump run passes even when the bumper
          # then updates Cargo.toml and forgets templates/gitlab-ci.yml.
          cargo test --locked --lib cli::gitlab_ci

      - name: Confirm exactly the four expected files changed
        if: steps.plan.outputs.release == 'true'
        run: |
          set -euo pipefail
          expected="Cargo.lock,Cargo.toml,desktop/rust/Cargo.lock,templates/gitlab-ci.yml"
          actual=$(git diff --name-only | sort | paste -sd, -)
          if [ "$actual" != "$expected" ]; then
            echo "::error::the bump changed an unexpected set of files: $actual"
            exit 1
          fi

      - name: Commit and tag
        if: steps.plan.outputs.release == 'true'
        env:
          VERSION: ${{ steps.plan.outputs.version }}
          TAG: ${{ steps.plan.outputs.tag }}
        run: |
          set -euo pipefail
          git config user.name "github-actions[bot]"
          git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
          git add Cargo.toml Cargo.lock desktop/rust/Cargo.lock templates/gitlab-ci.yml
          git commit -m "chore(release): v${VERSION}"
          # Annotated, body `rdc vX.Y.Z` -- the shape every existing tag has.
          git tag -a "$TAG" -m "rdc v${VERSION}"
          # --atomic so a rejected branch push cannot leave a dangling tag.
          #
          # If main moved since the gate ran, this push is REJECTED and the run
          # fails. That is deliberate: rebasing would put commits inside the tag
          # that the gate never tested.
          git push --atomic origin HEAD:main "$TAG"

  release:
    name: Release
    needs: prepare
    if: needs.prepare.outputs.released == 'true'
    # No trigger is involved, so the GITHUB_TOKEN event restriction that stops a
    # pushed tag from starting release.yaml never applies here.
    uses: ./.github/workflows/release.yaml
    # The tap job needs HOMEBREW_TAP_TOKEN.
    secrets: inherit
    with:
      tag: ${{ needs.prepare.outputs.tag }}
```

- [ ] **Step 2: Lint**

Run: `actionlint`
Expected: no output (exit 0). This validates the `uses: ./.github/workflows/release.yaml` call against Task 4's declared `tag` input, catching an input-name typo at author time rather than at 06:17 on a Monday.

- [ ] **Step 3: Verify the "nothing to release" branch is reachable and green**

Simulate a quiet week locally — the same two commands the workflow runs, over a docs-only range:

```bash
printf 'docs/readme.md\n' > /tmp/quiet-paths
printf 'docs: tidy\n' > /tmp/quiet-log
.github/scripts/release-plan.sh 0.7.0 /tmp/quiet-paths /tmp/quiet-log; echo "exit=$?"
```

Expected: `release=false` and `exit=0`.

- [ ] **Step 4: Verify the real-repository decision**

```bash
git diff --name-only v0.7.0..HEAD > /tmp/rdc-paths
git log --format='%s%n%b' v0.7.0..HEAD > /tmp/rdc-log
.github/scripts/release-plan.sh \
  "$(awk '/^version = "/ { gsub(/"/, "", $3); print $3; exit }' Cargo.toml)" \
  /tmp/rdc-paths /tmp/rdc-log
```

Expected: `release=true`, `bump=minor`, `version=0.8.0`, `tag=v0.8.0`.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/weekly-release.yaml
git commit -m "$(cat <<'MSG'
ci: add the weekly release workflow

Runs Monday 06:17 UTC (off the top of the hour, which GitHub names as a
high-load window) and on demand. Decides from the range since the last tag,
gates on `cargo test` + `cargo clippy -D warnings`, bumps the four files,
re-verifies with a --locked release build, then commits, tags, and calls
release.yaml directly.

A quiet week emits a ::notice:: and exits green -- it must not page anyone.
If main moved between the gate and the push, the push is rejected and the run
fails rather than rebasing untested commits into the tag.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

### Task 7: Document the contract in `CLAUDE.md`

`CLAUDE.md` currently tells a reader the `RDC_VERSION` pin is bumped by hand on every release. After Task 6 that is no longer true, and the single-literal property is now load-bearing rather than cosmetic. A stale instruction here is worse than none: it invites someone to "helpfully" restore a version literal into the prose.

**Files:**
- Modify: `CLAUDE.md:49-52`, and add one bullet to the "CI templates" list

**Interfaces:**
- Consumes: Tasks 1-6.
- Produces: nothing.

- [ ] **Step 1: Update the pin bullet**

Replace:

```markdown
- The **committed default must stay a pinned tag** — keep `RDC_VERSION` set to
  the newest release tag, and bump it whenever a new release ships. The deploy
  job runs `rdc sync --allow-deletes --yes` unattended, so a floating default
  would let a new rdc change what a destructive sync does with nobody watching.
```

with:

```markdown
- The **committed default must stay a pinned tag** — `RDC_VERSION` names the
  newest release tag. The deploy job runs `rdc sync --allow-deletes --yes`
  unattended, so a floating default would let a new rdc change what a
  destructive sync does with nobody watching. **You no longer bump this by
  hand**: `.github/workflows/weekly-release.yaml` rewrites it in the same
  commit as `Cargo.toml`, and `committed_template_pins_this_crates_version`
  (`src/cli/gitlab_ci.rs`) fails the build if the two ever drift.
- Because a script rewrites that line, the **full version literal must appear
  exactly once** in `templates/gitlab-ci.yml` — on the `RDC_VERSION` line, at a
  two-space indent. Everything else says `<version>`, `vX.Y.Z` or `vX.Y`.
  `the_version_pin_is_the_only_line_a_bumper_could_match` enforces both halves.
  Note `pre-0.7` in the `RDC_ASSET_SUFFIX` comment is a *historical* statement
  about releases that already shipped and must never be bumped; it survives
  because it is not the full literal.
```

- [ ] **Step 2: Add a bullet describing the release automation**

Append to the same "CI templates" bullet list:

```markdown
- Releases are cut by **`.github/workflows/weekly-release.yaml`** (Monday 06:17
  UTC, plus `workflow_dispatch`), not by hand. It releases only when a commit
  since the last tag touched `src/`, `templates/`, `desktop/`, `Cargo.toml` or
  `Cargo.lock`; the level is derived from commit types (any `feat`, any
  `type!:`, or `BREAKING CHANGE:` → minor, else patch), which is what keeps the
  template's documented `vX.Y` series pin — *"picks up fixes, never a new
  feature"* — honest. The decision and the bump are plain `sh` scripts in
  `.github/scripts/`, unit-tested by `tests/release_plan.rs` and
  `tests/bump_version.rs`. It calls `release.yaml` through `workflow_call`
  rather than pushing a tag, because a tag pushed with the default
  `GITHUB_TOKEN` does not start `on: push: tags` workflows. A manual
  `git push origin vX.Y.Z` still works unchanged.
```

- [ ] **Step 3: Verify no customer data leaked**

Run:

```bash
git diff --cached --stat
grep -rniE 'rossum\.app|/v1/(queues|hooks|schemas)/[0-9]' \
  .github CLAUDE.md docs/superpowers/plans/2026-08-26-automated-weekly-release.md \
  || echo "clean"
```

Expected: `clean`. Every example in this work uses `acme`-class placeholders or the repo's own public identifiers (`mrtnzlml/rdc`, `mrtnzlml/homebrew-tap`).

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "$(cat <<'MSG'
docs: record that CI now owns the CI template's version pin

CLAUDE.md told a reader to bump RDC_VERSION by hand on every release. The
weekly release workflow does that now, in the same commit as Cargo.toml, and
a test fails the build if the two drift. Also record why the full version
literal must appear exactly once in the template -- a script rewrites that
line, and prose carrying a stale version would be silently falsified.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
)"
```

---

## Final verification

- [ ] **Full suite**

Run: `cargo test --locked`
Expected: PASS. Baseline before this work is 1102 lib tests + the integration binaries; this adds 2 lib tests (Task 1) and two new integration binaries with 12 + 8 tests.

- [ ] **Clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: clean (it was clean on 2026-08-26 before this work).

- [ ] **Workflow lint**

Run: `actionlint`
Expected: no output.

- [ ] **Working tree**

Run: `git status --porcelain`
Expected: only `src/api/retry.rs` (someone else's in-flight edit — never staged, never reverted).

- [ ] **Do not push.** Leave the commits on local `main`. The maintainer publishes.

## Post-merge integration checks (require a push — maintainer's call)

These are the two things no local test can prove. Both come straight from the spec's Testing section.

1. **`workflow_dispatch` the weekly workflow once** against real `main` before the first schedule fires. Confirm it either skips cleanly (`::notice::`, green) or produces a release identical in shape to `v0.7.0`'s — same four CLI tarballs, same three desktop assets, and now a **green tap job**.
2. **One manual `git push origin vX.Y.Z`** after Task 4, proving the `github.ref_name` fallback still holds when `inputs.tag` is absent.

If the first dispatch cuts `v0.8.0` and a matrix build then fails, the recovery is: delete the tag, fix, re-dispatch. The post-bump release build makes that unlikely but not impossible, and the spec accepts it — inverting the order would mean rebuilding `release.yaml` around an uncommitted tree.
