//! Pre-flight validation of Rossum API field-length limits.
//!
//! The Rossum API enforces `max_length` on a number of string fields. A
//! local object that exceeds one is rejected with an opaque
//! `400 Bad Request` naming only the remote object id:
//!
//! ```text
//! PATCH /hooks/1234: Rossum API returned status 400:
//!   description: Ensure this field has no more than 2000 characters.
//! ```
//!
//! That failure is *permanent* — retrying can never succeed while the
//! local bytes stay oversized. Because the push phase runs before the
//! pull phase and its error aborts the cycle, a single oversized field
//! wedges the whole project: no pull ever lands again until a human
//! notices and edits the file. Validating locally converts that into an
//! actionable error raised before the first remote write, and lets
//! `--dry-run` predict it instead of reporting the doomed push as though
//! it would succeed.
//!
//! # Where the numbers come from
//!
//! Every limit below was harvested from the API's own `OPTIONS`
//! metadata (`actions.POST.<field>.max_length`) against a live Rossum
//! deployment, not from documentation. `hooks` nests its fields one
//! level deeper under a polymorphic wrapper
//! (`actions.POST.{function,webhook,job}.children.<field>.max_length`);
//! all three variants agree on the values recorded here.
//!
//! The server stays the authority: this table is an *early, friendlier*
//! rejection, never a replacement. A limit the table misses still fails
//! server-side exactly as it does today, so an incomplete table degrades
//! to current behavior rather than letting bad data through.
//!
//! Only **top-level string** fields are listed. Nested config limits
//! (e.g. `webhook.config.url`) are deliberately left to the server:
//! validating them would mean re-deriving each hook variant's shape
//! here, and a mistake would wrongly block a legitimate push — strictly
//! worse than the 400 it would replace.

use serde_json::Value;

/// One field whose local value is longer than the API accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitViolation {
    /// Where the offending value lives, in terms a human can act on: a
    /// top-level JSON key (`description`), or a built-up location for a
    /// nested or sidecar value (`formula on datapoint 'total_amount'`).
    /// Dynamic because the server's own error for nested fields is
    /// positional and carries no id — naming the object is the whole
    /// value this check adds over the raw 400.
    pub field: String,
    /// The API's declared `max_length` for this field.
    pub limit: usize,
    /// The local value's length, in the same unit the server counts.
    pub actual: usize,
}

/// Declared `max_length` for each kind's top-level string fields.
///
/// Kinds absent from this match (and fields absent from a kind's slice)
/// are simply not checked locally.
pub fn field_limits(kind: &str) -> &'static [(&'static str, usize)] {
    match kind {
        // `description` is the one that bites in practice: hook docs grow
        // past 2000 characters as a solution is documented in place.
        "hooks" => &[
            ("name", 255),
            ("description", 2000),
            ("extension_image_url", 200),
            ("read_more_url", 200),
        ],
        "queues" => &[("name", 255), ("rir_params", 255)],
        "schemas" => &[("name", 255)],
        // A rule's `description` cap is 255 — far tighter than a hook's 2000.
        "rules" => &[("name", 255), ("description", 255), ("trigger_condition", 4000)],
        "email_templates" => &[("name", 255), ("subject", 255)],
        "engines" => &[("name", 255)],
        "workspaces" => &[("name", 255)],
        "labels" => &[("name", 255), ("color", 7)],
        "inboxes" => &[("name", 255)],
        "engine_fields" => &[
            ("name", 50),
            ("label", 100),
            ("pre_trained_field_id", 50),
            ("subtype", 50),
        ],
        _ => &[],
    }
}

/// Check one outgoing payload against [`field_limits`].
///
/// Counts Unicode **code points**, not bytes: the server's limit comes
/// from Django's `MaxLengthValidator`, which measures Python `len(str)`.
/// Counting bytes would spuriously reject a value that fits, and only
/// for users whose text happens to be non-ASCII.
///
/// Values are counted **after trimming surrounding whitespace**, which is
/// what the server does before validating (verified live: a value at
/// exactly the limit plus a trailing newline is accepted).
///
/// Non-string values (`null`, numbers, objects) are skipped rather than
/// coerced — a length limit is meaningless for them, and the server
/// reports a type error that says so far more clearly than this check
/// could.
pub fn check_field_limits(kind: &str, body: &Value) -> Vec<LimitViolation> {
    let Some(obj) = body.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (field, limit) in field_limits(kind) {
        let Some(Value::String(s)) = obj.get(*field) else {
            continue;
        };
        if let Some(v) = check_text(*field, *limit, s) {
            out.push(v);
        }
    }
    out
}

/// Check one string against one limit, returning a violation if it is too
/// long. The single place length is measured — every check in this module
/// funnels through it, so the trimming rule cannot drift between them.
pub fn check_text(field: impl Into<String>, limit: usize, text: &str) -> Option<LimitViolation> {
    let actual = text.trim().chars().count();
    (actual > limit).then(|| LimitViolation { field: field.into(), limit, actual })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The reported incident: a hook whose `description` grew past 2000
    /// characters. Before this check, the only signal was a mid-push 400.
    #[test]
    fn hook_description_over_limit_is_reported() {
        let body = json!({ "name": "example-hook", "description": "x".repeat(2406) });
        assert_eq!(
            check_field_limits("hooks", &body),
            vec![LimitViolation { field: "description".to_string(), limit: 2000, actual: 2406 }],
        );
    }

    /// Boundary: the API rejects *more than* max_length, so exactly at
    /// the limit must pass. An off-by-one here would block valid pushes.
    #[test]
    fn hook_description_exactly_at_limit_is_accepted() {
        let body = json!({ "description": "x".repeat(2000) });
        assert_eq!(check_field_limits("hooks", &body), vec![]);
    }

    #[test]
    fn hook_description_one_over_limit_is_reported() {
        let body = json!({ "description": "x".repeat(2001) });
        assert_eq!(
            check_field_limits("hooks", &body),
            vec![LimitViolation { field: "description".to_string(), limit: 2000, actual: 2001 }],
        );
    }

    /// Django counts code points; counting bytes would reject a value
    /// that the server accepts. 1500 em-dashes are 4500 bytes but only
    /// 1500 characters, so this must NOT be flagged.
    #[test]
    fn length_is_counted_in_characters_not_bytes() {
        let s = "\u{2014}".repeat(1500);
        assert!(s.len() > 2000, "precondition: byte length exceeds the limit");
        let body = json!({ "description": s });
        assert_eq!(check_field_limits("hooks", &body), vec![]);
    }

    /// Same field name, different cap per kind: a rule's description is
    /// capped at 255, so a 300-char value is invalid for a rule but fine
    /// for a hook. A single global table would get one of these wrong.
    #[test]
    fn same_field_has_different_limit_per_kind() {
        let body = json!({ "description": "x".repeat(300) });
        assert_eq!(
            check_field_limits("rules", &body),
            vec![LimitViolation { field: "description".to_string(), limit: 255, actual: 300 }],
        );
        assert_eq!(check_field_limits("hooks", &body), vec![]);
    }

    /// Every violation must be reported, not just the first — otherwise
    /// the user fixes one field and hits the next on the following run.
    #[test]
    fn every_violating_field_is_reported() {
        let body = json!({ "name": "x".repeat(256), "description": "y".repeat(2500) });
        let got = check_field_limits("hooks", &body);
        assert_eq!(
            got,
            vec![
                LimitViolation { field: "name".to_string(), limit: 255, actual: 256 },
                LimitViolation { field: "description".to_string(), limit: 2000, actual: 2500 },
            ],
        );
    }

    /// A length limit is meaningless for a non-string; skip rather than
    /// coerce, and never panic.
    #[test]
    fn non_string_values_are_skipped() {
        for v in [json!(null), json!(12345), json!({"a": 1}), json!(["x"])] {
            let body = json!({ "description": v });
            assert_eq!(check_field_limits("hooks", &body), vec![]);
        }
    }

    #[test]
    fn unknown_kind_is_not_checked() {
        let body = json!({ "name": "x".repeat(9999) });
        assert_eq!(check_field_limits("annotations", &body), vec![]);
        assert_eq!(field_limits("annotations"), &[]);
    }

    #[test]
    fn non_object_body_is_not_checked() {
        assert_eq!(check_field_limits("hooks", &json!("a string")), vec![]);
        assert_eq!(check_field_limits("hooks", &json!(null)), vec![]);
    }

    /// Invariant: never validate a field that the push path strips from
    /// the outgoing body. Doing so would reject a push over a value the
    /// server is never going to see — a false positive strictly worse
    /// than the 400 this module replaces.
    #[test]
    fn no_validated_field_is_stripped_before_push() {
        for kind in [
            "hooks",
            "queues",
            "schemas",
            "rules",
            "email_templates",
            "engines",
            "workspaces",
            "labels",
            "inboxes",
            "engine_fields",
        ] {
            // Build a body carrying every validated field, strip it the
            // way the push path does, and assert the fields survive.
            let mut body = serde_json::Map::new();
            for (field, _) in field_limits(kind) {
                body.insert((*field).to_string(), json!("v"));
            }
            let mut body = Value::Object(body);
            crate::snapshot::create::strip_for_create(&mut body, kind);
            for (field, _) in field_limits(kind) {
                assert!(
                    body.get(*field).is_some(),
                    "{kind}.{field} is validated locally but stripped before push",
                );
            }
        }
    }

    /// Every kind rdc can push should have a limits entry, so a new kind
    /// can't silently opt out of pre-flight validation.
    #[test]
    fn every_pushable_kind_has_limits() {
        for kind in [
            "hooks",
            "queues",
            "schemas",
            "rules",
            "email_templates",
            "engines",
            "workspaces",
            "labels",
            "inboxes",
            "engine_fields",
        ] {
            assert!(
                !field_limits(kind).is_empty(),
                "pushable kind '{kind}' has no field limits recorded",
            );
        }
    }

    /// The server trims surrounding whitespace before validating: a value
    /// at exactly the limit plus a trailing newline is ACCEPTED. Verified
    /// live with a single request carrying a 2001-char formula and a
    /// 2000-char-plus-newline formula — only the first errored. Counting
    /// raw would reject every sidecar an editor added a final newline to.
    #[test]
    fn trailing_newline_does_not_count_toward_the_limit() {
        let body = json!({ "description": format!("{}\n", "x".repeat(2000)) });
        assert_eq!(check_field_limits("hooks", &body), vec![]);
    }

    /// Trimming applies to both ends and to whitespace generally, not just
    /// a newline. Trimming at least as much as the server is the safe side:
    /// under-report and the server still rejects; over-report and a valid
    /// push is blocked.
    #[test]
    fn surrounding_whitespace_does_not_count_toward_the_limit() {
        let body = json!({ "description": format!("  {}\t\n", "x".repeat(2000)) });
        assert_eq!(check_field_limits("hooks", &body), vec![]);
    }

    /// The reported length is what the server sees, so an over-limit value
    /// reports its TRIMMED length — otherwise "shorten it by N" is wrong.
    #[test]
    fn reported_length_is_the_trimmed_length() {
        let body = json!({ "description": format!("\n{}\n", "x".repeat(2500)) });
        assert_eq!(
            check_field_limits("hooks", &body),
            vec![LimitViolation { field: "description".to_string(), limit: 2000, actual: 2500 }],
        );
    }
}
