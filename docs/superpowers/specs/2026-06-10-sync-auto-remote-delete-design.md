# Sync: auto-resolve clean remote deletions locally

**Date:** 2026-06-10
**Status:** Implemented (2026-06-11)

## Problem

`rdc sync` prompts `[k]/[r]/[s]/[a]` for every object that was deleted on the
env, even when the local file is byte-canonically unchanged since the last
sync (`SyncClass::RemoteDelete`). In that state there is no conflict — the
three-way merge has an unambiguous answer (mirror the env's deletion locally)
— so the prompt is pure noise. Real-world trigger: deleting one engine on an
env produces one prompt per engine field.

Verified instance: `envs/dev-ap/engines/mtr-training/fields/item-code.json`
in the ferguson project — local canonical hash `e15a7c56…` equals the
lockfile base hash exactly, so the item classified as pure `RemoteDelete`,
yet sync prompted.

Deleting a **remote** resource must remain interactive — breaking an env by
accident is expensive; deleting a git-tracked local file is trivially
recoverable.

## Decisions (with user)

1. Auto-delete applies in **both** interactive and non-interactive
   (`--yes` / non-TTY / CI) runs. Symmetric with `RemoteEdit`, which already
   overwrites local files without prompting in both modes.
2. MDH orphans get the **same rule**: `indexes.json` unchanged vs the
   lockfile hash → auto-prune; locally modified → keep prompting.
3. No mass-delete threshold guard. A legitimate bulk deletion (whole engine)
   is indistinguishable by count from a listing anomaly, and the failure mode
   is benign: local-only, git-visible, self-healing (objects still present
   remotely re-pull as `RemoteCreate` on the next sync because their lockfile
   entries were dropped).

## Behavior matrix

| State | Class | Today | After |
|---|---|---|---|
| Local unchanged vs base, deleted on env | `RemoteDelete` | `[k]/[r]/[s]/[a]` prompt | auto: delete local, drop lockfile entry, one log line |
| Local edited, deleted on env | `LocalEditRemoteDelete` | prompt | prompt (real conflict — unchanged) |
| Local deleted, edited on env | `LocalDeleteRemoteEdit` | prompt | prompt (real conflict — unchanged) |
| Both deleted | `BothDeleted` | silent converge | unchanged |
| Local deleted, env unchanged (remote DELETE) | `LocalDelete` | Phase B `confirm_or_refuse` + `--allow-deletes` | unchanged — remote stays protected |
| MDH orphan, `indexes.json` unchanged vs base | bypasses classifier | prompt | auto-prune dir + base-cache mirror + lockfile entry |
| MDH orphan, `indexes.json` modified | bypasses classifier | prompt | prompt (unchanged) |

The auto path always emits a visible per-item `Action::Delete` event,
e.g. `engines/mtr-training/fields/item-code.json (deleted on dev-ap)` —
no question asked, but never invisible.

## Implementation (Approach A: auto-resolve in the resolver)

### `resolve_remote_deletes` (`src/cli/sync/execute.rs`)

Before the `!interactive` fallback and the prompt, branch on
`SyncClass::RemoteDelete`:

- Run the existing `Resolution::KeepRemote` mechanics for non-tombstone
  classes: remove the local JSON, hook/rule sidecar (both extensions for
  hooks), schema `formulas/` directory; drop the lockfile entry.
- Sweep a stale `<file>.<env>-deleted` marker left by an earlier `[s]`.
- Emit the `Action::Delete` event and continue to the next item.
- If the local file vanished mid-run, converge by dropping the lockfile
  entry (same semantics as `BothDeleted`) instead of today's warn-and-skip.

`LocalEditRemoteDelete` and `LocalDeleteRemoteEdit` continue through the
existing prompt / non-TTY marker flow unchanged.

### `prune_mdh_orphans` (`src/cli/sync/execute.rs`)

After the existing "no local file" branch: compute
`content_hash(indexes.json bytes, &Lockfile::default())` — the exact
comparison `decide_pull_action` uses — and compare to the lockfile's
`mdh_indexes` entry hash. Equal → `remove_mdh_dataset` + `Action::Delete`
event + count as pruned (sweep a stale marker too). Different → existing
interactive prompt / non-TTY marker flow.

### Dry-run (`src/cli/sync/mod.rs`)

`RemoteDelete` moves from the "would prompt" section to the pull-side
section as `- <kind>/<slug> (delete local; deleted on env)` and is counted
under "would pull" in the summary. Conflict classes stay under
"would prompt".

### Counters

`CycleOutcome::remote_deletes_resolved` keeps counting auto-resolved items.

### Watch mode

No code change needed; a cycle whose only events are clean remote deletions
no longer blocks on stdin (conflict-free cycles never touch stdin).

## Recovery story (replaces `[k]` for the clean class)

The snapshot is git-tracked: `git restore <file>` + `rdc sync` classifies
the file `LocalCreate` (its lockfile entry is gone) and POSTs it back to the
env — the same outcome `[k]` produces today, available after the fact and
reviewable in git first.

## Non-goals

- Empty parent directories left after a deletion are not swept (matches
  existing `[r]` behavior).
- No change to any remote-write path: Phase B confirm + `--allow-deletes`,
  both double-conflict prompts, and `push --allow-deletes` semantics are
  untouched.
- Kinds not wired in `resolve_remote_deletes` (warn-and-skip arm) keep
  warning.

## Tests

- Resolver: `RemoteDelete` auto-resolves without consuming scripted stdin;
  file + sidecar removed; lockfile entry dropped; stale `-deleted` marker
  swept; non-interactive identical; mid-run-missing local file converges.
- Regression pins: `LocalEditRemoteDelete` and `LocalDeleteRemoteEdit`
  still prompt; existing scripted `k`/`r` tests over `RemoteDelete` migrate
  to `LocalEditRemoteDelete` so prompt mechanics stay covered.
- MDH: unchanged orphan auto-prunes; modified orphan prompts; non-TTY
  modified orphan defers with marker (existing tests adjusted, new ones
  added).
- Dry-run: `RemoteDelete` listed under pull side, not "would prompt".

## Docs sweep

- `execute.rs` module header and `resolve_remote_deletes` doc-comment
  (currently: "All three destructive-direction classes share the same
  prompt").
- `classify.rs` spec-table comments where they describe prompting.
- The sync spec document's prompt/class table, if it enumerates
  `RemoteDelete` as prompted.
