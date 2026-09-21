# CLAUDE.md

Project-specific instructions for working in this repo.

## CI templates

- `templates/gitlab-ci.yml` is **embedded in the binary** with `include_str!`
  (`src/cli/init.rs`) and written to `.gitlab-ci.yml` by `rdc init`. Edit the
  template, never a copy. Two `# >>> rdc:…` regions in it are **generated** from
  `rdc.toml` (`src/cli/gitlab_ci.rs`): the archive job's `parallel:matrix` and
  the drafted deploy jobs. So the test is in two halves — everything outside the
  markers must match the template byte-for-byte, and the committed region bodies
  must equal what `render_regions` produces for the canonical `dev`/`test`/`prod`
  example. Keep the committed example in step with the renderer, or
  `committed_template_regions_match_the_renderer` will say so.
- `rdc init` splices those regions on **every** run, including `--env`, and never
  touches a pipeline that has no markers — unless `--force` is passed, which
  replaces such a file wholesale (markers included). A markered file is spliced
  even under `--force`, so the static half of a project's pipeline is theirs
  once written; taking a newer binary's static half means deleting the file and
  re-initing.
- The two regions are **not** spliced the same way, and the difference is
  load-bearing. `rdc:archive-envs` is fully derived from `rdc.toml`, so it is
  re-rendered every run. `rdc:deploy-jobs` is only *half* derived — rdc knows
  the job name, the user supplies `RDC_SRC` — so on an existing file it is
  **additive** (`render_regions_for_existing` → `merge_deploy_jobs`): its
  content survives verbatim and a draft is appended only for an env the file
  has never been offered one for. Re-rendering it would revert finished deploy
  buttons to drafts and resurrect the drafts a reader deliberately deleted.
  "Has been offered one" is read off the **archive region still on disk**,
  which the previous run wrote from `rdc.toml` and therefore names the env set
  as of that run — no ledger, no new syntax, nothing outside the file. So
  `render_regions` (create / wholesale rewrite) and `render_regions_for_existing`
  (splice) are both needed; don't collapse them.
- Markdown docs work the same way, with `regions::MARKDOWN` markers. Every
  env-derived line in `CLAUDE.md` / `README.md` must live **inside** a region
  (`rdc:envs`, `rdc:promote`, `rdc:sync`): on an existing file only the regions
  are spliced, so an env-derived line outside one freezes at whatever
  `rdc.toml` said the day the file was created. `write_doc_with_regions`
  hard-errors on a template carrying no markers, so a README must emit every
  region it declares unconditionally — including for a hand-emptied `rdc.toml`.
- The Python testkit under `templates/testkit/` is embedded the same way and
  scaffolded alongside `conftest.py` / `pytest.ini` / `requirements-dev.txt`. It
  supports txscript **1.1.0 and 1.2.0** from one code path; `_unwrap` must test
  `isinstance(result, EvalResult)` and never `getattr(result, "value", result)`,
  because a formula returning a field hands back a proxy whose `.value` is the
  datapoint's raw string. Its self-tests ship on purpose: `pytest -q` with
  nothing collected exits 5 and turns the pipeline's test job red.
- The **committed default is `RDC_RELEASE: "latest"`** — the scaffolded
  pipeline installs the newest release, so a fresh project works and fixes
  arrive without editing the file. The cost is real and accepted: the deploy job
  runs `rdc sync --allow-deletes --yes` unattended, so a release changes what a
  destructive sync does with nobody watching. A project that wants that decided
  by a commit sets `RDC_RELEASE: "tags/vX.Y.Z"`, or overrides it in a manual
  job's "Run job" form for one press. (This replaced a pinned `RDC_VERSION` on
  2026-09-21; reverting means reinstating the bumper's fourth edit and its
  tests.)
- The value is **the `api.github.com` path**, not a version — which is the whole
  reason the install script is 14 lines and not 35: `releases/latest` and
  `releases/tags/vX.Y.Z` are one `curl` with one variable in it. There is no
  `vX.Y` series form any more, and no `RDC_ASSET_SUFFIX` / `RDC_INSTALL_DIR` /
  `RDC_ASSET` either; the linux triple is inlined and one asset must match.
- Therefore the **full version literal must appear nowhere** in
  `templates/gitlab-ci.yml`, and `.github/scripts/bump-version.sh` must not
  touch that file: it edits three files, not four.
  `the_committed_template_floats_and_names_no_version` (`src/cli/gitlab_ci.rs`)
  enforces both halves, and `bumps_all_three_files` (`tests/bump_version.rs`)
  asserts the template comes back byte-identical. Prose says `<version>`,
  `vX.Y.Z` or `vX.Y`, never the literal.
- Releases are cut by **`.github/workflows/weekly-release.yaml`** (Monday 06:17
  UTC, plus `workflow_dispatch`), not by hand. It releases only when a commit
  since the last tag touched `src/`, `templates/`, `desktop/`, `Cargo.toml` or
  `Cargo.lock`; the level is derived from commit types (any `feat`, any
  `type!:`, or `BREAKING CHANGE:` → minor, else patch), which is what keeps the
  template's documented `vX.Y` series pin — *"picks up fixes, never a new
  feature"* — honest.
- It fires **three times** that Monday (06:17, 10:17, 14:17 UTC), because
  GitHub's docs say a scheduled run "can be delayed" and that "some queued jobs
  may be dropped" — and that is not theoretical: the very first tick,
  2026-08-31 06:17 UTC, never arrived, with no run, no red X and no
  notification. The later two are **retries**: only they pass `release-plan.sh`
  a fourth argument, the previous tag's age **in hours**, and under 24 it
  answers `release=false` / `reason=cooldown` and the retry goes quiet. So a
  dropped tick still ships, a delivered one is never released twice, and the
  06:17 tick's behaviour is unchanged. A day rather than a week on purpose: the
  ticks are within eight hours of each other, and a week-wide window would
  suppress a legitimate retry for days after any hand-cut release. Adding a
  cron means adding it to the retry set — a tick that is *not* `17 6 * * 1`
  inherits the cooldown, which is the safe default. The decision and the bump are plain `sh` scripts in
  `.github/scripts/`, unit-tested by `tests/release_plan.rs` and
  `tests/bump_version.rs`. It calls `release.yaml` through `workflow_call`
  rather than pushing a tag, because a tag pushed with the default
  `GITHUB_TOKEN` does not start `on: push: tags` workflows. A manual
  `git push origin vX.Y.Z` still works unchanged.
- **`.github/workflows/ci.yaml` runs the release gate on every push to `main`
  and every PR**, and it exists because that gate used to run *nowhere else*.
  Until 2026-09-11 `cargo test` / `clippy` / `doc` ran only inside
  `weekly-release.yaml`, so a regression on main stayed invisible until it
  killed a release — silently, once a week, costing the whole week. That is how
  2026-09-07 was lost: all three ticks died on the same four `drop_non_drop`
  errors, and the fix already existed locally, unpushed, before the first tick
  ran. The three gate steps are **duplicated on purpose** in `ci.yaml` and
  `weekly-release.yaml`; whatever CI accepts the release gate must also accept,
  so change one and you must change the other. `cargo fmt` is absent from both
  — this tree is not fmt-clean and never has been.
- **`rust-toolchain.toml` is the only place the Rust version is decided**, and
  no workflow may use `dtolnay/rust-toolchain` again: that action picks its
  toolchain from its own `@rev` and **never reads the file**, so it would
  install components and `targets:` onto a different toolchain than the one
  cargo actually runs — and the first stable release past the pin would fail
  `release.yaml`'s cross-builds on a missing target. Every workflow therefore
  materialises the pin with `rustup show` (plus `rustup target add` per matrix
  target in `release.yaml`, under `shell: bash` because windows-latest defaults
  to pwsh). Bump `channel` deliberately and meet the new lints when you choose;
  the pin is in the cargo cache keys so a stale `target/` cannot outlive it.
- The install script accepts exactly **two** forms, because `RDC_RELEASE` is an
  API path: `latest` (the default) and `tags/vX.Y.Z`. The `vX.Y` series form
  and its list-endpoint `python3` filter were **deleted on purpose** on
  2026-09-21, in favour of the shape a real project's pipeline had been running:
  nothing in `.github/` ever consumed the series form. Reinstating it means
  re-adding that filter, not un-commenting something.
- The repo is private, so the template installs rdc through
  `api.github.com/repos/<repo>/releases/assets/<id>` (resolved from the tag).
  The `releases/download/<tag>/<asset>` browser URL 404s even with a token —
  don't "simplify" the install back to it.
- Keep the env names as placeholders (`dev` → `test` → `prod`) with `# TODO`
  markers. `rdc.toml` stores envs in a `BTreeMap`, so init cannot derive a
  promotion chain from them (`dev`, `prod`, `test` would chain dev → prod → test).

## Customer confidentiality

- Never put customer names or customer-specific identifiers — org/division/region
  codes, real environment names, queue/engine/hook slugs, hostnames, URLs, or file
  paths — anywhere in this repository. This covers source, tests, docs, and
  fixtures **and git commit messages/descriptions** (history, not just the working
  tree). Use neutral placeholders instead (e.g. `acme`, `main`, `invoices`,
  `test`/`dev`/`prod`, `dev-eu`/`dev-us`). If customer-specific strings ever land,
  scrub them from both the file content and the commit history.
