# rdc

**Rossum Deployment as Code — snapshot, edit, and deploy Rossum configurations reliably.**

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to
disk for AI-assisted local development, lets you edit them in place, and
deploys them across environments.

## Opinionated by design

`rdc` does one thing and tries to do it without surprises. There is **one**
supported workflow:

1. **Pull** an environment into a local snapshot.
2. **Edit** the snapshot (JSON files, extracted `.py` code, formula sidecars).
3. **Push** changes back to that environment, or **deploy** them to another
   environment via `map` / `plan` / `apply`.

That's it. No partial pulls, no per-kind filters, no per-workspace scope
limiters. The whole environment is the unit of work; an `overlay.toml`
captures the per-env divergences (names, runtimes, thresholds) so the
canonical snapshot stays clean.

Defaults are chosen so the tool **just works**: MDH is auto-detected from
`api_base`, server-managed fields like `modified_at` are ignored at the
hash layer, transient API errors retry with backoff, color and progress
bars honor TTY detection, and `rdc push` with no edits exits silently.
If you find yourself reaching for a flag, it's probably either not there
on purpose, or the existing default is the one we'd recommend anyway.

## Capabilities

- **Pull** every kind in scope: organization, workspaces, queues,
  schemas (formula bodies extracted to `formulas/<id>.py`), inboxes,
  hooks (code extracted to `<slug>.py`), rules, labels, engines,
  engine fields, workflows, workflow steps, email templates, and MDH
  collections + indexes. The Data Storage URL is derived from
  `api_base` — no extra config. Per-queue sub-fetches (schema +
  inbox) and per-MDH-collection index fetches are pipelined.
- **Push** locally-edited objects back to the API for hooks, rules,
  labels, queues, schemas (formula round-trip), inboxes, email
  templates, engines, and engine fields. Workflows and workflow
  steps are pull-only (Rossum's API rejects PATCH on these with 405).
- **Deploy** across environments via `rdc map` / `rdc plan` / `rdc
  apply`. Apply rewrites cross-reference URLs from src to tgt, refuses
  to PATCH on tgt drift, and skips no-op deploys (idempotent).
- **Overlays** are bidirectional: applied on push, stripped on pull
  (spec §9.3) so cross-env diffs and deploys stay quiet about
  intentional per-env divergence.
- **Conflict + drift resolver.** Both pull and push open an
  interactive `[k]/[r]/[e]/[s]/[a]` resolver on TTY (spec §8.3 / §7.3
  step 5), including the combined-hash kinds (hooks json+py, schemas
  json+formulas) — one prompt per differing sub-file. CI / non-TTY /
  `--yes` keeps the shadow-file / skip-with-warning flow.
- **Resilience.** Every Rossum and Data Storage HTTP call retries on
  `429 Too Many Requests` and transient 5xx (502/503/504) with
  `Retry-After` / exponential backoff (up to 5 attempts).
- **Auxiliary commands.** `rdc init` accepts flags or runs an
  interactive wizard; `rdc status` is a read-only health check;
  `rdc diff` shows unified diffs (local vs remote, or two snapshots);
  `rdc auth` sets/refreshes tokens; `rdc repair --rebuild-lock`
  recovers a corrupted lockfile.
- **Distribution.** Single static binary via `curl | sh` (pre-built
  for darwin x86_64/aarch64 + linux x86_64) or `cargo install`.
- **AI-friendly snapshot.** `_index.md` includes a per-kind inventory
  plus a cross-references section that resolves `hook → queues`,
  `rule → queues`, and `email_template → queue` so AI agents can
  navigate without parsing every JSON.

See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full
design.

## Upgrade

Keep rdc current with one command:

```sh
rdc upgrade
```

This downloads the latest GitHub release for your platform, runs a
sanity-check `--version` on the new binary, and swaps it in atomically.
The previous binary is kept at `<install_dir>/rdc.bak` for one-shot
rollback (`mv rdc.bak rdc`).

`rdc upgrade --check` reports the latest available version without
installing. `rdc upgrade --version v0.0.2` pins to a specific tag —
useful as an emergency downgrade, but you may need to re-pull
afterward (see "Compatibility").

**Passive nudge.** Every command does a once-daily check against the
GitHub Releases API in the background. If a newer release exists, rdc
prints a one-line `note: rdc vX is available — run \`rdc upgrade\` to install`
at the top of the command. The check is best-effort: network errors,
API rate-limits, or unreachable cluster all fail silently — the nudge
just doesn't appear. Cache lives at `$XDG_CACHE_HOME/rdc/update.json`
(fallback `~/.cache/rdc/update.json`).

**Install-location detection.** `rdc upgrade` only self-replaces when
it's safe to do so:

| Install method | `rdc upgrade` behavior |
|---|---|
| `install.sh` / manual binary in a writable dir | Self-replaces atomically; previous binary kept as `rdc.bak`. |
| `cargo install --git …` | Refuses — would break cargo's bookkeeping. Prints the right `cargo install --force` invocation instead. |
| Read-only dir (`/usr/local/bin`, system package manager, etc.) | Refuses — prints the manual download URL + commands. |

**Self-replace is safe while rdc is running.** On Linux/macOS the
kernel keeps the running binary's inode alive after its directory
entry is replaced, so the in-flight `rdc upgrade` process completes
normally. The swap uses a copy-aside + atomic rename pattern so
`<install_dir>/rdc` is always a valid binary — a parallel shell tab
running `rdc` during the upgrade never sees a missing file.

## Compatibility

- **Backward compat (new binary, old artifacts):** the latest rdc
  always reads anything produced by any previous release. Lockfile
  versions migrate forward; project config and overlay tolerate
  missing fields via serde defaults.
- **Forward compat (older binary, newer artifacts):** not promised.
  But artifacts an older binary doesn't understand produce a clear
  error pointing at `rdc upgrade`, never silent corruption. For
  example, a downgrade-then-pull scenario where the lockfile was
  written by a newer rdc errors with: *"lockfile … was written by a
  newer rdc (lockfile version N, this rdc supports up to version M).
  Run `rdc upgrade` to install a matching binary."*
- **Downgrades.** `rdc upgrade --version <older>` is an emergency
  escape hatch. We don't promise the older binary can still read your
  snapshot or lockfile cleanly — you may have to delete the lockfile
  and re-pull.

### Upgrading from older lockfiles

A previous release changed how `content_hash` is computed: server-managed
fields (`modified_at`, `modifier`) are now stripped before hashing. The
first pull on a lockfile written before that change can surface
false-positive conflicts on every object.

To clear the storm without resolving each conflict by hand:

```sh
rdc repair --rebuild-lock <env>
```

Subsequent pulls will be clean. Real edits made before re-baselining
remain visible — `repair` only resets the hash; it does not discard
local edits.

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

Sample pull output (TTY):

```
⠁ workspaces  listing…
✓ workspaces: 12 items, 2.1s
⠁ queues      listing…
✓ queues: 23 items, 2 orphans skipped, 4.7s
⠁ hooks       listing…
✓ hooks: 30 items, 3.4s
…
✓ pull envs/dev: 256 items, 2 orphans skipped, 0 conflicts  18.6s
```

In non-TTY mode (CI / piped output), spinners are replaced with `→ kind: listing…` lines.

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

`rdc push <env>` runs in two phases: first it scans all local files against
the lockfile hashes (phase 1), then PATCHes only the changed objects (phase 2).
If nothing has changed, push exits immediately with a single summary line.

Sample push output (TTY):

```
⠁ push envs/dev  listing…
✓ push envs/dev: 256 files scanned, 1 changed
⠁ hooks  listing…
✓ hooks: 1 patched, 0.6s
✓ push envs/dev: 1 patched  1.0s
```

In non-TTY mode (CI / piped output), spinners are replaced with `→ kind: listing…` lines.

Each candidate is compared against the lockfile's `content_hash`:

- **No local edits** → skipped silently.
- **Local edits, remote unchanged since last pull** → PATCH succeeds.
- **Local edits AND remote drifted** → resolver opens (TTY) or skip
  + warning (non-TTY / `--yes`). See "Push drift resolver" below.

After a successful push, the local file is rewritten with the server's
authoritative response so the lockfile hash matches the file bytes.
Subsequent pulls are idempotent.

### Push drift resolver

When a remote object has changed since you last pulled, push uses the
same `[k]/[r]/[e]/[s]/[a]` shape as pull but with push-side semantics
(spec §7.3 step 5):

| Choice | Effect |
|--------|--------|
| `k` | **Force-push.** Send the local payload to the API anyway, overwriting the remote drift. |
| `r` | **Adopt remote.** Write the remote bytes to the local file and update the lockfile to match. No PATCH. Your local edit is discarded. |
| `e` | **Edit then force-push.** Open `$EDITOR` on a temp file with conflict markers; the saved bytes become the PATCH payload. |
| `s` | **Skip.** Leave both local and remote alone — same as the non-TTY fallback. |
| `a` | **Abort.** Stop the push entirely; lockfile is not saved. Re-running picks up where you left off. |

Without a TTY (or with `--yes`), every drift falls back to `s` (skip
with warning) — same as before.

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

**Noise-field suppression:** Server-managed fields (`modified_at`, `modifier`)
no longer contribute to the `content_hash`. A re-pull where only those fields
have changed is a no-op — the bar shows zero conflicts and the on-disk file
is unchanged. This eliminates a class of spurious conflicts that previously
affected all lockfiles written before this algorithm change.

`rdc pull` does three-way merge for every kind:

| Local edited? | Remote changed? | Action |
|---|---|---|
| no | no | (no-op) |
| no | yes | write the remote |
| yes | no | keep local |
| yes | yes | **conflict** — see resolver below |

For schemas, the "combined hash" covers `schema.json` plus every
`formulas/<id>.py` file, so a formula-only edit is detected correctly.
For hooks, the combined hash covers `<slug>.json` plus `<slug>.py`.

### Interactive resolver (TTY)

When stdin is a TTY and `--yes` was not passed, `rdc pull` opens an
inline resolver per conflict (spec §8.3):

```text
[1/3]  envs/dev/labels/audit-hold.json — conflict

--- local
+++ remote
@@ -1,7 +1,7 @@
 {
   "id": 9931,
-  "name": "Audit hold (LOCAL EDIT)",
+  "name": "Audit hold",
   ...

[k]eep local  [r]emote  [e]dit  [s]kip (shadow file)  [a]bort >
```

| Choice | Effect |
|--------|--------|
| `k` | Keep local. No write. Lockfile records the local hash. |
| `r` | Overwrite local with the remote bytes. Lockfile records the remote hash. |
| `e` | Open `$EDITOR` on a temp file containing git-style conflict markers. Saved bytes become the new local + lockfile hash. |
| `s` | Skip — fall back to the shadow-file behavior (writes `<file>.remote`, keeps local). |
| `a` | Abort the entire pull. The lockfile is **not** saved; nothing else is written from this point on. |

The resolver covers every kind that uses a single JSON file (queues,
inboxes, rules, labels, engines, engine fields, workflows, workflow
steps, email templates, MDH metadata) **and** combined-hash kinds
(hooks `.json` + `.py`, schemas `schema.json` + `formulas/*.py`). For
combined-hash kinds the resolver walks each sub-file in turn — you
can keep the local JSON but take the remote `.py`, for example. The
`[N/M]` header reflects the per-entity sub-file count (e.g. `[2/2]`
for the second of two files in a hook). One niche case stays on the
shadow-file flow even on TTY: a hook that adds/removes its `.py` file
between local and remote, or a schema whose formula set differs (one
side added a formula the other doesn't have). Add/remove decisions
aren't `[k]/[r]/[e]` shaped; resolve them by editing locally and
re-running pull.

### Conflict colors

When run in a TTY, `rdc pull` colorizes conflict prompts: the header is
bold yellow, `-` (local) lines are red, `+` (remote) lines are green,
hunk markers (`@@`) are cyan, and action letters (`[k]/[r]/[e]/[s]/[a]`)
are bold cyan. To force plain output, set `NO_COLOR=1` or pass `--no-color`.

### Non-interactive / CI (`--yes` or non-TTY)

Without a TTY (CI, output piped, or `--yes`), conflicts fall back to
the legacy shadow-file behavior: the local file is preserved and the
remote is written to `<file>.remote` next to it. A per-conflict
warning goes to stderr and the count appears in the summary line.

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
- **On pull** (spec §9.3), they're stripped from the snapshot
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

## Map — align slugs

`rdc map` has two modes; the verb is the same ("align slugs"), the
scope is set by how many envs you name:

### `rdc map <env>` — within-env

Slugs in the snapshot are sticky to the Rossum object ID: once a hook
has `validator-invoices` as its slug, that slug stays there even if
the hook is later renamed on the remote. This is intentional — cross-
references stay valid, pull stays idempotent, file paths don't churn.

The trade-off is cosmetic staleness: `hooks/validator-invoices.json`
whose JSON says `"name": "Validator Invoices v2"`.

`rdc map <env>` is the explicit user-driven action that brings stale
slugs into alignment. **Pull never moves files** — it stays clean for
review. When you're ready to commit the renames in a single, easy-to-
review diff, run:

```sh
rdc map dev               # interactive: y/N per rename
rdc map dev --yes         # apply all without prompting
rdc map dev --check       # list pending without modifying anything
```

The command is **cascade-aware**:

- Renaming a workspace moves the entire `workspaces/<old>/` directory
  to `<new>/` (one OS call, all children come along) and rewrites the
  leading segment of every `email_templates` compound key in the
  lockfile.
- Renaming a queue moves the queue directory (schema.json, inbox.json,
  formulas/, email-templates/ all move with it) and rewrites the
  `queues`, `schemas`, `inboxes` lockfile entries plus the middle
  segment of every `email_templates` compound key under that queue.
- Renaming a hook moves both `<slug>.json` and `<slug>.py`.
- Renaming refuses (and warns) when the new slug would collide with
  another existing slug.

Pull surfaces pending renames at the end of its summary; `rdc status`
lists each pending rename per env.

If `overlay.toml` or `.rdc/map/*.toml` reference an old slug, the
command **warns but does not modify** those files — they're user-
authored configs.

### `rdc map <src> <tgt>` — cross-env

When you've validated changes in a TEST env and want to ship them to
PROD, use the deploy commands.

`rdc map <src> <tgt>` — auto-match objects by slug between two envs
and write `.rdc/map/<src>→<tgt>.toml`. Re-runnable; existing entries
are preserved, new auto-matches are added. `--check` dry-runs without
writing.

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

**Apply is conservative and idempotent:**
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
| `--no-color` | Disable ANSI color output. Also honored via the `NO_COLOR` environment variable. |
| `--yes` | Skip interactive prompts (conflict resolver, init wizard). Auto-enabled when stdin isn't a TTY. |

**Transient-error handling.** Every Rossum and Data Storage HTTP
call retries automatically on:

- `429 Too Many Requests` — honors the `Retry-After` header if the
  server provides one.
- `502 Bad Gateway`, `503 Service Unavailable`, `504 Gateway
  Timeout` — transient infrastructure errors.

Up to 5 attempts total, with exponential backoff (1s, 2s, 4s, 8s,
16s — capped at 60s) between attempts. A stderr line is printed
each time so the tool isn't quietly hung. `500 Internal Server
Error` is **not** retried — it usually indicates a real server
bug and retrying papers over it. Other 4xx codes (auth, permission,
not-found, method) are returned to the caller as-is.

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
