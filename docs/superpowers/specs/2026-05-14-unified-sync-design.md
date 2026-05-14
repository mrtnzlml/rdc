# Unified Sync — Design

**Status:** Spec, awaiting review
**Date:** 2026-05-14
**Scope:** Introduce `rdc sync <env>` as the single primary verb for reconciling local snapshot and remote state. Replace the directional `pull` / `push` mental model. The two existing commands are retained as thin direction-explicit aliases (`sync --no-push`, `sync --no-pull`) so CI workflows that depend on unambiguous direction keep working.

## Goal

Remove "which command do I run?" from the day-to-day workflow. The user edits files locally; `rdc sync` brings local and remote into the same state in a single invocation. When divergence exists, the resolver handles it — same UI as today, semantically generalized to act in both directions.

The asymmetric "snapshot is canonical" model is preserved: on skip, on non-TTY runs, and on every default, local wins. Sync changes the user-facing surface, not the trust model.

## Non-goals

- **Background daemon / watch mode.** The motivation mentions "in sync all the time (non-intrusively)" but this spec is a foreground CLI command. A daemon can wrap `sync` later without changing its semantics.
- **Symmetric peer semantics (git-style).** Local and remote remain asymmetric: the on-disk snapshot is the user's editable source of truth.
- **Removing `pull` / `push` entirely.** They stay in the CLI, redefined as aliases. A future release may deprecate them; this one doesn't.
- **Cross-env reconciliation.** `rdc sync` is single-env (local ↔ remote for one env). Cross-env promotion is still `rdc deploy <src> <tgt>`, unchanged.
- **New conflict semantics.** The resolver UI (`[k]/[r]/[e]/[s]/[a]`) is unchanged. Sync generalizes what each choice *does*, not what the user sees.
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

`pull` and `push` are retained as **command-level aliases**:

- `rdc pull <env>` ≡ `rdc sync <env> --no-push`
- `rdc push <env>` ≡ `rdc sync <env> --no-pull`

Both keep their existing flags (`pull` already takes no flags beyond `--interactive`; `push` takes `--dry-run --diff --yes --allow-deletes`). The clap subcommands route into a single `sync::run` entry point with the appropriate `no_push` / `no_pull` set. Users who never type `sync` see no behavior change.

`--no-push` and `--no-pull` together → error pointing at `rdc status` (the read-only inspection command).

### Execution pipeline

A single pass with five phases:

1. **List remote.** Refactor of today's `cli::pull::run_drivers` Phase 1 into a `list_remote()` returning a typed catalog (per kind: list of `(id, slug, body)` from the API). The progress bar denominator is set in this phase.
2. **Scan local.** Reuses `cli::push::scan::scan` unchanged. Yields a `ChangeList` of locally-modified files and a `TombstoneList` of files tracked in the lockfile but missing on disk.
3. **Classify.** One pass over every `(kind, slug)` that appears on either side, against the lockfile, into one of eight classes:

   | Class | Local vs. lockfile | Remote vs. lockfile | Action |
   |---|---|---|---|
   | clean | matches | matches | no-op |
   | local-only edit | differs | matches | push (PATCH) |
   | local-only create | new file | absent | push (POST) |
   | local-only delete | tombstone | matches | push (DELETE), under destructive gate |
   | remote-only edit | matches | differs | pull (write local) |
   | remote-only create | absent | new | pull (write local) |
   | remote-only delete | matches | 404 from listing | prompt: drop local, or restore on remote? |
   | both diverged | differs | differs | three-way merge resolver |

4. **Plan.** Print the breakdown, confirm on TTY:

   ```
   Plan: sync test
     ← pull:    3 remote changes
                  hooks/validator-invoices
                  queues/cost-invoices
                  labels/audit-hold (new)
     → push:    2 local edits
                  email_templates/main/cost-invoices/rejection
                  rules/finance-totals
     ⚠ conflict: 1 (hooks/validator-totals — both diverged)

   Proceed? [y/N]
   ```

   `--dry-run` prints this plan and exits, no writes. `--diff` adds per-object unified diffs.

5. **Execute.** Single ordering:

   1. **Pull-side writes** first. Reason: if the push phase fails partway (network, auth, drift in a conflicting object), the local snapshot has at least caught up to the latest remote state; on retry, sync resumes from up-to-date local. The lockfile is saved per-success, so partial progress isn't lost.
   2. **Conflict resolution prompt** per conflicted object. The resolution determines whether the object goes through the pull-side or push-side writer, or both.
   3. **Push-side writes** in dependency order: `workspaces → schemas → queues → inboxes → email_templates → hooks → rules → labels → engines → engine_fields`.
   4. **Push-side deletes** (local tombstones → remote DELETE) in reverse dependency order, after the destructive gate is crossed.
   5. **Save lockfile, regenerate `_index.md`.**

### Conflict resolver under sync

The existing resolver UI is unchanged. Sync only changes what the caller does with each `Resolution`:

| Choice | Pull-only (`--no-push`) | Sync default | Push-only (`--no-pull`) |
|---|---|---|---|
| `[k]` keep local | no-op (hidden from prompt) | write local bytes to remote (PATCH) | write local bytes to remote (PATCH) |
| `[r]` remote | overwrite local with remote bytes | overwrite local | no-op (hidden from prompt) |
| `[e]` edit | $EDITOR → write local only | $EDITOR → write local and remote | $EDITOR → write remote only |
| `[s]` skip | shadow file `<file>.remote`; local untouched | shadow file `<file>.remote`; local untouched; lockfile records local hash | shadow file `<file>.remote`; local untouched |
| `[a]` abort | stop, lockfile unsaved | stop, lockfile unsaved | stop, lockfile unsaved |

`[s]` always preserves local. That is the "snapshot is canonical on skip" invariant from the README, made explicit. CI / non-TTY / `--yes` falls back to `[s]` automatically, matching today's behavior.

When `--no-push` or `--no-pull` is active, the resolver hides the option that would have no effect, and the prompt shows only the meaningful choices.

### Remote-side delete

Sync introduces one new case not present in either current command: **lockfile-tracked object missing from remote listing**.

Today's `pull` never sees this — the listing is authoritative for "what exists," so a deleted remote silently drops from the next pull, leaving a stale local file. Today's `push` doesn't probe missing remotes (it only acts on locally-changed files).

Under sync, the listing is cross-checked against the lockfile. A `lockfile entry + absent from listing` pair triggers:

```
labels/audit-hold was deleted on remote since the last sync.

[d] delete local (drop the file)
[r] restore on remote (treat local as canonical, POST it back)
[s] skip (write labels/audit-hold.remote-deleted as a marker)
[a] abort
```

Non-TTY / `--yes` defaults to `[s]` (skip-with-marker), matching the conflict-resolver fallback. `--allow-deletes` gates *outgoing* deletes (local tombstones → remote DELETE), not this case; the remote-side delete prompt has its own per-object decision.

### `--dry-run` and `--diff`

- `--dry-run` runs phases 1–4, prints the plan, exits. Zero writes either side. Drift-check GETs still happen for accurate field-level previews (same as `push --dry-run` today).
- `--diff` (with `--dry-run`) adds unified diffs for each in-scope object. POST candidates print as new-file diffs. Tombstones print as deleted-file diffs (`+++ /dev/null`). Mirrors `push --dry-run --diff` semantics.

### Idempotency invariants

The spec commits to these. They are testable as integration tests.

1. **Clean env, default flags**: 0 API writes, 0 local writes, exits silently with a one-line summary.
2. **Stable inputs**: re-running `rdc sync test` N times in a row produces the same lockfile.
3. **Conflict resolved via `[k]`**: subsequent sync runs exit silently (both sides now match the lockfile via PATCH).
4. **Conflict resolved via `[r]`**: subsequent sync runs exit silently (local now matches remote via overwrite).
5. **Conflict resolved via `[s]`**: subsequent sync runs re-surface the same conflict (until the `.remote` shadow is removed or the underlying divergence is resolved). The shadow file is not auto-deleted.

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
   - For pull-side writes: call per-kind `pull::*::process` functions, restricted to the classified pull subset. Today these process the full list; they gain an optional filter parameter (`only: Option<&BTreeSet<(String, String)>>`) that skips items not in the set when present.
   - For conflicts: handled inline by the per-kind processors (they already call the resolver; the caller decides whether to also issue a PATCH based on the `Resolution`).
   - For push-side writes: call per-kind `push::*::push` functions with a `ChangeList` filtered to the classified push subset.

Refactors required (all small, all in service of sharing code rather than duplicating):

- Extract `cli::pull::run_drivers` Phase 1 (listing) into `cli::pull::common::list_remote`. The current `run_drivers` becomes a thin wrapper that calls it and then processes everything (preserving `pull`-only behavior).
- Add an `only: Option<&BTreeSet<(kind, slug)>>` parameter to each `pull::*::process` function. When `Some`, skip items not in the set. When `None`, process all (today's behavior).
- `cli::pull::run` → thin wrapper: parses args, calls `sync::run(env, interactive, dry_run=false, diff=false, allow_deletes=false, no_push=true, no_pull=false)`.
- `cli::push::run` → thin wrapper: parses args, calls `sync::run(env, interactive, dry_run, diff, allow_deletes, no_push=false, no_pull=true)`.
- Resolver: no signature changes. Where the caller previously had implicit direction (pull never PATCHed, push never overwrote local), the sync caller picks based on `Resolution` + the `no_push` / `no_pull` flags.
- Progress bar: a single `OverallProgress` covers both directions. Denominator = `remote-listed + locally-changed - intersection`.

No on-disk schema changes. No lockfile format changes. No overlay changes. No new dependencies.

`cli::index::generate` runs once at the end, as today.

### CLI registration

In `src/cli/mod.rs`:

```rust
#[derive(Subcommand)]
pub enum Command {
    // ...
    /// Bring local snapshot and remote state into sync. Recommended verb for everyday use.
    Sync { /* env, --dry-run, --diff, --yes, --allow-deletes, --no-push, --no-pull */ },
    /// Mirror remote into the local snapshot (alias for `sync --no-push`).
    Pull { /* env, --interactive */ },
    /// Send local edits to remote (alias for `sync --no-pull`).
    Push { /* env, --dry-run, --diff, --yes, --allow-deletes */ },
    // ...
}
```

`rdc --help` lists `sync` first under "Working with an environment"; `pull` and `push` follow with the alias note in their summary line.

### README updates

The 60-second tour switches to:

```sh
rdc init ...
rdc auth test --token ...
rdc sync test            # was: rdc pull test
$EDITOR envs/test/hooks/validator-invoices.py
rdc sync test            # was: rdc push test
rdc deploy test prod
```

The "Mental model" section adds a sentence: `rdc sync` reconciles local ↔ remote in one step; `rdc pull` and `rdc push` remain for direction-explicit use (audit-only, deploy-from-snapshot).

The Commands table grows one row at the top.

### Errors & UX

- `rdc sync <env> --no-push --no-pull` → error: `use 'rdc status' for read-only inspection.`
- Missing env / missing token: same messages as today's `pull` / `push`.
- Listing failure mid-pipeline: same retry behavior (`429`, `5xx`) as today. On exhausted retries, exits non-zero with the last error; lockfile not saved.
- Local scan failure (e.g., malformed JSON in a snapshot file): same error as today's `push --dry-run`. Lockfile not saved.
- Conflict resolution path is identical to today's pull/push conflict UI — users familiar with either command see the same prompts.

### Testing

Unit tests (`src/cli/sync.rs`):

- `classify`: synthetic remote catalog + scan result + lockfile yields the expected class for each of the 8 cases.
- `--no-push` filter: every entry classified as push-side becomes a no-op in the executed action set.
- `--no-pull` filter: every entry classified as pull-side becomes a no-op in the executed action set.
- Remote-side delete detection: `lockfile entry + absent from listing` → expected `RemoteDeleted` class with the right slug.

Integration tests (`tests/`, wiremock-backed, matching `tests/` layout for deploy):

- `sync_clean_env`: clean state → 0 writes, exits silently.
- `sync_local_edit_only`: one local PATCH-class change, no remote drift → exactly one PATCH hits the mock.
- `sync_remote_change_only`: one remote drift, no local edits → local file rewritten, 0 PATCHes.
- `sync_conflict_keep_local`: both diverged, scripted-stdin `[k]` → one PATCH, lockfile records local hash.
- `sync_conflict_keep_remote`: both diverged, scripted-stdin `[r]` → 0 PATCHes, local rewritten.
- `sync_remote_deleted_then_skip`: lockfile entry + 404 from listing, `[s]` → `.remote-deleted` marker written, no further writes.
- `sync_remote_deleted_then_restore`: same setup, `[r]` → POST hits the mock to restore.
- `sync_no_push`: with local edits + remote drift → only the pull-side writes happen; summary names the skipped local edits.
- `sync_no_pull`: symmetric — only push-side writes happen.
- `sync_yes_with_tombstones_refused`: tombstones present, `--yes` without `--allow-deletes` → refused, zero writes.
- `sync_dry_run_diff`: prints scoped plan and per-object diffs; zero writes.
- `pull_alias_unchanged`: `rdc pull test` (no flags) behaves identically to today on a fixture that exercises pull-only paths.
- `push_alias_unchanged`: `rdc push test --yes` behaves identically to today on a fixture that exercises push-only paths.

The last two tests guard the alias contract: anything that worked before keeps working.

### Compatibility

- Lockfile format: unchanged.
- Overlay format: unchanged.
- `.rdc/map/*.toml`: untouched (deploy-only).
- Existing user scripts calling `rdc pull` or `rdc push`: continue to work. The alias layer is a true alias; flags and output format are preserved.

## Open questions

- **`pull` and `push` deprecation timeline.** Spec proposes retaining them indefinitely as aliases. If telemetry or feedback later shows nobody uses them directly, a future release can mark them deprecated. Out of scope for this spec.
- **Conflict order across kinds.** In the rare case of conflicts in dependent kinds (e.g., a queue and its schema both diverged with different per-side edits), the resolver runs in dependency order (schema first, then queue). Confirm during implementation that the prompt phrasing makes the dependency clear, or surface a single combined prompt.
- **Plan header limits.** For large pull or push sets (hundreds of changes), the plan output can get long. v1 lists everything; if it becomes unwieldy, fold to first-N + count (same approach noted in the selective-deployment spec's open questions).

## Out of scope (deferred)

- **Daemon / watch mode.** "In sync all the time, non-intrusively" is the motivating phrase, but the daemon is a separate product surface (file watcher, polling cadence, conflict batching, lockfile races between concurrent invocations). Doable on top of `sync` later.
- **`--only` selector for sync.** Sync acts naturally per-file (each change is its own atom). The `--only` machinery from deploy targets cross-env scope; it would be a different concept here. Add if a real use case emerges.
- **Multi-env sync** (`rdc sync test prod`). That's `rdc deploy`. Keep them separate.
- **Removing `pull` / `push` entirely.** Possible later; not part of this change.
