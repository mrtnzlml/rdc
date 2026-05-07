# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M7. Pull side feature-complete (M6) plus three-way conflict detection on subsequent pulls (M7). See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design and `docs/superpowers/plans/` for implementation plans.

## Quick start

```sh
cargo install --path .

mkdir my-rossum-project && cd my-rossum-project
rdc init --name my-project \
  --env dev=https://YOUR-ORG.rossum.app/api/v1:YOUR_ORG_ID

# Optional: enable MDH dataset snapshots by editing rdc.toml:
#   [envs.dev]
#   data_storage_base = "https://YOUR-ORG.rossum.app/data/v1"

# Provide a token for the dev env:
echo '{"api_token":"YOUR_TOKEN"}' > secrets/dev.secrets.json
# OR: export RDC_TOKEN_DEV=YOUR_TOKEN

rdc pull dev
ls envs/dev/
# email-templates/  engines/  engine-fields/  hooks/  labels/  mdh/  organization.json
# rules/  workflows/  workflow-steps/  workspaces/
```

## Conflict handling

`rdc pull` is now safe to re-run. The lockfile's `content_hash` is used as the
"base" for a three-way comparison:

- If you haven't edited the local file and the remote changed → write the remote.
- If you edited the local file and the remote is unchanged → keep your edit.
- If both you and the remote changed → preserve your local file and write the
  remote alongside as `<slug>.json.remote` for inspection. The pull summary
  reports the conflict count and a per-conflict warning is printed to stderr.

Three-way detection is currently active for hooks, organization, rules, labels,
engines, engine fields, workflows, workflow steps, and email templates.
Schemas, queues, inboxes, and MDH still always-overwrite — they will join in
M8.

## Tests

```sh
cargo test
```
