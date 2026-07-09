# Design: ignore `score_threshold` on migrate (opt-in to carry)

Date: 2026-07-09
Status: implemented

## Problem

`score_threshold` values are tuned **per queue / per organization** and are
expected to diverge across orgs over time. Today `rdc migrate` carries them
verbatim from the source snapshot into the target, so promoting a snapshot
(e.g. `test` → `prod`) silently overwrites the target org's tuning with the
source org's numbers.

Two distinct fields are in scope (both verified against the Rossum reference):

1. **Per-datapoint** `score_threshold` (float 0–1) — lives on individual
   datapoints inside a schema's `content`. The field's AI-confidence cutoff for
   auto-validation.
2. **Queue-level** `default_score_threshold` (float 0–1) — the queue's fallback
   cutoff used when a datapoint has no `score_threshold` of its own.

### Current behavior (verified)

- `transform_file` (`src/cli/migrate/mod.rs`) copies a schema's `content`
  **verbatim** (only `rdc://` ref substitution + overlay are applied), so
  per-datapoint `score_threshold` is carried source → target.
- `reconcile_target_identity` reconciles only **top-level** object fields
  against the target file; it never descends into `content`, and
  `default_score_threshold` is treated as ordinary deployable content, so the
  queue default is carried too.
- The overlay engine (`src/overlay.rs`) deep-merges per-object fields but
  cannot walk the `content` array or delete keys — so it structurally cannot
  express "drop per-datapoint thresholds." (This is why an overlay-only
  solution was rejected.)

## Goal

On migrate, **ignore** both threshold fields by default — let the target org
own them — with a **CLI flag to opt back into** carrying the source's values
(today's behavior).

## Semantics

Thresholds are treated like env-specific values the target owns:

- **Matched target** (the target snapshot file already exists): take the
  **target's** current value for each affected field.
  - Schema: match datapoints by their stable `id` (recursively, through
    sections / multivalue / tuple nesting). For each source datapoint that also
    exists in the target, set its `score_threshold` to the target's value; if
    the target datapoint has no `score_threshold`, remove the key.
  - Queue: set `default_score_threshold` to the target queue's value, or remove
    it if the target lacks it.
  This preserves the target org's tuning across re-migrations (idempotent: the
  next migrate reads the same target file and re-applies the same values).
- **New target** (no target file yet): **drop** the field entirely
  (`score_threshold` from every datapoint, `default_score_threshold` from the
  queue) so it falls back to the queue/server default. The target org tunes
  later.

Field identification is **position-agnostic by key name**:
`default_score_threshold` is matched wherever it sits in the queue body
(top-level or nested under `settings`), because the sources disagree on its
exact position and we must not assume. `score_threshold` is matched on
datapoint objects within `content`. No other key (`automation`, other
`settings`) is touched.

## Opt-in

New flag `rdc migrate --migrate-score-thresholds` (default: **off**).

- Off (default): run threshold reconciliation as above.
- On: skip reconciliation entirely — source thresholds carry verbatim (today's
  behavior).

The flag is threaded `Command::Migrate` → `migrate::run` → `transform_file`.
It composes with `--only`, `--mirror`, and `--dry-run` unchanged (the
reconciliation is a per-file content transform, not a file-plan change).

## Implementation sketch

A new module-private function in `src/cli/migrate/mod.rs`, e.g.:

```
fn reconcile_score_thresholds(
    value: &mut Value,   // migrated object (post-subst, post-overlay)
    kind: &str,          // "schemas" | "queues" | other (no-op)
    tgt_path: &Path,     // target snapshot file (may not exist)
)
```

Called from `transform_file` after overlay application and before the final
`sort_url_arrays` + serialize, gated on `!migrate_score_thresholds`. It loads
the target file (if present), then:

- **schemas**: walk `content` collecting the target's datapoint
  `id → score_threshold` map; for each source datapoint, overwrite with the
  target's value or remove the key (matched); remove all `score_threshold`
  (new).
- **queues**: find `default_score_threshold` by key. If the target has a value,
  adopt it — updating in place when the migrated body already has the key, else
  inserting at the target's placement (under `settings` if that's where the
  target holds it, otherwise top-level) so a target that tuned a default the
  source never set is not silently reset. If the target lacks it (or is new),
  remove it everywhere.

Helpers (`find_key_value`, `set_existing_key`, `remove_key_everywhere`,
`collect_datapoint_thresholds`, `apply_datapoint_thresholds`) reuse the existing
recursive-walk style (cf. `walk_strings_mut`, `strip_noise_fields`).

## Backward compatibility

- **Behavior change**: migrate no longer carries thresholds by default. This is
  intentional and documented; `--migrate-score-thresholds` restores the prior
  behavior exactly.
- **No format change**: CLI-only flag. Overlay/lockfile formats and existing
  project files are unaffected; existing overlays keep working.
- **Idempotent**: re-running migrate reads the target file and re-applies the
  same target values (or the same drop), producing byte-stable output.
- The CLI help text and migrate docs note the new default and the flag.

## Testing

Unit tests for `reconcile_score_thresholds`:
- matched schema preserves the target's per-datapoint `score_threshold`,
  including nested sections and datapoint-`id` matching;
- new schema drops every `score_threshold`;
- matched queue preserves the target's `default_score_threshold`
  position-agnostically (top-level and under `settings`);
- new queue drops `default_score_threshold`;
- `--migrate-score-thresholds` carries source values verbatim (no-op pass);
- source datapoint absent from the target → key dropped (treated as new field).

Integration test in `tests/cli_migrate.rs`: migrate a schema+queue from src to a
matched tgt and assert the tgt's thresholds survive; assert the flag carries the
source's.

## Out of scope

- Any threshold-like field other than the two named keys.
- Sync/pull behavior (each env's snapshot remains its own source of truth).
- Remapping thresholds by value (e.g. scaling) — not requested.
