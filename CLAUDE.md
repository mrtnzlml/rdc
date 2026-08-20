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
  `template_carries_both_regions` / the init test will say so.
- `rdc init` splices those regions on **every** run, including `--env`, and never
  touches a pipeline that has no markers. A markered file is spliced even under
  `--force`, so the static half of a project's pipeline is theirs once written;
  taking a newer binary's static half means deleting the file and re-initing.
- The Python testkit under `templates/testkit/` is embedded the same way and
  scaffolded alongside `conftest.py` / `pytest.ini` / `requirements-dev.txt`. It
  supports txscript **1.1.0 and 1.2.0** from one code path; `_unwrap` must test
  `isinstance(result, EvalResult)` and never `getattr(result, "value", result)`,
  because a formula returning a field hands back a proxy whose `.value` is the
  datapoint's raw string. Its self-tests ship on purpose: `pytest -q` with
  nothing collected exits 5 and turns the pipeline's test job red.
- The **committed default must stay a pinned tag** — keep `RDC_VERSION` set to
  the newest release tag, and bump it whenever a new release ships. The deploy
  job runs `rdc sync --allow-deletes --yes` unattended, so a floating default
  would let a new rdc change what a destructive sync does with nobody watching.
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
