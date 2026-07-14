# Bidirectional deployment via an N-way slug map — design

- **Date:** 2026-07-14
- **Status:** Approved (design), pending implementation plan
- **Area:** `migrate`, mapping file, `deploy/map`, `paths`, docs

## Summary

`rdc` already runs `migrate` in either direction (`migrate dev prod` and
`migrate prod dev` execute the same code — envs are peers, the only rejection is
`src == tgt`). Object **identity** is anchored in the retained target snapshot
files (`reconcile_target_identity`), not in the mapping. So the mechanism is
already bidirectional; the weakness is the **slug-alignment layer**: it is stored
as **one file per ordered pair** (`.rdc/map/<src>-to-<tgt>.toml`), the two
directions are independent files that can silently disagree, and the files are
bloated with one identity row per shared object.

This design replaces the per-pair files with a **single, generic, N-way mapping
file** — `.rdc/mapping.toml` — in which each entry is a logical object and each
environment names its own slug. Direction disappears (any pair is read from the
same rows), global consistency is enforced by construction, and only genuine
cross-env divergences are stored (identity is the default).

## Motivation — the two scenarios

- **Build a new project:** promote `dev`/`test` → `prod` (forward).
- **Attend an existing project:** replicate `prod` → `dev`/`test` (reverse), edit
  in a lower env, then promote back up.

Both must be first-class and safe to round-trip (`prod → dev → prod` must PATCH
the same prod objects, never POST duplicates).

## Verified facts (grounding)

All confirmed by reading the code, not assumed:

1. **Envs are peers.** `EnvConfig` is `{api_base, org_id}` only; no "primary" or
   direction (`config/mod.rs`). `migrate` is fully direction-parameterized
   (`migrate/mod.rs`), only rejecting `src == tgt`.
2. **Identity lives in the snapshot, not the mapping.**
   `reconcile_target_identity` (`migrate/mod.rs:864`): if a file exists at the
   remapped target path, the target's `id`/`url`/`organization`/reverse-refs are
   copied onto the content (→ `sync` PATCHes); otherwise server fields are
   stripped (→ `sync` POSTs). The chain is **slug → path → existing target file →
   identity**.
3. **The mapping is a one-way slug-rename table.** `<src>-to-<tgt>.toml`
   (`paths.rs:124`), keyed `src_slug → tgt_slug` per kind. `prod-to-dev.toml` and
   `dev-to-prod.toml` are separate files, not guaranteed inverses.
4. **`auto_match` only ever inserts identity pairs** (`deploy/map.rs match_kind`),
   and no consumer needs them materialized: `build_subst` skips identity pairs,
   and `mirror_prune_paths` / `unique_template_skips` remap through `tgt_slug`,
   which already **falls back to identity** when a pair is absent
   (`migrate/mod.rs:45`). The `map.values()` calls at `migrate/mod.rs:749,810`
   operate on `serde_json` bodies, not the `Mapping`.
5. **`push`/`sync` can CREATE every kind, including workspaces**
   (`push/workspaces.rs`), so replicating into an empty target org works.
6. **rdc cannot create organizations** (no `create_organization` path; `org_id`
   is supplied at `init`). "Greenfield" always means an existing-but-empty target
   org that rdc fills.
7. **`mapping.hook_templates` is vestigial.** Nothing reads it (only the struct
   field, `Default`, and a test reference it); it is absent from `SUBST_KINDS`
   (`migrate/mod.rs:32`) and `kind_map`/`lookup_tgt_slug` return `None` for it.
   Cross-cluster hook-template retargeting is done by
   `retarget_hook_template(url, api_base)` (`store_extensions.rs:63`), a host-swap
   that keeps the id and never touches the mapping. Its `mapping.rs` doc comment
   still cites `rdc deploy`, which was removed.
8. **`.rdc/map/` is committed** (only `/.rdc/cache` and `/.rdc/state/*.lock|*.base`
   are gitignored), so mapping files are shared across a team and backward
   compatibility is a real requirement — the format change must land as a clean,
   reviewable git diff.
9. **Env names are `[A-Za-z0-9-_]`** (`init.rs`). (Relevant only to the rejected
   per-pair-filename options; the chosen design puts no env names in filenames.)
10. **On-disk slugs are globally unique per kind** (prior id-pinned-slug work), so
    a slug uniquely identifies an object — identity-default lookup is unambiguous.

Real-world scale check on a live multi-env project (6 envs across two promotion
tracks, 3 committed pairwise files, ~25–30 KB each): **1,085 identity rows, 0
slug renames, 6 `hook_template` URL rows total.** The entire pairwise apparatus
stored 6 facts as ~1,091 rows. Under this design its mapping file becomes empty.

## Design

### 1. The generic N-way mapping file

One project-level file, `.rdc/mapping.toml`. Each entry is a **logical object**;
each environment names its own slug. Only objects whose slug **differs** across
envs need an entry — identical slugs map 1:1 by default (no entry).

```toml
# .rdc/mapping.toml
version = 2

# Only objects whose slug differs across envs appear here.
# Identical slugs map 1:1 automatically.

[[queues]]
dev  = "invoices"
prod = "invoices-prod"

[[hooks]]
dev  = "master-data-hub"
test = "mdh-test"
prod = "mdh-prod"
```

- Per-kind arrays-of-tables for every deployable kind: `workspaces`, `hooks`,
  `rules`, `labels`, `schemas`, `queues`, `inboxes`, `email_templates`,
  `engines`, `engine_fields`. **No `hook_templates`** (dropped — see §5).
- Each row is a `BTreeMap<env_name, slug>`. Compound-key kinds keep their
  compound slugs per env (`email_templates` = `<ws>/<q>/<template>`,
  `engine_fields` = `<engine>/<field>`) — each env states its own, exactly as the
  legacy files did.
- A row lists only the envs relevant to that divergence; unlisted envs fall back
  to identity (see lookup rules).
- The file is often small or absent. For a project with no cross-env renames it
  **does not exist**.

### 2. On-disk type and orientation

New on-disk type (in `mapping.rs`):

```
struct GenericMapping {
    version: u32,                             // 2
    workspaces: Vec<BTreeMap<String,String>>, // each map: env_name -> slug
    hooks:      Vec<BTreeMap<String,String>>,
    // ... one Vec<row> per kind above ...
}
```

`GenericMapping::orient(src_env, tgt_env) -> Mapping` produces the **existing**
in-memory `Mapping` (per-kind `BTreeMap<src_slug, tgt_slug>`) that `migrate`
already consumes — so `tgt_slug`, `build_subst`, `lookup_tgt_slug`, and
`remap_relative` are **unchanged**:

- For each kind, for each row: if `row` contains both `src_env` and `tgt_env` and
  the two slugs differ, insert `row[src_env] -> row[tgt_env]`.
- Rows are keyed by the **source env's column**: a lookup for `(src_env,
  src_slug)` matches only a row whose `row[src_env] == src_slug`. This is what
  keeps same-named objects in different tracks/workspaces from cross-matching.
- Absent pair, or `tgt_env` column missing from the matched row → the existing
  identity fallback in `tgt_slug` returns `src_slug`.

### 3. Round-trip guardrails

The N-way table structurally prevents the original hazard: there is no second
file to disagree with, and a single table cannot hold two conflicting slugs for
one env. Remaining, deliberately minimal, guardrails:

- **CREATE-vs-UPDATE summary at `migrate` time.** `reconcile_target_identity`
  already classifies each object matched-vs-new; `migrate` prints a summary line
  (e.g. `→ prod: 42 update, 0 create`). On a promote-back the user expects
  all-updates; an unexpected create is the red flag for a missing rename row.
  (`sync --dry-run` already previews the POSTs/PATCHes.)
- **`mapping.toml` validation** (hard errors, clear messages): every env column
  name must be a real env in `rdc.toml`; within a kind, two rows may not map the
  same `(env, slug)` to different targets.
- **Stale rows are warned, not auto-pruned.** A hand-authored row referencing a
  slug that no longer exists on disk is surfaced for the user to fix (consistent
  with the existing "mapping files are user-authored; warn rather than rewrite"
  stance in `realign.rs`). No silent deletion of hand-authored content.
- **No fuzzy "similar-object" heuristic** — too imprecise; the create/update
  summary is the honest signal.

### 4. Greenfield / replicate flow

Verified constraint: the target org pre-exists (rdc cannot create orgs), and
`rdc init` (add env) → `rdc migrate prod dev` → `rdc sync dev` already works into
an empty tree. The gaps are discoverability and overlay authoring:

- **Docs:** add a "Replicate an existing env into a new one" section (README
  currently shows only `test → prod`).
- **Empty-target hint:** when the target snapshot is empty, `migrate` prints one
  line — that the run will create N objects, and to author
  `envs/<tgt>/overlay.toml` for env-specific values.
- **No overlay scaffold** (explicitly out of scope; keep the change minimal).
- **No new top-level verb** — the existing two-step is documented, not replaced.

### 5. What is removed / simplified

- **Per-pair files** (`.rdc/map/<src>-to-<tgt>.toml`) → one `.rdc/mapping.toml`.
- **Identity-pair persistence** → dropped; identity is the lookup default.
- **`auto_match`'s persistence role** → removed. Its only output was identity
  pairs; nothing needs them materialized (fact 4). Renames are hand-authored.
  `deploy/map.rs` is slimmed/removed accordingly (verify no other caller first).
- **`hook_templates` in the mapping** → dropped. It is vestigial (fact 7);
  the current binary already ignores it, so removal is behavior-neutral.
  Cross-cluster retargeting stays with `retarget_hook_template`.
- **`migrate` no longer writes the mapping** in normal operation (renames are
  hand-authored; identity is default). The only write is the one-time v1→v2
  conversion (§Backward compatibility).

`build_subst` (portable `rdc://<kind>/<slug>` ref rewriting) is unchanged — it
consumes the oriented `Mapping` exactly as today.

## Backward compatibility & v1→v2 migration

Mapping files are committed, so migration must be clean and reviewable.

- **Detection is by filename.** New format = `.rdc/mapping.toml`; legacy =
  `.rdc/map/*-to-*.toml`. They never collide.
- **Lazy, one-time auto-migration** on first mapping access: if `mapping.toml` is
  absent but legacy files exist, convert, write `mapping.toml`, and delete the
  legacy files (a clean git diff: N deletions + 1 addition, or just deletions
  when there are no renames).
- **Conversion algorithm** (per kind):
  1. Parse each legacy file into edges `(env_a, slug_a) — (env_b, slug_b)`
     (env names from the `<a>-to-<b>` filename; sides from key/value).
  2. Drop identity edges (`slug_a == slug_b`).
  3. Union-find the remaining edges into connected components; each component
     becomes one N-way row.
  4. **Hard-error on inconsistency** — if a component forces one env to two
     different slugs, list the conflict and abort (write nothing). This is the
     exact latent-drift class the design eliminates going forward.
  5. Drop all `hook_templates` sections.
- **Acceptance against a real multi-env project:** the live project measured
  above (0 renames, `hook_templates` dropped) produces an **empty/absent**
  `mapping.toml`; its three legacy files are removed with zero behavior change.
  Migration will be run there carefully (no destructive git; diff reviewed;
  follow-up `migrate`/`sync` confirmed byte-stable and idempotent).

## Testing

- **Unit:**
  - `orient` lookup: identity default; source-env-column keying disambiguates
    same-slug objects across tracks; missing `tgt_env` column → identity.
  - `GenericMapping` TOML load/save round-trip (array-of-tables, env-name keys).
  - v1→v2 conversion: transitive multi-file union-find join; identity-edge drop;
    inconsistency hard-error; `hook_templates` drop; multi-pair merge.
  - `mapping.toml` validation (unknown env column; conflicting rows).
- **Live (sandbox org):** greenfield replicate A→B, edit in B, promote B→A →
  assert PATCH with zero duplicate creates; confirm reverse-direction symmetry.
- **Real-project acceptance:** 3 legacy files → empty `mapping.toml`, no churn,
  idempotent re-run.

## Affected code (file-level map)

- `mapping.rs` — new `GenericMapping` type + `orient`; keep `Mapping` as the
  oriented in-memory view; remove vestigial `hook_templates`; v1 parse + convert.
- `paths.rs` — `mapping_file` → single `.rdc/mapping.toml` (drop `<src>-to-<tgt>`).
- `cli/migrate/mod.rs` — load `GenericMapping`, orient for `(src,tgt)`; drop
  `auto_match`/stale-prune write path; add empty-target hint + create/update
  summary; run lazy v1→v2 conversion.
- `cli/deploy/map.rs` — remove `auto_match` and stale helpers (or reduce to the
  warn-only stale check); confirm no remaining caller.
- `cli/deploy/realign.rs` — mapping scan already format-agnostic (text match);
  verify it still finds slugs in the new layout.
- `README.md` — replicate-an-env section; note the single mapping file.

## Out of scope

- Merging `overlay.toml` (env-specific field values) into the mapping file.
  Mapping aligns **slugs**; overlay sets **values**. A unified "env-specifics"
  file is a separate future project.
- Any change to `reconcile_target_identity` / how identity is anchored.
- Creating organizations or workspaces beyond what `sync` already does.
- A dedicated `replicate`/`clone` command.
- Re-examining `retarget_hook_template`'s same-id assumption (pre-existing; not
  introduced here).
