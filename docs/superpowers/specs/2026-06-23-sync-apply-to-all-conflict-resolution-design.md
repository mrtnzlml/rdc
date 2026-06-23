# Sync: "apply to all remaining" conflict resolution

**Date:** 2026-06-23
**Status:** Design — approved, pre-implementation

## Problem

`rdc sync` resolves conflicts one file at a time. When a sync produces many
conflicts (common during an environment migration or after a long divergence),
the operator must answer the same `[k]`/`[r]` prompt dozens of times even when
they already know they want a single, uniform outcome — e.g. "take everything
from the remote env" or "keep all my local edits".

There is no batch/sticky answer for conflict resolution today. (A sticky
"apply to all" exists, but only for `prompt_token_owner` — `inquire::Confirm`
at `src/cli/resolve.rs:1314` — and it does not cover conflict resolution.)

## Goal

Let the operator, **from inside the existing conflict prompt**, choose to apply
one resolution to **all remaining conflicts** in the current sync run, after a
single confirmation. No new CLI flag.

## Decisions (locked during brainstorming)

1. **Mechanism: in-prompt sticky**, not an up-front CLI flag. The escape hatch
   lives inside the conflict prompt loop and is discovered/triggered mid-run.
2. **Scope: both phases.** The sticky choice spans the content-conflict loop
   (`resolve_conflicts`, `BothDiverged`) **and** the delete-conflict loop
   (`resolve_remote_deletes`, `LocalEditRemoteDelete` / `LocalDeleteRemoteEdit`).
   One decision clears every remaining *prompted* conflict in the run.
3. **Keys: uppercase = all.** `K` = keep local for all remaining, `R` = use
   `{env}` for all remaining. A bulk confirmation is the safety net.

## Behavior

At any conflict prompt where **more than one prompted conflict remains in the
whole run**:

- `K` → keep local for **all** remaining conflicts (current + the rest, both phases).
- `R` → use `{env}` for **all** remaining conflicts (current + the rest, both phases).

Pressing `K`/`R` triggers one `[y/N]` confirmation (default **No**) that prints
an impact breakdown. On **yes**, the sticky choice is recorded and the current
item plus every subsequent prompted conflict resolves the chosen way with no
further prompts. On **no**, control returns to the current item's normal prompt;
nothing is resolved.

`RemoteDelete` (local unchanged, remote deleted) and `BothDeleted` already
auto-resolve without a prompt and are **unaffected** — the sticky only governs
the three *prompted* conflict classes.

### Prompt UI

Both prompt functions gain two uppercase options, shown **only when more than
one prompted conflict remains in the run** (mirrors how `[h]` only appears when
`hunk_count >= 2`):

Content conflict (`prompt_resolve_with_bytes_and_color`, `src/cli/resolve.rs:288`):

```
[k] keep local   [r] use prod
[K] keep ALL local   [R] use prod for ALL
[e] edit  [h] hunk-by-hunk  [s] skip  [a] abort >
```

Delete conflict (`prompt_remote_delete_with_color`, `src/cli/resolve.rs:425`):

```
[k] keep local (restore on prod)   [r] use prod (delete local)
[K] keep ALL local   [R] use prod for ALL
[s] skip  [a] abort >
```

When only one prompted conflict remains, the `[K]`/`[R]` line is omitted (it
would be identical to `[k]`/`[r]`).

### Key parsing / backward compatibility

Today the parser reads only the first char, **case-insensitively**: `K`→`k`,
`R`→`r`, `S`→`s`, `A`→`a`, `E`→`e`, `H`→`h` (`src/cli/resolve.rs:301`).

This design removes uppercase aliasing **only for `K` and `R`**, repurposing
them as the all-variants. `e`/`h`/`s`/`a` retain their current case-insensitive
behavior. Lowercase `k`/`r` behavior is **unchanged**.

The bulk confirmation (default No) is the backstop: a habitual capital-letter
typist who hits `K`/`R` by accident sees the confirmation and can decline,
landing back on the per-file prompt — no silent behavior change in the
dangerous direction.

### Confirmation

One confirmation at choice-time, default **No**, with counts derived from the
classified items (which both resolvers already receive in full):

```
Use prod for all 42 remaining conflicts?
  39 content files overwritten with prod
   2 local files deleted (remote deleted them)
   1 local file recreated from prod
Continue? [y/N]
```

For `K` (keep all local) the breakdown describes the keep-local outcomes
(local content kept and pushed; remote-deleted files restored on `{env}`;
locally-deleted files deleted on `{env}`).

Counts span both phases and are computed from the remaining prompted conflicts
at the moment of the choice. Categories with a zero count are omitted from the
breakdown (e.g. a run with no delete conflicts shows only the overwrite line).
If the choice is made during the delete phase, no content line appears.

## Per-class mapping

The sticky choice auto-selects the **existing** per-conflict resolution for each
class — no new resolution semantics are introduced.

| Class | `R` (use env) | `K` (keep local) |
|---|---|---|
| `BothDiverged` | overwrite local content with remote | keep local content, push it |
| `LocalEditRemoteDelete` | delete local file (mirror remote delete) | restore the object on `{env}` |
| `LocalDeleteRemoteEdit` | recreate local file from `{env}` | delete the object on `{env}` |

## Implementation shape

Grounded in the current code:

- **`Resolution` enum** (`src/cli/resolve.rs:54`): add two additive variants,
  `KeepLocalAll` and `KeepRemoteAll`. No existing variant changes — additive at
  the type level.
- **Prompt functions** (`prompt_resolve_with_bytes_and_color`,
  `prompt_remote_delete_with_color`): take a flag/count indicating whether the
  `[K]`/`[R]` options should be offered (i.e. whether >1 prompted conflict
  remains in the run); parse `K`→`KeepLocalAll`, `R`→`KeepRemoteAll` only when
  offered; otherwise treat them as unrecognized (reprompt) to avoid surprising
  uppercase behavior when the options aren't shown.
- **Sticky state**: a `BulkChoice` (`AllLocal` / `AllRemote`) carried as
  `Option<BulkChoice>`, owned by `execute::run` and threaded by `&mut` into
  **both** `resolve_conflicts(...)` (`src/cli/sync/execute.rs:81`, called at
  :2966) and `resolve_remote_deletes(...)` (:2030, called at :2980), which run
  sequentially in the same `run`.
- **Loop logic** in each resolver:
  - If the sticky is already set, skip the prompt and apply the corresponding
    `KeepLocal`/`KeepRemote` resolution directly to the item.
  - If a prompt returns `KeepLocalAll`/`KeepRemoteAll`, run the confirmation
    (computed from the full `classified` slice both resolvers already hold). On
    **yes**, set the sticky and resolve the current item with the mapped
    `KeepLocal`/`KeepRemote`. On **no**, re-prompt the current item.
- **Cross-phase counts**: both resolvers receive the full `classified` slice, so
  the count of remaining prompted conflicts (across both phases) is derivable in
  place without new plumbing of precomputed totals.

## Edge cases

- **Last / only conflict**: `[K]`/`[R]` are hidden; behaves exactly as today.
- **Non-interactive (`--yes` or non-TTY)**: unchanged. `is_interactive` is false
  (`src/cli/resolve.rs:77`), no prompts fire, so the sticky never engages; the
  shadow-file fallback for conflicts and the skip-marker for deletes are
  untouched.
- **`--dry-run`**: unchanged — it prints the plan and exits before resolution.
- **Decline confirmation**: returns to the current item's normal per-file prompt;
  no item is resolved and the sticky stays unset.
- **`e` / `h` after sticky**: once the sticky is set, no further prompts fire, so
  edit / hunk-by-hunk are simply not offered for the swept items (they remain
  available for any item resolved before the sticky was set).
- **`--allow-deletes`**: unaffected. That flag governs clean `LocalDelete` pushes
  (`src/cli/sync/execute.rs:3006`), a non-conflict path the sticky does not touch.

## Testing

Tests follow the existing pattern: canned `std::io::Cursor` input as `BufRead`,
captured `Vec<u8>` output (`src/cli/resolve.rs` test module ~:2110;
`resolve_conflicts` / `resolve_remote_deletes` integration tests in
`src/cli/sync/execute.rs` ~:3591 / ~:4103).

- **Unit (resolve.rs)**:
  - Typing `R` (when offered) → `Resolution::KeepRemoteAll`; `K` → `KeepLocalAll`.
  - `[K]`/`[R]` suppressed and `R`/`K` treated as unrecognized when only one
    prompted conflict remains.
  - Color and plain modes still render the prompt (extend existing color tests).
- **Integration (execute.rs)**:
  - `R\ny\n` at content item 1 of N → every content item resolves `KeepRemote`
    **and** the sticky carries into `resolve_remote_deletes` so the delete-phase
    items resolve `KeepRemote` too — assert filesystem + lockfile state for both
    phases.
  - Same for `K\ny\n` (keep-local across both phases: local pushed; restores /
    remote-deletes as mapped).
  - Confirm-declined path: `R\nn\nk\n` → only the current item is `KeepLocal`,
    the rest still prompt.
  - Backward-compat: lowercase `r` / `k` produce byte-identical behavior to the
    pre-change resolvers (single-item resolution, no sticky).

## Out of scope (YAGNI)

- No `S` (skip-all) variant.
- No new CLI flag (e.g. `--accept remote|local`).
- No change to non-interactive / CI behavior.
- No change to clean `--allow-deletes` deletion pushes.
