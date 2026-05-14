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
    _remote_hashes: &BTreeMap<(String, String), String>,
    _scan_changes: &BTreeMap<(String, String), String>,
    _scan_tombstones: &BTreeSet<(String, String)>,
    _locked: &BTreeMap<(String, String), String>,
) -> Vec<ClassifiedItem> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    // populated in subsequent tasks
}
