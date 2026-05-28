//! Strict 3-way merge for sidecar code (`.py` / `.js` / formula).
//!
//! v1 deliberately refuses any line-merge for sidecar code. The merge
//! succeeds only when at most one side has changed the file relative
//! to base: the rationale is that producing partially-merged Python
//! or JavaScript text — even when the line-level diffs don't overlap
//! — risks landing a syntactically valid but semantically broken file
//! (e.g. both sides added a `return` statement at different indents).
//!
//! When both sides edited the same sidecar, the merge returns
//! `MergeOutcome::Conflict` and the sync executor falls through to
//! the interactive prompt — which uses the styled diff renderer to
//! show local vs remote and let the user keep one side, edit, or
//! invoke per-hunk resolution.
//!
//! A future iteration may introduce structural merge (top-level
//! function/class identity, recursive content merge for matching
//! names) once we've measured how often strict mode actually
//! conflicts in practice. See `docs/superpowers/specs/` for the
//! sketch.

use super::MergeOutcome;

/// `path_label` is the label used in the log line ("hook.code",
/// "rule.trigger_condition", "schema.formula.<datapoint>") — never
/// the local disk path. The merge itself doesn't care about the
/// labeling; the caller passes it through to whatever logging
/// surface they have.
pub fn merge3_sidecar(
    base: &[u8],
    local: &[u8],
    remote: &[u8],
    path_label: &str,
) -> MergeOutcome<Vec<u8>> {
    if local == base && remote == base {
        return MergeOutcome::Merged {
            merged: base.to_vec(),
            local_paths: Vec::new(),
            remote_paths: Vec::new(),
        };
    }
    if local == base {
        return MergeOutcome::Merged {
            merged: remote.to_vec(),
            local_paths: Vec::new(),
            remote_paths: vec![path_label.to_string()],
        };
    }
    if remote == base {
        return MergeOutcome::Merged {
            merged: local.to_vec(),
            local_paths: vec![path_label.to_string()],
            remote_paths: Vec::new(),
        };
    }
    if local == remote {
        // Both sides converged on the same content. Take it.
        return MergeOutcome::Merged {
            merged: local.to_vec(),
            local_paths: vec![path_label.to_string()],
            remote_paths: Vec::new(),
        };
    }
    MergeOutcome::Conflict {
        reasons: vec![path_label.to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_changes_returns_base() {
        let b = b"def x():\n    pass\n";
        let out = merge3_sidecar(b, b, b, "hook.code");
        match out {
            MergeOutcome::Merged { merged, local_paths, remote_paths } => {
                assert_eq!(merged, b);
                assert!(local_paths.is_empty());
                assert!(remote_paths.is_empty());
            }
            MergeOutcome::Conflict { reasons } => panic!("unexpected conflict: {reasons:?}"),
        }
    }

    #[test]
    fn only_local_changed_returns_local() {
        let b = b"def x():\n    pass\n";
        let l = b"def x():\n    return 1\n";
        let r = b;
        let out = merge3_sidecar(b, l, r, "hook.code");
        match out {
            MergeOutcome::Merged { merged, local_paths, remote_paths } => {
                assert_eq!(merged, l);
                assert_eq!(local_paths, vec!["hook.code".to_string()]);
                assert!(remote_paths.is_empty());
            }
            other => panic!("expected Merged, got {other:?}"),
        }
    }

    #[test]
    fn only_remote_changed_returns_remote() {
        let b = b"def x():\n    pass\n";
        let l = b;
        let r = b"def x():\n    return 2\n";
        let out = merge3_sidecar(b, l, r, "hook.code");
        match out {
            MergeOutcome::Merged { merged, local_paths, remote_paths } => {
                assert_eq!(merged, r);
                assert!(local_paths.is_empty());
                assert_eq!(remote_paths, vec!["hook.code".to_string()]);
            }
            other => panic!("expected Merged, got {other:?}"),
        }
    }

    #[test]
    fn both_changed_same_way_takes_one() {
        let b = b"def x():\n    pass\n";
        let same = b"def x():\n    return 7\n";
        let out = merge3_sidecar(b, same, same, "hook.code");
        match out {
            MergeOutcome::Merged { merged, .. } => {
                assert_eq!(merged, same);
            }
            other => panic!("expected Merged, got {other:?}"),
        }
    }

    #[test]
    fn both_changed_differently_is_conflict() {
        let b = b"def x():\n    pass\n";
        let l = b"def x():\n    return 1\n";
        let r = b"def x():\n    return 2\n";
        let out = merge3_sidecar(b, l, r, "hook.code");
        match out {
            MergeOutcome::Conflict { reasons } => {
                assert_eq!(reasons, vec!["hook.code".to_string()]);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
}
