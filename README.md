# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M9. Pull side feature-complete with three-way conflict detection across all kinds (M7, M8, M9), per-env `_index.md` generation, and formula-aware schema hashes. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design.

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
# _index.md  email-templates/  engines/  engine-fields/  hooks/  labels/
# mdh/  organization.json  rules/  workflows/  workflow-steps/  workspaces/
```

## Conflict handling

`rdc pull` is safe to re-run. The lockfile's `content_hash` is used as the
"base" for a three-way comparison:

- If you haven't edited the local file and the remote changed → write the remote.
- If you edited the local file and the remote is unchanged → keep your edit.
- If both you and the remote changed → preserve your local file and write the
  remote alongside as `<file>.remote` for inspection. The pull summary
  reports the conflict count and a per-conflict warning is printed to stderr.

Three-way detection covers every kind: organization, workspaces, queues,
schemas (combined hash covers schema.json + every formula `.py` file), inboxes, hooks, rules, labels, engines, engine
fields, workflows, workflow steps, email templates, and MDH collections +
indexes.

`rdc pull` also generates `envs/<env>/_index.md` listing the per-kind
inventory. The index is regenerated on every pull; do not edit it by hand.

## Tests

```sh
cargo test
```
