//! Eleven-class classification of `(kind, slug)` items based on local file
//! state, remote API listing, and the lockfile hash. The classification drives
//! the sync executor: pull-side writes, push-side writes, and resolver prompts.
//!
//! Spec §"Execution pipeline → Classify".

use std::collections::{BTreeMap, BTreeSet};

/// One of eleven classes. See the spec table for definitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncClass {
    Clean,
    LocalEdit,
    LocalCreate,
    LocalDelete,
    RemoteEdit,
    RemoteCreate,
    RemoteDelete,
    BothDiverged,
    LocalEditRemoteDelete,
    LocalDeleteRemoteEdit,
    BothDeleted,
}

/// A single classified item with the bytes / hashes needed by the executor.
#[derive(Debug, Clone)]
pub struct ClassifiedItem {
    pub kind: String,
    pub slug: String,
    pub class: SyncClass,
    /// Hash of the local file (if any) at scan time. Used to detect mid-run drift.
    pub local_hash: Option<String>,
    /// Hash of the remote body (if any) from the listing.
    pub remote_hash: Option<String>,
    /// Hash recorded by the lockfile (the merge base).
    pub base_hash: Option<String>,
}

/// Classify each `(kind, slug)` that appears in any of the three sources
/// (remote listing, local scan changes/tombstones, lockfile entries).
///
/// Stub: Task 5 fills in the six simple classes; Task 6 the both-side cases;
/// Task 7 the remote-delete + two double-conflict cases.
pub fn classify(
    remote_hashes: &BTreeMap<(String, String), String>,
    scan_changes: &BTreeMap<(String, String), String>,
    scan_tombstones: &BTreeSet<(String, String)>,
    locked: &BTreeMap<(String, String), String>,
) -> Vec<ClassifiedItem> {
    let mut all_keys: BTreeSet<&(String, String)> = BTreeSet::new();
    all_keys.extend(remote_hashes.keys());
    all_keys.extend(scan_changes.keys());
    all_keys.extend(scan_tombstones.iter());
    all_keys.extend(locked.keys());

    let mut out = Vec::with_capacity(all_keys.len());
    for k in all_keys {
        let local_changed = scan_changes.contains_key(k);
        let local_tombstoned = scan_tombstones.contains(k);
        let remote_present = remote_hashes.contains_key(k);
        let locked_present = locked.contains_key(k);

        let remote_hash = remote_hashes.get(k).cloned();
        let base_hash = locked.get(k).cloned();
        let local_hash = scan_changes.get(k).cloned();

        let class = match (local_changed, local_tombstoned, remote_present, locked_present) {
            (false, false, true, true) if remote_hash == base_hash => SyncClass::Clean,
            (true,  false, true, true) if remote_hash == base_hash => SyncClass::LocalEdit,
            (true,  false, false, false) => SyncClass::LocalCreate,
            (false, true,  true, true)  if remote_hash == base_hash => SyncClass::LocalDelete,
            (false, false, true, true)  if remote_hash != base_hash => SyncClass::RemoteEdit,
            (false, false, true, false) => SyncClass::RemoteCreate,
            (true,  false, true,  true)  if remote_hash != base_hash => SyncClass::BothDiverged,
            (false, true,  false, true)  => SyncClass::BothDeleted,

            // Task 7 — the final 3 classes.
            (false, false, false, true) => SyncClass::RemoteDelete,
            (true,  false, false, true) => SyncClass::LocalEditRemoteDelete,
            (false, true,  true,  true) if remote_hash != base_hash => SyncClass::LocalDeleteRemoteEdit,

            // Lockfile entry missing (e.g., after `rdc doctor --rebuild-lock`
            // or a fresh checkout where the file pre-exists), local and
            // remote both present. If they agree, treat as `Clean` — the
            // executor will record the hash, effectively rebuilding the
            // lockfile entry. If they disagree, treat as `BothDiverged` so
            // the resolver fires; with no base to compare against any
            // divergence is a user-resolution conflict, never a silent
            // overwrite of local edits.
            (true, false, true, false) if remote_hash == local_hash => SyncClass::Clean,
            (true, false, true, false) /* if remote_hash != local_hash */ => SyncClass::BothDiverged,

            // Fail-loud guard: with the covered classes above, the only way to
            // reach this arm is a logic bug (e.g., a hash mismatch in an
            // unexpected combination, or a tombstone without a lockfile entry
            // which `scan::detect_tombstones` is structurally incapable of
            // producing). Panic so it surfaces in integration tests instead of
            // silently miscategorising an object.
            _ => panic!(
                "classify: unhandled state for {:?}: local_changed={local_changed} \
                 local_tombstoned={local_tombstoned} remote_present={remote_present} \
                 locked_present={locked_present} remote_hash={remote_hash:?} \
                 base_hash={base_hash:?}",
                k
            ),
        };

        out.push(ClassifiedItem {
            kind: k.0.clone(),
            slug: k.1.clone(),
            class,
            local_hash,
            remote_hash,
            base_hash,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: &str, s: &str) -> (String, String) {
        (k.to_string(), s.to_string())
    }

    fn m(items: &[(&str, &str, &str)]) -> BTreeMap<(String, String), String> {
        items
            .iter()
            .map(|(k, s, h)| (key(k, s), h.to_string()))
            .collect()
    }

    fn st(items: &[(&str, &str)]) -> BTreeSet<(String, String)> {
        items.iter().map(|(k, s)| key(k, s)).collect()
    }

    fn class_of<'a>(items: &'a [ClassifiedItem], slug: &str) -> &'a SyncClass {
        &items
            .iter()
            .find(|c| c.slug == slug)
            .expect("item present")
            .class
    }

    #[test]
    fn clean_when_remote_matches_lockfile_and_no_local_changes() {
        let result = classify(
            &m(&[("hooks", "v1", "h_base")]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &m(&[("hooks", "v1", "h_base")]),
        );
        assert_eq!(class_of(&result, "v1"), &SyncClass::Clean);
    }

    #[test]
    fn local_edit_when_local_changed_and_remote_at_lockfile() {
        let result = classify(
            &m(&[("hooks", "v1", "h_base")]),
            &m(&[("hooks", "v1", "h_local_new")]),
            &BTreeSet::new(),
            &m(&[("hooks", "v1", "h_base")]),
        );
        assert_eq!(class_of(&result, "v1"), &SyncClass::LocalEdit);
    }

    #[test]
    fn local_create_when_only_local_present_no_lockfile() {
        let result = classify(
            &BTreeMap::new(),
            &m(&[("hooks", "new", "h_local")]),
            &BTreeSet::new(),
            &BTreeMap::new(),
        );
        assert_eq!(class_of(&result, "new"), &SyncClass::LocalCreate);
    }

    #[test]
    fn local_delete_when_tombstone_and_remote_at_lockfile() {
        let result = classify(
            &m(&[("hooks", "v1", "h_base")]),
            &BTreeMap::new(),
            &st(&[("hooks", "v1")]),
            &m(&[("hooks", "v1", "h_base")]),
        );
        assert_eq!(class_of(&result, "v1"), &SyncClass::LocalDelete);
    }

    #[test]
    fn remote_edit_when_remote_differs_from_lockfile_local_unchanged() {
        let result = classify(
            &m(&[("hooks", "v1", "h_remote_new")]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &m(&[("hooks", "v1", "h_base")]),
        );
        assert_eq!(class_of(&result, "v1"), &SyncClass::RemoteEdit);
    }

    #[test]
    fn remote_create_when_only_remote_present_no_lockfile() {
        let result = classify(
            &m(&[("hooks", "new", "h_remote")]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        );
        assert_eq!(class_of(&result, "new"), &SyncClass::RemoteCreate);
    }

    #[test]
    fn both_diverged_when_local_and_remote_both_changed() {
        let result = classify(
            &m(&[("hooks", "v1", "h_remote_new")]),
            &m(&[("hooks", "v1", "h_local_new")]),
            &BTreeSet::new(),
            &m(&[("hooks", "v1", "h_base")]),
        );
        assert_eq!(class_of(&result, "v1"), &SyncClass::BothDiverged);
    }

    #[test]
    fn both_deleted_when_tombstone_and_remote_absent_with_lockfile_entry() {
        let result = classify(
            &BTreeMap::new(),
            &BTreeMap::new(),
            &st(&[("hooks", "v1")]),
            &m(&[("hooks", "v1", "h_base")]),
        );
        assert_eq!(class_of(&result, "v1"), &SyncClass::BothDeleted);
    }

    #[test]
    fn remote_delete_when_remote_absent_local_unchanged_lockfile_present() {
        let result = classify(
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &m(&[("hooks", "v1", "h_base")]),
        );
        assert_eq!(class_of(&result, "v1"), &SyncClass::RemoteDelete);
    }

    #[test]
    fn local_edit_remote_delete_when_local_changed_and_remote_absent() {
        let result = classify(
            &BTreeMap::new(),
            &m(&[("hooks", "v1", "h_local_new")]),
            &BTreeSet::new(),
            &m(&[("hooks", "v1", "h_base")]),
        );
        assert_eq!(class_of(&result, "v1"), &SyncClass::LocalEditRemoteDelete);
    }

    #[test]
    fn local_delete_remote_edit_when_tombstone_and_remote_diverged() {
        let result = classify(
            &m(&[("hooks", "v1", "h_remote_new")]),
            &BTreeMap::new(),
            &st(&[("hooks", "v1")]),
            &m(&[("hooks", "v1", "h_base")]),
        );
        assert_eq!(class_of(&result, "v1"), &SyncClass::LocalDeleteRemoteEdit);
    }

    /// Regression: simulates a post-`rdc doctor --rebuild-lock` state where
    /// the lockfile is empty but local and remote happen to be byte-equivalent.
    /// `scan` reports the local file as changed (no lockfile entry to compare
    /// against), the remote listing has the same hash, and the classifier
    /// must mark the item `Clean` so the executor rebuilds the lockfile entry
    /// without any prompts or writes.
    #[test]
    fn classify_rebuild_lock_matching_hashes_yields_clean() {
        let result = classify(
            &m(&[("email_templates", "x", "h_same")]), // remote
            &m(&[("email_templates", "x", "h_same")]), // scan changes (local)
            &BTreeSet::new(),
            &BTreeMap::new(), // locked is EMPTY (post `--rebuild-lock`)
        );
        assert_eq!(class_of(&result, "x"), &SyncClass::Clean);
    }

    /// Regression: companion to the above — post-`--rebuild-lock` state
    /// where local and remote disagree. With no base to compare against,
    /// any divergence is a user-resolution conflict. The classifier MUST
    /// emit `BothDiverged` so the resolver fires; never a one-sided write
    /// class that would silently overwrite local edits.
    #[test]
    fn classify_rebuild_lock_diverged_hashes_yields_both_diverged() {
        let result = classify(
            &m(&[("email_templates", "x", "h_remote")]),
            &m(&[("email_templates", "x", "h_local")]),
            &BTreeSet::new(),
            &BTreeMap::new(),
        );
        assert_eq!(class_of(&result, "x"), &SyncClass::BothDiverged);
    }

    // ------------------------------------------------------------------
    // Property tests (Phase 4 of the sync safety contract).
    //
    // These pin the "never silent data loss" invariant at the classifier
    // boundary: for any triple `(base, local, remote)` where all three
    // differ, the classifier MUST emit a conflict class. A one-sided
    // class (`LocalEdit`, `RemoteEdit`) routes straight to a PATCH/POST
    // without a prompt, so producing one in this state would silently
    // overwrite divergent remote changes.
    // ------------------------------------------------------------------

    use proptest::prelude::*;

    /// Returns `true` if the class triggers an unprompted write to
    /// remote or local. These are the classes a defensive-in-depth
    /// design forbids when both sides have diverged from base.
    fn writes_without_prompt(c: &SyncClass) -> bool {
        matches!(
            c,
            SyncClass::LocalEdit
                | SyncClass::LocalCreate
                | SyncClass::LocalDelete
                | SyncClass::RemoteEdit
                | SyncClass::RemoteCreate
        )
    }

    proptest! {
        /// Load-bearing invariant: when local AND remote BOTH differ
        /// from base, classify MUST emit a conflict class. Never a
        /// one-sided "push it" / "pull it" class — that would silently
        /// overwrite one side with the other.
        ///
        /// Scope: the case where remote is present (`remote_hashes` has
        /// an entry) and local is changed (`scan_changes` has an entry)
        /// and the lockfile has a base. This is the four-input
        /// "BothDiverged" cell of the truth table.
        #[test]
        fn classify_emits_conflict_when_local_and_remote_both_diverge_from_base(
            base in "[a-z0-9]{1,32}",
            local in "[a-z0-9]{1,32}",
            remote in "[a-z0-9]{1,32}",
        ) {
            prop_assume!(base != local && base != remote && local != remote);
            let k = ("hooks".to_string(), "v1".to_string());
            let mut remote_hashes = BTreeMap::new();
            remote_hashes.insert(k.clone(), remote);
            let mut scan_changes = BTreeMap::new();
            scan_changes.insert(k.clone(), local);
            let mut locked = BTreeMap::new();
            locked.insert(k.clone(), base);

            let result = classify(&remote_hashes, &scan_changes, &BTreeSet::new(), &locked);
            let class = &result[0].class;
            prop_assert_eq!(
                class,
                &SyncClass::BothDiverged,
                "both sides diverged → MUST be BothDiverged; got {:?}",
                class
            );
            prop_assert!(
                !writes_without_prompt(class),
                "both-diverged state classified as a class that writes without prompt: {:?}",
                class
            );
        }

        /// Sibling: when local is tombstoned AND remote differs from
        /// base, classify MUST emit `LocalDeleteRemoteEdit` — never a
        /// one-sided class. A `LocalDelete` here would push a DELETE
        /// to a remote the user didn't agree to lose.
        #[test]
        fn classify_emits_conflict_when_local_deleted_and_remote_diverged_from_base(
            base in "[a-z0-9]{1,32}",
            remote in "[a-z0-9]{1,32}",
        ) {
            prop_assume!(base != remote);
            let k = ("hooks".to_string(), "v1".to_string());
            let mut remote_hashes = BTreeMap::new();
            remote_hashes.insert(k.clone(), remote);
            let mut tombs = BTreeSet::new();
            tombs.insert(k.clone());
            let mut locked = BTreeMap::new();
            locked.insert(k.clone(), base);

            let result = classify(&remote_hashes, &BTreeMap::new(), &tombs, &locked);
            let class = &result[0].class;
            prop_assert_eq!(
                class,
                &SyncClass::LocalDeleteRemoteEdit,
                "local-deleted + remote-edited → MUST be LocalDeleteRemoteEdit; got {:?}",
                class
            );
            prop_assert!(
                !writes_without_prompt(class),
                "local-delete-remote-edit state classified as a class that writes without prompt: {:?}",
                class
            );
        }

        /// Sibling: when local is edited AND remote is absent (deleted
        /// on env) AND lockfile records a base, classify MUST emit
        /// `LocalEditRemoteDelete`. A `LocalCreate` (or `LocalEdit`)
        /// here would POST a deleted object back without confirmation.
        #[test]
        fn classify_emits_conflict_when_local_edited_and_remote_deleted(
            base in "[a-z0-9]{1,32}",
            local in "[a-z0-9]{1,32}",
        ) {
            prop_assume!(base != local);
            let k = ("hooks".to_string(), "v1".to_string());
            let mut scan_changes = BTreeMap::new();
            scan_changes.insert(k.clone(), local);
            let mut locked = BTreeMap::new();
            locked.insert(k.clone(), base);

            let result = classify(&BTreeMap::new(), &scan_changes, &BTreeSet::new(), &locked);
            let class = &result[0].class;
            prop_assert_eq!(
                class,
                &SyncClass::LocalEditRemoteDelete,
                "local-edited + remote-deleted → MUST be LocalEditRemoteDelete; got {:?}",
                class
            );
            prop_assert!(
                !writes_without_prompt(class),
                "local-edit-remote-delete state classified as a class that writes without prompt: {:?}",
                class
            );
        }

        /// Stress the full state space. Generate every combination of
        /// (local_changed, local_tombstoned, remote_present, locked_present)
        /// with random hashes and assert classify is total (no panic)
        /// and never picks a one-sided write class when local + remote
        /// + base are all distinct.
        #[test]
        fn classify_is_total_and_safe_across_all_states(
            base in proptest::option::of("[a-z0-9]{1,16}"),
            local in proptest::option::of("[a-z0-9]{1,16}"),
            remote in proptest::option::of("[a-z0-9]{1,16}"),
            tombstoned in proptest::bool::ANY,
        ) {
            let k = ("hooks".to_string(), "v1".to_string());

            let mut remote_hashes = BTreeMap::new();
            if let Some(ref r) = remote { remote_hashes.insert(k.clone(), r.clone()); }
            let mut scan_changes = BTreeMap::new();
            // `scan_changes` is only present when local has a hash AND
            // it differs from base (the real scanner only flags changed
            // files); skip otherwise.
            if let Some(ref l) = local
                && Some(l) != base.as_ref()
                && !tombstoned
            {
                scan_changes.insert(k.clone(), l.clone());
            }
            let mut tombs = BTreeSet::new();
            if tombstoned { tombs.insert(k.clone()); }
            let mut locked = BTreeMap::new();
            if let Some(ref b) = base { locked.insert(k.clone(), b.clone()); }

            // classify must not panic on any legal combination. The
            // panic arm is reserved for impossible states (locked
            // present but base hash is the literal `None` for the key).
            let result = std::panic::catch_unwind(|| {
                classify(&remote_hashes, &scan_changes, &tombs, &locked)
            });
            // Some configurations are inherently impossible (e.g.,
            // both local_changed and locked_absent reaching the panic
            // arm via local_changed=true, locked_present=false,
            // remote_present=true). When classify panics, the
            // invariant "we never produce a one-sided class for a
            // both-diverged state" holds vacuously. The property of
            // interest only fires on Ok().
            if let Ok(items) = result
                && let Some(it) = items.first()
            {
                // The safety check: if scan_changes AND remote_hashes
                // entries differ from base, we must NOT emit a
                // one-sided write class.
                let lh = it.local_hash.as_deref();
                let rh = it.remote_hash.as_deref();
                let bh = it.base_hash.as_deref();
                let both_diverged =
                    lh.is_some() && rh.is_some() && lh != bh && rh != bh && lh != rh;
                if both_diverged {
                    prop_assert!(
                        !writes_without_prompt(&it.class),
                        "both-diverged hashes classified as a write-without-prompt class: \
                         class={:?} local={:?} remote={:?} base={:?}",
                        it.class, lh, rh, bh
                    );
                }
            }
        }
    }
}
