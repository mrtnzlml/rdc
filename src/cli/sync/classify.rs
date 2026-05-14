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
            _ => continue, // Task 7 fills in the rest.
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
        items.iter().map(|(k, s, h)| (key(k, s), h.to_string())).collect()
    }

    fn st(items: &[(&str, &str)]) -> BTreeSet<(String, String)> {
        items.iter().map(|(k, s)| key(k, s)).collect()
    }

    fn class_of<'a>(items: &'a [ClassifiedItem], slug: &str) -> &'a SyncClass {
        &items.iter().find(|c| c.slug == slug).expect("item present").class
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
}
