# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M10. Pull side feature-complete (M7-M9). `rdc push` for hooks (round-trip closed). Other kinds still pull-only — push for queues/schemas/rules/etc. is future work. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design.

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

## Push (M10 — hooks only)

`rdc push <env>` PATCHes locally-edited hooks back to the Rossum API. Each
hook is checked against the lockfile's content_hash:

- No local edits → skipped silently.
- Local edits AND remote unchanged since last pull → PATCH succeeds.
- Local edits AND remote drifted → push aborted for that hook with a warning.
  Run `rdc pull` to fetch the remote, resolve, then push again.

After a successful push, the lockfile is updated with the server's
authoritative response.

**M10 limitations:**
- Hooks only. Queues, schemas, rules, labels, etc. cannot be pushed yet.
- Updates only. New objects (creates) and deletes are not supported.
- Single-phase. No two-phase send for cross-references (not needed for hooks).

## Tests

```sh
cargo test
```
