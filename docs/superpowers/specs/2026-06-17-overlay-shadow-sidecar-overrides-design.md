# Shadow-file sidecar overrides for `rdc migrate`

- Date: 2026-06-17
- Status: Approved design (pre-implementation)

## Problem

`rdc migrate <src> <tgt>` promotes a snapshot from one environment to another.
JSON objects can be tailored per target env via `overlay.toml` (a deep-merge of
attributes applied only during migrate — the "C-1" model in `src/overlay.rs`).
But **code/formula sidecar files are copied verbatim** (`transform_file`,
`src/cli/migrate/mod.rs:232-237`):

| Sidecar | JSON home | On-disk file |
|---|---|---|
| hook code | `config.code` | `hooks/<slug>.{py,js}` |
| rule code | `trigger_condition` | `rules/<slug>.py` |
| schema formula | `content[].formula` | `workspaces/<ws>/queues/<q>/formulas/<field>.py` |

There is no way to make a sidecar env-specific. Real case: a `sftp_export_path`
formula returns a constant path (e.g. `"/test/exports"`) that must differ in
production. Promoting test→prod clobbers prod's value. `overlay.toml`
cannot reach it: formulas are extracted *out* of `schema.json` into sidecars, and
even in-JSON the field lives in the `content[]` array, which the overlay's
deep-merge replaces wholesale rather than targeting (`merge_field`,
`src/overlay.rs:158-169`).

## Goal

Let a target env override the *content* of a code/formula sidecar during
`migrate`, without restating unrelated config.

### Non-goals (v1)

- Inline content in `overlay.toml` (considered, then dropped to keep scope minimal).
- Unified-diff / patch files (deferred; would add a dependency and patch brittleness).
- Overriding arbitrary files or whole JSON objects (JSON is already overlay-able).
- *Inventing* a sidecar that does not exist in the source; v1 only *replaces*.

## Design

### The `overlay/` directory

A new per-env directory `envs/<env>/overlay/` (sibling of the existing
`envs/<env>/overlay.toml`, `src/paths.rs:81`). Its files mirror the snapshot tree
using the **target** env's slugs/paths and shadow sidecar files. Example:

```
envs/prod/overlay/workspaces/main/queues/invoices/formulas/sftp_export_path.py
```

overrides the migrated
`envs/prod/workspaces/main/queues/invoices/formulas/sftp_export_path.py`.

The directory is committed to git as part of the env's configuration. It is **not**
a `MANAGED_DIR` (`src/cli/migrate/mod.rs:398` lists only `hooks, workspaces, rules,
labels, engines, workflows, mdh`), so `migrate`'s `enumerate_files` never treats it
as snapshot content, and pull/sync (which work off the lockfile + managed kinds)
ignore it.

### Behavior during `migrate`

`transform_file` (`src/cli/migrate/mod.rs:214-303`) processes each source file. For
non-JSON files (sidecars) it currently reads the source bytes and writes them
verbatim to the remapped target path (`:232-237`). New behavior:

1. Compute the target relative path `dst_rel` (already remapped src→tgt slugs).
2. If `<tgt_root>/overlay/<dst_rel>` exists, write **its** bytes to
   `<tgt_root>/<dst_rel>` instead of the source's.
3. Otherwise, copy verbatim (unchanged).

The override is a whole-file replacement; when a shadow exists the source sidecar's
content is ignored (its only role is to prove the path is a real sidecar — see
validation). The result is written into the target snapshot as an ordinary
`.py`/`.js`, so push/sync and `combined_hash` (`src/snapshot/codec/mod.rs:106-120`)
treat it as normal content. This stays within the migrate-only C-1 model: the
overlay is applied only at migrate time, and changing a shadow requires re-running
migrate.

### Scope guard: sidecars only

A shadow may target only a **code/formula sidecar**: a `.py`/`.js` file that
`classify_for_selection` (`src/cli/migrate/mod.rs:117-145`) maps to a
`hooks`/`rules`/`schemas` coordinate. JSON files are not shadow-able (use
`overlay.toml`).

### Transparent-error rule

Before writing anything, `migrate` validates `overlay/`:

- Build `produced` = the set of target sidecar relpaths migrate produces from the
  **full** source enumeration (`enumerate_files(src)` → remap → keep only sidecars).
  This is independent of `--only`.
- Enumerate every file under `<tgt_root>/overlay/`.
- Any overlay file whose relpath ∉ `produced` is a **hard error**: it overwrites
  nothing (a typo, a stale path, a `.json`, or a sidecar absent from the source).
  The error names the offending file(s) and aborts **before any target file is
  written** (fail-fast).

A shadow can therefore only *replace* an existing source sidecar, never *invent*
one. Validating against the full (not `--only`-filtered) set means scoping a run
with `--only` never produces false "overwrites nothing" errors for valid shadows it
simply did not apply this run.

### Interactions

- **`--only`**: filters which source files are processed/overridden; validation
  uses the full set (above), so a valid-but-unselected shadow does not error.
- **`--mirror`**: prunes target files not produced from source. `overlay/` is not
  enumerated, so it is never pruned; the overridden sidecar is produced at the
  target path, so it is kept.
- **`reconcile_target_identity`, ref substitution, JSON overlay**: JSON-only;
  sidecars do not pass through them, so they are unaffected.
- **Hashing**: `overlay/` is never hashed (it is not a managed sidecar); only the
  resulting target sidecar is, exactly as today.

## Backward compatibility

Purely additive:

- New binary, project without `overlay/`: no behavioral change.
- New binary, project with `overlay/`: shadows apply; dangling shadows error.
- **Old binary, project with `overlay/`**: the dir is ignored (not a `MANAGED_DIR`),
  so shadows silently do not apply (sidecars copied verbatim, as today) and no
  dangling-error fires. → Documented requirement: this feature needs rdc ≥ the
  release that ships it.
- `overlay.toml` parsing and the `Overlay` struct are **unchanged** — no new
  sections, no `OVERLAY_VERSION` change, no new dependency.

## Testing

Integration (`tests/cli_migrate.rs`):

- A shadow replaces a formula's content end-to-end; the target sidecar holds the
  shadow content.
- Sidecars without a shadow are copied from source unchanged.
- Shadows for hook code (both `.py` and the `.js` runtime variant) and rule code.
- A dangling shadow (no matching source sidecar) → error naming the file, with no
  target files written.
- A `.json` shadow → error (sidecars only).
- `--only` excludes a sidecar whose valid shadow exists → no error; the shadow is
  simply not applied that run.
- A follow-up (simulated) sync reads the overridden sidecar as normal content with
  hash parity.

Unit: sidecar-path detection / `produced`-set construction; `overlay/` enumeration;
dangling detection.

## Open implementation notes

- Reuse `classify_for_selection` to detect sidecar paths and to build `produced`.
- Place validation fail-fast in `migrate::run` before the per-file write loop.
- Confirm in implementation that pull/sync ignore `overlay/` in practice (expected:
  yes, since they operate off the lockfile + managed kinds).
