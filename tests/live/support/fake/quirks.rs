//! The learned-facts layer: behaviors of the real Rossum API that `rdc` had to
//! discover in a live org, expressed once, executably.
//!
//! Every entry in [`QUIRKS`] carries evidence for the fact it names, in one
//! of two shapes distinguished by [`Quirk::proven_by`]'s own syntax:
//!
//! - A LIVE citation, `<scenario file>::<test fn>` (a double colon) — a live
//!   scenario actually asserts the fact. Checked by
//!   `every_live_citation_actually_proves_it`, which reads the cited file and
//!   confirms the cited test exists in it — not that the test proves the
//!   RIGHT thing, which is a human judgment call this guard cannot make, but
//!   at least that the citation cannot rot into a dangling reference.
//! - A SOURCE citation, `<repo file>:<line>` (a single colon) — no live
//!   scenario proves this fact; the evidence is a repo comment, a captured
//!   fixture, or (for `modelled: false` entries) the description of a gap.
//!   Checked by `every_source_citation_names_a_real_file`, which confirms the
//!   cited file exists.
//!
//! [`Quirk::modelled`] is orthogonal to which citation shape is used: it says
//! whether the fake actually IMPLEMENTS the fact. Most quirks are `modelled:
//! true` with a live citation. A `modelled: true` quirk with a SOURCE
//! citation means "the fake does this, but no live scenario proves it yet."
//! A `modelled: false` quirk means "the fake does NOT do this yet" — recorded
//! here anyway, so a known gap is visible in the same table as everything the
//! fake gets right, rather than living only in a doc comment somewhere a
//! reader has to already know to check.
//!
//! The rule this encodes: the fake invents nothing, and neither does this
//! registry — an entry's citation must point at real, checkable evidence, and
//! its shape must not overstate what that evidence proves. When a `fake_*`
//! test and its `live_*` twin disagree on a LIVE-cited fact, exactly one of
//! two things is true — the model here is wrong, or `rdc` is wrong. Weakening
//! a citation to make a test pass is not a third option; downgrading it to an
//! honestly-labeled SOURCE citation, when that is what the evidence actually
//! supports, is not weakening — it's correcting an overclaim.

use serde_json::{json, Value};

use super::state::OrgState;

/// The response-shaping seam. `route()` (`mod.rs`) passes every response
/// body it builds through here, keyed by `(kind, method)` — `kind` is the
/// canonical path a `kinds::Spec` is keyed by (e.g. `"queues"`), or
/// `"organizations"` for the one endpoint that sits outside that registry.
/// So far exactly one pair has a rule: `("organizations", "PATCH")`, which
/// implements quirk `organization_patch_response_is_not_get_shaped` below.
/// Every other `(kind, method)` passes through unchanged — this function is a
/// no-op for them, not merely untested for them: `route()` calls it
/// unconditionally for every kind and method, so a rule added here for one
/// pair can never silently apply to another.
pub fn shape_response(kind: &str, method: &str, body: &mut Value) {
    if (kind, method) == ("organizations", "PATCH") {
        shape_organization_patch_response(body);
    }
}

/// Quirk `organization_patch_response_is_not_get_shaped`. Models two of the
/// three real differences documented at
/// `src/cli/push/organization.rs:161-177` between a real
/// `PATCH /organizations/{id}` response and what `GET` on the same id
/// returns:
///
/// - the PATCH response carries `rir_key`, which GET omits entirely;
/// - it normalizes values inside `settings` (`width: 140` comes back
///   `140.0`; an empty `annotation_list_table` comes back
///   `{ "columns": [] }`).
///
/// The third documented difference — `users` returned in a different order —
/// is deliberately NOT modelled: this fake's organization always carries
/// `users: []` (`state.rs::OrgState::new`), so reversing an empty list is a
/// no-op and an assertion on it would be vacuous. Seeding synthetic users
/// just to make the reorder observable would change the organization body
/// every pull sees, for a purely cosmetic difference — and the two
/// differences modelled above already make a naive write-back of this
/// response detectably wrong on the next pull, which is the property this
/// whole exercise exists to protect.
fn shape_organization_patch_response(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else { return };
    // Presence is what matters, not the value — see the quirk's doc comment
    // above and `organization_patch_response_is_not_get_shaped`'s citation.
    obj.entry("rir_key").or_insert_with(|| json!("fake-rir-key"));
    if let Some(settings) = obj.get_mut("settings") {
        normalize_settings(settings);
    }
}

/// Recurses through the whole `settings` subtree — an object, an array, or a
/// leaf at any depth — because the real server's normalization isn't scoped
/// to one fixed key path; it applies wherever `width` or an empty
/// `annotation_list_table` appear underneath `settings`.
fn normalize_settings(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.get("annotation_list_table") == Some(&json!({})) {
                map.insert("annotation_list_table".into(), json!({ "columns": [] }));
            }
            if let Some(width) = map.get_mut("width")
                && let Some(i) = width.as_i64()
            {
                *width = json!(i as f64);
            }
            for v in map.values_mut() {
                normalize_settings(v);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                normalize_settings(v);
            }
        }
        _ => {}
    }
}

pub struct Quirk {
    pub name: &'static str,
    /// Whether the fake actually implements this behavior. `false` marks a
    /// real API fact this registry records but the fake does not yet
    /// reproduce.
    pub modelled: bool,
    /// See the module doc comment: `<scenario file>::<test fn>` (live proof)
    /// or `<repo file>:<line>` (documented, not live-proven).
    pub proven_by: &'static str,
}

pub const QUIRKS: &[Quirk] = &[
    Quirk {
        name: "queue_create_materializes_typed_email_template_defaults",
        modelled: true,
        proven_by: "email_templates.rs::live_email_templates_round_trip",
    },
    Quirk {
        name: "queue_delete_is_async_and_cascades",
        modelled: true,
        // This citation proves only the ASYNC half: `live_push_create_ordering`
        // asserts `status == "deletion_requested"` at `ordering.rs:348`. The
        // CASCADE half (the queue's auto-created email templates and inbox
        // disappearing with it) has NO live assertion anywhere in the tree —
        // it is documented only in `state.rs::delete()`'s and
        // `graph.rs::cascade_queue_delete`'s doc comments, and pinned
        // offline by `graph.rs::a_queue_delete_cascades_to_its_templates_and_inbox`,
        // which exercises the fake's OWN implementation of the rule, not the
        // real API. The previous citation here,
        // `conflicts_deletes.rs::live_conflicts_deletes`, proved neither
        // half: that scenario's delete branch deletes a RULE, never a
        // queue, and never observes a 202 or a cascade.
        proven_by: "ordering.rs::live_push_create_ordering",
    },
    Quirk {
        name: "engine_delete_refused_while_a_queue_awaits_deletion",
        modelled: true,
        // `live_push_create_ordering` proves the CONSEQUENCE `rdc` draws from
        // this refusal — it warns by slug (the `expected_warning` assertion)
        // and keeps the lockfile entry so a later sync retries it (the
        // `lf_after` assertion) — but it never asserts the specific error
        // code the fake emits here, `engine_attached_to_queues_waiting_for_deletion`.
        // That exact string is repo-documented, not live-asserted, at
        // `tests/live/support/teardown.rs:62`.
        proven_by: "ordering.rs::live_push_create_ordering",
    },
    Quirk {
        name: "unresolvable_ref_is_an_invalid_hyperlink",
        modelled: true,
        // No live scenario provokes an unresolvable ref:
        // `cross_refs.rs::live_cross_refs` has no negative path, and the
        // only other mention of this exact string, `ordering.rs:110-112`,
        // documents why correct creation ORDER prevents the 400 from ever
        // firing live — it is never triggered, let alone asserted. The
        // evidence for the exact message is `src/snapshot/refs.rs:159`: the
        // refusal `rdc`'s whole deferred-relink path is built around.
        // `validate::on_write` matches it, and it is offline-tested at
        // `state.rs::a_ref_that_matches_no_object_is_an_invalid_hyperlink`.
        proven_by: "src/snapshot/refs.rs:159",
    },
    Quirk {
        name: "over_length_field_is_refused_after_trailing_whitespace_trim",
        modelled: true,
        proven_by: "server_truth.rs::live_field_limits_match_the_server",
    },
    Quirk {
        name: "queue_carries_one_engine_slot_only",
        modelled: true,
        proven_by: "server_truth.rs::live_queue_engine_slot_counts_values_not_keys",
    },
    Quirk {
        name: "organization_patch_response_is_not_get_shaped",
        modelled: true,
        // `quirks::shape_response` now models two of the three differences
        // documented at `src/cli/push/organization.rs:161-177` between a
        // real `PATCH /organizations/{id}` response and what `GET` on the
        // same id returns: the PATCH response carries `rir_key`, which GET
        // omits entirely, and it normalizes `settings` (`width: 140` comes
        // back `140.0`; an empty `annotation_list_table` comes back
        // `{ "columns": [] }`). See `shape_organization_patch_response`'s
        // doc comment for why the third difference — `users` reordering —
        // is deliberately left unmodelled (this org's `users` is always
        // `[]`, so reversing it is vacuous).
        //
        // This is a SOURCE citation, not a LIVE one, and that is a
        // deliberate choice, not an oversight. The live scenario that
        // touches organization push, `organization.rs::live_organization_
        // settings_push`, asserts that a pushed column persists remotely
        // and that a second sync converges to zero pulls — it never asserts
        // the PATCH-vs-GET shape difference itself (no check that a raw
        // PATCH response carries `rir_key`, or that `settings` comes back
        // normalized): that assertion would have to happen against the raw
        // HTTP response, and this test only ever looks at the org through
        // rdc's own file on disk. Citing it here would repeat the mistake
        // stage 1's reviews already caught twice — a citation pointing at a
        // plausible-sounding test that never actually exercises the fact.
        // The real evidence is this file (`src/cli/push/organization.rs:161-
        // 177`, the comment the incident is recorded in) plus an OFFLINE
        // integration test that reproduces the asymmetry on purpose with a
        // hand-built mock and asserts on it directly — `rir_key` never
        // reaching disk, `settings` picking up the server's normalization,
        // and a third sync pulling zero items:
        // `tests/cli_sync.rs::sync_organization_write_back_keeps_the_shape_a_pull_would_produce`.
        // That test predates this fake and doesn't run through it, so it
        // cannot serve as this quirk's citation either — hence the SOURCE
        // form, naming the fact's original documentation.
        proven_by: "src/cli/push/organization.rs:161",
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
        // `create_unchecked`, not `create`: these are the server's OWN
        // objects, materialized as a side effect of `POST /queues`, not
        // POSTed by a client — the checks a client POST answers to must not
        // apply here, and in particular the unique-typed-template rule would
        // otherwise refuse the very defaults it exists to compare against.
        let _ = st.create_unchecked("email_templates", body);
    }
}

/// Whether `file` is safe to join onto the scenarios root: a plain filename
/// with no path separators and no `..` component, ending in `.rs`. For a LIVE
/// citation (`<file>::<fn>`) only — a scenario file always sits flat under
/// `tests/live/scenarios`, so a path separator here is already suspicious.
///
/// Extracted out of `every_live_citation_actually_proves_it` so the shape
/// check itself is unit-testable — before this split, its three failure modes
/// (nonexistent test, wrong file, path-traversing citation) were each proven
/// only by a manual edit-run-restore cycle, so a regression here would not be
/// caught by CI. See that test's doc comment for why the check runs on the
/// raw `file` string, before it is ever joined onto `root`.
fn is_plain_scenario_filename(file: &str) -> bool {
    !file.is_empty()
        && file.ends_with(".rs")
        && !file.contains('/')
        && !file.contains('\\')
        && !file.contains("..")
}

/// The equivalent safety check for a SOURCE citation (`<file>:<line>`): these
/// legitimately span directories (`src/cli/push/organization.rs`), so `/` is
/// allowed — only escaping the repo root is not. Same reasoning as
/// `is_plain_scenario_filename`'s doc comment: checked on the raw string,
/// before it is ever joined onto the crate root, because `Path::join` does
/// not confine its result to that root.
fn is_safe_repo_relative_path(file: &str) -> bool {
    !file.is_empty()
        && file.ends_with(".rs")
        && !file.starts_with('/')
        && !file.contains('\\')
        && !file.split('/').any(|seg| seg == "..")
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
    /// Runs only on LIVE citations (`proven_by` containing `::`) — see the
    /// module doc comment for the other shape, checked by
    /// `every_source_citation_names_a_real_file` below.
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
    ///
    /// This still only checks that the cited function EXISTS, not that it
    /// proves the right thing — that is exactly the gap that let
    /// `queue_delete_is_async_and_cascades` and
    /// `unresolvable_ref_is_an_invalid_hyperlink` cite scenarios that never
    /// exercised the fact they were attached to. Closing that gap needs a
    /// human reading the cited test, which is why every entry above also
    /// carries a doc comment saying exactly what its citation does and does
    /// not prove.
    #[test]
    fn every_live_citation_actually_proves_it() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live/scenarios");
        for q in QUIRKS.iter().filter(|q| q.proven_by.contains("::")) {
            let (file, test) = q
                .proven_by
                .split_once("::")
                .unwrap_or_else(|| panic!("quirk '{}' has a malformed citation: {}", q.name, q.proven_by));
            assert!(
                is_plain_scenario_filename(file),
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

    /// The other citation shape: a SOURCE citation (`<file>:<line>`, no
    /// `::`), used when no live scenario proves the fact — including every
    /// `modelled: false` entry, which by definition can have no live proof.
    /// Weaker than the live check (there is no line-number or content
    /// verification, only that the file exists), but that asymmetry is
    /// honest: a source citation was never claiming live proof in the first
    /// place, only that a reader who follows it lands on a real file.
    #[test]
    fn every_source_citation_names_a_real_file() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for q in QUIRKS.iter().filter(|q| !q.proven_by.contains("::")) {
            let (file, _loc) = q
                .proven_by
                .split_once(':')
                .unwrap_or_else(|| panic!("quirk '{}' has a malformed citation: {}", q.name, q.proven_by));
            assert!(
                is_safe_repo_relative_path(file),
                "quirk '{}' cites '{file}', which is not a safe repo-relative path \
                 (no absolute path, no `..` segment, must end in `.rs`)",
                q.name
            );
            assert!(
                root.join(file).is_file(),
                "quirk '{}' cites '{file}', which is not a file in this repo",
                q.name
            );
        }
    }

    /// Pins `is_plain_scenario_filename`'s three failure modes directly,
    /// against synthetic strings, so `every_live_citation_actually_proves_it`
    /// above cannot regress silently — see its doc comment for the incident
    /// this guards against (`"../../../src/main.rs::main"` "proving" itself
    /// against `async fn main()`).
    #[test]
    fn scenario_filename_shape_is_checked_before_it_is_joined_onto_root() {
        assert!(is_plain_scenario_filename("email_templates.rs"), "a plain filename is fine");
        assert!(!is_plain_scenario_filename(""), "an empty segment");
        assert!(!is_plain_scenario_filename(".."), "a bare ..");
        assert!(
            !is_plain_scenario_filename("../../../src/main.rs"),
            "a path-traversing citation"
        );
        assert!(!is_plain_scenario_filename("/etc/passwd.rs"), "an absolute path");
        assert!(
            !is_plain_scenario_filename("..\\src\\main.rs"),
            "a Windows-style path-traversing citation"
        );
        assert!(!is_plain_scenario_filename("sub/dir.rs"), "a nested path");
    }

    /// The same pin for `is_safe_repo_relative_path`, which deliberately
    /// allows `/` (a source citation legitimately spans directories) but
    /// must still reject the same escapes.
    #[test]
    fn source_path_shape_is_checked_before_it_is_joined_onto_root() {
        assert!(
            is_safe_repo_relative_path("src/cli/push/organization.rs"),
            "a nested repo-relative path is fine"
        );
        assert!(!is_safe_repo_relative_path(""), "an empty segment");
        assert!(!is_safe_repo_relative_path(".."), "a bare ..");
        assert!(
            !is_safe_repo_relative_path("../../../etc/passwd.rs"),
            "a path-traversing citation"
        );
        assert!(!is_safe_repo_relative_path("/etc/passwd.rs"), "an absolute path");
        assert!(
            !is_safe_repo_relative_path("src/../../../etc/passwd.rs"),
            "a `..` segment buried mid-path"
        );
        assert!(
            !is_safe_repo_relative_path("..\\src\\main.rs"),
            "a Windows-style path-traversing citation"
        );
        assert!(!is_safe_repo_relative_path("src/main"), "must end in .rs");
    }

    /// Every quirk is visible and self-consistent: `modelled: false` can
    /// never pair with a LIVE citation, because an unimplemented behavior
    /// cannot have live proof of the fake's own conduct — a `false` entry
    /// making that claim would be lying about strength of evidence, exactly
    /// what this whole registry exists to prevent.
    #[test]
    fn an_unmodelled_quirk_never_claims_a_live_citation() {
        for q in QUIRKS.iter().filter(|q| !q.modelled) {
            assert!(
                !q.proven_by.contains("::"),
                "quirk '{}' is unmodelled but cites '{}' as if a live scenario proved it",
                q.name,
                q.proven_by
            );
        }
    }
}
