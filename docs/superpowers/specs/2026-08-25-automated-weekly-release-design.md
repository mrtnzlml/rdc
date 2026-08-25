# Automated weekly release

## Problem

Every rdc release is hand-cut. The maintainer bumps four files, writes an
annotated tag, pushes, and hopes the suite was green when they last ran it.
Nothing in CI checks the tree before the tag exists, and nothing notices when
shippable work has piled up: 22 commits sat unreleased behind `v0.7.0`
(2026-08-24) at the time of writing, 8 of them `feat:` and 5 `fix:`.

The goal is a scheduled workflow that cuts a release **when, and only when,**
something shippable landed — verified first, unattended after that.

## Verified facts

### GitHub Actions, quoted from the docs

| Question | What the docs say |
| --- | --- |
| Does a tag pushed with the default token start `on: push: tags` workflows? | **No.** *"When you use the repository's `GITHUB_TOKEN` to perform tasks, events triggered by the `GITHUB_TOKEN` will not create a new workflow run, with the following exceptions: `workflow_dispatch` and `repository_dispatch` events always create workflow runs."* |
| Is `schedule` punctual? | No. *"The `schedule` event can be delayed during periods of high loads of GitHub Actions workflow runs. High load times include the start of every hour. If the load is sufficiently high enough, some queued jobs may be dropped."* |
| Which ref do scheduled workflows run on? | *"Scheduled workflows run on the latest commit on the default branch"* and *"will only run on the default branch."* |
| Can we gate publication behind a required reviewer? | **No, not on this account.** *"Users with GitHub Free plans can only configure environments for public repositories."* |

This is the single fact that decides the architecture: a scheduled job cannot
create a tag and expect `release.yaml` to fire.

### This repository, probed

| Probe | Result |
| --- | --- |
| `.github/workflows/` contents | exactly three files: `release.yaml`, `desktop-build.yml`, `desktop-release.yml` |
| `grep -rn "cargo test\|cargo clippy\|cargo fmt" .github/` | **no matches — nothing verifies the tree in CI, ever** |
| `gh api repos/mrtnzlml/rdc` | `private: true`, `default_branch: main` |
| `gh api .../branches/main/protection` | `403 "Upgrade to GitHub Pro or make this repository public"` → the account is **GitHub Free**, and there is no branch protection to work around |
| `gh api .../environments` | `total_count: 0`, and unavailable on Free-private anyway |
| `gh api .../actions/permissions` | `enabled: true`, `allowed_actions: all` |
| tag object types for `v0.5.0`/`v0.6.0`/`v0.7.0` | all `tag` (annotated); `v0.7.0`'s full body is exactly `rdc v0.7.0` |
| `github.ref_name` uses in `release.yaml` | **four**: lines 50 (package), 79 (desktop `version_label`), 130 and 198 (tap job) |
| version literals in `templates/gitlab-ci.yml` | seven, on lines 43, 44, 48, 51, 52, 68, 69 — of which **line 52 (`the pre-0.7 unversioned names`) is a historical statement that must never be bumped** |
| any test pinning the template's `RDC_VERSION` to the crate version | **none** (`grep -rn RDC_VERSION src tests` is empty) |
| `curl -L .../releases/download/v0.7.0/rdc-0.7.0-x86_64-unknown-linux-gnu.tar.gz` unauthenticated | **HTTP 404** |
| last four release runs | all `failure`; on run `32704367553` every job succeeded **except** `Bump Homebrew tap formula` |
| commit-type discipline, last 100 commits | `feat` 30, `fix` 29, `docs` 21, `test` 9, `chore` 5, `refactor` 3, `ci` 2, `perf` 1 — conventional commits are used consistently enough to derive a bump level from |
| paths touched by `docs:`/`test:` commits since `v0.7.0` | only `docs/`, `README.md`, `tests/` — none touched `src/` or `templates/`. (Historically some do: `docs(codec): correct organization module doc` edited `src/`.) |
| commit volume | 164 commits/month across 16 distinct days |
| files a release commit touches | four — `Cargo.toml`, `Cargo.lock`, `desktop/rust/Cargo.lock`, `templates/gitlab-ci.yml` (confirmed against `ea4aa2e`) |
| `desktop/pubspec.yaml` version | `1.0.0+1`, never bumped by a release — the desktop version is decoupled, `release.yaml` only passes the tag as an artifact filename label |

### Cost, measured from run `32704367553` (the real v0.7.0 release)

Per-job wall time, rounded up to the whole minute:

| Runner | Jobs | Job-minutes | Included-minutes (10×/2×/1×) | List rate |
| --- | --- | --- | --- | --- |
| macOS | CLI arm64 (5), CLI x86 (3), desktop (11) | 19 | 190 | $1.18 @ $0.062/min |
| Windows | CLI (9), desktop (17) | 26 | 52 | $0.26 @ $0.010/min |
| Linux | CLI (5), desktop (7), publish (1), tap (1) | 14 | 14 | $0.08 @ $0.006/min |
| **total** | | **59** | **256** | **$1.52** |

GitHub Free includes **2,000 minutes/month** for private repositories. So:

- weekly ≈ 1,100 included-minutes/month for the releases themselves, plus a
  ~20-minute Linux gate every week whether or not it ships → **≈ 1,190/month,
  about 60% of the allowance**, and less on weeks that skip
- daily would have been ≈ 7,700 ≈ **3.8× the allowance**

Two caveats recorded honestly. First, the per-minute rates above are quoted from
the billing docs; the 10×/2×/1× figures are GitHub's standard multiplier table,
and the ratio implied by the current rates is slightly different for Windows
(1.67×), so the included-minute column is approximate while the dollar column is
not. Second, **every run's `billable` field reports `0 ms`** — for all four
releases probed and the desktop probe run alike — so these numbers are computed
from wall time, not read from GitHub's meter. They corroborate the previously
measured "+$0.83/release" for the desktop jobs (recomputed here as $0.89).

## Decisions

| Question | Decision |
| --- | --- |
| Cadence | **Weekly**, and only when the week produced something shippable |
| Bump level | **Derived from commit types**: any `feat:` → minor, otherwise patch |
| Verification | **A gate inside the weekly job** — test, clippy, release build — not a separate push-CI workflow |
| Human checkpoint | **None.** Gate green → bump, tag, build, publish |
| "Worth releasing" | **Path-based**: a commit since the last tag touched `src/`, `templates/`, `Cargo.toml`, or `Cargo.lock` |
| Failing tap job | **Fix it** with the job's own `GITHUB_TOKEN` |
| Plumbing | **Make `release.yaml` callable** and call it; no PAT |

The bump rule and the ship rule deliberately use different signals. Paths answer
*whether* the shipped artifact changed and cannot be fooled by a mislabelled
commit; types answer *how big* the change was and match the discipline already
visible in the log. A `docs:` commit that edits a `src/` doc comment will
occasionally cut a no-op patch release — a $1.52 false positive, accepted in
exchange for never silently withholding a real fix.

Minor-per-week was rejected because `templates/gitlab-ci.yml` documents `v0.7`
as *"newest patch in that line; picks up fixes, never a new feature"*. Under a
weekly minor, every series pin a project writes freezes the day it is written
and never picks up another fix. Deriving the level keeps that promise true.

## Design

### 1. `.github/workflows/weekly-release.yaml`

```yaml
on:
  schedule:
    - cron: "17 6 * * 1"   # Monday, off the top of the hour
  workflow_dispatch:
```

Off-the-hour because the docs warn that high-load times "include the start of
every hour". `workflow_dispatch` so a release can be forced without waiting.
A `concurrency` group shared with `release.yaml` so a scheduled run and a manual
`git push origin vX.Y.Z` can never interleave.

`actions/checkout` **must** set `fetch-depth: 0`. The default depth-1 checkout
cannot see the previous tag, and every decision below is a range against it.

Permissions: `contents: write` (push the bump commit and tag).

### 2. Decide

```
prev=$(git describe --tags --abbrev=0 --match 'v*')
```

**Ship gate.** `git diff --name-only "$prev"..HEAD -- src templates Cargo.toml Cargo.lock`.
Empty → emit a `::notice::` naming the commit count and exit **green**. A quiet
week is a success, not a failure; it must not page anyone.

**Bump level.** Scan `git log --format=%s "$prev"..HEAD` (and `%b` for
`BREAKING CHANGE`). Any subject matching `^feat(\(.+\))?!?:`, or any breaking
marker, → minor. Otherwise → patch. On a 0.x crate a breaking change is a minor
bump, so `feat!` and `feat` collapse to the same rule.

The new version is computed from `Cargo.toml`'s current version, not from the
tag, so a hand-edited version can never be silently reverted.

### 3. Gate

Same job, before a single byte is written:

```
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

Clippy is in the list for a specific reason: v0.6.0 shipped with six clippy
errors on `main` because the tag went out while clippy was still running.

`cargo fmt` is deliberately **absent**. This tree is not fmt-clean under the
local rustfmt and never has been; adding it would make every week red.

The release build is **not** here — it runs after the bump (§4), where it does
double duty as the lockfile check. Ordering is load-bearing throughout: verify,
bump, re-verify, and only then tag.

Red gate → the run fails, loudly, with no commit and no tag.

### 4. Bump the four files

**`Cargo.toml`** — one `version =` line, under `[package]` (verified: the only
occurrence; `[workspace.package]` carries `edition` and `license` only). Assert
exactly one match before writing.

**`Cargo.lock` and `desktop/rust/Cargo.lock`** — a surgical edit of the
`version` key inside the `[[package]] name = "rdc"` block, each asserted to
produce a **1-line diff** (`git diff --numstat` → `1 1`). This is preferred over
`cargo metadata`, which would need either a warm registry cache (`--offline`) or
a network fetch, and which can rewrite unrelated entries if the lock is stale.

The edits are then verified by cargo rather than trusted. All three run
**after** the bump and **before** the commit:

- root: `cargo build --release --locked` — `--locked` fails if the lockfile no
  longer matches the manifest, and a green build means the tag can never be
  created against a tree that cannot produce a release binary. Duplicated work
  (`release.yaml`'s matrix rebuilds the same target) costing ~$0.06 of Linux
  time; the profile is `lto = "fat"`, `codegen-units = 1`, so budget for it
- desktop: `cargo metadata --locked --manifest-path desktop/rust/Cargo.toml`
- template pin: `cargo test --locked --lib cli::gitlab_ci`, which re-runs the
  test from §5. It must run *after* the bump — a pre-bump run passes even when
  the bumper then updates `Cargo.toml` and forgets the template

The desktop check matters because Cargokit does not build with `--locked`, so a
bad edit there would otherwise surface as a dirty tree on some future build
rather than as a failure here.

**`templates/gitlab-ci.yml`** — the `RDC_VERSION` pin, per the CLAUDE.md
contract: the deploy job runs `rdc sync --allow-deletes --yes` unattended, so
the pin must always name a real, current tag.

### 5. Make the template safe to bump mechanically

Today the version appears on seven lines of the template, and line 52 —
`# the pre-0.7 unversioned names, so this pipeline works against old and new` —
is a *historical* statement that a naive `sed 's/0\.7/0\.8/'` would silently
falsify. Rewrite the surrounding comments so the literal appears exactly **once**:

- line 43 → `# An exact tag is reproducible and is what this file ships with.`
- line 44 → prose about "a series prefix" rather than restating `"v0.7"`
- line 48 → `RDC_VERSION: "v0.7.0"` — the one literal, and the only line the
  bumper touches
- line 51 → `rdc-<version>-x86_64-unknown-linux-gnu.tar.gz` instead of a frozen
  example filename
- line 52 → **unchanged**, it is a fact about pre-0.7 releases
- lines 68–69 → `vX.Y.Z` / `vX.Y` placeholders in the three-forms table

Then add a test beside `committed_template_regions_match_the_renderer` in
`src/cli/gitlab_ci.rs` asserting that the `RDC_VERSION` parsed out of
`GITLAB_CI_TEMPLATE` equals `env!("CARGO_PKG_VERSION")`. Nothing tests this
today, so a half-finished bump — by the workflow or by hand — currently ships.
Because both files move in the same commit, the test holds on every committed
state — before a release and after one. It only ever fails on the intermediate
CI state where `Cargo.toml` has been bumped and the template has not, which is
exactly the bug it exists to catch (hence the post-bump re-run in §4).

These comment edits are safe against the existing region test: `splice` compares
only the marker regions, and comments live outside them.

### 6. Commit, tag, publish

Commit as `github-actions[bot]` (the identity the tap job already uses), subject
`chore(release): vX.Y.Z`, matching every release commit in the log. Annotated
tag with body `rdc vX.Y.Z`, matching `v0.7.0`'s actual tag body. Push both with
the default `GITHUB_TOKEN`.

If the push is rejected because `main` moved since the gate ran, **fail** — do
not rebase. A rebase would put commits into the tag that the gate never tested.

Then call the release directly:

```yaml
release:
  needs: prepare
  if: needs.prepare.outputs.released == 'true'
  uses: ./.github/workflows/release.yaml
  secrets: inherit
  with:
    tag: ${{ needs.prepare.outputs.tag }}
```

No trigger is involved, so the `GITHUB_TOKEN` restriction never applies.

### 7. `release.yaml` changes

Add `workflow_call` alongside the existing trigger, with a `tag` input:

```yaml
on:
  push:
    tags: ['v*']
  workflow_call:
    inputs:
      tag: { type: string, required: true }
```

Replace all four `github.ref_name` uses (lines 50, 79, 130, 198) with
`inputs.tag || github.ref_name`, and add `ref: ${{ inputs.tag || github.ref }}`
to each `actions/checkout` so the build is pinned to the tag rather than to
whatever `main` happens to be. The manual `git push origin vX.Y.Z` path keeps
working byte-identically — `inputs.tag` is empty there and `github.ref_name`
takes over.

This mirrors how `release.yaml` already consumes `desktop-build.yml`, so it is
the pattern already in the house.

### 8. Fix the tap job

Swap the unauthenticated `curl -fsSL .../releases/download/...` for
`gh release download --repo mrtnzlml/rdc` under the job's own `GITHUB_TOKEN`,
which already carries `contents: write` on this repo. Keep `HOMEBREW_TAP_TOKEN`
for the tap checkout — the two tokens address different repos and must not be
confused.

**What this does not fix.** `brew install mrtnzlml/tap/rdc` still fails while the
repo is private: Homebrew fetches the same `releases/download/...` URL without a
token, and that URL returns 404 (verified directly). This change makes the run
green and the formula accurate; making `brew install` work needs either a public
repo or dropping Homebrew, which is a separate decision.

Without it, every week produces a red run and a failure email for a job that has
failed identically four releases running — which trains the maintainer to ignore
exactly the signal this whole design exists to send.

## Failure modes

| Scenario | Behaviour |
| --- | --- |
| Nothing shippable landed | `::notice::`, exit green, no tag, ~$0.12 spent |
| Suite or clippy red | Run fails; no commit, no tag, no release. Fix and either wait a week or `workflow_dispatch` |
| `main` moved between gate and push | Push rejected → run fails. Deliberate: never tag untested commits |
| A matrix build fails after the tag exists | **Tag with no release.** Recovery is deleting the tag and re-running. The post-bump release build (§4) makes this unlikely; inverting the order would mean rebuilding `release.yaml` around an uncommitted tree, which is not worth it |
| Schedule delayed or dropped by GitHub load | Next week picks up the whole backlog; the range is `$prev..HEAD`, not "last 7 days" |
| Two releases race | `concurrency` group serialises them |

## Backward compatibility

- **Projects already using the CI template are untouched.** `rdc init` splices
  only the marker regions into an existing `.gitlab-ci.yml`, so neither the
  comment rewrite nor the pin bump reaches a file already on disk. A project's
  `RDC_VERSION` stays exactly where its author put it.
- **The `vX.Y` series pin keeps its documented meaning**, because fix-only weeks
  cut patches. This was the deciding argument against weekly minors.
- **Manual releases keep working unchanged** — same trigger, same four
  `github.ref_name` fallbacks, same file list.
- **`rdc upgrade`'s once-daily passive nudge** will now surface a new version
  most weeks instead of every month or two. That is the intended effect, but it
  is a real change in how chatty the tool is. `src/upgrade.rs`'s stated policy —
  new binaries read old artifacts; older binaries must error clearly rather than
  corrupt — is what makes a faster cadence safe, and it is unchanged here.
- **Version numbers move faster.** Roughly 4 releases/month instead of ~1.
  Nothing in the tree parses its own version, and the lockfile's compatibility
  check is keyed to a lockfile format version rather than the crate version, so
  this is cosmetic.

## Out of scope

- Making the repo public, or making `brew install` work for a private repo.
- Per-push CI. Rejected on cost: ~240–600 extra Linux-minutes/month on top of
  ~1,190 for the weekly job would crowd the 2,000 allowance.
- A `CHANGELOG.md`. `generate_release_notes: true` already produces notes from
  the commit log, and there is no changelog in the tree today.
- Promoting a release to a minor by hand after the fact.

## Testing

- Unit: the version-derivation and ship-gate logic extracted into a shell
  function or small script, exercised against synthetic `git log`/`git diff`
  output — feat-present, fix-only, docs-only, empty range, breaking marker.
- Unit: the new `RDC_VERSION` == `CARGO_PKG_VERSION` test in
  `src/cli/gitlab_ci.rs`, plus the existing region test proving the comment
  rewrite did not disturb the markers.
- Unit: the lockfile edit asserted to a 1-line diff on both lockfiles.
- Integration: `workflow_dispatch` the weekly workflow once against real `main`
  before the first schedule fires, and confirm it either skips cleanly or
  produces a release identical in shape to `v0.7.0`'s.
- Integration: one manual `git push origin vX.Y.Z` after the `release.yaml`
  refactor, proving the `github.ref_name` fallback still holds.

## Files touched

| File | Change |
| --- | --- |
| `.github/workflows/weekly-release.yaml` | new |
| `.github/workflows/release.yaml` | `workflow_call` + `tag` input; four `ref_name` fallbacks; `ref:` on checkouts; tap job uses `gh release download` |
| `templates/gitlab-ci.yml` | comments de-duplicated so the version literal appears once |
| `src/cli/gitlab_ci.rs` | new test pinning the template's `RDC_VERSION` to `CARGO_PKG_VERSION` |
| `CLAUDE.md` | document that the pin is now bumped by CI, and that the single literal is load-bearing |
