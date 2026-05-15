# rdc

**Rossum Deployment as Code — snapshot, edit, deploy.**

`rdc` is the tool for managing Rossum.ai configurations. Sync an environment into a local snapshot, edit it in place, sync again to send the changes back, promote them to another env. One verb per phase. Idempotent re-runs do nothing. There is no second tool to learn.

```sh
rdc sync test
$EDITOR envs/test/hooks/validator-invoices.py
rdc sync test
rdc deploy test prod
```

That's the whole loop.

## Goals

- **Make Rossum configurations editable like code.** Every workspace, queue, schema, hook, rule, label, email template, engine, and MDH dataset lives on disk as plain JSON plus extracted Python — diffable, reviewable, version-controllable.
- **Make cross-env promotion a single command.** `rdc deploy test prod` bootstraps a fresh target, patches an existing one, rewrites every cross-reference URL on the wire, and is idempotent on subsequent runs. No `map` / `plan` / `apply` triage.
- **Make AI-assisted editing first-class.** A regenerated `_index.md` per environment gives agents a single entry point to navigate the whole snapshot without parsing every JSON file by hand.

## Principles

These are the rules `rdc` follows so you don't have to think about them:

- **Just works.** Defaults are the recommendations. If you reach for a flag, it's probably either not there on purpose, or the existing default is the one to use.
- **Plan before apply.** Every command that touches the remote shows what it will do, asks once on a TTY, and accepts `--yes` for CI. `--dry-run` runs the same code paths without writing anywhere.
- **Idempotent everywhere.** Re-running `rdc sync` on a converged environment writes nothing. Re-running `rdc deploy` on a synced env is zero API calls.
- **The environment is the unit of work.** No partial syncs, no per-kind filters, no per-workspace scope limiters. Whole envs in, whole envs out.
- **Snapshot is canonical, including absence.** The on-disk files are the source you edit; remote is reconciled toward them. Removing a local file is the declarative way to delete the corresponding remote object — `rdc sync` will offer to make it so, with an explicit confirmation gate. Per-env divergences (display names, runtimes, thresholds) live in `overlay.toml` so the canonical snapshot stays clean.
- **Cross-references resolve automatically.** When `rdc deploy` POSTs a hook to PROD that references TEST queue URLs, the body sent has those URLs rewritten to the matching PROD queue URLs. The user never sees this.
- **Errors are actionable.** A missing `rdc.toml` says "not an rdc project — run `rdc init` here." A drifted target says "run `rdc sync <env>` first." A failed token says "Invalid token (401)."
- **Atomic on disk.** All writes go through a temp-file rename. A crash mid-write never leaves a half-written JSON.
- **Resilient on the wire.** Transient HTTP errors (`429`, `502`, `503`, `504`) retry with exponential backoff up to 5 attempts.

## Install

Single binary via `curl | sh` (macOS + Linux x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/mrtnzlml/rossum-deployment-manager-experiment/main/install.sh | sh
```

Downloads the matching pre-built release into `~/.local/bin/rdc`. Add that directory to your `PATH` if it isn't already.

Pin to a specific version:

```sh
curl -fsSL https://raw.githubusercontent.com/mrtnzlml/rossum-deployment-manager-experiment/main/install.sh | sh -s -- v0.0.1
```

Windows (PowerShell):

```powershell
$dest = "$env:USERPROFILE\.rdc\bin"
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Invoke-WebRequest -Uri "https://github.com/mrtnzlml/rossum-deployment-manager-experiment/releases/latest/download/rdc-x86_64-pc-windows-msvc.tar.gz" -OutFile "$env:TEMP\rdc.tar.gz"
tar -xzf "$env:TEMP\rdc.tar.gz" -C $dest
[Environment]::SetEnvironmentVariable("Path", "$env:Path;$dest", "User")
```

From source:

```sh
cargo install --git https://github.com/mrtnzlml/rossum-deployment-manager-experiment
```

Supported pre-built platforms: macOS (Intel + Apple Silicon), Linux x86_64, Windows x86_64. For Linux aarch64 / Windows aarch64 / other, build from source.

## A 60-second tour

```sh
mkdir my-rossum-project && cd my-rossum-project

# Bootstrap. Repeatable: re-running adds new envs without disturbing the existing ones.
rdc init --env test=https://your-org.rossum.app/api/v1:123456 \
         --env prod=https://your-org.rossum.app/api/v1:789012

# Set tokens. Validated against the Rossum API before writing the 0600-mode secrets file.
rdc auth test --token <test-token>
rdc auth prod --token <prod-token>

# Sync both envs into local snapshots.
rdc sync test
rdc sync prod
```

`envs/test/` now contains the complete snapshot:

```
envs/test/
├── _index.md                   ← generated inventory + cross-references
├── organization.json
├── workspaces/
│   └── invoices-ap/
│       ├── workspace.json
│       └── queues/
│           └── cost-invoices/
│               ├── queue.json
│               ├── schema.json
│               ├── inbox.json
│               ├── formulas/
│               │   └── amount_total.py
│               └── email-templates/
│                   └── default-rejection-template.json
├── hooks/
│   ├── validator-invoices.json
│   └── validator-invoices.py    ← extracted from config.code
├── rules/
│   ├── validate-totals.json
│   └── validate-totals.py       ← extracted from trigger_condition
├── labels/
└── mdh/                         ← only on clusters with MDH
    └── customers/
        ├── collection.json
        └── indexes.json
```

Edit a hook's Python:

```sh
$EDITOR envs/test/hooks/validator-invoices.py
```

Preview what would change:

```sh
$ rdc sync test --dry-run
→ rdc sync test (dry run)

would push
  ⊙ hooks/validator-invoices → PATCH

✔ Dry run: 1 would push, 0 would pull, 0 would prompt (no writes)
```

Send the edit to test:

```sh
$ rdc sync test
✓ sync envs/test: 1 patched, 0.6s
```

Promote everything that's diverged from prod into prod:

```sh
$ rdc deploy test prod
Plan: test → prod
  + create:  (none)
  ~ update:  field-level deltas

Proceed? [y/N] y
Applied 1 hooks (1 PATCHes) from test to prod
Deployed test → prod: 0 created, 0 deleted, 2 API calls, 1.4s
```

Re-running yields `0 PATCHes`. The whole environment is now in sync.

## Mental model

Three local primitives plus one remote source of truth. Knowing them makes everything else obvious.

- **`envs/<env>/`** — the **snapshot**. Plain JSON files plus extracted `.py` code. This is what you edit. `rdc sync` reconciles snapshot ↔ remote in one pass; `rdc deploy` reconciles snapshot → remote across envs.
- **`.rdc/state/<env>.lock.json`** — the **lockfile**. Per-object `content_hash` from the last successful sync. Serves as the merge base for three-way comparison. Auto-managed; commit to git alongside the snapshot.
- **`envs/<env>/overlay.toml`** — the **overlay**. Per-env values applied to outbound bodies, stripped from inbound bodies. Optional, but the right tool for divergences like "PROD's hooks use `python3.12-secure` while TEST's use `python3.12`."
- **The remote API** — the source of truth for what's actually running.

`rdc sync` is the only command for local ↔ remote reconciliation. Pull-side and push-side actions happen in one invocation; the snapshot stays canonical (any skipped conflict preserves local).

Cross-references between resources are URL-based. `rdc deploy` rewrites them automatically when moving objects between envs, using the slug-to-slug mapping stored in `.rdc/map/<src>-to-<tgt>.toml` (built silently on first deploy; hand-edit for renames).

## Commands

| Command | What it does |
|---|---|
| `rdc init` | Create a new project, or add an env to an existing one. |
| `rdc auth <env>` | Set/refresh the API token for `<env>`. Validates before writing. |
| `rdc sync <env>` | Reconcile snapshot ↔ remote in one pass. Pulls remote changes, sends local edits, three-way merges on conflict. `--no-push` (audit mode) and `--no-pull` (deploy mode) restrict the direction for CI. `--allow-deletes` gates DELETEs from tombstones. `--watch` runs a foreground continuous sync. |
| `rdc deploy <src> <tgt>` | One-shot cross-env promotion. Plan-then-apply with confirmation. |
| `rdc diff <env>` | Local-vs-remote diff (one GET per edited object). |
| `rdc diff <a> <b>` | Two snapshots, no API calls. |
| `rdc repair <env>` | Local-state surgery. `--rebuild-lock` re-pulls; `--rename-slugs` realigns stale filenames. |
| `rdc upgrade` | Self-update the binary. |

Every command that writes to the remote (`sync`, `deploy`) takes `--dry-run` to print the plan without sending anything.

## Edit the snapshot

Most files are plain JSON; open them in your editor and save. After any edit, `rdc sync <env> --dry-run` lists the changed files and `rdc sync <env>` sends them.

For objects with extracted code, the on-disk layout splits one logical object into two files. The JSON describes the object; the sidecar carries the code. **Always edit the sidecar** — sync strips the inlined field on the pull-side write and splices it back in for the push-side write.

| Kind | On-disk files |
|---|---|
| Hook with Python | `hooks/<slug>.json` + `hooks/<slug>.py` |
| Hook with Node.js | `hooks/<slug>.json` + `hooks/<slug>.js` |
| Rule with trigger | `rules/<slug>.json` + `rules/<slug>.py` |
| Schema with formulas | `workspaces/<ws>/queues/<q>/schema.json` + `…/formulas/<field-id>.py` |

The hook sidecar extension follows `config.runtime` in the JSON — `.js` when the runtime starts with `node` (e.g. `nodejs18.x`, `nodejs20.x`), `.py` otherwise (e.g. `python3.12`, `python3.12-secure`). Change the runtime in JSON and the next pull renames the sidecar automatically; rules and schema formulas are always Python.

### Create a new object

Author the JSON (and any `.py`) directly. Omit `id` and `url` — the server assigns them. Sync detects "local file exists, no lockfile entry" and POSTs:

```sh
$ cat > envs/test/labels/audit-hold.json <<'JSON'
{ "name": "Audit hold", "organization": "https://your-org.rossum.app/api/v1/organizations/123456" }
JSON
$ rdc sync test
created labels/audit-hold (id 10198)
```

The server's response (with `id`, `url`, timestamps) is written back to disk in canonical form, and the lockfile records the new object.

Supported kinds for create: workspaces, schemas, queues, inboxes, hooks, rules, labels, engines, engine fields, email templates. Workflows and workflow steps are read-only at the Rossum API.

### Store extensions

Extensions installed via the Rossum store (Master Data Hub, Email
Notifications, Document Splitting, …) live under `hooks/` like every
other hook, marked by `"extension_source": "rossum_store"` in the JSON.

`rdc sync` round-trips the full server body — including the
template-managed `description`, `guide`, `settings_schema`, and the
private webhook `config`. Diffs and edits work the same way as for
regular hooks; only `settings`, `queues`, `active`, `run_after`,
`metadata`, and customer-set `config.*` values are typically meant to
diverge between environments.

`rdc sync` knows to use the Rossum store install endpoint
(`POST /hooks/create`) when creating one on a fresh environment, then
PATCHes any customisations from the snapshot. If a previous sync left
an orphan (install succeeded, PATCH failed), the next sync detects it
by `(name, hook_template)` and resumes without creating a duplicate.

`rdc deploy` resolves the target cluster's matching `hook_template`
URL by `(name, type, extension_source)` match against
`GET /hook_templates` on the target, caching the pair in
`.rdc/map/<src>-to-<tgt>.toml` under `[hook_templates]`. The first deploy
that needs a `token_owner` on the target also prompts you to pick the
target's service-account user from a list (ranked with
`system_user__*` first); your choice is saved to
`envs/<tgt>/overlay.toml`. Subsequent deploys read the overlay
non-interactively.

Update, delete, drift detection, and conflict resolution are identical
to a regular hook.

For automated CI deploys, set `token_owner` in
`envs/<env>/overlay.toml` ahead of time — either per-hook:

```toml
[hooks.master-data-hub]
"token_owner" = "https://prod.rossum.app/api/v1/users/<system-user-id>"
```

or as a shared default for every store extension in the env:

```toml
[defaults]
store_extension_token_owner = "https://prod.rossum.app/api/v1/users/<system-user-id>"
```

`rdc deploy --yes` refuses to start when neither is present, listing
the missing hook(s) and the file to edit.

If the source cluster has a store template that's not available on the
target cluster (e.g., a `request_access` template the target org hasn't
been onboarded to), `rdc deploy` refuses with an actionable error
naming the template — install it manually via the Rossum UI on the
target and re-run `rdc sync` first.

### Delete an object

Removing the local file (and committing that removal to your repo) is the declarative way to say "delete this from remote." The lockfile entry remains, which is rdc's signal that the object *was* tracked and is now meant to be gone. `rdc sync <env>` discovers these tombstones in its scan phase and, after confirmation, issues `DELETE /<kind>/<id>` for each in reverse dependency order.

```sh
$ rm envs/test/labels/audit-hold.json
$ rdc sync test --dry-run
→ rdc sync test (dry run)

would push
  ⊙ labels/audit-hold → DELETE

✔ Dry run: 1 would push, 0 would pull, 0 would prompt (no writes)

$ rdc sync test
✓ sync envs/test: 261 files scanned, 0 changed, 1 to delete

⚠️  The following 1 object(s) would be DELETED from the remote:
  - labels/audit-hold (id 10198)

Proceed with deletion? [y/N] y
  - labels/audit-hold: deleted
✓ deletes: 1 removed, 0 skipped
```

The destructive section needs **two intentional acts** — `--yes` does not bypass:

1. Removing the local file (act 1).
2. Either answering `y` to the prompt on a TTY, or passing `--allow-deletes` (act 2).

Non-TTY runs (CI) without `--allow-deletes` refuse the sync and list the tombstones, pointing at the flag. This is intentional: a typoed `rm -rf envs/` in a script shouldn't quietly wipe production.

Per-object drift check: if the remote's `modified_at` has changed since the last `rdc sync`, the resolver opens — `[k]` keep delete (force DELETE despite the drift), `[s]` skip (leave alone), `[a]` abort. Non-TTY drift defaults to skip-with-warning.

Cascade order is reverse of creates: deleting a workspace directory removes its queues, schemas, inboxes, email templates, hooks attached to those queues, and so on before the workspace itself. The dry-run preview shows the exact sequence.

```sh
rdc sync test --dry-run --diff   # preview deletes line-by-line
```

`rdc sync --dry-run --diff` fetches each tombstone's remote and renders it as a deleted-file diff (`+++ /dev/null`) so you can audit exactly what would disappear.

Supported kinds for delete: same as create, minus the read-only kinds (workflows, workflow steps, MDH datasets). A tombstone for those is silently ignored.

### Preview a sync

```sh
rdc sync test --dry-run
```

Lists every changed file and which kind would receive a POST/PATCH/DELETE — no API calls made by default. Add `--diff` to also fetch the current remote per object and print unified diffs (and full deleted-body / would-be-POST-body previews).

## Watch mode

`rdc sync --watch <env>` runs a foreground continuous sync. Save a file in `envs/<env>/`; the daemon pushes the change within a second or two. Run `rdc sync` from another shell, or edit via the Rossum UI; the daemon pulls within the configured poll interval (default `60s`).

```sh
$ rdc sync --watch test
watching envs/test/ ...
polling test every 60s ...

[14:02:17] → cycle: pushed 1, pulled 0, conflicts 0, deletes 0 (0.6s)
[14:05:41] ← cycle: pushed 0, pulled 1, conflicts 0, deletes 0 (0.4s)
[14:09:03] hooks/finance-totals — conflict
  local has changes:
    - threshold: 0.85
    + threshold: 0.95
  test has changes:
    - threshold: 0.85
    + threshold: 0.80
  [k] keep local  [r] use test  [e] edit  [s] skip  [a] abort > k
[14:09:21] → cycle: pushed 1, pulled 0, conflicts 1, deletes 0 (0.4s)
```

Ctrl-C stops the watch. The daemon stays foreground — it doesn't fork; close the terminal tab to stop it. `--no-poll` disables remote polling (file events only). `--poll-interval 30s` tunes the cadence.

While the watch is running, you can still run `rdc sync test`, `rdc deploy test prod`, etc. from other shells — they coordinate via an advisory lock and wait briefly if the watch is mid-cycle.

Conflicts and destructive deletes prompt inline in the watch terminal. Non-destructive cycles auto-apply silently. `-v` prints every cycle including idle polls.

## Promote test → prod

The one-line answer is `rdc deploy test prod`. The details:

```
Plan: test → prod
  + create:  4 workspaces, 24 schemas, 24 queues, 27 hooks, 1 rule, 46 labels
  ~ update:  field-level deltas (resolved at execute time)

Proceed? [y/N] y

  → workspaces       4 created
  → schemas         24 created
  → queues          24 created
  → hooks           27 created
  → rules            1 created
  → labels          46 created
Applied 22 hooks, 0 rules, 0 labels, ... (22 PATCHes) from test to prod

Deployed test → prod: 126 created, 0 deleted, 144 API calls, 89.1s
```

What's happening inside:

1. **Auto-mapping.** Same-slug objects in `test` and `prod` are paired silently. Hand-curated renames in `.rdc/map/test-to-prod.toml` are preserved.
2. **Plan.** What would be created, what would be patched, what would be deleted (`--mirror` only).
3. **Confirm.** TTY prompts; CI passes `--yes`.
4. **Create.** Dependency order: `workspaces → schemas → queues → inboxes → email_templates → hooks → rules → labels → engines → engine_fields`. POSTing each missing resource. Each create updates an in-memory mapping so the next kind's URL rewriter knows where the just-created peers live.
5. **Update.** Per-kind PATCH sweep. Fetches the tgt remote for drift check, normalises both sides (strip env-specific `id` / `url` / `organization` + noise fields, sort set-like arrays), and PATCHes only when content differs. Re-running yields `0 PATCHes`.
6. **Delete** (with `--mirror`). Reverse dependency order. Two confirmations: one for the deploy as a whole, a second specifically for the destructive section.

### Preview a deploy

```sh
rdc deploy test prod --dry-run
```

Traces the same code paths — drift checks, URL rewrites, overlay application, idempotency comparison — but suppresses every actual POST/PATCH/DELETE. The wording switches to "would be created" so you can't mistake the report for a real run.

### Mirror mode

```sh
rdc deploy test prod --mirror
```

Adds a `- delete:` section to the plan. Objects in `tgt` without a matching slug in `src` are removed (reverse dependency order, children first). `--yes` does **not** bypass the mirror confirmation — it's asked separately because the deletions are irreversible.

### Selective deployment

Deploy only part of the snapshot by passing one or more `--only <selector>` flags:

```sh
rdc deploy test prod --only hooks/validator-invoices
rdc deploy test prod --only 'schemas/cost-*' --only queues/cost-invoices
rdc deploy test prod --only '*/cost-invoices'
```

Selector forms:

| Form | Example | Matches |
|---|---|---|
| `<kind>/<slug>` | `hooks/validator-invoices` | exact `(kind, slug)` |
| `<kind>/<glob>` | `schemas/cost-*` | `*` glob in the slug segment |
| `email_templates/<ws>/<q>/<tpl>` | `email_templates/main/cost-invoices/rejection` | email template by compound key |
| `*/<glob>` | `*/cost-invoices` | any kind whose slug matches |

The selection applies to every phase (creates, updates, deletes). A selector that matches zero objects errors out so typos can't produce silent no-ops.

If a selected object references a peer (e.g. a hook references a queue) that isn't in the selection and isn't already on the target env, rdc prompts on TTY to include the missing peer, or refuses with an actionable error on `--yes`/CI.

The mapping file at `.rdc/map/<src>-to-<tgt>.toml` is untouched — `--only` is a one-shot scope filter, not a way to edit cross-env pairings.

### Cross-references handled automatically

When a hook in `src` references `https://test.rossum.app/api/v1/queues/600`, the body sent to `tgt` has that URL rewritten to `https://prod.rossum.app/api/v1/queues/<prod-queue-id>`. Same mechanism for `queue.workspace`, `queue.schema`, `email_template.queue`, `rule.queues`, `hook.run_after`. Strings that don't match a known src object are left alone.

Server-computed back-references like `queue.hooks` or `email_template.triggers` are stripped before sending — they're populated by the API based on each child's parent URL, not by client input.

### Auto-created peers

`POST /queues` triggers Rossum to auto-create five default email templates per queue. Deploy lists them right after each queue POST, captures them into the tgt lockfile, and the later update sweep PATCHes them with src-side customisations. You don't see this; it just works.

### Cross-env diff

`rdc diff <src> <tgt>` reuses the same machinery to keep the output focused on semantic drift. It strips env-specific noise (`id`, `url`, `organization`, server timestamps, computed back-refs) on both sides, and — if `.rdc/map/<src>-to-<tgt>.toml` exists — rewrites cross-reference URLs on the src side into their tgt form using the saved mapping. Same-content objects compare equal; deploy-managed ID and URL changes are silent. Without the mapping file the diff still strips top-level noise but cross-reference URLs (`hook.queues`, `queue.workspace`, …) show as differences.

## Overlays — per-env values

Some values are intrinsically per-env: a friendly display name, a hardened runtime version, a webhook URL pointing at the env's own observability endpoint. `envs/<env>/overlay.toml` declares them once:

```toml
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"config.runtime" = "python3.12-secure"

[schemas.cost-invoices]
"settings.default_score_threshold" = 0.95

[queues.cost-invoices]
"automation_level" = "always"
```

Bidirectional:

- **Outbound (sync push-side, deploy)**, overlay values are merged into the body sent to the env. PROD ends up with `"Validator (PROD)"`.
- **Inbound (sync pull-side)**, overlay values are stripped before write. The on-disk JSON shows the canonical pre-overlay form, so `rdc diff test prod` stays quiet about intentional divergences.

The lockfile records the stripped hash, so subsequent syncs are idempotent.

Sections: `[hooks.<slug>]`, `[rules.<slug>]`, `[labels.<slug>]`, `[schemas.<queue-slug>]`, `[queues.<queue-slug>]`, `[inboxes.<queue-slug>]`, `[engines.<slug>]`, `[engine_fields.<slug>]`, `[email_templates."<ws>/<q>/<template>"]`.

If you add an overlay after a sync, run `rdc sync` once more to re-baseline — the lockfile's pre-strip hash won't match the new post-strip form otherwise.

## Conflicts & drift

A three-way merge runs on every sync. When both local and the env have diverged since the last successful sync, an inline resolver opens on TTY. Labels are env-aware so the asymmetry is concrete — the other side is named after the env you're syncing with, never the abstract word "remote":

```
[1/3]  envs/test/labels/audit-hold.json — conflict

--- local
+++ test
@@ -1,7 +1,7 @@
 {
   "id": 9931,
-  "name": "Audit hold (LOCAL EDIT)",
+  "name": "Audit hold",
   …

[k] keep local  [r] use test  [e] edit  [s] skip  [a] abort >
```

| Choice | Effect |
|---|---|
| `k` | Keep local. PATCH the env with local bytes; lockfile records the local hash. |
| `r` | Overwrite local with the env's bytes; lockfile records the env hash. |
| `e` | Open `$EDITOR` on git-style conflict markers. The saved bytes go to both sides; lockfile records the saved hash. |
| `s` | Skip. Local kept; the env's bytes land at `<file>.<env-name>` for review. |
| `a` | Abort. Nothing else writes from this point. |

When `--no-push` is active (audit mode) the `[k]` choice is hidden — nothing local can be sent. When `--no-pull` is active (deploy mode) the `[r]` choice is hidden — nothing local can be overwritten.

CI / non-TTY / `--yes` falls back to `[s]`: local stays on disk, the env's bytes land at `<file>.<env-name>`, summary lists the count. Resolve by editing locally and re-running.

## Recover from drift

`rdc repair <env>` is the umbrella for local-state surgery. Pick one mode — neither runs implicitly because both touch on-disk files in irreversible ways:

| Flag | What it does | Online? |
|---|---|---|
| `--rebuild-lock` | Back up the existing lockfile and re-sync from scratch. Used after lockfile corruption or a hash-input change in a new `rdc` release. **Local edits are lost.** | yes |
| `--rename-slugs` | Rename any local file whose slug no longer matches its JSON `name`. Cascade-aware: renaming a workspace moves the whole subtree, renaming a queue carries its schema / inbox / formulas / email-templates along. | no |

Slugs in the snapshot are sticky to the Rossum object ID: once a hook has `validator-invoices` as its slug, that slug stays there even if the hook is later renamed on the remote. This is intentional — cross-references stay valid, sync stays idempotent, file paths don't churn. `rdc repair <env> --rename-slugs` is the explicit user-driven action that brings stale slugs into alignment when you're ready to commit the renames in a reviewable diff.

```sh
rdc repair test --rename-slugs            # interactive: y/N per rename
rdc repair test --rename-slugs --yes      # apply all without prompting
rdc repair test --rename-slugs --check    # list pending, no writes
```

Sync surfaces pending renames in its summary.

## File layout

| Path | Purpose |
|---|---|
| `rdc.toml` | Project config: name + per-env `api_base` and `org_id` |
| `secrets/<env>.secrets.json` | Per-env API token. Gitignored. Mode 0600 on Unix. |
| `envs/<env>/_index.md` | Generated inventory of every object with names, paths, and cross-references. Don't edit. |
| `envs/<env>/organization.json` | Org metadata (read-only on remote) |
| `envs/<env>/overlay.toml` | Per-env overrides. Optional. |
| `envs/<env>/workspaces/<ws>/...` | Workspace + nested queues / schemas / formulas / email templates |
| `envs/<env>/<kind>/<slug>.json` | Org-scoped kinds (hooks, rules, labels, engines, …) |
| `envs/<env>/mdh/<dataset>/{collection,indexes}.json` | Master Data Hub (on clusters that have it) |
| `.rdc/state/<env>.lock.json` | Merge base. Auto-managed. |
| `.rdc/map/<src>-to-<tgt>.toml` | Slug-to-slug mapping built by `rdc deploy`. Hand-edit for cross-env renames. |

## Authentication

Token resolution per env, in priority order:

1. `RDC_TOKEN_<ENV_UPPER>` environment variable (e.g. `RDC_TOKEN_TEST`). Recommended for CI.
2. `secrets/<env>.secrets.json` — `{"api_token": "..."}`. Recommended locally. `rdc init` adds `secrets/` to `.gitignore`.

Set or rotate:

```sh
rdc auth test --token <new-token>
```

Validates against `GET /organizations/{org_id}` before writing the file (mode 0600 on Unix). For pipe input that keeps the token out of shell history:

```sh
read -s T && echo "$T" | rdc auth test
```

Master Data Hub is pulled automatically when the cluster has it enabled. The Data Storage URL is derived from `api_base`; no extra config to set. On clusters without MDH, the lookup returns 404 and `rdc` skips silently.

## Resilience

Every Rossum and Data Storage HTTP call retries automatically on:

- `429 Too Many Requests` — honors `Retry-After` if the server provides one.
- `502` / `503` / `504` — transient infrastructure.

Up to 5 attempts with exponential backoff (1s, 2s, 4s, 8s, 16s, capped at 60s). A stderr line marks each retry so the tool never sits silent. `500 Internal Server Error` is **not** retried — treating it as transient masks real server bugs.

## Upgrade

Keep `rdc` current with one command:

```sh
rdc upgrade
```

Downloads the latest GitHub release for your platform, runs `--version` on the new binary as a sanity check, and swaps it in atomically. The previous binary is kept at `<install_dir>/rdc.bak` for one-shot rollback.

`rdc upgrade --check` reports the latest available version without installing. `rdc upgrade --version vX.Y.Z` pins to a specific tag.

Once per day, every command does a background check against the GitHub Releases API. If a newer release exists, a one-line note appears at the top of the command's output. The check is best-effort — network errors, rate limits, or unreachable clusters fail silently.

**Install-location detection.** `rdc upgrade` only self-replaces when it's safe:

| Install method | Behavior |
|---|---|
| `install.sh` / manual binary in a writable dir | Self-replaces atomically; previous binary kept as `rdc.bak` (or `rdc.bak.exe` on Windows). |
| `cargo install --git …` | Refuses — would break cargo's bookkeeping. Prints the right `cargo install --force` invocation instead. |
| Read-only dir (`/usr/local/bin`, system package manager, `C:\Program Files`, …) | Refuses — prints the manual download URL + commands. |

The swap uses a copy-aside + atomic rename pattern, so a parallel shell tab running `rdc` during the upgrade never sees a missing file. On Linux/macOS the kernel keeps the running binary alive after its directory entry is replaced, so an in-flight `rdc upgrade` completes normally. On Windows the OS allows renaming a running `.exe` but not overwriting one, so the current binary is renamed aside to `rdc.bak.exe` before the new one is placed at the original path.

## Compatibility

- **Backward compat (new binary, old artifacts):** the latest `rdc` reads anything produced by any previous release. Lockfile versions migrate forward; project config and overlay tolerate missing fields via serde defaults.
- **Forward compat (older binary, newer artifacts):** not promised. Newer-version artifacts that an older binary doesn't understand produce a clear error pointing at `rdc upgrade`, never silent corruption.

A rare class of releases changes how `content_hash` is computed (e.g. stripping a newly-noisy server-managed field). The release notes will say so explicitly when it happens. After such a release, run `rdc repair <env> --rebuild-lock` once to clear any false-positive conflicts on the first re-sync.

## Tests

```sh
cargo test
```
