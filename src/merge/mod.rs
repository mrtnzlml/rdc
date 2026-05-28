//! 3-way merge for sync's `BothDiverged` conflict class.
//!
//! When local and remote both diverge from the base (the last-synced
//! snapshot, recovered from the sidecar base cache in
//! `.rdc/state/<env>.base/`), this module's `merge3_json` /
//! `merge3_sidecar` try to produce a clean merge automatically. The
//! sync executor consults them before falling through to the
//! interactive conflict prompt — so a deploy that edited field A
//! while the user edited field B locally completes without a
//! per-hunk walk.
//!
//! Both algorithms refuse aggressive resolutions: any genuinely
//! overlapping edit returns `MergeOutcome::Conflict` so the user
//! makes the call. The merge never invents a third value.

pub mod json3;
pub mod sidecar;

/// Outcome of a single 3-way merge attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome<T> {
    /// Auto-resolved cleanly. `merged` is the bytes/value to write.
    /// `paths` lists the disjoint edit locations (one per side) so
    /// the caller can log a tidy "auto-merge (local: name; remote:
    /// events)" line.
    Merged {
        merged: T,
        local_paths: Vec<String>,
        remote_paths: Vec<String>,
    },
    /// The merge can't proceed without user input. `reasons` describes
    /// what overlapped, in dotted-path form for JSON (`content.fields[id=total].label`)
    /// or a single sentinel for sidecars (`code`).
    Conflict { reasons: Vec<String> },
}
