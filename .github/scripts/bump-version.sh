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

# Cargo.toml: the single `version = "..."` line at column 0, under [package].
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
