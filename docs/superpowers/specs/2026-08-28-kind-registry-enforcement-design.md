# Kind-registry enforcement, duplicate-rename guard, dry-run ref validation

## Problem

Three follow-ups from the `saved_views` branch
(`2026-08-27-saved-views-managed-kind-design.md`). The first is the important
one; the other two are defects that branch surfaced but did not fix.

**1. Adding a kind means registering it at ~15 dispatch sites, enumerated
nowhere but in prose.** Both real bugs on the saved-views branch were the same
failure, from the same incomplete hand-written list:

- `push::scan::change_list_from_classified` had no `saved_views` arm and a
  `_ => {}` catch-all, so every ordinary `LocalEdit`/`LocalCreate` was dropped
  before the push phase — no error, no warning, no request. The whole push half
  of the kind was dead, and every task-level test still passed, because driver
  tests build their `BTreeMap`s by hand.
- `sync::execute`'s `BothDeleted` dispatch had no `saved_views` arm either, so
  the `else` warn branch fired forever: a `Warn` on every sync, a lockfile entry
  claiming rdc tracks an object that exists nowhere, and a phantom `_index.md`
  row (it reads the lockfile, not disk). The tombstone loop filters on
  `LocalDelete` only, so nothing else clears it.

Kind #15 will hit this again. `snapshot::limits`'s
`every_pushable_kind_has_limits` already demonstrates the antidote for one site.

**2. `realign::detect_flat_kind` can half-apply a rename.** Its guard is
`proposed != *slug && !by_slug.contains_key(&proposed)` — it blocks a collision
with an *existing* slug but not two *pending* renames proposing the same new
one. Lockfile `old-name` + `old-name-2`, both files renamed to "New name" in the
UI, both propose `new-name`: the first `move_file` succeeds, the second bails
with "destination … already exists". No data loss (`move_file` guards), and a
second `doctor` run self-heals, but the user gets a half-applied rename and an
error. Pre-existing and shared across flat kinds — except `saved_views` is where
it will surface, because that spec records duplicate names as *routine* for the
kind (per-user namespaces, and the API does not enforce uniqueness).

**3. `migrate --dry-run` cannot forecast a saved-view refusal.** The check reads
each promoted view back off disk after the loop, and dry-run writes nothing, so
it prints an honest "not ref-validated" line instead. Dry-run is the main thing
people run before a promote, and this repo has already had to fix dry-run
blindness once (the MDH preview).

## Verified facts

### The dispatch-site matrix (read off the source, 2026-08-28)

| Site | Kinds handled | Silent on a miss? |
| --- | --- | --- |
| `ChangeList` struct fields | 12 (11 + `organization`) | no — compile error |
| `Tombstones` struct fields | 11 | no — compile error |
| `DeleteCounts` fields | 11 + `skipped`/`failed` | no — compile error |
| `change_list_from_classified` arms | 12 | **YES — `_ => {}`** |
| `BothDeleted` `matches!` | 11 | **YES — `else` warns forever** |
| `LocalDelete` tombstone arm | 11 | **YES** |
| `RemoteDeleteRefs` arms | 11 | **YES** |
| `deletes::apply_outcome` arms | 11 | **YES — `_ => {}`** |
| `DEPLOYABLE_KINDS` | 13 (+ `mdh`, `organization`) | no — used to drive `list_slugs` |
| `list_slugs` arms | 13 | **YES** |
| `realign::priority` | 4 named + `_ => 2` | yes, but correct fall-through for a flat leaf |
| `realign::compound_prefix_pairs` | 4 named + `_ => Vec::new()` | yes, but correct for a flat kind |
| pull-subset dispatch | 12 | yes — but `schemas`/`inboxes` are absent BY DESIGN (written by `pull::queues::process`) |

**There are no current gaps.** Every site is internally consistent, and the two
exclusion rules that make it look otherwise are deliberate:

- `organization` is push-capable (PATCH only — rdc never creates or deletes an
  org), so it is present in `change_list_from_classified` and absent from both
  delete paths and from `Tombstones`.
- `mdh` bypasses the sync classifier entirely (its own staged push cycle), and
  `workflows`/`workflow_steps` are pull-only, so all three are absent from every
  classifier site.

So this work **locks in correct state rather than repairing it**. That is a
correction to the saved-views branch's own risk note, which predicted gaps in
other kinds.

The capability sets are already mirrored exactly by two struct definitions:
`ChangeList`'s 12 fields are the push-capable set, and `Tombstones`' 11 are the
deletable set.

### Existing API this design leans on

- `slugify_unique(input: &str, used: &HashSet<String>) -> String`
  (`src/slug.rs:37`) appends `-2`, `-3`, … against a used-set.
- `DeleteCounts::total_deleted()` (`src/cli/push/deletes.rs:68`) and
  `apply_outcome(&mut DeleteCounts, kind: &str, outcome: DeleteOutcome)` (`:323`).
- `transform_file` (`src/cli/migrate/mod.rs:811`) already carries three
  `&mut Vec` accumulators (`id_hits`, `carried_prefixes`, `missing_schema_ids`)
  and 16 call sites. `settle(dst_path, bytes, dry_run)` classifies against the
  target and writes only when `!dry_run` — so **dry-run does compute the
  transformed body**, it just discards it.
- `promoted_saved_views: Vec<(String, PathBuf)>` (`:2707`) already exists but
  holds only the slug and target-relative path, not the body.
- `src/lib.rs` is a flat `pub mod` list; a new module slots in alphabetically.

## Decisions

1. **One capability model** in a new `src/kinds.rs`, with two sets, not one.
2. **Collapse only the list that can collapse**; enforce the rest with tests.
3. **`detect_flat_kind` suffixes duplicate proposals** (`new-name`,
   `new-name-2`) rather than skipping them, matching what a fresh `pull`
   produces. Two passes, so a stable object can never collide with itself.
4. **Dry-run and the real run share ONE validation path**, over a projected
   target set. Two paths for one check is the bug class this whole document is
   about.
5. **No `LOCKFILE_VERSION` bump. No new `rdc.toml` key. No new CLI flag. No new
   Cargo dependency.**

## Design

### A. `src/kinds.rs` — the capability model

```rust
/// Kinds `rdc sync` can write to the Rossum API.
///
/// `organization` is here but NOT in [`DELETABLE`]: rdc PATCHes an org and
/// never creates or deletes one. `mdh` is absent because it bypasses the sync
/// classifier entirely (its own staged push cycle), and
/// `workflows`/`workflow_steps` because they are pull-only at the API.
pub const PUSH_CAPABLE: &[&str] = &[
    "workspaces", "queues", "schemas", "inboxes", "email_templates",
    "hooks", "rules", "labels", "saved_views", "engines", "engine_fields",
    "organization",
];

/// Kinds a tombstone can turn into a remote DELETE.
pub const DELETABLE: &[&str] = &[
    "workspaces", "queues", "schemas", "inboxes", "email_templates",
    "hooks", "rules", "labels", "saved_views", "engines", "engine_fields",
];
```

Order within each list is irrelevant — every consumer is a membership test or an
iteration whose assertions are order-free. POST ordering lives in
`push_classified` and in `DEPLOYABLE_KINDS`, both of which stay where they are.

`DEPLOYABLE_KINDS` is NOT moved: it is a migrate concept, ordered by POST
dependency, and it includes `mdh`/`organization`. A test asserts it is a
superset of `PUSH_CAPABLE`, so the two cannot drift apart silently.

### B. Collapse the `BothDeleted` list

`sync::execute`'s `matches!(it.kind.as_str(), "labels" | "workspaces" | …)`
becomes `crate::kinds::DELETABLE.contains(&it.kind.as_str())`. One duplicated
literal list disappears. The `else` warn branch **stays** — it remains the right
behaviour for a kind genuinely outside the set, and it is what made the
saved-views gap eventually visible.

### C. Generic accessors + enforcement tests

`change_list_from_classified` and the tombstone arm cannot collapse to a
membership test: each arm derives a different on-disk path and writes a different
struct field. So each gets a test that iterates the capability set and asserts
the site *handles* every member. That needs one small new API, mirroring
`GenericMapping::kind_rows`:

```rust
impl ChangeList {
    /// Is `(kind, slug)` present? `organization` is a singleton keyed by the
    /// reserved slug `"self"`, so it answers through this same call.
    /// `false` for a kind this struct does not track — use `tracks` to tell
    /// "untracked kind" from "tracked kind, absent slug".
    pub fn contains(&self, kind: &str, slug: &str) -> bool;
    /// Does this struct have a slot for `kind` at all?
    pub fn tracks(&self, kind: &str) -> bool;
}
impl Tombstones {
    pub fn contains(&self, kind: &str, slug: &str) -> bool;
    pub fn tracks(&self, kind: &str) -> bool;
}
```

**Corrected during planning:** an earlier draft of this section proposed
`kind_map(kind) -> Option<&BTreeMap<..>>`. That cannot work —
`ChangeList.organization` is an `Option<PathBuf>` singleton, not a map, because
rdc PATCHes exactly one org per env. A boolean `contains` covers the maps and the
singleton uniformly, and it is all the enforcement tests need, so there is no
reason to expose the maps at all.

Both are `match kind { … }` over the existing fields — themselves compile-checked
by the struct definition. Tests:

| Test | Asserts |
| --- | --- |
| `every_push_capable_kind_reaches_the_change_list` | for each `PUSH_CAPABLE` kind, a `LocalEdit` item is reported by `ChangeList::contains(kind, slug)`. **The R15 bug.** |
| `every_push_capable_kind_has_a_change_list_slot` | `ChangeList::tracks` is true for each — so the accessor cannot silently miss a field |
| `every_deletable_kind_reaches_the_tombstones` | for each `DELETABLE` kind, a `LocalDelete` item is reported by `Tombstones::contains(kind, slug)` |
| `every_deletable_kind_is_counted_by_apply_outcome` | for each, a `Deleted` outcome raises `total_deleted()` by one |
| `every_deployable_kind_is_listable` | `list_slugs` resolves for each `DEPLOYABLE_KINDS` entry |
| `deployable_kinds_covers_push_capable` | `DEPLOYABLE_KINDS ⊇ PUSH_CAPABLE` |
| `organization_is_push_capable_but_not_deletable` | pins the asymmetry deliberately, so a future edit has to argue with a test |

`RemoteDeleteRefs` is deliberately NOT covered: its arms need a populated
`RemoteCatalog` per kind, so a generic loop would cost more scaffolding than the
guard is worth. `BothDeleted` needs no test — after B, membership *is* the const.

### D. `detect_flat_kind` — two passes

```
pass 1: for each slug, base = slugify(name).
        if base == slug the object is stable: reserve slug, emit nothing.
pass 2: for the remaining slugs, proposed = slugify_unique(name, &reserved).
        if proposed != slug: emit the rename and reserve proposed.
```

The two passes exist to prevent a self-collision: seeding `slugify_unique` with
every lockfile slug in a single pass would see a stable object's own slug in the
used-set and propose `-2` for it, inventing a rename where none is due.

Deterministic — `lockfile.objects[kind]` is a `BTreeMap`, so iteration is
slug-sorted and the lowest-sorting duplicate keeps the bare slug. Convergent — a
second `doctor` run finds every object stable. The existing
`!by_slug.contains_key(&proposed)` guard is subsumed by `reserved`, which starts
as every existing slug.

### E. Dry-run: one path over a projected target set

Two changes, both in `src/cli/migrate/mod.rs`:

**The body.** `transform_file` gains a fourth accumulator and pushes
`(slug, transformed_value)` itself for a saved view, because only it holds the
post-overlay body. `promoted_saved_views` becomes
`Vec<(String, serde_json::Value)>` and the caller's `push((slug, dst_rel))`
goes away. There are 16 call sites, but only ONE is production — `run_at`'s file loop. The other 15 are tests that already pass `&mut Vec::new()` three times and take a fourth mechanically, so the signature change is far cheaper than the raw count suggests.

Collecting post-overlay is what keeps the documented escape hatch working: the
overlay is applied inside `transform_file` before `settle`, so an overlay that
replaces `query` is what gets validated.

**`known` becomes projected, in both modes:**

```
known = { classify(rel) for rel in existing_target_files }
      ∪ { classify(dst_rel) for dst_rel this run would write }
      − { classify(rel) for rel in mirror_pruned }
```

The loop already computes every `dst_rel`; it simply does not keep them. This is
equivalent to today's post-write enumeration for a real run — existing files
cover the target-only objects an overlay may legitimately point at, and the write
set covers objects this run creates — and it is *correct* for dry-run, where a
disk read would miss everything the run would create and produce false
"unresolvable" errors.

It also removes the second walk of the target tree.

Because that equivalence is the whole risk, it is pinned rather than assumed:
`projected_known_matches_the_post_write_enumeration` runs a real (non-dry)
migrate and asserts the projected set equals `enumerate_files(tgt)` mapped
through `classify`.

The `dry_run` info line about not validating is deleted; dry-run now reports the
same refusal a real run does.

## Backward compatibility

- No `LOCKFILE_VERSION` bump, no `rdc.toml` key, no CLI flag, no new dependency.
- `src/kinds.rs` is additive.
- `detect_flat_kind`'s behaviour changes **only** where two objects' names
  slugify to the same slug. Single-name cases are byte-identical. Two sub-cases,
  both duplicate-name:
  - Two objects both renamed to the same new name: previously a half-applied
    rename plus an error, now two renames (`new-name`, `new-name-2`).
  - One object renamed onto a name a *stable* object already owns: previously
    skipped **permanently** — `doctor` would never realign it, so the slug stayed
    mismatched forever — now suffixed (`<taken>-2`). This is the more consistent
    outcome: it is what a fresh `pull` produces for duplicate names, and it
    converges, whereas the old skip did not. Found during implementation, where
    it changed one pre-existing test from `assert!(pending.is_empty())` to an
    exact `assert_eq!` on the suffixed rename.
- `migrate --dry-run` gains output it did not previously emit (a refusal where
  it used to print an info line). Any test asserting exact dry-run text must be
  re-checked — the implementation plan calls this out explicitly.
- `transform_file` is private to the module; its signature change reaches no
  external caller.

## Testing

Unit: the seven enforcement tests in C; `detect_flat_kind` gets a
stable-object-is-not-renamed test, a two-duplicates-get-suffixed test, and a
three-duplicates test (`x`, `x-2`, `x-3`) proving the reservation accumulates;
`projected_known_matches_the_post_write_enumeration`; and a dry-run test showing
the refusal now fires with the same message the real run produces.

Regression guard: the `BothDeleted` test added on the saved-views branch must
still pass after B replaces the literal list with the const.

Full suite plus `cargo clippy --all-targets -- -D warnings` once, at the end.

## Failure modes

| Situation | Behaviour |
| --- | --- |
| A future kind is added to `ChangeList` but not to `change_list_from_classified` | `every_push_capable_kind_reaches_the_change_list` fails |
| …added to `PUSH_CAPABLE` but no `ChangeList` field | `ChangeList::tracks` is false; `every_push_capable_kind_has_a_change_list_slot` fails |
| …added to `DELETABLE` but not to the tombstone arm or `apply_outcome` | the corresponding test fails |
| A kind is added to `DEPLOYABLE_KINDS` but not `list_slugs` | `every_deployable_kind_is_listable` fails |
| Someone makes `organization` deletable | `organization_is_push_capable_but_not_deletable` fails, forcing the argument into the open |
| Two objects' names collide under `doctor` | both rename, suffixed, deterministically |
| Three or more collide | `x`, `x-2`, `x-3`, … |
| `migrate --dry-run` on a view whose ref cannot cross | same refusal the real run gives |
| A dry-run projection disagrees with the real enumeration | Backed by three tests, none named `projected_known_matches_the_post_write_enumeration` — that test was specified here but replaced during planning and never written. What actually backs the row: `projected_known_a_write_beats_a_prune_of_the_same_slug` (the unit collision case), `migrate_mirror_accepts_a_saved_view_ref_to_a_queue_whose_workspace_moved` (a real `--mirror` run where an object moves path, driving the compiled binary), and `dry_run_and_real_run_reach_the_same_saved_view_verdict` (outcome equivalence across modes). Corrected after Task 7's failure-mode walk flagged the mismatch. |

## Out of scope

- `RemoteDeleteRefs` coverage (needs a per-kind populated `RemoteCatalog`).
- Moving `DEPLOYABLE_KINDS` into `kinds.rs` (it is migrate-specific and ordered).
- The pull-subset dispatch (its exclusion rule is "queue-nested", a different
  axis from push-capability).
- Any change to `mdh`, `workflows` or `workflow_steps` participation.
- The other deferred saved-views follow-ups: the overlay-escape-hatch test, the
  `field.<schema_id>` key limitation, and the foreign-host residual gap.
