# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to
disk for AI-assisted local development, lets you edit them in place, and
deploys them across environments.

**Status:** M30. Pull all kinds (incl. MDH collections + indexes —
data storage URL derived from `api_base`, no extra config); push
and deploy for hooks, rules, labels, queues, schemas (formula
bodies round-trip), inboxes, email templates, engines, and engine
fields. Overlays are bidirectional: applied on push, stripped on
pull (spec §9.3). `rdc apply` rewrites cross-reference URLs from
src to tgt, refuses to PATCH on tgt drift, and skips no-op
deploys (idempotent). `_index.md` includes per-kind inventory
plus a cross-references section that resolves `hook → queues`,
`rule → queues`, `email_template → queue`. Pull pipelines
per-queue and per-MDH-collection sub-fetches via a bounded
`--concurrency` (spec §7.2 / §16, default 5). All HTTP calls
retry gracefully on `429 Too Many Requests` with `Retry-After` /
exponential backoff. `rdc init` accepts flags or runs an
interactive wizard; `rdc status` for a read-only health check;
`rdc diff` for unified diffs (local vs remote, or two
snapshots); `rdc auth` to set/refresh tokens; `rdc repair
--rebuild-lock` for lockfile recovery. Distributable via
`curl | sh` or `cargo install`. See
`docs/superpowers/specs/2026-05-06-rdc-design.md` for the full
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

# Bootstrap. Two modes:
#   1. Non-interactive (CI-friendly):
rdc init --name my-project \
  --env dev=https://YOUR-ORG.rossum.app/api/v1:YOUR_ORG_ID
#   2. Interactive wizard (when run from a terminal with no flags):
rdc init
# → prompts for project name, then loops over env name + api_base + org_id
# → blank env name finishes the loop

# After init, set the API token (one of these):
rdc auth dev --token YOUR_TOKEN          # validates + writes secrets/dev.secrets.json (mode 0600)
echo '{"api_token":"YOUR_TOKEN"}' > secrets/dev.secrets.json
export RDC_TOKEN_DEV=YOUR_TOKEN

# MDH (Master Data Hub) is pulled automatically when available — the
# data storage URL is derived from api_base, so there's no extra
# config to set. On clusters without MDH the lookup returns 404 and
# rdc skips silently.

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
└── mdh/                         (only when the cluster has MDH)
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

**Writable kinds:** hooks, rules, labels, schemas (with formula
bodies), queues, inboxes, email templates, engines, engine fields.
Schema push splices extracted formulas back into `content[]` before
sending. Email-template push walks the queue-scoped
`email-templates/` directories.

**Pull-only kinds:**
- **Workflows + workflow steps** — Rossum's API is read-only for
  these (`OPTIONS` returns `Allow: GET, HEAD, OPTIONS`). The
  snapshot captures them so you can review changes, but `rdc push`
  and `rdc apply` cannot send updates back.
- **MDH collections + indexes** — push not yet implemented.

If your token lacks permission for a writable kind (e.g. engines on
some plans return 403), `rdc pull` warns and skips that kind, leaving
other kinds intact.

**Out of scope:** Creates (POST) and deletes are not supported —
`rdc push` only updates existing objects. No two-phase send for
cross-references.

## Status — health check

`rdc status [env]` prints a read-only summary per env: token
presence, auth, lockfile, and which local files differ from the
lockfile (= what `rdc push` would attempt to send). With no
argument, runs for every env defined in `rdc.toml`.

## Diff — see what changed

Two modes, both read-only:

`rdc diff <env>` — compares the local snapshot against what the
remote API currently returns. One GET per object; no PATCHes.

```sh
$ rdc diff dev
--- hooks/validator-invoices.py (local)
+++ hooks/validator-invoices.py (remote)
@@ -1,3 +1,2 @@
 def validate(payload):
     return {}
-# my local edit
```

`rdc diff <a> <b>` — compares two local snapshots without touching
the API. Useful for "what's different between TEST and PROD?".

```sh
$ rdc diff test prod
--- hooks/validator-invoices.json (in test)
+++ hooks/validator-invoices.json (in prod)
@@ -3,4 +3,4 @@
   "name": "Validator: invoices",
-  "config": { "runtime": "python3.12" }
+  "config": { "runtime": "python3.11" }
```

Output is a standard unified diff (3 lines of context). If a file
exists in only one snapshot, the missing side is reported.



```
$ rdc status dev
Env 'dev'
  api_base: https://YOUR-ORG.rossum.app/api/v1
  org_id:   123456
  token:    present
  auth:     ok (org 'YOUR-ORG', id 123456)
  lockfile: v2, 48 objects across 10 kinds
  edits:    1 file(s) differ from lockfile:
            hooks/validator-invoices
```

The `edits:` line lists everything `rdc push` would consider
sending. It does not call the API beyond the auth check, and it
does not modify any files.

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
inventory-by-kind plus a cross-references section. Don't edit it
by hand.

The cross-references section answers "what's attached to what" without
needing to grep every JSON. It maps each writable kind that holds queue
references to its queue slug(s):

```markdown
## Cross-references

### hooks → queues
- `master-data-hub` → playground-vol-1, playground-vol-2
- `validator-invoices` → cost-invoices
- `sftp-import` → (none)

### rules → queues
- `validate-totals` → cost-invoices

### email_templates → queue
- `invoices-ap/cost-invoices/default-rejection-template` → `cost-invoices`
```

URLs are resolved to slugs via the lockfile, so the section reflects
exactly what's in the snapshot. Orphan refs (URLs whose target isn't
in the snapshot) are silently dropped.

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

The overlay's dotted-path keys are bidirectional:

- **On push**, they're merged into the outbound PATCH body, overwriting
  any value at that path. The remote ends up with the env-specific
  value.
- **On pull** (M26 / spec §9.3), they're stripped from the snapshot
  before write. The on-disk JSON reflects the canonical (pre-overlay)
  form, so `rdc diff test prod` and `rdc map test prod` stay quiet
  about env-specific differences. The lockfile records the stripped
  hash, so subsequent pulls and pushes are idempotent.

Round-trip: TEST and PROD snapshots agree on the canonical content.
Each env's `overlay.toml` declares the env-specific deltas. Push
re-applies them on the way out; pull strips them on the way in.

**Sections:** `[hooks.<slug>]`, `[rules.<slug>]`, `[labels.<slug>]`,
`[schemas.<queue-slug>]`, `[queues.<queue-slug>]`,
`[inboxes.<queue-slug>]`, `[engines.<slug>]`,
`[engine_fields.<slug>]`,
`[email_templates."<ws-slug>/<q-slug>/<template-slug>"]`.

**Limitations:**
- Simple dotted paths only; no JMESPath wildcards or array filters.
- The overlay must exist BEFORE you pull. If you add an overlay
  after pulling, the next pull strips and the next push re-applies
  — but the lockfile's pre-strip hash will now mismatch, which the
  drift check surfaces as "remote has changed". Run `rdc pull` once
  more after editing the overlay to re-baseline.

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
formula bodies), inboxes, email templates, engines, engine fields.
Same kinds as push, by design — if you can push it within an env,
you can deploy it across envs.

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

**Apply is conservative and idempotent (M29):**
- **URL rewriting.** Cross-references (hook.queues, queue.schema,
  email_template.queue, etc.) get rewritten from src URLs to tgt
  URLs via the lockfiles + mapping. Strings that don't match a
  known src object are left alone.
- **Drift detection.** Before each PATCH, apply fetches the tgt
  remote and compares its post-overlay-strip hash to the tgt
  lockfile. If they differ, someone changed tgt since you last
  pulled it — apply skips that object with a warning instructing
  you to `rdc pull <tgt>` first.
- **Idempotency.** If the post-overlay payload already equals the
  tgt remote, apply skips the PATCH. Re-running `rdc apply` on an
  in-sync deploy results in `0 PATCHes`. Verified live with 256
  mapped objects → 0 API calls.

**Limitations:**
- Updates only (no creates / deletes).

## Global flags

These flags can be passed to any subcommand:

| Flag | Description |
|------|-------------|
| `--concurrency N` | Maximum parallel API calls inside `rdc pull`. Default 5 (spec §16). Also reads `RDC_CONCURRENCY`. |
| `--json` | Reserved for machine-readable output (no-op today). |
| `--no-color` | Reserved for disabling ANSI color (rdc currently emits plain text). |
| `--verbose`, `--debug` | Reserved for log level (no-op today). |
| `--yes` | Skip interactive prompts. Today only `rdc init`'s wizard is interactive, and it auto-disables on non-TTY stdin. |

**Concurrency.** `rdc pull` lists every kind sequentially (one
`list_*` call per kind), but a queue tree of 25 queues otherwise
needs 50 sequential round-trips for schema + inbox, and a 10-MDH
deploy needs 20 sequential round-trips for regular + search
indexes. With `--concurrency N`, the per-queue and per-collection
sub-fetches are pipelined `N` at a time. Beyond ~5 the gains
flatten because the upstream `list_*` calls remain serial.

```sh
rdc --concurrency 10 pull dev
RDC_CONCURRENCY=10 rdc pull dev   # equivalent
```

**429 handling.** Every Rossum and Data Storage HTTP call retries
on `429 Too Many Requests`: it sleeps for the `Retry-After`
header if present, otherwise exponential backoff (1s, 2s, 4s, 8s,
16s — capped at 60s), up to 5 attempts total. A stderr line is
printed each time so the tool isn't quietly hung. Higher
concurrency makes 429s more likely on busy clusters; the retry
keeps pulls succeeding without intervention.

## Authentication

Tokens are loaded per env, in priority order:

1. Environment variable `RDC_TOKEN_<ENV_UPPER>` (e.g. `RDC_TOKEN_DEV`).
   Recommended for CI.
2. `secrets/<env>.secrets.json` — `{"api_token": "..."}`. Recommended
   locally; add `secrets/` to `.gitignore` (`rdc init` does this).

To set or rotate a token:

```sh
# Validates the token by hitting GET /organizations/{org_id} before
# writing. Writes secrets/<env>.secrets.json with mode 0600 on Unix.
rdc auth dev --token <new-token>

# Or pipe via stdin (token never appears in shell history):
read -s T && echo "$T" | rdc auth dev
```

Loud error if neither is set.

**Master Data Hub** is pulled automatically when the cluster has it
enabled — no extra config. The data storage URL is derived from
`api_base`. Examples:

| `api_base`                                  | derived data storage URL                              |
|---------------------------------------------|-------------------------------------------------------|
| `https://api.elis.rossum.ai/v1`             | `https://elis.rossum.ai/svc/data-storage/api`         |
| `https://customer.rossum.app/api/v1`        | `https://customer.rossum.app/svc/data-storage/api`    |

The API and Data Storage services share the same parent domain on
every Rossum cluster; the API is reached via the `api.` subdomain
(or a `/api` path prefix on clusters that use the bare domain),
while Data Storage sits at the bare parent domain plus
`/svc/data-storage/api`. On clusters without MDH, the first call
returns 404 and rdc skips silently — no `mdh/` directory created,
no count in the summary.

## Repair — recover a broken lockfile

If your `.rdc/state/<env>.lock.json` becomes corrupted or you've
lost it, `rdc repair --rebuild-lock <env>` backs it up (to
`<name>.bak.<unix-ts>`) and runs `rdc pull <env>` from a clean
slate to reconstruct it.

```sh
rdc repair dev --rebuild-lock
# Backed up existing lockfile to .rdc/state/dev.lock.json.bak.1762000000
# Note: rdc pull will now overwrite local snapshot files with remote contents.
# ...
# Lockfile rebuilt for env 'dev'.
```

**Warning:** because the rebuilt pull has no merge base, every
local file is overwritten with what the remote currently has.
Commit your snapshot to git first if you might have unsaved edits.

## Layout cheat sheet

| File | Purpose |
|------|---------|
| `rdc.toml` | Project config: project name, envs (api_base + org_id) |
| `secrets/<env>.secrets.json` | Per-env API token (gitignored) |
| `envs/<env>/_index.md` | Generated inventory; do not edit |
| `envs/<env>/organization.json` | Org metadata (read-only on remote) |
| `envs/<env>/overlay.toml` | Per-env overrides (applied on push, stripped on pull; optional) |
| `envs/<env>/workspaces/<ws>/...` | Workspace + nested queues |
| `envs/<env>/<kind>/<slug>.json` | Org-scoped kinds (hooks, rules, etc.) |
| `.rdc/state/<env>.lock.json` | Merge base; auto-managed |
| `.rdc/map/<src>→<tgt>.toml` | Env-pair mapping for deploy |

## Tests

```sh
cargo test
```
