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
- The **committed default must stay a pinned tag** — `RDC_VERSION` names the
  newest release tag. The deploy job runs `rdc sync --allow-deletes --yes`
  unattended, so a floating default would let a new rdc change what a
  destructive sync does with nobody watching. **You no longer bump this by
  hand**: `.github/workflows/weekly-release.yaml` rewrites it in the same commit
  as `Cargo.toml`, and `committed_template_pins_this_crates_version`
  (`src/cli/gitlab_ci.rs`) fails the build if the two ever drift.
- Because a script rewrites that line, the **full version literal must appear
  exactly once** in `templates/gitlab-ci.yml` — on the `RDC_VERSION` line, at a
  two-space indent. Everything else says `<version>`, `vX.Y.Z` or `vX.Y`.
  `the_version_pin_is_the_only_line_a_bumper_could_match` enforces both halves.
  Note `pre-0.7` in the `RDC_ASSET_SUFFIX` comment is a *historical* statement
  about releases that already shipped and must never be bumped; it survives
  because it is not the full literal.
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
- The install script *also* accepts `latest` (newest release) and a series
  prefix like `v0.6` (newest patch in that line), resolved through
  `api.github.com`. Those are **deliberate opt-ins for our own CI** — don't
  make either the default, and don't delete the branches thinking they're a
  mistake.
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
