# CLAUDE.md

Project-specific instructions for working in this repo.

## CI templates

- `templates/gitlab-ci.yml` is **embedded in the binary** with `include_str!`
  (`src/cli/init.rs`) and written to `.gitlab-ci.yml` by `rdc init`. Edit the
  template, never a copy — `tests/cli_init.rs` compares the two byte-for-byte.
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
