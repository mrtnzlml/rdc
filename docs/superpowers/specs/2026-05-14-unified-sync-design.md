# Unified Sync — Design

**Status:** Spec, awaiting review
**Date:** 2026-05-14
**Scope:** Introduce `rdc sync <env>` as the single command for reconciling local snapshot and remote state. The existing `rdc pull` and `rdc push` commands are removed; their direction-explicit behavior is available via the `--no-push` and `--no-pull` flags on `sync`.

## Goal

Remove "which command do I run?" from the day-to-day workflow. The user edits files locally; `rdc sync` brings local and remote into the same state in a single invocation. When divergence exists, the resolver handles it — same UI as today, semantically generalized to act in both directions.

The asymmetric "snapshot is canonical" model is preserved: on skip, on non-TTY runs, and on every default, local wins. Sync changes the user-facing surface, not the trust model.

## Non-goals

- **Background daemon / watch mode.** The motivation mentions "in sync all the time (non-intrusively)" but this spec is a foreground CLI command. A daemon can wrap `sync` later without changing its semantics.
- **Symmetric peer semantics (git-style).** Local and remote remain asymmetric: the on-disk snapshot is the user's editable source of truth.
- **Cross-env reconciliation.** `rdc sync` is single-env (local ↔ remote for one env). Cross-env promotion is still `rdc deploy <src> <tgt>`, unchanged.
- **New conflict-resolver shape.** The five-option shape (`[k]/[r]/[e]/[s]/[a]`) is preserved. The only UI change is to interpolate the env name into labels (e.g., `[r] use production`) — no new options, no new keystrokes.
- **Operation filtering.** No `--only` selector for sync in v1. The deploy `--only` machinery (see `2026-05-14-selective-deployment-design.md`) is intentionally not generalized to sync; per-file edits don't need it.

## Background

Today's architecture (verified by reading `src/cli/pull/mod.rs` and `src/cli/push/mod.rs`):

- **`pull`** lists every kind from the API (Phase 1), then processes each kind in dependency order (Phase 2), writing JSON + extracted `.py` to disk. Conflicts trigger `cli::resolve::resolve_combined_file` with `[k]/[r]/[e]/[s]/[a]`.
- **`push`** scans local files for changes and tombstones (`push::scan`), gates destructive deletes through a two-act confirmation (`push::deletes::confirm_or_refuse`), then runs per-kind drivers in dependency order. Per-object drift detection inside each driver calls `cli::resolve::resolve_push_drift` — the same resolver, with push-side semantics on `[k]/[r]`.
- Both share `src/cli/resolve.rs` (1101 lines). Both maintain idempotency: clean env → 0 writes / 0 API calls.
- Both save the lockfile only on success; both regenerate `_index.md` at the end.

The two commands have grown structurally identical: list/scan, classify, plan, execute, save state. They differ only in direction. The direction is what the user has to remember.

## Design

### CLI surface

```
rdc sync <env> [--dry-run] [--diff] [--yes] [--allow-deletes] [--no-push] [--no-pull]
```

Default: bring local and remote into the same state, prompting on conflicts.

Direction-explicit modes (for CI workflows that need unambiguous behavior):

- `--no-push` — audit mode. Pull remote changes into local; never POST/PATCH/DELETE on the remote. Used for drift detection against a committed snapshot.
- `--no-pull` — deploy mode. Send local edits to the remote; never overwrite local files. Used for pipelines that publish a committed snapshot and must not mutate the working copy.

`--no-push` and `--no-pull` together → error pointing at `rdc status` (the read-only inspection command).

The previous `rdc pull` and `rdc push` commands are removed. User scripts that called them need to be updated:

- `rdc pull <env>` → `rdc sync <env> --no-push`
- `rdc push <env>` → `rdc sync <env> --no-pull`
- `rdc push <env> --yes --allow-deletes` → `rdc sync <env> --no-pull --yes --allow-deletes`

### Execution pipeline

A single pass with five phases:

1. **List remote.** Refactor of today's `cli::pull::run_drivers` Phase 1 into a `list_remote()` returning a typed catalog (per kind: list of `(id, slug, body)` from the API). The progress bar denominator is set in this phase.
2. **Scan local.** Reuses `cli::push::scan::scan` unchanged. Yields a `ChangeList` of locally-modified files and a `TombstoneList` of files tracked in the lockfile but missing on disk.
3. **Classify.** One pass over every `(kind, slug)` that appears on either side, against the lockfile, into one of eleven classes:

   | Class | Local vs. lockfile | Remote vs. lockfile | Action |
   |---|---|---|---|
   | clean | matches | matches | no-op |
   | local-only edit | differs | matches | push (PATCH) |
   | local-only create | new file (no lockfile entry) | absent | push (POST) |
   | local-only delete | tombstone | matches | push (DELETE), under destructive gate |
   | remote-only edit | matches | differs | pull (write local) |
   | remote-only create | absent | new (no lockfile entry) | pull (write local) |
   | remote-only delete | matches | absent from listing | conflict resolver (see §"Conflict resolver under sync") |
   | both diverged | differs | differs | three-way merge resolver |
   | local edit + remote delete | differs | absent from listing | conflict resolver (delete-aware variant) |
   | local delete + remote edit | tombstone | differs | conflict resolver (delete-aware variant) |
   | both deleted | tombstone | absent from listing | silent convergence: drop lockfile entry, no writes |

4. **Plan.** Print the breakdown, confirm on TTY:

   ```
   Plan: sync test
     ← pull:    3 changes from test
                  hooks/validator-invoices
                  queues/cost-invoices
                  labels/audit-hold (new)
     → push:    2 local edits
                  email_templates/main/cost-invoices/rejection
                  rules/finance-totals
     ⚠ conflict: 1 (hooks/validator-totals — both diverged)

   Proceed? [y/N]
   ```

   The arrow direction and the env name make the data flow explicit at a glance: `←` pulls into local from the named env, `→` pushes local out to the named env.

   `--dry-run` prints this plan and exits, no writes. `--diff` adds per-object unified diffs.

5. **Execute.** Single ordering:

   1. **Conflict resolution prompt** per conflicted object (including remote-deleted objects, which use the same resolver with delete-specific labels). The resolution determines whether the object goes through the pull-side writer, the push-side writer, or both.
   2. **Push-side writes** in dependency order: `workspaces → schemas → queues → inboxes → email_templates → hooks → rules → labels → engines → engine_fields`. Push runs before pull so that resolved local edits land on the remote as soon as the resolver finishes — deploy local edits ASAP after conflict resolution.
   3. **Push-side deletes** (local tombstones → remote DELETE) in reverse dependency order, after the destructive gate is crossed.
   4. **Pull-side writes** after the push completes. Pull and push touch disjoint `(kind, slug)` sets, so this swap doesn't create races. Per-object drift checks inside each push driver (`resolve_push_drift`) and the conflict resolver in step 1 still guarantee no silent overwrite of remote-only changes.
   5. **Save lockfile, regenerate `_index.md`.**

### Conflict resolver under sync

The resolver UI keeps its existing shape (`[k]/[r]/[e]/[s]/[a]`) but its labels become **env-aware**. The "remote" side is named after the env passed to `rdc sync` (e.g., `production`, `test`, `staging`). This removes the abstract "remote" word from every prompt and makes the asymmetry concrete: the user sees exactly which environment is on the other side of the decision.

Example, on `rdc sync production`:

```
hooks/validator-invoices — conflict

local has changes:
  <unified diff snippet>

production has changes:
  <unified diff snippet>

[k] keep local   [r] use production   [e] edit   [s] skip   [a] abort >
```

Action semantics for the standard "both diverged" conflict:

| Choice | Default sync | `--no-push` (audit) | `--no-pull` (deploy) |
|---|---|---|---|
| `[k]` keep local | PATCH the env with local bytes; lockfile records local hash | no-op; hidden from prompt | PATCH the env with local bytes; lockfile records local hash |
| `[r]` use <env-name> | overwrite local with env bytes; lockfile records env hash | overwrite local; lockfile records env hash | no-op; hidden from prompt |
| `[e]` edit | $EDITOR on conflict markers; saved bytes → both sides | $EDITOR; saved bytes → local only | $EDITOR; saved bytes → env only |
| `[s]` skip | shadow file `<file>.<env-name>`; local untouched; lockfile records local hash | same | same |
| `[a]` abort | stop; lockfile unsaved | same | same |

`[s]` always preserves local. That is the "snapshot is canonical on skip" invariant from the README, made explicit. The shadow file is named with the env (`<file>.<env-name>`) so the artifact is unambiguous when the project has multiple envs. CI / non-TTY / `--yes` falls back to `[s]` automatically.

When `--no-push` or `--no-pull` is active, the prompt hides the choices that would have no effect.

#### Remote-side delete via the resolver

The `remote-only delete` class (lockfile entry present, object absent from the env's listing) is folded into the same resolver, with a shape that makes the destructive direction explicit:

```
labels/audit-hold — deleted on production

local has the file (last sync hash: <hash>):
  <pretty-printed JSON preview>

production has it deleted.

[k] keep local (restore on production)   [r] use production (delete local)   [s] skip   [a] abort >
```

Action semantics for the remote-delete case:

| Choice | Default sync | `--no-push` (audit) | `--no-pull` (deploy) |
|---|---|---|---|
| `[k]` keep local (restore on <env-name>) | POST the local body to recreate the object on the env | no-op; hidden | POST the local body to recreate the object |
| `[r]` use <env-name> (delete local) | remove the local file; drop the lockfile entry | remove the local file; drop the lockfile entry | no-op; hidden |
| `[e]` | not applicable to deletes; hidden | hidden | hidden |
| `[s]` skip | write `<file>.<env-name>-deleted` marker; local file untouched; lockfile entry retained — re-prompts next sync | same | same |
| `[a]` abort | stop; lockfile unsaved | same | same |

Non-TTY / `--yes` falls back to `[s]` (skip-with-marker). Destructive directions — restoring on the env, or deleting locally — are never taken silently in CI.

`--allow-deletes` does *not* auto-confirm the `[r]` choice here. That flag gates outgoing deletes (local tombstones → remote DELETE). Mirror-from-remote is a separate destructive direction and only an explicit `[r]` choice triggers it.

#### Double-conflict cases (edit + delete on opposite sides)

Two derived cases reuse the same resolver shape; only the labels differ to make the destruction direction explicit:

- **Local edit + remote delete** (`differs` + `absent`): the user has unsynced local edits to an object that was deleted on the env.
  - `[k]` keep local (restore on `<env-name>` with your unsynced edits)
  - `[r]` use `<env-name>` (delete local — your unsynced edits are dropped)
  - `[s]` skip (`<file>.<env-name>-deleted` marker; local with edits retained)
  - `[a]` abort

- **Local delete + remote edit** (`tombstone` + `differs`): the user tombstoned an object locally, but the env has unsynced changes to it.
  - `[k]` keep local (push DELETE to `<env-name>` — the env's unsynced changes are dropped)
  - `[r]` use `<env-name>` (restore local file from the env's bytes — the tombstone is undone)
  - `[s]` skip (`<file>.<env-name>-conflict` marker; both sides retained)
  - `[a]` abort

Non-TTY / `--yes` falls back to `[s]` in both cases. The destructive directions (`[k]` or `[r]` either way) need an explicit user choice.

The **both-deleted** class (`tombstone` + `absent`) is a silent convergence: both sides agree the object is gone, so the sync just drops the lockfile entry. No prompt, no diff.

### `--dry-run` and `--diff`

- `--dry-run` runs phases 1–4, prints the plan, exits. Zero writes either side. Drift-check GETs still happen for accurate field-level previews (same as `push --dry-run` today).
- `--diff` (with `--dry-run`) adds unified diffs for each in-scope object. POST candidates print as new-file diffs. Tombstones print as deleted-file diffs (`+++ /dev/null`). Mirrors `push --dry-run --diff` semantics.

### Idempotency invariants

The spec commits to these. They are testable as integration tests.

1. **Clean env, default flags**: 0 API writes, 0 local writes, exits silently with a one-line summary.
2. **Stable inputs**: re-running `rdc sync test` N times in a row produces the same lockfile.
3. **Conflict resolved via `[k]`**: subsequent sync runs exit silently (both sides now match the lockfile via PATCH).
4. **Conflict resolved via `[r]`**: subsequent sync runs exit silently (local now matches remote via overwrite).
5. **Conflict resolved via `[s]`**: subsequent sync runs re-surface the same conflict (until the `<file>.<env-name>` shadow is removed or the underlying divergence is resolved). The shadow file is not auto-deleted.

### Plan-before-apply

`sync` always prints a plan before any write. The single confirmation gate covers all phases (pull writes, push writes, conflict resolutions). The destructive subset (push-side DELETEs from local tombstones, remote-side delete restore via `[r]`) has its own additional gate, matching today's two-act delete flow:

- TTY: prompt before any DELETE.
- `--yes` alone: refuses if there are tombstones.
- `--yes --allow-deletes`: proceeds without per-object prompts.

This mirrors today's `push --yes --allow-deletes`. No new safety semantics.

### Code shape

New module: `src/cli/sync.rs`. Public surface:

```rust
pub async fn run(
    env: &str,
    interactive: bool,
    dry_run: bool,
    diff: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
) -> Result<()>
```

Body composes the existing per-kind drivers:

1. Lockfile, client, overlay loaded once at the top.
2. `list_remote(...)` — extracted from `cli::pull::run_drivers` Phase 1, returns a typed `RemoteCatalog`.
3. `cli::push::scan::scan(...)` — reused unchanged.
4. `classify(remote_catalog, scan_result, lockfile) -> Classification` — new helper.
5. Plan render + confirm.
6. Execute:
   - For pull-side writes: call per-kind `pull::*::process` functions, passing the classified pull subset (a `&BTreeSet<(kind, slug)>`). Each `process` loops over the listed catalog and skips items not in the set.
   - For conflicts: handled inline by the per-kind processors (they already call the resolver; the caller decides whether to also issue a PATCH based on the `Resolution`).
   - For push-side writes: call per-kind `push::*::push` functions with a `ChangeList` filtered to the classified push subset.

Refactors required (all small, all in service of sharing code rather than duplicating):

- Extract `cli::pull::run_drivers` Phase 1 (listing) into `cli::pull::common::list_remote`. `run_drivers` itself is deleted along with `cli::pull::run`.
- Each `pull::*::process` function takes the classified pull subset directly (a `&BTreeSet<(kind, slug)>` of slugs to write). The function loops over the listed catalog and skips items not in the set. With no top-level `pull` command remaining, there is no caller that needs "process everything" behavior.
- `cli::pull::run` and `cli::push::run` are deleted (they were the CLI entry points). The per-kind drivers under `cli::pull::*` and `cli::push::*` remain as internal modules consumed by `sync::run`. A later rename to `cli::sync::pull_drivers::*` / `cli::sync::push_drivers::*` could reflect the new ownership; not required for this change.
- Resolver: the prompt-rendering functions in `cli::resolve` gain an `env_name: &str` parameter so the `[r]` label, headers, and shadow-file paths can interpolate the actual env name. Existing callers update with a one-line plumbing change. The `Resolution` enum gains no new variants; the new remote-delete case is communicated by what the caller passes in (existing-local + absent-remote signals it) and the caller dispatches on `Resolution` accordingly.
- Progress bar: a single `OverallProgress` covers both directions. Denominator = `remote-listed + locally-changed - intersection`.

No on-disk schema changes. No lockfile format changes. No overlay changes. No new dependencies.

`cli::index::generate` runs once at the end, as today.

### CLI registration

In `src/cli/mod.rs`, the `Pull` and `Push` variants of `Command` are removed; one `Sync` variant takes their place:

```rust
#[derive(Subcommand)]
pub enum Command {
    // ...
    /// Reconcile the local snapshot and the env's remote state in one pass.
    Sync { /* env, --dry-run, --diff, --yes, --allow-deletes, --no-push, --no-pull */ },
    // ...
}
```

`rdc --help` lists `sync` under "Working with an environment"; the prior `pull` and `push` entries are gone.

### README updates

The 60-second tour switches to:

```sh
rdc init ...
rdc auth test --token ...
rdc sync test
$EDITOR envs/test/hooks/validator-invoices.py
rdc sync test
rdc deploy test prod
```

The "Mental model" section gets a single sentence describing `rdc sync` as the only command for local ↔ remote reconciliation.

The Commands table replaces the `rdc pull` and `rdc push` rows with a single `rdc sync` row. The flags column for `rdc sync` mentions `--no-push` (audit mode) and `--no-pull` (deploy mode) for CI cases.

### Errors & UX

- `rdc sync <env> --no-push --no-pull` → error: `use 'rdc status' for read-only inspection.`
- Missing env / missing token: clear, actionable messages (same shape as today).
- Listing failure mid-pipeline: same retry behavior (`429`, `5xx`) as today. On exhausted retries, exits non-zero with the last error; lockfile not saved.
- Local scan failure (e.g., malformed JSON in a snapshot file): clear error, lockfile not saved.
- Conflict resolution path uses the env-aware prompt shape described above. The `<env-name>` label is interpolated from the env arg.

### Testing

Unit tests (`src/cli/sync.rs`):

- `classify`: synthetic remote catalog + scan result + lockfile yields the expected class for each of the 11 cases.
- `--no-push` filter: every entry classified as push-side becomes a no-op in the executed action set.
- `--no-pull` filter: every entry classified as pull-side becomes a no-op in the executed action set.
- Remote-side delete detection: `lockfile entry + absent from listing` → expected `RemoteDeleted` class with the right slug.

Integration tests (`tests/`, wiremock-backed, matching `tests/` layout for deploy):

- `sync_clean_env`: clean state → 0 writes, exits silently.
- `sync_local_edit_only`: one local PATCH-class change, no remote drift → exactly one PATCH hits the mock.
- `sync_remote_change_only`: one remote drift, no local edits → local file rewritten, 0 PATCHes.
- `sync_conflict_keep_local`: both diverged, scripted-stdin `[k]` → one PATCH, lockfile records local hash.
- `sync_conflict_use_env`: both diverged, scripted-stdin `[r]` → 0 PATCHes, local rewritten with env bytes.
- `sync_conflict_skip_writes_env_shadow`: `[s]` → `<file>.<env-name>` shadow file written, local untouched.
- `sync_remote_deleted_restore_via_keep_local`: lockfile entry + absent from listing, `[k]` → POST hits the mock to restore.
- `sync_remote_deleted_mirror_via_use_env`: same setup, `[r]` → local file removed, lockfile entry dropped, no further writes.
- `sync_remote_deleted_skip`: same setup, `[s]` → `<file>.<env-name>-deleted` marker written, no further writes.
- `sync_remote_deleted_yes_falls_back_to_skip`: same setup, `--yes` → `[s]` taken automatically; no destructive direction in CI.
- `sync_no_push`: with local edits + remote drift → only the pull-side writes happen; summary names the skipped local edits.
- `sync_no_pull`: symmetric — only push-side writes happen.
- `sync_yes_with_tombstones_refused`: tombstones present, `--yes` without `--allow-deletes` → refused, zero writes.
- `sync_dry_run_diff`: prints plan and per-object diffs; zero writes.
- `sync_env_name_in_prompt`: scripted-stdin TTY run on `rdc sync production`; captured prompt text contains `use production` and `<file>.production` (not literal "remote").
- `sync_local_edit_remote_delete_keep_local`: scripted `[k]` → POST restores the object on the env with the local edited body.
- `sync_local_delete_remote_edit_keep_local`: scripted `[k]` → DELETE removes the object on the env despite its unsynced changes.
- `sync_both_deleted_silent`: tombstone + absent from listing → lockfile entry dropped, no prompt, no writes.

### Compatibility

- Lockfile format: unchanged.
- Overlay format: unchanged.
- `.rdc/map/*.toml`: untouched (deploy-only).
- **`rdc pull` and `rdc push` are removed.** User scripts that called them break and must be updated to `rdc sync <env>` (or `rdc sync <env> --no-push` / `--no-pull` for direction-explicit CI cases).
- Shadow-file path: `<file>.remote` → `<file>.<env-name>` for the conflict-skip artifact, and `<file>.<env-name>-deleted` for the remote-delete skip marker. Existing `.remote` files left over from prior runs are not auto-migrated; they remain on disk as user artifacts and `rdc sync` will re-surface the underlying conflict on the next run.

## Open questions

- **Conflict order across kinds.** In the rare case of conflicts in dependent kinds (e.g., a queue and its schema both diverged with different per-side edits), the resolver runs in dependency order (schema first, then queue). Confirm during implementation that the prompt phrasing makes the dependency clear, or surface a single combined prompt.
- **Plan header limits.** For large pull or push sets (hundreds of changes), the plan output can get long. Initial behavior is to list everything; if it becomes unwieldy, fold to first-N + count (same approach noted in the selective-deployment spec's open questions).
- **Remote-delete preview length.** The remote-deleted prompt shows a pretty-printed JSON preview of the local file. For large objects (10+ KB hooks, schemas with many fields) the preview is unwieldy. Initial behavior is to elide after ~40 lines with a count; revisit if confusing.

## Out of scope (deferred)

- **Daemon / watch mode.** "In sync all the time, non-intrusively" is the motivating phrase, but the daemon is a separate product surface (file watcher, polling cadence, conflict batching, lockfile races between concurrent invocations). Doable on top of `sync` later.
- **`--only` selector for sync.** Sync acts naturally per-file (each change is its own atom). The `--only` machinery from deploy targets cross-env scope; it would be a different concept here. Add if a real use case emerges.
- **Multi-env sync** (`rdc sync test prod`). That's `rdc deploy`. Keep them separate.
