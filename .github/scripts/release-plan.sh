#!/bin/sh
# Decide whether the commits since the last release are worth releasing, and at
# what level. Pure by construction: it reads two files, writes key=value lines
# to stdout, and touches neither git nor the working tree -- which is what makes
# `tests/release_plan.rs` able to drive it with synthetic input.
#
# Usage: release-plan.sh <current-version> <changed-paths-file> <log-file> [hours]
#   current-version     e.g. 0.7.0, read out of Cargo.toml by the caller
#   changed-paths-file  `git diff --name-only "$prev"..HEAD`, one path per line
#   log-file            `git log --format='%s%n%b' "$prev"..HEAD`
#   hours               optional, whole hours since the previous release went
#                       out. Empty or absent means "no cooldown"; see below.
#
# Output, ready for `>> "$GITHUB_OUTPUT"`:
#   release=false / reason=...                     nothing to do
#   release=true / bump=... / version=... / tag=... cut this release
set -eu

usage='usage: release-plan.sh <current-version> <changed-paths-file> <log-file> [hours]'
current=${1:?$usage}
paths=${2:?$usage}
log=${3:?$usage}
hours=${4:-}

[ -f "$paths" ] || { echo "release-plan: no such file: $paths" >&2; exit 1; }
[ -f "$log" ] || { echo "release-plan: no such file: $log" >&2; exit 1; }

# Refuse to guess at a version we cannot parse -- silently mis-bumping is worse
# than a red run.
printf '%s\n' "$current" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' || {
  echo "release-plan: '$current' is not an x.y.z version" >&2
  exit 1
}

# Cooldown. weekly-release.yaml fires three times a Monday rather than once,
# because GitHub is explicit that a scheduled run "can be delayed during periods
# of high loads" and that "some queued jobs may be dropped" -- and a single
# weekly tick has no tolerance for that. Only the two retry ticks pass an age;
# the week's first tick and every `workflow_dispatch` pass nothing and are
# gated exactly as they always were. A retry that arrives after the first tick
# already released must stay quiet; one that arrives because the first tick was
# dropped must go through.
#
# Mostly the empty range does that on its own: a retry after a successful
# release sees no commits since the new tag and falls out of the ship gate
# below. The cooldown covers the one case the ship gate cannot see -- shippable
# commits landing between the first tick and a retry, on the same Monday
# morning -- which would otherwise cut a second release the same day.
#
# The window is a day, and measured in hours, because what a retry really needs
# to know is "has this week's release already gone out". The three ticks sit
# within eight hours of each other, so a day cleanly separates this week's
# release from any earlier one. A week-wide window would instead suppress a
# legitimate retry for days after any hand-cut release, which is not
# hypothetical: v0.8.0 was cut by hand on a Thursday, four days before the
# Monday tick that went missing.
if [ -n "$hours" ]; then
  printf '%s\n' "$hours" | grep -qE '^[0-9]+$' || {
    echo "release-plan: '$hours' is not a whole number of hours" >&2
    exit 1
  }
  if [ "$hours" -lt 24 ]; then
    printf 'release=false\nreason=cooldown\n'
    exit 0
  fi
fi

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
  printf 'release=false\nreason=nothing-shippable\n'
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
