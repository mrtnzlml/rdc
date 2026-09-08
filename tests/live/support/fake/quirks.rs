//! The learned-facts layer: behaviors of the real Rossum API that `rdc` had to
//! discover in a live org, expressed once, executably.
//!
//! Every entry in [`QUIRKS`] carries the live scenario that proves it, and
//! `every_quirk_names_a_live_scenario_that_proves_it` enforces the citation.
//! The rule this encodes: the fake invents nothing. When a `fake_*` test and
//! its `live_*` twin disagree, exactly one of two things is true — the model
//! here is wrong, or `rdc` is wrong. Weakening the scenario is not a third
//! option.

use serde_json::{json, Value};

use super::state::OrgState;

pub struct Quirk {
    pub name: &'static str,
    /// `<scenario file>::<test fn>`.
    pub proven_by: &'static str,
}

pub const QUIRKS: &[Quirk] = &[
    Quirk {
        name: "queue_create_materializes_typed_email_template_defaults",
        proven_by: "email_templates.rs::live_email_templates_round_trip",
    },
    Quirk {
        name: "queue_delete_is_async_and_cascades",
        proven_by: "conflicts_deletes.rs::live_conflicts_deletes",
    },
    Quirk {
        name: "engine_delete_refused_while_a_queue_awaits_deletion",
        proven_by: "ordering.rs::live_push_create_ordering",
    },
    Quirk {
        name: "unresolvable_ref_is_an_invalid_hyperlink",
        proven_by: "cross_refs.rs::live_cross_refs",
    },
    Quirk {
        name: "over_length_field_is_refused_after_trailing_whitespace_trim",
        proven_by: "server_truth.rs::live_field_limits_match_the_server",
    },
    Quirk {
        name: "queue_carries_one_engine_slot_only",
        proven_by: "server_truth.rs::live_queue_engine_slot_counts_values_not_keys",
    },
];

/// The five typed defaults `POST /queues` materializes.
///
/// Provenance here is layered, and the two halves are not equally strong.
/// `live_email_templates_round_trip` proves that a freshly created queue
/// already carries typed defaults nobody asked for, and that
/// `push::email_templates`'s adopt path matches them by `type` instead of
/// creating duplicates — but that scenario's `our_templates` helper
/// deliberately filters every system-managed default out by run-id prefix
/// (see its module doc), so it never counts them or names them. It is
/// evidence that defaults exist and are matched by type, not evidence of
/// which five they are.
///
/// The exact set below — these five names, types, subjects and messages, no
/// more and no fewer — comes instead from
/// `testdata/live/snapshot/**/email-templates/`: the five files with no
/// run-id prefix in a tree captured from a real org, i.e. the ones the
/// harness never seeded. That is real evidence, but a captured fixture
/// rather than a live assertion — nothing in the suite today would fail if a
/// live org grew a sixth default or renamed one of these five. A live
/// scenario that pins the default set by name, rather than filtering it
/// away, is the thing that would upgrade this half of the quirk from fixture
/// to proof. `message` is copied verbatim from the same fixture files rather
/// than invented, per this task's own standard: a real value already sitting
/// in a file we read is never a reason to make one up.
///
/// Three are `custom`; only `rejection_default` and
/// `email_with_no_processable_attachments` are unique-typed, which is what
/// makes the adopt-by-type path in `push::email_templates` meaningful.
const QUEUE_DEFAULT_TEMPLATES: &[(&str, &str, &str, &str)] = &[
    (
        "Annotation status change - confirmed",
        "custom",
        "Document confirmed",
        "<p>Your document has been confirmed.</p>",
    ),
    (
        "Annotation status change - exported",
        "custom",
        "Document exported",
        "<p>Your document has been exported.</p>",
    ),
    (
        "Annotation status change - received",
        "custom",
        "Document received",
        "<p>Your document has been received.</p>",
    ),
    (
        "Default rejection template",
        "rejection_default",
        "Document rejected",
        "<p>Your document has been rejected.</p>",
    ),
    (
        "Email with no processable attachments",
        "email_with_no_processable_attachments",
        "No processable documents",
        "<p>No processable documents were found in this email.</p>",
    ),
];

/// Quirk `queue_create_materializes_typed_email_template_defaults`.
///
/// `POST /queues` on the real API creates these server-side; a blind POST of
/// one afterwards is refused (`src/cli/push/email_templates.rs:97`).
pub fn materialize_queue_defaults(st: &mut OrgState, queue_url: &str) {
    for (name, ty, subject, message) in QUEUE_DEFAULT_TEMPLATES {
        let body: Value = json!({
            "name": name,
            "type": ty,
            "subject": subject,
            "message": message,
            "queue": queue_url,
            "automate": false,
        });
        // `create` here is the store's own path, so ids stay monotonic.
        let _ = st.create("email_templates", body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::fake::state::{ListQuery, OrgState};
    use serde_json::json;

    #[test]
    fn a_new_queue_gets_the_servers_typed_defaults() {
        let mut s = OrgState::new("http://127.0.0.1:9/api/v1".to_string(), 1);
        // A real `schema` url: Task 7 makes a schema-less `POST /queues` a
        // 400, and this test should not need to change when that lands.
        let schema = s.create("schemas", json!({ "name": "S" })).unwrap();
        let q = s
            .create("queues", json!({ "name": "Invoices", "schema": schema["url"] }))
            .unwrap();
        let listed = s.list("email_templates", &ListQuery { page: 1, page_size: 100 });
        let names: Vec<&str> = listed["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "Annotation status change - confirmed",
                "Annotation status change - exported",
                "Annotation status change - received",
                "Default rejection template",
                "Email with no processable attachments",
            ]
        );
        for t in listed["results"].as_array().unwrap() {
            assert_eq!(t["queue"], q["url"], "each default belongs to the queue");
        }
    }

    #[test]
    fn the_unique_typed_defaults_carry_their_types() {
        let mut s = OrgState::new("http://127.0.0.1:9/api/v1".to_string(), 1);
        let schema = s.create("schemas", json!({ "name": "S" })).unwrap();
        s.create("queues", json!({ "name": "Invoices", "schema": schema["url"] }))
            .unwrap();
        let listed = s.list("email_templates", &ListQuery { page: 1, page_size: 100 });
        let types: Vec<&str> = listed["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            vec![
                "custom",
                "custom",
                "custom",
                "rejection_default",
                "email_with_no_processable_attachments",
            ]
        );
    }

    /// A quirk nobody can prove against a real org is a quirk someone
    /// invented. Written in the same spirit as `tests/command_references.rs`:
    /// the check is mechanical so the citation cannot rot silently.
    ///
    /// Deliberately reads only the CITED file per quirk, rather than
    /// concatenating every scenario file into one blob and checking file
    /// existence and function existence as two independent facts. The
    /// concatenated version let `"server_truth.rs::live_conflicts_deletes"`
    /// pass even though `live_conflicts_deletes` is defined in
    /// `conflicts_deletes.rs` — the citation named a real file and a real
    /// test, just not the same one, which the split check could never catch.
    /// Reading the pair together is what makes the citation trustworthy: a
    /// reader who follows it must land on the actual proof.
    ///
    /// The `file` half is validated as a plain filename BEFORE it is ever
    /// joined onto `root`, rather than joined and sandboxed after the fact:
    /// `Path::join` does not confine its result to `root` — a segment
    /// carrying `..` walks upward out of it, and an absolute segment
    /// replaces `root` outright. Without this check, a citation like
    /// `"../../../src/main.rs::main"` reads `src/main.rs` instead of a
    /// scenario file and "proves" itself against `async fn main()`, which
    /// happens to contain the substring `fn main(` — a self-proving citation
    /// is exactly the false-confidence hole this guard exists to close, so
    /// the shape check fails loudly rather than trusting the path.
    #[test]
    fn every_quirk_names_a_live_scenario_that_proves_it() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live/scenarios");
        for q in QUIRKS {
            let (file, test) = q
                .proven_by
                .split_once("::")
                .unwrap_or_else(|| panic!("quirk '{}' has a malformed citation: {}", q.name, q.proven_by));
            let is_plain_filename = !file.is_empty()
                && file.ends_with(".rs")
                && !file.contains('/')
                && !file.contains('\\')
                && !file.contains("..");
            assert!(
                is_plain_filename,
                "quirk '{}' cites '{file}', which is not a plain scenario filename \
                 (no path separators, no `..`, must end in `.rs`)",
                q.name
            );
            let path = root.join(file);
            let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("quirk '{}' cites '{file}', which cannot be read: {e}", q.name)
            });
            assert!(
                src.contains(&format!("fn {test}(")),
                "quirk '{}' cites '{test}', which '{file}' does not define",
                q.name
            );
        }
    }
}
