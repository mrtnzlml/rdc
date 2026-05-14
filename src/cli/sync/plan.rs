//! User-facing "Plan: sync <env>" rendering for the sync executor.
//!
//! Spec §"Execution pipeline → Plan".

use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
use std::fmt::Write;

/// Render the plan block shown to the user before sync executes.
/// Returns a `String` that callers print to stderr or stdout.
pub fn render_plan(env: &str, items: &[ClassifiedItem]) -> String {
    let mut pull: Vec<&ClassifiedItem> = Vec::new();
    let mut push: Vec<&ClassifiedItem> = Vec::new();
    let mut conflicts: Vec<&ClassifiedItem> = Vec::new();
    let mut tombstone_resolves: Vec<&ClassifiedItem> = Vec::new();

    for it in items {
        match it.class {
            SyncClass::RemoteEdit | SyncClass::RemoteCreate => pull.push(it),
            SyncClass::LocalEdit | SyncClass::LocalCreate | SyncClass::LocalDelete => push.push(it),
            SyncClass::BothDiverged
            | SyncClass::LocalEditRemoteDelete
            | SyncClass::LocalDeleteRemoteEdit => conflicts.push(it),
            SyncClass::RemoteDelete => tombstone_resolves.push(it),
            SyncClass::Clean | SyncClass::BothDeleted => {}
        }
    }

    let mut out = String::new();
    writeln!(out, "Plan: sync {env}").ok();

    if !pull.is_empty() {
        writeln!(out, "  ← pull:    {} change{} from {env}",
            pull.len(),
            if pull.len() == 1 { "" } else { "s" }
        ).ok();
        for it in &pull {
            let note = if matches!(it.class, SyncClass::RemoteCreate) { " (new)" } else { "" };
            writeln!(out, "               {}/{}{note}", it.kind, it.slug).ok();
        }
    }

    if !push.is_empty() {
        writeln!(out, "  → push:    {} local edit{}",
            push.len(),
            if push.len() == 1 { "" } else { "s" }
        ).ok();
        for it in &push {
            writeln!(out, "               {}/{}", it.kind, it.slug).ok();
        }
    }

    if !conflicts.is_empty() {
        writeln!(out, "  ⚠ {} conflict{} (needs resolution)",
            conflicts.len(),
            if conflicts.len() == 1 { "" } else { "s" }
        ).ok();
        for it in &conflicts {
            let tag = match it.class {
                SyncClass::BothDiverged => "both diverged",
                SyncClass::LocalEditRemoteDelete => "local edit, deleted on env",
                SyncClass::LocalDeleteRemoteEdit => "local delete, edited on env",
                _ => "",
            };
            writeln!(out, "               {}/{}  — {tag}", it.kind, it.slug).ok();
        }
    }

    if !tombstone_resolves.is_empty() {
        writeln!(out, "  ⚠ deleted on {env}: {} object{}",
            tombstone_resolves.len(),
            if tombstone_resolves.len() == 1 { "" } else { "s" }
        ).ok();
        for it in &tombstone_resolves {
            writeln!(out, "               {}/{}", it.kind, it.slug).ok();
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str, slug: &str, class: SyncClass) -> ClassifiedItem {
        ClassifiedItem {
            kind: kind.into(), slug: slug.into(), class,
            local_hash: Some("h".into()),
            remote_hash: Some("h".into()),
            base_hash: Some("h".into()),
        }
    }

    #[test]
    fn render_plan_groups_by_direction() {
        let items = vec![
            item("hooks", "v1", SyncClass::RemoteEdit),
            item("queues", "c1", SyncClass::RemoteEdit),
            item("labels", "a1", SyncClass::RemoteCreate),
            item("rules", "r1", SyncClass::LocalEdit),
            item("hooks", "vt", SyncClass::BothDiverged),
        ];
        let out = render_plan("test", &items);
        assert!(out.contains("Plan: sync test"), "header: {out}");
        assert!(out.contains("3 changes from test"), "pull count: {out}");
        assert!(out.contains("1 local edit"), "push count: {out}");
        assert!(out.contains("1 conflict"), "conflict count: {out}");
        assert!(out.contains("hooks/v1"), "pull item: {out}");
        assert!(out.contains("labels/a1 (new)"), "remote create tag: {out}");
        assert!(out.contains("rules/r1"), "push item: {out}");
        assert!(out.contains("hooks/vt"), "conflict item: {out}");
        assert!(out.contains("both diverged"), "conflict tag: {out}");
    }

    #[test]
    fn render_plan_empty_when_nothing_classified() {
        let items: Vec<ClassifiedItem> = vec![];
        let out = render_plan("test", &items);
        assert_eq!(out.trim(), "Plan: sync test");
    }

    #[test]
    fn render_plan_clean_and_both_deleted_are_ignored() {
        let items = vec![
            item("hooks", "x", SyncClass::Clean),
            item("hooks", "y", SyncClass::BothDeleted),
        ];
        let out = render_plan("prod", &items);
        // Header only; no sections.
        assert_eq!(out.trim(), "Plan: sync prod");
        assert!(!out.contains("hooks/x"));
        assert!(!out.contains("hooks/y"));
    }

    #[test]
    fn render_plan_remote_delete_in_its_own_section() {
        let items = vec![item("labels", "audit", SyncClass::RemoteDelete)];
        let out = render_plan("production", &items);
        assert!(out.contains("deleted on production"), "remote-delete header: {out}");
        assert!(out.contains("labels/audit"), "item: {out}");
    }
}
