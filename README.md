# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M12. Pull side complete. Push for hooks. Deploy workflow (`rdc map`/`plan`/`apply`) for hooks across envs. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design.

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

## Overlays (M11 — push side, hooks only)

`envs/<env>/overlay.toml` declares values that should always be set when
pushing to that env, regardless of the canonical snapshot. Useful for
per-env names, secrets, URLs.

```toml
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"config.runtime" = "python3.12-secure"
```

On `rdc push`, the overlay's dotted-path keys are merged into the outbound
PATCH body, overwriting any value at that path. The overlay is the source
of truth for declared keys; manual edits to those keys in the snapshot are
overwritten by the overlay on push.

**M11 limitations:**
- Hooks only (matches push scope).
- Push-side only — pull does not strip overlay-managed values yet.
- Simple dotted paths only; no JMESPath wildcards or array filters.

## Deploy (M12 — TEST → PROD for hooks)

`rdc map <src> <tgt>` — auto-match hook slugs between two envs and write
`.rdc/map/<src>→<tgt>.toml`. The mapping file is hand-editable; entries
that auto-match by slug are added on each run.

`rdc plan --from <src> --to <tgt>` — show what apply would do
(read-only, no API calls).

`rdc apply --from <src> --to <tgt>` — for each mapped hook, read the src
snapshot, apply tgt's overlay, PATCH tgt's API. Used after pushing changes
through TEST and ready to roll them to PROD.

**Typical flow:**

```sh
rdc pull test                          # pull both envs once
rdc pull prod
rdc map test prod                      # auto-match by slug
$EDITOR .rdc/map/test→prod.toml        # hand-curate any rename mappings
rdc plan --from test --to prod         # preview
rdc apply --from test --to prod        # execute
```

**M12 limitations:**
- Hooks only.
- Updates only (no creates / deletes).
- No drift detection between local tgt snapshot and remote tgt.
- No overlay-managed diff exclusion (overlay always overrides).
- Apply is not idempotent — every run PATCHes mapped hooks.

## Tests

```sh
cargo test
```
