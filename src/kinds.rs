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
