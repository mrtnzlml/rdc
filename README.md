# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M4. `rdc init` and `rdc pull <env>` cover organizations, workspaces (with optional regex filter), queues, schemas (with formula extraction), inboxes, hooks, rules, labels, engines, and engine fields. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design and `docs/superpowers/plans/` for implementation plans.

## Quick start

```sh
cargo install --path .

mkdir my-rossum-project && cd my-rossum-project
rdc init --name my-project \
  --env dev=https://YOUR-ORG.rossum.app/api/v1:YOUR_ORG_ID

# Provide a token for the dev env:
echo '{"api_token":"YOUR_TOKEN"}' > secrets/dev.secrets.json
# OR: export RDC_TOKEN_DEV=YOUR_TOKEN

rdc pull dev
ls envs/dev/
# engines/  engine-fields/  hooks/  labels/  organization.json  rules/  workspaces/
```

## Tests

```sh
cargo test
```
