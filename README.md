# rdc

**Rossum Deployment as Code — snapshot, edit, deploy.**

`rdc` is an opinionated CLI for managing Rossum.ai configurations as files on disk. Sync an environment into a local snapshot, edit it, sync again to send the changes back.

```sh
rdc sync test
$EDITOR envs/test/hooks/validator-invoices.py
rdc sync test
```

That's the loop.

## Install

Homebrew (macOS, Linux x86_64):

```sh
brew install mrtnzlml/tap/rdc
```

Windows (PowerShell):

```powershell
$dest = "$env:USERPROFILE\.rdc\bin"
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Invoke-WebRequest -Uri "https://github.com/mrtnzlml/rdc/releases/latest/download/rdc-x86_64-pc-windows-msvc.tar.gz" -OutFile "$env:TEMP\rdc.tar.gz"
tar -xzf "$env:TEMP\rdc.tar.gz" -C $dest
[Environment]::SetEnvironmentVariable("Path", "$env:Path;$dest", "User")
```

From source:

```sh
cargo install --git https://github.com/mrtnzlml/rdc
```

Supported pre-built platforms: macOS (Intel + Apple Silicon), Linux x86_64, Windows x86_64. For Linux aarch64 / Windows aarch64 / other, build from source.

## Quick start

```sh
mkdir my-rossum-project && cd my-rossum-project
rdc init
```

`rdc init` walks you through setting up one or more envs — env name, API base URL, org ID, API token — then syncs each into a local snapshot.

For an env named `test`, you now have:

```
envs/test/
├── _index.md
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
│   └── validator-invoices.py
├── rules/
│   ├── validate-totals.json
│   └── validate-totals.py
├── labels/
└── mdh/                         ← only on clusters with MDH
    └── customers/
        ├── collection.json
        └── indexes.json
```

Edit a file, then sync to send the change back:

```sh
$EDITOR envs/test/hooks/validator-invoices.py
rdc sync test
```

To promote changes between envs, see [Deploy](#rdc-deploy).

## `rdc sync`

Reconciles the local snapshot with the remote env in one pass — pulls remote changes, sends local edits, creates objects from new files, and deletes objects whose local files you removed (with confirmation).

```sh
rdc sync test
```

### Edit a file

Most files are plain JSON — open them in your editor and save. After any edit, run `rdc sync <env>`.

For objects with executable code (hooks, rules, schema formulas), the code lives in a sidecar file alongside the JSON. Edit the sidecar.

| Kind | On-disk files |
|---|---|
| Hook with Python | `hooks/<slug>.json` + `hooks/<slug>.py` |
| Hook with Node.js | `hooks/<slug>.json` + `hooks/<slug>.js` |
| Rule with trigger | `rules/<slug>.json` + `rules/<slug>.py` |
| Schema with formulas | `…/queues/<q>/schema.json` + `…/queues/<q>/formulas/<field-id>.py` |

### Preview a sync

```sh
rdc sync test --dry-run
```

Lists every change that would be sent — POSTs, PATCHes, DELETEs — without writing.

### Conflicts

When both local and the env have changed since the last sync, an inline resolver opens for each conflicting file:

```
[k] keep local  [r] use test  [e] edit  [s] skip  [a] abort >
```

- `k` — push local bytes to the env.
- `r` — overwrite local with the env's bytes.
- `e` — open `$EDITOR` on git-style conflict markers.
- `s` — skip; the env's bytes land at `<file>.<env-name>` for review.
- `a` — abort.

### Create or delete

Author a new JSON file (omit `id` and `url` — the server assigns them) and `rdc sync` will POST it on the next run. Remove a file and the next sync will list it as a pending DELETE and ask before sending.

```sh
$ rm envs/test/labels/audit-hold.json
$ rdc sync test
! The following 1 object(s) would be DELETED from the remote:
  - labels/audit-hold (id 10198)

Proceed with deletion? [y/N] y
```

## `rdc deploy`

Promotes one env's snapshot to another. Plans every create/update, prints the per-object diff, waits for confirmation, then applies.

```sh
rdc deploy test prod
```

Example session:

```
--- hooks/validator-invoices.json (src after overlay+rewrite+normalize)
+++ hooks/validator-invoices.json (tgt remote, normalized)
@@ ...
-  "name": "Validator: invoices",
+  "name": "Validator (PROD)",

Would apply 1 hooks (1 PATCHes) from test to prod
Proceed? [y/N] y
Applied 1 hooks (1 PATCHes) from test to prod
Deployed test -> prod: 0 created, 0 deleted, 2 API calls, 1.4s
```

Re-running on a synced env is a no-op.

### Preview a deploy

```sh
rdc deploy test prod --dry-run
```

Same plan output, no API writes.

### Selective deploy

Deploy only part of the snapshot by passing one or more `--only <selector>` flags:

```sh
rdc deploy test prod --only hooks/validator-invoices
rdc deploy test prod --only 'schemas/cost-*'
```

Selector forms:

| Form | Example | Matches |
|---|---|---|
| `<kind>/<slug>` | `hooks/validator-invoices` | exact `(kind, slug)` |
| `<kind>/<glob>` | `schemas/cost-*` | `*` glob in the slug segment |
| `*/<glob>` | `*/cost-invoices` | any kind whose slug matches |

### Hook secrets

Hook secret values aren't copied between envs. If the target's `secrets/<tgt>.hook-secrets.json` is missing keys, `rdc deploy` lists them, pre-populates the file with empty placeholders, and aborts — fill in the values and re-run.

## Commands

| Command | What it does |
|---|---|
| `rdc init` | Create a new project, or add an env to an existing one. Prompts interactively. |
| `rdc auth <env>` | Set or refresh the API token for `<env>`. |
| `rdc sync <env>` | Reconcile snapshot ↔ remote in one pass. |
| `rdc deploy <src> <tgt>` | Promote one env's snapshot to another. |
| `rdc diff <env>` | Local-vs-remote diff for `<env>`. |
| `rdc diff <a> <b>` | Diff two local snapshots. |
| `rdc repair <env>` | Local-state recovery (`--rebuild-lock`, `--rename-slugs`). |
| `rdc upgrade` | Self-update the binary. |

Every command that writes to the remote takes `--dry-run`. Use `rdc <command> --help` for the full flag list.

## Authentication

`rdc init` and `rdc auth` are the normal ways to set a token. For automation or rotation, three credential sources are checked per-env, in priority order:

1. `RDC_TOKEN_<ENV>` — used as-is.
2. `secrets/<env>.secrets.json` — cached token from a previous `rdc auth` or auto-login.
3. `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` — used for an auto-login when the cached token is missing or expired.

`<ENV>` is the env name from `rdc.toml`, uppercased, with non-alphanumerics replaced by `_`. So `test` → `RDC_TOKEN_TEST`, `dev-ap` → `RDC_TOKEN_DEV_AP`.

### Set or rotate manually

```sh
rdc auth test --token <new-token>
```

```sh
rdc auth test --username alice@example.com
# password is prompted on TTY (masked)
```
