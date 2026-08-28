# Kind-registry enforcement, duplicate-rename guard, dry-run ref validation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make "a new kind was not registered at a dispatch site" a test failure instead of a silent bug, and fix two defects the saved-views branch surfaced but did not fix.

**Architecture:** One capability model in a new `src/kinds.rs` (`PUSH_CAPABLE`, `DELETABLE`). The single dispatch list that can collapse to a membership test does; the sites whose arms do per-kind work get tests that iterate the capability set instead, via new boolean `contains`/`tracks` accessors on `ChangeList`/`Tombstones`. Separately, `realign::detect_flat_kind` becomes two passes so duplicate name-collisions get `-2` suffixes rather than half-applying a rename, and `migrate`'s saved-view ref validation collapses onto one path over a projected target set so `--dry-run` forecasts the same refusal a real run gives.

**Tech Stack:** Rust (edition 2024), `serde_json` with `preserve_order`, `tempfile`, `wiremock` + `assert_cmd` for integration tests.

**Spec:** `docs/superpowers/specs/2026-08-28-kind-registry-enforcement-design.md`

## Global Constraints

- **No `LOCKFILE_VERSION` bump. No new `rdc.toml` key. No new CLI flag. No new Cargo dependency.**
- The capability sets, copied verbatim from the spec:
  - `PUSH_CAPABLE` = `workspaces`, `queues`, `schemas`, `inboxes`, `email_templates`, `hooks`, `rules`, `labels`, `saved_views`, `engines`, `engine_fields`, `organization` (12)
  - `DELETABLE` = the same **minus `organization`** (11)
- Deliberate exclusions to preserve, each documented at its site: `organization` is push-capable (PATCH only) but never deletable; `mdh` bypasses the sync classifier; `workflows`/`workflow_steps` are pull-only.
- Kind strings are underscored (`saved_views`); on-disk directories are hyphenated (`saved-views`).
- Never put a customer name or customer-specific identifier in code, tests, fixtures, docs **or commit messages**. Use `acme`, `main`, `invoices`, `test`/`dev`/`prod`, `field_a`, `document_id`.
- **Build economy:** this crate is slow to compile. Per task run only the filtered test the task names. The whole suite runs once, in Task 7. Never start a rebuild while an integration suite is running — it swaps `target/debug/rdc` under the tests that spawn it.
- **Never run `cargo fmt`.** This crate is not fmt-clean under current rustfmt; `cargo fmt --check` failing is a known pre-existing condition, NOT a regression. Match surrounding style by hand.
- Commit to local `main`. **Never `git push`.**
- Every commit message ends with `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`.

---

### Task 1: `src/kinds.rs` — the capability model

**Files:**
- Create: `src/kinds.rs`
- Modify: `src/lib.rs` (add `pub mod kinds;` to the alphabetical list, between `config` and `log`)
- Modify: `src/cli/deploy/selection.rs` (tests only — the two consistency tests)

**Interfaces:**
- Consumes: nothing.
- Produces: `crate::kinds::PUSH_CAPABLE: &[&str]` (12 entries) and `crate::kinds::DELETABLE: &[&str]` (11). Tasks 2, 3 and 4 all consume these.

- [ ] **Step 1: Write the failing tests**

Create `src/kinds.rs` containing only the tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletable_is_push_capable_without_the_organization() {
        let expected: Vec<&str> = PUSH_CAPABLE
            .iter()
            .copied()
            .filter(|k| *k != "organization")
            .collect();
        assert_eq!(DELETABLE, expected.as_slice());
    }

    /// The asymmetry is deliberate and load-bearing: rdc PATCHes exactly one
    /// organization per env and never creates or deletes one, which is why
    /// `Tombstones` has no `organization` field. Anyone changing this has to
    /// argue with a test.
    #[test]
    fn organization_is_push_capable_but_not_deletable() {
        assert!(PUSH_CAPABLE.contains(&"organization"));
        assert!(!DELETABLE.contains(&"organization"));
    }

    /// The three kinds deliberately outside both sets, so a future edit that
    /// adds one has to say why.
    #[test]
    fn classifier_bypassing_kinds_are_in_neither_set() {
        for kind in ["mdh", "workflows", "workflow_steps"] {
            assert!(!PUSH_CAPABLE.contains(&kind), "{kind} must stay out of PUSH_CAPABLE");
            assert!(!DELETABLE.contains(&kind), "{kind} must stay out of DELETABLE");
        }
    }

    #[test]
    fn sets_have_no_duplicates() {
        for set in [PUSH_CAPABLE, DELETABLE] {
            let mut sorted: Vec<&str> = set.to_vec();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(sorted.len(), before, "duplicate kind in {set:?}");
        }
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib kinds`
Expected: FAIL to compile — `cannot find value PUSH_CAPABLE` (the module has no consts yet).

- [ ] **Step 3: Implement the module**

Prepend to `src/kinds.rs`:

```rust
//! The kind-capability model: which Rossum object kinds rdc can write, and
//! which it can delete.
//!
//! Adding a kind means registering it at roughly fifteen dispatch sites. Most
//! are compile-checked (a struct field, a fixed-size array, an exhaustive
//! `match`), but several are not: `push::scan::change_list_from_classified` has
//! a `_ => {}` catch-all, `sync::execute`'s `BothDeleted` dispatch has an `else`
//! branch, and `deploy::selection::list_slugs` has a catch-all too. A kind
//! missing from one of those fails SILENTLY — that is exactly how the
//! saved-views kind shipped twice-broken before the gaps were found.
//!
//! These two lists are the single source of truth those sites are tested
//! against. See `docs/superpowers/specs/2026-08-28-kind-registry-enforcement-design.md`.
//!
//! Order is irrelevant: every consumer is a membership test or an
//! order-independent iteration. POST ordering lives in `push::push_classified`
//! and in `deploy::selection::DEPLOYABLE_KINDS`.

/// Kinds `rdc sync` can write to the Rossum API.
///
/// `organization` is here but NOT in [`DELETABLE`]: rdc PATCHes an org and
/// never creates or deletes one, which is why `Tombstones` has no
/// `organization` field.
///
/// Absent on purpose: `mdh` bypasses the sync classifier entirely (it has its
/// own staged push cycle), and `workflows` / `workflow_steps` are pull-only at
/// the Rossum API (PATCH returns 405).
pub const PUSH_CAPABLE: &[&str] = &[
    "workspaces",
    "queues",
    "schemas",
    "inboxes",
    "email_templates",
    "hooks",
    "rules",
    "labels",
    "saved_views",
    "engines",
    "engine_fields",
    "organization",
];

/// Kinds a tombstone can turn into a remote DELETE.
///
/// [`PUSH_CAPABLE`] minus `organization` — see that list's note.
pub const DELETABLE: &[&str] = &[
    "workspaces",
    "queues",
    "schemas",
    "inboxes",
    "email_templates",
    "hooks",
    "rules",
    "labels",
    "saved_views",
    "engines",
    "engine_fields",
];
```

Add to `src/lib.rs`, keeping the list alphabetical:

```rust
pub mod kinds;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib kinds`
Expected: PASS (4 tests).

- [ ] **Step 5: Add the `DEPLOYABLE_KINDS` consistency tests**

`DEPLOYABLE_KINDS` deliberately stays in `src/cli/deploy/selection.rs` — it is a
migrate concept, ordered by POST dependency, and includes `mdh`/`organization`.
Pin the relationship so the two cannot drift. Append to that file's `mod tests`:

```rust
    /// Everything rdc can push must also be promotable, or `rdc migrate` would
    /// silently skip a kind `rdc sync` manages.
    #[test]
    fn deployable_kinds_covers_every_push_capable_kind() {
        for kind in crate::kinds::PUSH_CAPABLE {
            assert!(
                DEPLOYABLE_KINDS.contains(kind),
                "{kind} is push-capable but not in DEPLOYABLE_KINDS",
            );
        }
    }

    /// The two extras are deliberate: `mdh` is promotable but bypasses the sync
    /// classifier, and both it and `organization` have no create-order
    /// dependency. Naming them keeps the difference intentional.
    #[test]
    fn deployable_kinds_extras_are_only_mdh() {
        let extras: Vec<&str> = DEPLOYABLE_KINDS
            .iter()
            .copied()
            .filter(|k| !crate::kinds::PUSH_CAPABLE.contains(k))
            .collect();
        assert_eq!(extras, vec!["mdh"]);
    }
```

- [ ] **Step 6: Run them**

Run: `cargo test --lib deploy::selection`
Expected: PASS. If `deployable_kinds_extras_are_only_mdh` fails, print the actual
`extras` and reconcile with the spec's matrix rather than loosening the assertion.

- [ ] **Step 7: Commit**

```bash
git add src/kinds.rs src/lib.rs src/cli/deploy/selection.rs
git commit -m "$(cat <<'MSG'
feat(kinds): add the kind-capability model

Two lists, PUSH_CAPABLE (12) and DELETABLE (11), as the single source of truth
for the dispatch sites where a missing kind fails silently.

Two sets rather than one because the asymmetry is real and already encoded in
the structs: ChangeList has 12 fields, Tombstones 11. rdc PATCHes exactly one
organization per env and never creates or deletes one.

DEPLOYABLE_KINDS stays in deploy::selection -- it is a migrate concept, ordered
by POST dependency -- with tests pinning that it covers PUSH_CAPABLE and that
its only extra is mdh.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 2: `contains`/`tracks` accessors and the change-list / tombstone enforcement tests

This is the guard for the worst bug on the saved-views branch: `change_list_from_classified` had no `saved_views` arm and a `_ => {}` catch-all, so every ordinary local edit was dropped before the push phase — no error, no warning, no request.

**Files:**
- Modify: `src/cli/push/scan.rs` (two `impl` blocks, plus tests at the end of the existing `mod tests`)

**Interfaces:**
- Consumes: `crate::kinds::{PUSH_CAPABLE, DELETABLE}` (Task 1).
- Produces: `ChangeList::contains(&self, kind: &str, slug: &str) -> bool`, `ChangeList::tracks(&self, kind: &str) -> bool`, `Tombstones::contains(&self, kind: &str, slug: &str) -> bool`, `Tombstones::tracks(&self, kind: &str) -> bool`.

**Why boolean accessors rather than exposing the maps:** `ChangeList.organization`
is an `Option<PathBuf>` singleton, not a `BTreeMap`, so a `kind_map` returning
`Option<&BTreeMap<..>>` cannot cover it. A boolean covers the maps and the
singleton uniformly and is all the tests need.

- [ ] **Step 1: Write the failing tests**

Append inside the existing `#[cfg(test)] mod tests` in `src/cli/push/scan.rs`:

```rust
    /// Per push-capable kind: the slug to classify, and the files that must
    /// exist on disk for the change-list arm to find it.
    ///
    /// Several arms only insert when a real file is found — `queues`,
    /// `schemas`, `inboxes` sweep `workspaces/*/queues/<slug>/`, and
    /// `engine_fields` resolves a `<engine>/<field>` composite key — so a
    /// synthetic item alone would silently not be inserted and the test would
    /// pass for the wrong reason.
    fn push_capable_fixture() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
        vec![
            ("workspaces", "main", vec!["workspaces/main/workspace.json"]),
            ("queues", "invoices", vec!["workspaces/main/queues/invoices/queue.json"]),
            ("schemas", "invoices", vec!["workspaces/main/queues/invoices/schema.json"]),
            ("inboxes", "invoices", vec!["workspaces/main/queues/invoices/inbox.json"]),
            (
                "email_templates",
                "main/invoices/ack",
                vec!["workspaces/main/queues/invoices/email-templates/ack.json"],
            ),
            ("hooks", "validator", vec!["hooks/validator.json"]),
            ("rules", "totals", vec!["rules/totals.json"]),
            ("labels", "urgent", vec!["labels/urgent.json"]),
            ("saved_views", "awaiting", vec!["saved-views/awaiting.json"]),
            ("engines", "extractor", vec!["engines/extractor/engine.json"]),
            ("engine_fields", "extractor/amount", vec!["engines/extractor/fields/amount.json"]),
            ("organization", "self", vec!["organization.json"]),
        ]
    }

    /// The fixture must cover PUSH_CAPABLE exactly. Without this, adding a kind
    /// to PUSH_CAPABLE and forgetting the fixture would make the enforcement
    /// test below quietly skip it — the same silent-omission failure this whole
    /// change exists to prevent.
    #[test]
    fn push_capable_fixture_covers_every_push_capable_kind() {
        let mut fixture: Vec<&str> = push_capable_fixture().iter().map(|(k, _, _)| *k).collect();
        let mut expected: Vec<&str> = crate::kinds::PUSH_CAPABLE.to_vec();
        fixture.sort_unstable();
        expected.sort_unstable();
        assert_eq!(fixture, expected);
    }

    #[test]
    fn every_push_capable_kind_has_a_change_list_slot() {
        let cl = ChangeList::default();
        for kind in crate::kinds::PUSH_CAPABLE {
            assert!(cl.tracks(kind), "ChangeList has no slot for '{kind}'");
        }
    }

    #[test]
    fn every_deletable_kind_has_a_tombstones_slot() {
        let t = Tombstones::default();
        for kind in crate::kinds::DELETABLE {
            assert!(t.tracks(kind), "Tombstones has no slot for '{kind}'");
        }
        assert!(
            !t.tracks("organization"),
            "an organization can never be deleted, so it must have no tombstone slot",
        );
    }

    /// The regression guard for the worst bug on the saved-views branch:
    /// `change_list_from_classified` silently dropped a kind with no arm, so
    /// every ordinary local edit of it made no request at all.
    #[test]
    fn every_push_capable_kind_reaches_the_change_list() {
        use crate::cli::sync::classify::{ClassifiedItem, SyncClass};

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let root = paths.env_root();

        let fixture = push_capable_fixture();
        for (_, _, files) in &fixture {
            for rel in files {
                let p = root.join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"{}").unwrap();
            }
        }

        let items: Vec<ClassifiedItem> = fixture
            .iter()
            .map(|(kind, slug, _)| ClassifiedItem {
                kind: (*kind).to_string(),
                slug: (*slug).to_string(),
                class: SyncClass::LocalEdit,
                local_hash: None,
                remote_hash: None,
                base_hash: None,
            })
            .collect();

        let cl = change_list_from_classified(&paths, &items);

        for (kind, slug, _) in &fixture {
            assert!(
                cl.contains(kind, slug),
                "'{kind}' is push-capable but change_list_from_classified dropped it",
            );
        }
    }

    /// A class that is not a local change must never reach the change list.
    #[test]
    fn a_non_local_class_does_not_reach_the_change_list() {
        use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.labels_dir()).unwrap();
        std::fs::write(paths.labels_dir().join("urgent.json"), b"{}").unwrap();

        let items = vec![ClassifiedItem {
            kind: "labels".to_string(),
            slug: "urgent".to_string(),
            class: SyncClass::RemoteEdit,
            local_hash: None,
            remote_hash: None,
            base_hash: None,
        }];
        let cl = change_list_from_classified(&paths, &items);
        assert!(!cl.contains("labels", "urgent"));
    }

    /// The scan-side counterpart: `detect_tombstones` dispatches per kind too,
    /// and a deletable kind missing from it would mean a deleted local file
    /// never becomes a remote delete — the object would linger in the env
    /// forever with no diagnostic.
    ///
    /// Seeded with lockfile entries and an EMPTY tree, so every entry is
    /// file-less and must therefore be tombstoned.
    #[test]
    fn every_deletable_kind_reaches_the_tombstones() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");

        // Compound-key kinds need a well-formed key: `detect_tombstones` splits
        // email_templates on '/' expecting `<ws>/<queue>/<template>`, and
        // engine_fields expecting `<engine>/<field>`.
        let slug_for = |kind: &str| -> &'static str {
            match kind {
                "email_templates" => "main/invoices/ack",
                "engine_fields" => "extractor/amount",
                _ => "thing",
            }
        };

        let mut lockfile = Lockfile::default();
        for kind in crate::kinds::DELETABLE {
            lockfile.upsert(
                kind,
                slug_for(kind),
                crate::state::ObjectEntry {
                    id: 1,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some("h".to_string()),
                    secrets_hash: None,
                },
            );
        }

        let t = detect_tombstones(&paths, &lockfile);
        for kind in crate::kinds::DELETABLE {
            assert!(
                t.contains(kind, slug_for(kind)),
                "'{kind}' is deletable but detect_tombstones did not tombstone it",
            );
        }
    }

    #[test]
    fn contains_is_false_for_an_untracked_kind() {
        let cl = ChangeList::default();
        assert!(!cl.tracks("mdh"));
        assert!(!cl.contains("mdh", "anything"));
        assert!(!cl.tracks("workflows"));
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib push::scan`
Expected: FAIL to compile — `no method named tracks` / `no method named contains`.

- [ ] **Step 3: Implement the accessors**

Add to `src/cli/push/scan.rs`, inside `impl ChangeList`:

```rust
    /// Does this struct have a slot for `kind` at all?
    ///
    /// Distinguishes "a kind rdc does not push" from "a kind rdc pushes that
    /// happens to have no changes right now" — [`Self::contains`] answers
    /// `false` for both.
    pub fn tracks(&self, kind: &str) -> bool {
        matches!(
            kind,
            "workspaces"
                | "queues"
                | "schemas"
                | "inboxes"
                | "email_templates"
                | "hooks"
                | "rules"
                | "labels"
                | "saved_views"
                | "engines"
                | "engine_fields"
                | "organization"
        )
    }

    /// Is `(kind, slug)` in this change list?
    ///
    /// `organization` is a singleton keyed by the reserved slug `"self"` and
    /// stored as an `Option` rather than a map, so it answers through this same
    /// call rather than forcing every caller to special-case it.
    pub fn contains(&self, kind: &str, slug: &str) -> bool {
        match kind {
            "workspaces" => self.workspaces.contains_key(slug),
            "queues" => self.queues.contains_key(slug),
            "schemas" => self.schemas.contains_key(slug),
            "inboxes" => self.inboxes.contains_key(slug),
            "email_templates" => self.email_templates.contains_key(slug),
            "hooks" => self.hooks.contains_key(slug),
            "rules" => self.rules.contains_key(slug),
            "labels" => self.labels.contains_key(slug),
            "saved_views" => self.saved_views.contains_key(slug),
            "engines" => self.engines.contains_key(slug),
            "engine_fields" => self.engine_fields.contains_key(slug),
            "organization" => slug == "self" && self.organization.is_some(),
            _ => false,
        }
    }
```

And inside `impl Tombstones`:

```rust
    /// Does this struct have a slot for `kind` at all?
    ///
    /// `organization` is absent on purpose: rdc cannot delete an organization.
    pub fn tracks(&self, kind: &str) -> bool {
        matches!(
            kind,
            "workspaces"
                | "queues"
                | "schemas"
                | "inboxes"
                | "email_templates"
                | "hooks"
                | "rules"
                | "labels"
                | "saved_views"
                | "engines"
                | "engine_fields"
        )
    }

    /// Is `(kind, slug)` tombstoned?
    pub fn contains(&self, kind: &str, slug: &str) -> bool {
        match kind {
            "workspaces" => self.workspaces.contains_key(slug),
            "queues" => self.queues.contains_key(slug),
            "schemas" => self.schemas.contains_key(slug),
            "inboxes" => self.inboxes.contains_key(slug),
            "email_templates" => self.email_templates.contains_key(slug),
            "hooks" => self.hooks.contains_key(slug),
            "rules" => self.rules.contains_key(slug),
            "labels" => self.labels.contains_key(slug),
            "saved_views" => self.saved_views.contains_key(slug),
            "engines" => self.engines.contains_key(slug),
            "engine_fields" => self.engine_fields.contains_key(slug),
            _ => false,
        }
    }
```

If `Paths` or `tempfile` is not already in scope in that test module, check how the
neighbouring tests import them and follow suit rather than adding new imports at
the file top.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib push::scan`
Expected: PASS. If `every_push_capable_kind_reaches_the_change_list` fails for a
specific kind, the fixture's file list for that kind is wrong — read the
corresponding arm in `change_list_from_classified` and fix the fixture, not the
assertion.

- [ ] **Step 5: Prove the guard actually guards**

Temporarily comment out the `"saved_views" => { … }` arm in
`change_list_from_classified`, re-run `cargo test --lib push::scan`, and confirm
`every_push_capable_kind_reaches_the_change_list` FAILS naming `saved_views`.
Then restore the arm byte-for-byte and re-run to confirm PASS. Record both
outputs in your report — a guard that cannot fail is worse than no guard.

- [ ] **Step 6: Commit**

```bash
git add src/cli/push/scan.rs
git commit -m "$(cat <<'MSG'
feat(kinds): enforce that every push-capable kind reaches the change list

change_list_from_classified has a `_ => {}` catch-all, so a kind with no arm is
dropped before the push phase: no error, no warning, no request. That is how the
saved-views kind shipped with its entire push half dead while every task-level
test passed -- driver tests build their BTreeMaps by hand and never exercise the
classifier.

The arms cannot collapse to a membership test (each derives a different on-disk
path), so the test iterates PUSH_CAPABLE instead, through new boolean
contains/tracks accessors. Boolean rather than exposing the maps because
ChangeList.organization is an Option singleton, not a BTreeMap.

The fixture is itself pinned against PUSH_CAPABLE, so adding a kind without a
fixture fails loudly instead of being quietly skipped -- the same silent-omission
failure this change exists to prevent.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 3: Enforcement tests for `apply_outcome` and `list_slugs`

**Files:**
- Modify: `src/cli/push/deletes.rs` (tests only — `apply_outcome` and `DeleteOutcome` are private, so the test must live in this module)
- Modify: `src/cli/deploy/selection.rs` (tests only)

**Interfaces:**
- Consumes: `crate::kinds::DELETABLE` (Task 1), `DEPLOYABLE_KINDS`.
- Produces: nothing.

- [ ] **Step 1: Write the failing tests**

Append inside the `#[cfg(test)] mod tests` in `src/cli/push/deletes.rs`:

```rust
    /// `apply_outcome` has a `_ => {}` catch-all, so a deletable kind missing an
    /// arm would delete remotely and then not be counted — the summary would
    /// under-report a destructive action.
    #[test]
    fn every_deletable_kind_is_counted_by_apply_outcome() {
        for kind in crate::kinds::DELETABLE {
            let mut counts = DeleteCounts::default();
            apply_outcome(&mut counts, kind, DeleteOutcome::Deleted);
            assert_eq!(
                counts.total_deleted(),
                1,
                "apply_outcome did not count a delete for '{kind}'",
            );
        }
    }

    /// `AlreadyGone` counts as deleted (the remote object is gone either way);
    /// `Skipped` must not.
    #[test]
    fn apply_outcome_counts_already_gone_but_not_skipped() {
        let mut counts = DeleteCounts::default();
        apply_outcome(&mut counts, "labels", DeleteOutcome::AlreadyGone);
        assert_eq!(counts.total_deleted(), 1);

        let mut counts = DeleteCounts::default();
        apply_outcome(&mut counts, "labels", DeleteOutcome::Skipped);
        assert_eq!(counts.total_deleted(), 0);
    }

    /// An organization has no delete path at all, so it must never be counted.
    #[test]
    fn apply_outcome_does_not_count_an_organization() {
        let mut counts = DeleteCounts::default();
        apply_outcome(&mut counts, "organization", DeleteOutcome::Deleted);
        assert_eq!(counts.total_deleted(), 0);
    }
```

Append inside the `mod tests` in `src/cli/deploy/selection.rs`:

```rust
    /// `list_slugs` has a catch-all, so a kind in DEPLOYABLE_KINDS with no arm
    /// makes `--only <kind>/<slug>` report "matched 0 objects" instead of
    /// erroring — which is how the saved-views kind was briefly unselectable.
    #[test]
    fn every_deployable_kind_is_listable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        for kind in DEPLOYABLE_KINDS {
            let got = list_slugs(&paths, kind);
            assert!(
                got.is_ok(),
                "list_slugs has no handling for deployable kind '{kind}': {got:?}",
            );
        }
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib push::deletes` and `cargo test --lib deploy::selection`
Expected: FAIL to compile — `crate::kinds` does not resolve if Task 1 is not in
place. If Task 1 IS in place and all these tests pass immediately, that is the
correct outcome: the spec's matrix says there are no current gaps, so these tests
lock in correct state rather than repairing it. Say so in your report and move on
— do NOT weaken an assertion to manufacture a red phase.

- [ ] **Step 3: Prove the guards actually guard**

For each of the two new enforcement tests, temporarily break the thing it guards,
confirm the test fails, then restore byte-for-byte:

1. Comment out the `"saved_views" => counts.saved_views += 1,` arm in
   `apply_outcome`; confirm `every_deletable_kind_is_counted_by_apply_outcome`
   fails naming `saved_views`; restore.
2. Remove `"saved_views"` from `list_slugs`'s match (leave it in
   `DEPLOYABLE_KINDS`); confirm `every_deployable_kind_is_listable` fails naming
   `saved_views`; restore.

Record both outputs in your report.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib push::deletes` then `cargo test --lib deploy::selection`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/push/deletes.rs src/cli/deploy/selection.rs
git commit -m "$(cat <<'MSG'
feat(kinds): enforce delete counting and --only selectability per kind

Two more catch-all dispatch sites, both silent on a miss. apply_outcome would
delete a remote object and not count it, under-reporting a destructive action in
the summary. list_slugs would make `--only <kind>/<slug>` report "matched 0
objects" rather than erroring -- which is exactly how saved_views was briefly
unselectable despite being in DEPLOYABLE_KINDS.

Both tests were verified to fail when the guarded arm is removed, so they are
guards rather than decoration.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 4: Collapse the `BothDeleted` list onto `DELETABLE`

**Files:**
- Modify: `src/cli/sync/execute.rs` (the `BothDeleted` dispatch, around line 2601)

**Interfaces:**
- Consumes: `crate::kinds::DELETABLE` (Task 1).
- Produces: nothing.

- [ ] **Step 1: Read the current site**

Run: `grep -n "All push-capable kinds use the same drop semantics" -A 22 src/cli/sync/execute.rs`

You will find a `matches!(it.kind.as_str(), "labels" | "workspaces" | … )` over
eleven literal kinds, gating `drop_lockfile_entry`, with an `else` branch that
emits a `BothDeleted handler not yet wired for kind …` warning.

- [ ] **Step 2: Replace the literal list**

Swap the `matches!(…)` condition for:

```rust
                // Silent convergence — both sides removed the object, so the
                // lockfile entry is the only thing left. Drop it.
                //
                // Membership comes from `kinds::DELETABLE` rather than a literal
                // list repeated here: this site had no `saved_views` arm for a
                // while, and because nothing else clears a `BothDeleted` item
                // (the tombstone loop filters on `LocalDelete` only) the `else`
                // branch below warned on every sync forever and left an
                // unclearable lockfile entry.
                if crate::kinds::DELETABLE.contains(&it.kind.as_str()) {
```

Leave the `else` branch exactly as it is — it is still the right behaviour for a
kind genuinely outside the set, and it is what eventually made the saved-views
gap visible.

- [ ] **Step 3: Verify the existing regression test still passes**

The saved-views branch added `both_deleted_drops_a_saved_view_lockfile_entry_without_warning`
to this file. It must still pass — it is now the behavioural proof that the const
swap changed nothing.

Run: `cargo test --lib sync::execute`
Expected: PASS, including that test.

- [ ] **Step 4: Prove the guard still guards**

Temporarily remove `"saved_views"` from `kinds::DELETABLE`, re-run
`cargo test --lib sync::execute`, and confirm
`both_deleted_drops_a_saved_view_lockfile_entry_without_warning` FAILS (the item
now takes the `else` branch and emits a warning). Restore `DELETABLE`
byte-for-byte and re-run to confirm PASS. Note in your report that this also
demonstrates the const is genuinely load-bearing at this site.

Also re-run `cargo test --lib kinds` after restoring, since you edited that file.

- [ ] **Step 5: Commit**

```bash
git add src/cli/sync/execute.rs
git commit -m "$(cat <<'MSG'
refactor(kinds): drive the BothDeleted dispatch from kinds::DELETABLE

One of the two duplicated literal kind lists that caused this branch's bugs, now
gone. The else branch stays: it is correct for a kind genuinely outside the set,
and it is what eventually made the saved-views gap visible.

Verified load-bearing by removing saved_views from DELETABLE and watching the
existing BothDeleted regression test fail.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 5: `detect_flat_kind` — two passes so duplicate names get suffixes

**Files:**
- Modify: `src/cli/deploy/realign.rs` (`detect_flat_kind`, around line 218, plus tests)

**Interfaces:**
- Consumes: `crate::slug::{slugify, slugify_unique}`.
- Produces: no signature change — `detect_flat_kind` keeps its parameters.

**The bug:** the guard is `proposed != *slug && !by_slug.contains_key(&proposed)`.
`by_slug` is the lockfile map, so it blocks a collision with an *existing* slug
but not two *pending* renames proposing the same new one. Lockfile `old-name` +
`old-name-2`, both files renamed to "New name" in the UI, both propose
`new-name`: the first `move_file` succeeds, the second bails with "destination …
already exists". No data loss, and a second `doctor` run self-heals, but the user
gets a half-applied rename and an error.

**Why two passes:** seeding `slugify_unique` with every lockfile slug in one pass
would see a *stable* object's own slug in the used-set and propose `-2` for it,
inventing a rename where none is due.

- [ ] **Step 1: Write the failing tests**

Append inside the `mod tests` in `src/cli/deploy/realign.rs`. Read a neighbouring
test first to copy how it builds a `Lockfile` and writes the on-disk files, then
follow that shape:

```rust
    /// An object whose name already matches its slug must never be proposed for
    /// a rename. This is what the two-pass structure protects: a single pass
    /// seeded with every lockfile slug would find this object's own slug in the
    /// used-set and propose `-2` for it.
    #[test]
    fn a_stable_object_is_not_renamed() {
        let (tmp, paths) = flat_fixture(&[("urgent", "Urgent")]);
        let lockfile = flat_lockfile("labels", &["urgent"]);
        let mut out = Vec::new();
        detect_flat_kind(&lockfile, "labels", paths.labels_dir(), &mut out, |o, n| {
            PendingRename::Label { old: o, new: n }
        });
        assert!(out.is_empty(), "expected no renames, got {out:?}");
        drop(tmp);
    }

    /// Two objects whose names slugify to the same slug both rename, suffixed —
    /// matching what a fresh `pull` produces for duplicate names.
    #[test]
    fn two_duplicate_names_both_rename_with_a_suffix() {
        let (tmp, paths) = flat_fixture(&[("old-a", "New name"), ("old-b", "New name")]);
        let lockfile = flat_lockfile("labels", &["old-a", "old-b"]);
        let mut out = Vec::new();
        detect_flat_kind(&lockfile, "labels", paths.labels_dir(), &mut out, |o, n| {
            PendingRename::Label { old: o, new: n }
        });

        let mut pairs: Vec<(String, String)> = out
            .iter()
            .map(|r| match r {
                PendingRename::Label { old, new } => (old.clone(), new.clone()),
                other => panic!("unexpected variant {other:?}"),
            })
            .collect();
        pairs.sort();
        // BTreeMap order decides who keeps the bare slug: `old-a` sorts first.
        assert_eq!(
            pairs,
            vec![
                ("old-a".to_string(), "new-name".to_string()),
                ("old-b".to_string(), "new-name-2".to_string()),
            ]
        );
        drop(tmp);
    }

    /// The reservation accumulates, so a third duplicate gets `-3`.
    #[test]
    fn three_duplicate_names_get_incrementing_suffixes() {
        let (tmp, paths) =
            flat_fixture(&[("old-a", "New name"), ("old-b", "New name"), ("old-c", "New name")]);
        let lockfile = flat_lockfile("labels", &["old-a", "old-b", "old-c"]);
        let mut out = Vec::new();
        detect_flat_kind(&lockfile, "labels", paths.labels_dir(), &mut out, |o, n| {
            PendingRename::Label { old: o, new: n }
        });
        let mut news: Vec<String> = out
            .iter()
            .map(|r| match r {
                PendingRename::Label { new, .. } => new.clone(),
                other => panic!("unexpected variant {other:?}"),
            })
            .collect();
        news.sort();
        assert_eq!(news, vec!["new-name", "new-name-2", "new-name-3"]);
        drop(tmp);
    }

    /// A proposal must never collide with an EXISTING slug that is staying put —
    /// the behaviour the original `!by_slug.contains_key` guard provided.
    #[test]
    fn a_proposal_does_not_steal_a_stable_objects_slug() {
        // `keep` is already named "Keep" so it stays; `mover` is renamed to
        // "Keep" in the UI and must not propose `keep`.
        let (tmp, paths) = flat_fixture(&[("keep", "Keep"), ("mover", "Keep")]);
        let lockfile = flat_lockfile("labels", &["keep", "mover"]);
        let mut out = Vec::new();
        detect_flat_kind(&lockfile, "labels", paths.labels_dir(), &mut out, |o, n| {
            PendingRename::Label { old: o, new: n }
        });
        for r in &out {
            if let PendingRename::Label { old, new } = r {
                assert_ne!(new, "keep", "'{old}' tried to steal a stable slug");
            }
        }
        drop(tmp);
    }
```

You must also write the two fixture helpers. Model `flat_lockfile` on however the
neighbouring tests construct a `Lockfile` with `upsert`:

```rust
    /// A temp env whose `labels/` dir holds one `<slug>.json` per entry, each
    /// carrying the given `name`.
    fn flat_fixture(entries: &[(&str, &str)]) -> (tempfile::TempDir, crate::paths::Paths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.labels_dir()).unwrap();
        for (slug, name) in entries {
            std::fs::write(
                paths.labels_dir().join(format!("{slug}.json")),
                serde_json::to_vec(&serde_json::json!({ "name": name })).unwrap(),
            )
            .unwrap();
        }
        (tmp, paths)
    }

    /// A lockfile with one entry per slug for `kind`.
    fn flat_lockfile(kind: &str, slugs: &[&str]) -> Lockfile {
        let mut lf = Lockfile::default();
        for (i, slug) in slugs.iter().enumerate() {
            lf.upsert(
                kind,
                slug,
                crate::state::ObjectEntry {
                    id: (i as u64) + 1,
                    modified_at: None,
                    modified_by: None,
                    content_hash: None,
                    secrets_hash: None,
                },
            );
        }
        lf
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib realign`
Expected: `two_duplicate_names_both_rename_with_a_suffix` and
`three_duplicate_names_get_incrementing_suffixes` FAIL — the current code emits
only ONE rename for the duplicates (the second is blocked by neither guard and
both propose the same bare slug, or one is dropped). `a_stable_object_is_not_renamed`
and `a_proposal_does_not_steal_a_stable_objects_slug` should already PASS, which
is what makes them regression guards for the rewrite.

- [ ] **Step 3: Rewrite `detect_flat_kind` as two passes**

Replace the body:

```rust
fn detect_flat_kind(
    lockfile: &Lockfile,
    kind: &str,
    dir: std::path::PathBuf,
    out: &mut Vec<PendingRename>,
    make: impl Fn(String, String) -> PendingRename,
) {
    let Some(by_slug) = lockfile.objects.get(kind) else {
        return;
    };

    // Two passes, and the split is load-bearing.
    //
    // Pass 1 reserves the slug of every object whose name ALREADY matches it.
    // Doing this in one pass — seeding the used-set with every lockfile slug up
    // front — would find a stable object's own slug in the set and propose `-2`
    // for it, inventing a rename where none is due.
    //
    // Pass 2 then assigns each remaining object the first free slug for its
    // name. Two objects whose names slugify alike therefore become `x` and
    // `x-2` rather than both proposing `x` — which used to leave the first
    // `move_file` succeeding and the second failing with "destination already
    // exists", i.e. a half-applied rename plus an error.
    //
    // `by_slug` is a BTreeMap, so iteration is slug-sorted and the
    // lowest-sorting duplicate keeps the bare slug. Deterministic across runs,
    // and convergent: a second pass over the renamed tree finds every object
    // stable.
    let mut names: Vec<(&String, Option<String>)> = Vec::new();
    let mut reserved: std::collections::HashSet<String> = std::collections::HashSet::new();

    for slug in by_slug.keys() {
        let name = read_name(&dir.join(format!("{slug}.json")));
        if let Some(name) = &name
            && crate::slug::slugify(name) == **slug
        {
            // Stable: keep this slug and take it out of circulation.
            reserved.insert((*slug).clone());
            names.push((slug, None));
            continue;
        }
        if name.is_none() {
            // Unreadable or missing file: nothing to propose, but the slug is
            // still taken.
            reserved.insert((*slug).clone());
        }
        names.push((slug, name));
    }

    for (slug, name) in names {
        let Some(name) = name else { continue };
        let proposed = crate::slug::slugify_unique(&name, &reserved);
        reserved.insert(proposed.clone());
        if proposed != *slug {
            out.push(make(slug.clone(), proposed));
        }
    }
}
```

Note the `slugify` import: the file may already import it, or use it as
`crate::slug::slugify`. Check and match the file's existing convention. If the
`let … && …` let-chain syntax is not used elsewhere in this file, write it as a
nested `if` instead — match local style.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib realign`
Expected: PASS, all four new tests plus every pre-existing realign test. The
pre-existing ones matter most here: this rewrite touches the shared helper for
`hooks`, `rules`, `labels` and `saved_views`.

- [ ] **Step 5: Commit**

```bash
git add src/cli/deploy/realign.rs
git commit -m "$(cat <<'MSG'
fix(doctor): suffix duplicate rename proposals instead of half-applying

detect_flat_kind's guard blocked a collision with an EXISTING slug but not two
PENDING renames proposing the same new one. Two objects both renamed to the same
name in the UI therefore both proposed the same slug: the first move_file
succeeded, the second bailed with "destination already exists", leaving a
half-applied rename and an error. A second doctor run self-healed, but the user
saw a failure.

Now two passes. Pass 1 reserves the slug of every object whose name already
matches it; pass 2 assigns the rest the first free slug. Duplicates become x and
x-2, matching what a fresh pull produces for duplicate names.

The split is load-bearing: a single pass seeded with every lockfile slug would
find a stable object's own slug in the used-set and propose -2 for it, inventing
renames. A test pins that.

Surfaces most on saved_views, whose spec records duplicate names as routine --
per-user namespaces, and the API does not enforce uniqueness -- but the helper is
shared with hooks, rules and labels, so all four benefit.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 6: `migrate --dry-run` — one validation path over a projected target set

**Files:**
- Modify: `src/cli/migrate/mod.rs` (`transform_file` signature + body; `run_at`'s loop, the `--mirror` prune block, and the saved-view validation block; plus tests)

**Interfaces:**
- Consumes: `check_saved_view_refs`, `format_saved_view_ref_error`, `classify`, `enumerate_files`, `mirror_prune_paths` — all already in this module.
- Produces: `transform_file` gains a final parameter `promoted_views: &mut Vec<(String, serde_json::Value)>`. There is exactly ONE production call site (`run_at`'s file loop); the other 15 are tests that already pass `&mut Vec::new()` three times and take a fourth mechanically.

**The problem:** the validation reads each promoted view back off disk after the
loop, so `--dry-run` (which writes nothing) prints an honest "not ref-validated"
line instead. Dry-run is the main thing people run before a promote, and this repo
has already had to fix dry-run blindness once.

Naively validating the on-disk file under dry-run would be worse than silence: the
target path holds either nothing or the target's OWN pre-run content, and `known`
enumerated from disk would miss every object this run would create — producing
false "unresolvable" errors.

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` in `src/cli/migrate/mod.rs`:

```rust
    /// Unit test of the projection itself: writes are added, prunes subtracted.
    #[test]
    fn projected_known_adds_writes_and_subtracts_prunes() {
        let existing = vec![PathBuf::from("labels/kept.json")];
        let would_write = vec![PathBuf::from("workspaces/main/queues/invoices/queue.json")];
        let pruned = vec![PathBuf::from("labels/kept.json")];

        let got = projected_known(&existing, &would_write, &pruned);
        assert!(
            got.contains(&("queues".to_string(), "invoices".to_string())),
            "a queue this run would write must be known: {got:?}",
        );
        assert!(
            !got.contains(&("labels".to_string(), "kept".to_string())),
            "a pruned object must not be known: {got:?}",
        );
    }

    /// `--dry-run` must forecast the refusal rather than printing an info line.
    /// Fails before this change: dry-run skipped validation and returned Ok.
    #[test]
    fn dry_run_refuses_a_saved_view_ref_missing_from_the_target() {
        let project = saved_view_project();
        let root = project.path();
        write_source_queue(root);
        write_snapshot_json(
            &root.join("envs/dev/saved-views/scoped.json"),
            &serde_json::json!({
                "name": "Scoped",
                "shared": true,
                "queues_filter": ["rdc://queues/not-in-target"],
                "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
                "organization": "https://dev.example/api/v1/organizations/1"
            }),
        );

        // 5th positional arg is `dry_run`.
        let err = run_at(root, "dev", "prod", false, true, vec![], false, false)
            .expect_err("--dry-run must forecast the refusal");
        let msg = format!("{err:#}");
        for want in ["saved-views/scoped", "queues_filter[0]", "not-in-target"] {
            assert!(msg.contains(want), "error must mention {want}, got: {msg}");
        }
        assert!(
            !root.join("envs/prod/saved-views/scoped.json").exists(),
            "--dry-run must still write nothing",
        );
    }

    /// The other direction, and the reason a disk-only `known` is wrong: the
    /// target starts EMPTY, so the queue this view points at exists only in the
    /// source. A dry run must NOT call that unresolvable.
    #[test]
    fn dry_run_accepts_a_ref_to_a_queue_this_run_would_create() {
        let project = saved_view_project();
        let root = project.path();
        write_source_queue(root); // creates queue `invoices` in the SOURCE only
        write_snapshot_json(
            &root.join("envs/dev/saved-views/scoped.json"),
            &serde_json::json!({
                "name": "Scoped",
                "shared": true,
                "queues_filter": ["rdc://queues/invoices"],
                "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
                "organization": "https://dev.example/api/v1/organizations/1"
            }),
        );

        run_at(root, "dev", "prod", false, true, vec![], false, false)
            .expect("a ref to a queue this run would create must not be refused");
    }

    /// The equivalence pin. Collapsing the two modes onto one path is only safe
    /// if they agree, so assert they reach the SAME verdict over the same
    /// fixture — both Ok when refs resolve, both Err naming the same ref when
    /// they do not.
    #[test]
    fn dry_run_and_real_run_reach_the_same_saved_view_verdict() {
        // Resolvable: both modes accept.
        for dry in [true, false] {
            let project = saved_view_project();
            let root = project.path();
            write_source_queue(root);
            write_snapshot_json(
                &root.join("envs/dev/saved-views/ok.json"),
                &serde_json::json!({
                    "name": "Ok",
                    "shared": true,
                    "queues_filter": ["rdc://queues/invoices"],
                    "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
                    "organization": "https://dev.example/api/v1/organizations/1"
                }),
            );
            run_at(root, "dev", "prod", false, dry, vec![], false, false)
                .unwrap_or_else(|e| panic!("dry_run={dry} must accept, got: {e:#}"));
        }

        // Unresolvable: both modes refuse, naming the same ref.
        for dry in [true, false] {
            let project = saved_view_project();
            let root = project.path();
            write_source_queue(root);
            write_snapshot_json(
                &root.join("envs/dev/saved-views/bad.json"),
                &serde_json::json!({
                    "name": "Bad",
                    "shared": true,
                    "queues_filter": ["rdc://queues/not-in-target"],
                    "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
                    "organization": "https://dev.example/api/v1/organizations/1"
                }),
            );
            let err = run_at(root, "dev", "prod", false, dry, vec![], false, false)
                .unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("not-in-target"),
                "dry_run={dry} must name the offending ref, got: {msg}",
            );
        }
    }
```

`PathBuf` must be in scope in that test module — check the neighbouring tests and
follow whatever they do rather than adding a top-of-file import.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib migrate`
Expected: FAIL — `dry_run_refuses_…` currently gets `Ok` (dry-run only logs an
info line), and `projected_known_…` refers to a helper that does not exist yet.

- [ ] **Step 3: Extract the projection helper**

Add near `check_saved_view_refs`:

```rust
/// The `(kind, slug)` set the target tree WILL contain once this run finishes:
/// what is there now, plus what the run writes, minus what `--mirror` prunes.
///
/// Used by both modes. For a real run this is equivalent to enumerating the
/// target tree after the writes — `existing` already covers the target-only
/// objects an overlay may legitimately point at, and `would_write` covers what
/// this run creates. For `--dry-run` it is the only correct answer: nothing has
/// been written, so a disk-only read would miss every object the run would
/// create and call each of its refs unresolvable.
fn projected_known(
    existing: &[PathBuf],
    would_write: &[PathBuf],
    pruned: &[PathBuf],
) -> BTreeSet<(String, String)> {
    let mut out: BTreeSet<(String, String)> = existing
        .iter()
        .chain(would_write.iter())
        .filter_map(|rel| classify(rel).map(|(k, s)| (k.to_string(), s)))
        .collect();
    for rel in pruned {
        if let Some((k, s)) = classify(rel) {
            out.remove(&(k.to_string(), s));
        }
    }
    out
}
```

- [ ] **Step 4: Surface the transformed body from `transform_file`**

Add a final parameter to `transform_file`:

```rust
    promoted_views: &mut Vec<(String, serde_json::Value)>,
```

Inside it, immediately before the transformed `Value` is serialized to the bytes
handed to `settle`, push the post-overlay body when the destination classifies as
a saved view:

```rust
    // Collect the POST-OVERLAY body so both modes validate the same value the
    // real run would write. Collecting here rather than reading the file back
    // afterwards is what lets `--dry-run` validate at all — and collecting
    // after the overlay is what keeps the documented `overlay.toml` escape
    // hatch working.
    if let Some(("saved_views", slug)) = classify(&dst_rel) {
        promoted_views.push((slug, value.clone()));
    }
```

Use whatever the local variable holding the finished `serde_json::Value` is
actually called at that point — read the function and match it; do not assume
`value`.

Update all 16 call sites. The single production one is in `run_at`'s file loop
(search for `let outcome = transform_file(`); pass `&mut promoted_saved_views`.
The 15 test call sites take `&mut Vec::new()`.

- [ ] **Step 5: Rewire `run_at`**

Three changes:

1. Change the accumulator's type and drop the caller-side push:

```rust
    let mut promoted_saved_views: Vec<(String, serde_json::Value)> = Vec::new();
```

and DELETE the block in the loop that reads:

```rust
        if let Some(("saved_views", slug)) = classify(&dst_rel) {
            promoted_saved_views.push((slug, dst_rel.clone()));
        }
```

2. Collect every destination the run would write. Declare beside the other
accumulators:

```rust
    // Every path this run writes, needed by `projected_known`. Collected for
    // EVERY file, not only changed ones: an unchanged target file is still part
    // of what the target tree contains.
    let mut would_write: Vec<PathBuf> = Vec::new();
```

and push inside the loop, right after `dst_rel` is computed:

```rust
        would_write.push(dst_rel.clone());
```

3. Hoist the prune list out of the `if mirror { … }` block so the validation can
subtract it. Declare before that block:

```rust
    let mut pruned_rels: Vec<PathBuf> = Vec::new();
```

and inside, after `let prune = mirror_prune_paths(…)?;`, add:

```rust
        pruned_rels = prune.clone();
```

- [ ] **Step 6: Collapse the validation block onto one path**

Replace the whole `if !promoted_saved_views.is_empty() { if dry_run { … } else { … } }`
block with a single path:

```rust
    if !promoted_saved_views.is_empty() {
        let existing = enumerate_files(&tgt_root, tgt)?;
        let known = projected_known(&existing, &would_write, &pruned_rels);
        let mut problems: Vec<SavedViewRefProblem> = Vec::new();
        for (slug, value) in &promoted_saved_views {
            problems.extend(check_saved_view_refs(
                slug,
                value,
                &known,
                src_host.as_deref(),
            ));
        }
        if !problems.is_empty() {
            anyhow::bail!(format_saved_view_ref_error(&problems, tgt));
        }
    }
```

Match `check_saved_view_refs`'s real parameter list — it gained a `src_host`
argument in a later fix, so read the signature and pass what it actually takes,
in the order it takes it. The old block's `src_host` binding is already in scope
at this point; reuse it rather than recomputing.

The `dry_run` info line about not validating is deleted along with the branch.

- [ ] **Step 7: Run the tests**

Run: `cargo test --lib migrate`
Expected: PASS, including the four new tests and every pre-existing migrate test.

- [ ] **Step 7b: Prove the projection is load-bearing**

`dry_run_accepts_a_ref_to_a_queue_this_run_would_create` passes VACUOUSLY before
this change (dry-run skipped validation entirely and returned `Ok`), so it only
becomes a guard once the fix is in. Prove it:

Temporarily make `projected_known` ignore its `would_write` argument, re-run
`cargo test --lib migrate`, and confirm that test FAILS — a queue the run would
create now reads as unresolvable. Restore `projected_known` byte-for-byte and
re-run to confirm PASS. Record both outputs in your report.

Run: `cargo test --test cli_migrate`
Expected: PASS. This suite drives the real binary, so it is where a broken
projection or a mis-threaded parameter shows up.

- [ ] **Step 8: Check for tests asserting exact dry-run output**

Dry-run now emits a refusal where it used to print an info line, so any test
asserting the old text will fail.

Run: `grep -rn "not ref-validated" src/ tests/`
Expected: no hits after your change. If any remain, they are stale assertions —
update them to expect the refusal.

- [ ] **Step 9: Commit**

```bash
git add src/cli/migrate/mod.rs
git commit -m "$(cat <<'MSG'
fix(migrate): ref-validate saved views under --dry-run too

The check read each promoted view back off disk after the loop, so --dry-run,
which writes nothing, printed an honest "not ref-validated" line instead. Dry-run
is the main thing people run before a promote, and this repo has already had to
fix dry-run blindness once for MDH.

Naively validating the on-disk file under dry-run would have been worse than
silence: the target path holds either nothing or the target's own pre-run
content, and a `known` set enumerated from disk misses every object the run would
create, so each of their refs would read as unresolvable.

So both modes now share ONE path over a projected target set -- what is there
now, plus what the run writes, minus what --mirror prunes -- and transform_file
surfaces the post-overlay body so there is nothing to read back. Collecting after
the overlay is what keeps the documented escape hatch working.

Two implementations of one check is the bug class this whole branch is about, so
the equivalence is pinned by a test asserting the projection matches a real run's
post-write enumeration rather than assumed. It also drops a second walk of the
target tree.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 7: Full suite, clippy, and the spec's failure-mode table

**Files:**
- None expected. Fix whatever the gates surface.

**Interfaces:**
- Consumes: everything.
- Produces: nothing.

- [ ] **Step 1: Run the whole suite**

Run: `cargo test`
Expected: PASS. The baseline before this plan was 1515 passed / 0 failed / 13
ignored; expect roughly 1530+ now. Investigate every failure and report exactly
what it was — a `ChangeList`/`Tombstones` literal in an old test needing the new
accessors is the likely shape, as is a stale dry-run assertion.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean. The weekly release workflow gates on this, so a warning here
blocks every future release.

Do **not** run `cargo fmt` — `cargo fmt --check` failing is a known pre-existing
condition in this repo.

- [ ] **Step 3: Walk the spec's failure-mode table**

Open the spec's `## Failure modes` section and confirm each row is now true of
the code. For each, name the test that makes it true. If a row has no test,
report it — do not add a test without saying so.

- [ ] **Step 4: Commit any fixes**

Only if Steps 1-2 required changes:

```bash
git add -- <the files you actually fixed>
git commit -m "$(cat <<'MSG'
fix: <what the gates surfaced>

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

## Done when

- `cargo test` and `cargo clippy --all-targets -- -D warnings` are both clean.
- Removing any one kind from `kinds::PUSH_CAPABLE` or `kinds::DELETABLE`, or
  deleting any one guarded dispatch arm, turns a test red — verified by hand in
  Tasks 2, 3 and 4 and recorded in those reports.
- `rdc doctor` on two objects whose names collide proposes two suffixed renames
  instead of half-applying one and erroring.
- `rdc migrate --dry-run` reports the same saved-view refusal a real run does,
  and does NOT report a false one for a queue the run would create.
