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
#   release=true / bump=... / version=... / tag=... cut this release
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
