# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to
disk for AI-assisted local development, lets you edit them in place, and
deploys them across environments.

**Status:** M19. Pull all kinds; push and deploy for hooks, rules,
labels, queues, schemas (formula bodies round-trip), inboxes, and
email templates. Distributable via `curl | sh` or `cargo install`.
See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full
design.

## Install

Quickest path (macOS + Linux x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/mrtnzlml/rossum-deployment-manager-experiment/main/install.sh | sh
```

Downloads the matching pre-built binary from the latest GitHub release
and installs it to `~/.local/bin/rdc`. Add that directory to your `PATH`
if it isn't already.

To install a specific version:

```sh
curl -fsSL https://raw.githubusercontent.com/mrtnzlml/rossum-deployment-manager-experiment/main/install.sh | sh -s -- v0.0.1
```

Or build from source with Rust:

```sh
cargo install --git https://github.com/mrtnzlml/rossum-deployment-manager-experiment
# or, in a clone:
cargo install --path .
```

**Supported pre-built platforms:** macOS (Intel + Apple Silicon),
Linux x86_64. For Linux aarch64, Windows, or other platforms, build
from source.

## Quick start

```sh
mkdir my-rossum-project && cd my-rossum-project

# Bootstrap: name + at least one env (api_base:org_id).
rdc init --name my-project \
  --env dev=https://YOUR-ORG.rossum.app/api/v1:YOUR_ORG_ID

# Provide a token for the dev env (one of these):
echo '{"api_token":"YOUR_TOKEN"}' > secrets/dev.secrets.json
export RDC_TOKEN_DEV=YOUR_TOKEN

# Optional: enable MDH (Master Data Hub) dataset snapshots by editing
# rdc.toml:
#   [envs.dev]
#   data_storage_base = "https://YOUR-ORG.rossum.app/data/v1"

# Pull a complete snapshot of the env into envs/dev/.
rdc pull dev
```

After `rdc pull dev`, `envs/dev/` contains:

```
envs/dev/
├── _index.md                   ← generated inventory of every object
├── organization.json
├── workspaces/
│   └── invoices-ap/
│       ├── workspace.json
│       └── queues/
│           └── cost-invoices/
│               ├── queue.json
│               ├── schema.json
│               ├── inbox.json   (only if the queue has an inbox)
│               ├── formulas/    (one .py per datapoint with a formula)
│               │   └── amount_total.py
│               └── email-templates/
│                   ├── annotation-status-change-confirmed.json
│                   └── default-rejection-template.json
├── hooks/
│   ├── validator-invoices.json
│   └── validator-invoices.py    (extracted from config.code)
├── rules/
├── labels/
├── engines/
├── engine-fields/
├── workflows/
├── workflow-steps/
└── mdh/                         (only if data_storage_base is set)
    └── <dataset-slug>/
        ├── collection.json
        └── indexes.json
```

The lockfile (`.rdc/state/dev.lock.json`) records each object's
`id`, `url`, `modified_at`, and a `content_hash` of what was just
written. It is the **merge base** for the three-way comparison on
subsequent pulls and pushes.

`rdc pull` is **idempotent**: running it again with no remote changes
writes nothing.

## Editing the snapshot

Most kinds are plain JSON files; edit them directly.

For files with extracted code, edit the `.py` file, **not** the JSON:

- **Hook code** lives in `envs/<env>/hooks/<slug>.py`. The JSON's
  `config.code` is stripped on pull and re-inlined on push. Don't edit
  `config.code` in the JSON — the `.py` file is the source of truth.
- **Schema formulas** live in
  `envs/<env>/workspaces/<ws>/queues/<q>/formulas/<field-id>.py`. The
  JSON's `formula` properties are stripped on pull and re-inlined on push.

After any edit, `rdc push <env>` sends the change.

## Push

`rdc push <env>` PATCHes locally-edited objects back to the Rossum API.
Each candidate is compared against the lockfile's `content_hash`:

- **No local edits** → skipped silently.
- **Local edits, remote unchanged since last pull** → PATCH succeeds.
- **Local edits AND remote drifted** → push aborted for that object
  with a warning. Run `rdc pull` to fetch the remote, resolve, then
  push again.

After a successful push, the local file is rewritten with the server's
authoritative response so the lockfile hash matches the file bytes.
Subsequent pulls are idempotent.

**Writable kinds:** hooks, rules, labels, schemas (with formula bodies),
queues, inboxes, email templates. Schema push splices extracted
formulas back into `content[]` before sending. Email-template push
walks the queue-scoped `email-templates/` directories.

**Pull-only kinds:** engines, engine_fields, workflows, workflow_steps,
MDH collections + indexes. Push for these is future work.

**Out of scope:** Creates (POST) and deletes are not supported — `rdc
push` only updates existing objects. No two-phase send for
cross-references.

## Conflict handling

`rdc pull` does field-level three-way merge for every kind:

| Local edited? | Remote changed? | Action |
|---|---|---|
| no | no | (no-op) |
| no | yes | write the remote |
| yes | no | keep local |
| yes | yes | **conflict** — keep local, write remote next to it as `<file>.remote` |

A per-conflict warning goes to stderr and the count appears in the
summary line.

For schemas, the "combined hash" covers `schema.json` plus every
`formulas/<id>.py` file, so a formula-only edit is detected correctly.
For hooks, the combined hash covers `<slug>.json` plus `<slug>.py`.

After a conflict, the local copy is canonical; review the
`<file>.remote` (and `<dir>.remote/` for schema formulas), then either
delete it (keep local) or overwrite the local file with it (take
remote) and re-run pull.

`rdc pull` also regenerates `envs/<env>/_index.md`, an
inventory-by-kind. Don't edit it by hand.

## Overlays — per-env values

`envs/<env>/overlay.toml` declares values that should always be set
when pushing to that env, regardless of what the snapshot says. Useful
for per-env names, runtimes, automation thresholds.

```toml
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"config.runtime" = "python3.12-secure"

[schemas.cost-invoices]
"settings.default_score_threshold" = 0.95

[queues.cost-invoices]
"automation_level" = "always"

[email_templates."invoices-ap/cost-invoices/default-rejection-template"]
"subject" = "[PROD] Your invoice was rejected"
```

The overlay's dotted-path keys are merged into the outbound PATCH body,
overwriting any value at that path. Manual edits to overlay-managed
keys in the snapshot are silently overwritten by the overlay on push.

**Sections:** `[hooks.<slug>]`, `[rules.<slug>]`, `[labels.<slug>]`,
`[schemas.<queue-slug>]`, `[queues.<queue-slug>]`,
`[inboxes.<queue-slug>]`,
`[email_templates."<ws-slug>/<q-slug>/<template-slug>"]`.

**Limitations:**
- Push-side only — pull does not strip overlay-managed values yet.
- Simple dotted paths only; no JMESPath wildcards or array filters.

## Deploy — copy from one env to another

When you've validated changes in a TEST env and want to ship them to
PROD, use the deploy commands.

`rdc map <src> <tgt>` — auto-match objects by slug between two envs
and write `.rdc/map/<src>→<tgt>.toml`. Re-runnable; existing entries
are preserved, new auto-matches are added.

`rdc plan --from <src> --to <tgt>` — read-only. Print what
`rdc apply` would do.

`rdc apply --from <src> --to <tgt>` — for each mapped object, read
the src snapshot, apply tgt's overlay, PATCH tgt's API.

**Typical workflow:**

```sh
rdc pull test                          # pull both envs once
rdc pull prod
rdc map test prod                      # auto-match by slug
$EDITOR .rdc/map/test→prod.toml        # hand-curate renames if any
rdc plan --from test --to prod         # preview
rdc apply --from test --to prod        # execute
rdc pull prod                          # refresh prod snapshot post-apply
```

**Deployable kinds:** hooks, rules, labels, queues, schemas (with
formula bodies), inboxes, email templates. Same kinds as push, by
design — if you can push it within an env, you can deploy it across
envs.

The mapping file uses one section per kind:

```toml
version = 1

[hooks]
validator-invoices = "validator-invoices"
sftp-import = "sftp-import-prod"   # rename across envs

[queues]
cost-invoices = "cost-invoices"

[schemas]
cost-invoices = "cost-invoices"

[email_templates]
"invoices-ap/cost-invoices/default-rejection-template" = "invoices-ap/cost-invoices/default-rejection-template"
```

For queue-nested kinds (queues, schemas, inboxes), the key is the
queue's slug. For email templates, the key is the compound
`<ws-slug>/<q-slug>/<template-slug>`. Auto-match writes 1:1 entries
wherever both src and tgt have an object with the same key; you
hand-edit for renames.

**Limitations:**
- Updates only (no creates / deletes).
- No drift detection between local tgt snapshot and remote tgt.
- Apply is not idempotent — every run PATCHes mapped objects.
- Overlays apply unconditionally; they override even if src has the
  same value.

## Authentication

Tokens are loaded per env, in priority order:

1. Environment variable `RDC_TOKEN_<ENV_UPPER>` (e.g. `RDC_TOKEN_DEV`).
   Recommended for CI.
2. `secrets/<env>.secrets.json` — `{"api_token": "..."}`. Recommended
   locally; add `secrets/` to `.gitignore` (`rdc init` does this).

Loud error if neither is set.

For Master Data Hub, `data_storage_base` is set under
`[envs.<name>]` in `rdc.toml`. The same API token is reused.

## Layout cheat sheet

| File | Purpose |
|------|---------|
| `rdc.toml` | Project config: name, envs, optional `data_storage_base` |
| `secrets/<env>.secrets.json` | Per-env API token (gitignored) |
| `envs/<env>/_index.md` | Generated inventory; do not edit |
| `envs/<env>/organization.json` | Org metadata (read-only on remote) |
| `envs/<env>/overlay.toml` | Per-env push-side overrides (optional) |
| `envs/<env>/workspaces/<ws>/...` | Workspace + nested queues |
| `envs/<env>/<kind>/<slug>.json` | Org-scoped kinds (hooks, rules, etc.) |
| `.rdc/state/<env>.lock.json` | Merge base; auto-managed |
| `.rdc/map/<src>→<tgt>.toml` | Env-pair mapping for deploy |

## Tests

```sh
cargo test
```
