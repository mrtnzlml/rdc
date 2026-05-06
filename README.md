# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M3 (workspace tree). Implements `rdc init`, `rdc pull <env>` for organizations, workspaces (with optional regex filter), queues, schemas (with formula extraction), inboxes, and hooks. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design and `docs/superpowers/plans/` for implementation plans.

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
tree envs/dev -L 5
# envs/dev/
# ├── hooks/
# ├── organization.json
# └── workspaces/
#     └── <workspace>/
#         ├── workspace.json
#         └── queues/
#             └── <queue>/
#                 ├── queue.json
#                 ├── schema.json
#                 ├── inbox.json (if present)
#                 └── formulas/<field_id>.py (one per formula field)
```

## Tests

```sh
cargo test
```
