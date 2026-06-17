# CLAUDE.md

Project-specific instructions for working in this repo.

## CI templates

- The CI templates under `templates/` (e.g. `gitlab-ci-archival.yml`) must
  **pin to the latest released rdc version** — keep `RDC_VERSION` set to the
  newest release tag (concrete `releases/download/<tag>/<asset>` URL), and bump
  it whenever a new release ships. Do not use GitHub's floating
  `releases/latest/download/<asset>` alias.
