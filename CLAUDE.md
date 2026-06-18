# CLAUDE.md

Project-specific instructions for working in this repo.

## CI templates

- The CI templates under `templates/` (e.g. `gitlab-ci-archival.yml`) must
  **pin to the latest released rdc version** — keep `RDC_VERSION` set to the
  newest release tag (concrete `releases/download/<tag>/<asset>` URL), and bump
  it whenever a new release ships. Do not use GitHub's floating
  `releases/latest/download/<asset>` alias.

## Customer confidentiality

- Never put customer names or customer-specific identifiers — org/division/region
  codes, real environment names, queue/engine/hook slugs, hostnames, URLs, or file
  paths — anywhere in this repository. This covers source, tests, docs, and
  fixtures **and git commit messages/descriptions** (history, not just the working
  tree). Use neutral placeholders instead (e.g. `acme`, `main`, `invoices`,
  `test`/`dev`/`prod`, `dev-eu`/`dev-us`). If customer-specific strings ever land,
  scrub them from both the file content and the commit history.
