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
//! [`field_limits`] itself lists only **top-level string** fields. Coverage
//! is extended past that table in two ways: [`check_schema_content`] and
//! [`check_rule_actions`] walk nested JSON (a schema's content tree, a
//! rule's `actions[]`), and `ChangeList::field_limit_violations` (in
//! `cli::push::scan`) separately reads two sidecar files that never appear
//! in the JSON at all — a rule's `trigger_condition` and a schema
//! datapoint's `formula`. All five locations were individually confirmed
//! against a live deployment.
//!
//! Everything else nested is still deliberately left to the server: hook
//! `webhook.config.url`, `config.secret`, `config.app.url`,
//! `job.config.actor_name`, `sideload[]`. Validating those would mean
//! re-deriving each hook variant's shape here, and a mistake would wrongly
//! block a legitimate push — strictly worse than the 400 it would replace.

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

/// `rules.trigger_condition` is capped at 4000 characters. For every
/// snapshot `rdc pull` produces, the codec always extracts it into a
/// `<slug>.py` sidecar (see `snapshot::rule::read_rule_value`), so a
/// `trigger_condition` entry in [`field_limits`] — which only inspects
/// top-level JSON keys — would be dead for every pulled project. It lives
/// here instead, and `ChangeList::field_limit_violations` reads the
/// sidecar directly and checks it against this constant.
///
/// That "dead" claim isn't universal, though: `read_rule_value` splices
/// the sidecar back only `if py_path.exists()`, so a rule JSON hand-edited
/// to carry an inline string `trigger_condition` with no sibling `.py` is
/// pushed verbatim and is checked by neither this constant nor
/// [`field_limits`]. That's an accepted under-report — the safe side of
/// "never over-report" — for a shape `rdc` itself never produces.
pub const RULE_TRIGGER_CONDITION_LIMIT: usize = 4000;

/// A schema datapoint's `formula` is capped at 2000 characters. Like
/// `trigger_condition` it is extracted to a sidecar (`formulas/<id>.py`)
/// and so cannot live in [`field_limits`]. The server's rejection for this
/// field is positional and carries no datapoint id, which is why naming the
/// sidecar locally is worth more here than for a top-level field.
pub const SCHEMA_FORMULA_LIMIT: usize = 2000;

/// A schema datapoint's `prompt` is capped at 5000 characters, and
/// `memory.index_formula` at 2000. Both stay inline in `schema.json`
/// (unlike `formula`, which is extracted to a sidecar).
pub const SCHEMA_PROMPT_LIMIT: usize = 5000;
pub const SCHEMA_INDEX_FORMULA_LIMIT: usize = 2000;

/// Check the length-capped fields nested inside a schema's content tree.
///
/// Walks the same shape `snapshot::schema::extract_formulas` walks:
/// `children` is an array for sections and tuples but a single object for
/// a multivalue (its element schema). Descending into both is what covers
/// line-item column fields rather than only top-level datapoints.
///
/// `formula` is checked here too even though it normally lives in a
/// sidecar: `merge_formulas` splices a sidecar only when the datapoint has
/// no `formula` key, so one written directly into `schema.json` is pushed
/// verbatim and must be validated.
pub fn check_schema_content(body: &Value) -> Vec<LimitViolation> {
    let mut out = Vec::new();
    if let Some(content) = body.get("content").and_then(|c| c.as_array()) {
        for node in content {
            walk_schema_node(node, &mut out);
        }
    }
    out
}

fn walk_schema_node(node: &Value, out: &mut Vec<LimitViolation>) {
    let Some(obj) = node.as_object() else { return };

    if obj.get("category").and_then(|c| c.as_str()) == Some("datapoint") {
        let id = obj.get("id").and_then(|i| i.as_str()).unwrap_or("<unnamed>");

        if let Some(Value::String(s)) = obj.get("prompt") {
            out.extend(check_text(format!("prompt on datapoint '{id}'"), SCHEMA_PROMPT_LIMIT, s));
        }
        if let Some(Value::String(s)) = obj.get("formula") {
            out.extend(check_text(
                format!("formula on datapoint '{id}'"),
                SCHEMA_FORMULA_LIMIT,
                s,
            ));
        }
        if let Some(Value::String(s)) = obj.get("memory").and_then(|m| m.get("index_formula")) {
            out.extend(check_text(
                format!("memory.index_formula on datapoint '{id}'"),
                SCHEMA_INDEX_FORMULA_LIMIT,
                s,
            ));
        }
    }

    match obj.get("children") {
        Some(Value::Array(children)) => {
            for child in children {
                walk_schema_node(child, out);
            }
        }
        Some(child @ Value::Object(_)) => walk_schema_node(child, out),
        _ => {}
    }
}

/// A rule action's `payload.content` — the message text shown to the
/// operator — is capped at 4096 characters.
pub const RULE_ACTION_CONTENT_LIMIT: usize = 4096;

/// Check the length-capped fields inside a rule's `actions` array.
///
/// The `OPTIONS` metadata nests these under a polymorphic wrapper
/// (`actions.child.show_message.payload.content`), but that is a metadata
/// artifact — the same one hooks exhibit. On the wire each action is flat
/// with a `type` discriminator, so one uniform path covers every action
/// kind. Only `payload.content` is validated: the sibling `id` is a
/// server-generated UUID and `payload.schema_id` is bounded by the schema
/// field id rules, so neither is free text a human can overgrow.
///
/// The 4096 cap is applied to every action `type` uniformly. That harvest
/// was verified, not assumed: `OPTIONS /v1/rules` declares `payload.content`
/// at exactly 4096 for exactly two action types — `show_message` and
/// `add_automation_blocker` — and **no other action type declares a
/// `payload.content` field at all** (the rest declare only `id` at 50).
/// There is no action type this uniform limit could be wrong for.
pub fn check_rule_actions(body: &Value) -> Vec<LimitViolation> {
    let mut out = Vec::new();
    let Some(actions) = body.get("actions").and_then(|a| a.as_array()) else {
        return out;
    };
    for (i, action) in actions.iter().enumerate() {
        let Some(Value::String(content)) = action.get("payload").and_then(|p| p.get("content"))
        else {
            continue;
        };
        let ty = action
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("action");
        out.extend(check_text(
            format!("actions[{i}] ({ty}) payload.content"),
            RULE_ACTION_CONTENT_LIMIT,
            content,
        ));
    }
    out
}

/// A structural problem in an organization's `settings`: a wrong enum value, a
/// missing required key, or the polymorphic wrapper shape `OPTIONS` advertises
/// and the API rejects.
///
/// Distinct from [`LimitViolation`], which is only ever about length — most of
/// these have no limit and no measured length to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsProblem {
    /// Where it lives, in terms a human can act on:
    /// `settings.annotation_list_table.columns[0].data_type`.
    pub location: String,
    /// What is wrong, phrased so the message reads as the fix.
    pub problem: String,
}

/// The two column tables under an organization's `settings`. Both take the
/// identical column shape (verified via `OPTIONS /v1/organizations/{id}`).
const ORG_COLUMN_TABLES: &[&str] = &["annotation_list_table", "request_dashboard_table"];
/// `schema_id`'s declared `max_length` on an org column.
const ORG_SCHEMA_ID_LIMIT: usize = 50;
const ORG_DATA_TYPES: &[&str] = &["string", "boolean", "date", "number"];

/// Validate the parts of an organization's `settings` that rdc pushes, against
/// the shape `OPTIONS /v1/organizations/{id}` declares.
///
/// Worth doing offline rather than leaving to the server for one specific
/// reason: `OPTIONS` advertises each column as a polymorphic wrapper
/// (`{"schema": {…}}` / `{"meta": {…}}`) and the API then **rejects** that
/// shape — the accepted body is flat. Anyone reading the API metadata and
/// hand-writing a column hits a `400 column_type: This field is required.`
/// mid-sync; this check names the trap instead.
///
/// A body with no `settings` key produces no problems. What an absent
/// `settings` MEANS is the push driver's decision, not this function's.
pub fn check_organization_settings(body: &Value) -> Vec<SettingsProblem> {
    let mut out = Vec::new();
    let Some(settings) = body.get("settings").and_then(|s| s.as_object()) else {
        return out;
    };
    for table in ORG_COLUMN_TABLES {
        let Some(columns) = settings
            .get(*table)
            .and_then(|t| t.get("columns"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for (i, column) in columns.iter().enumerate() {
            let at = format!("settings.{table}.columns[{i}]");
            let Some(obj) = column.as_object() else {
                out.push(SettingsProblem { location: at, problem: "must be an object".to_string() });
                continue;
            };
            // The wrapper shape OPTIONS advertises and the API refuses.
            if obj.len() == 1 && (obj.contains_key("schema") || obj.contains_key("meta")) {
                out.push(SettingsProblem {
                    location: at,
                    problem: "column is wrapped in a \"schema\"/\"meta\" key; the API wants its \
                              fields flat (OPTIONS advertises the wrapper but rejects it)"
                        .to_string(),
                });
                continue;
            }
            let mut require = |keys: &[&str], out: &mut Vec<SettingsProblem>| {
                for key in keys {
                    if !obj.contains_key(*key) {
                        out.push(SettingsProblem {
                            location: at.clone(),
                            problem: format!("missing required key `{key}`"),
                        });
                    }
                }
            };
            match obj.get("column_type").and_then(|v| v.as_str()) {
                Some("schema") => {
                    require(&["visible", "width", "schema_id", "data_type"], &mut out);
                    if let Some(Value::String(id)) = obj.get("schema_id") {
                        let actual = id.trim().chars().count();
                        if actual > ORG_SCHEMA_ID_LIMIT {
                            out.push(SettingsProblem {
                                location: format!("{at}.schema_id"),
                                problem: format!(
                                    "{actual} characters; the API accepts at most {ORG_SCHEMA_ID_LIMIT}"
                                ),
                            });
                        }
                    }
                    if let Some(Value::String(dt)) = obj.get("data_type")
                        && !ORG_DATA_TYPES.contains(&dt.as_str())
                    {
                        out.push(SettingsProblem {
                            location: format!("{at}.data_type"),
                            problem: format!("`{dt}` is not one of {}", ORG_DATA_TYPES.join(", ")),
                        });
                    }
                }
                Some("meta") => require(&["visible", "width", "meta_name"], &mut out),
                Some(other) => out.push(SettingsProblem {
                    location: format!("{at}.column_type"),
                    problem: format!("`{other}` is not one of schema, meta"),
                }),
                None => out.push(SettingsProblem {
                    location: at.clone(),
                    problem: "missing required key `column_type`".to_string(),
                }),
            }
        }
    }
    out
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
        // `trigger_condition` is NOT here on purpose: see
        // `RULE_TRIGGER_CONDITION_LIMIT` for why (dead for pulled snapshots,
        // but not universally — a hand-written inline value with no `.py`
        // sidecar is an accepted under-report, not a covered case).
        "rules" => &[("name", 255), ("description", 255)],
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
///
/// `str::trim` strips Unicode `White_Space`; the server's DRF
/// `CharField(trim_whitespace=True)` calls Python `str.strip()`, which
/// additionally strips the four C0 control characters `\x1c`–`\x1f` (the
/// file/group/record/unit separators). A value surrounded by exactly those
/// bytes would be trimmed further server-side than here — a theoretical
/// over-report, vanishingly unlikely given those characters essentially
/// never occur in authored text.
pub fn check_text(field: impl Into<String>, limit: usize, text: &str) -> Option<LimitViolation> {
    let actual = text.trim().chars().count();
    (actual > limit).then(|| LimitViolation { field: field.into(), limit, actual })
}

/// Fields the API refuses to CREATE an object without, and that rdc can
/// legitimately end up not sending.
///
/// Deliberately tiny, and for the same reason [`field_limits`] is: the server
/// stays the authority, this is only an earlier and friendlier rejection. A
/// kind is listed here only when a *local, decidable* condition makes the
/// create doomed — never to re-declare the API's whole schema.
///
/// - **`inboxes` → `email_prefix`.** `POST /inboxes` answers
///   `400 non_field_errors: One of fields 'email_prefix' or 'email' needs to be
///   provided` (observed live). `email` cannot satisfy it from rdc's side:
///   [`crate::snapshot::create::strip_for_create`] removes it for inboxes
///   because it is server-derived (`<email_prefix>-<hash>@<host>`), so a
///   hand-written `email` never reaches the wire either. That leaves
///   `email_prefix` as the operative requirement.
///
/// Checked only for objects the push will POST; a PATCH that omits a key
/// leaves the remote's value alone and needs nothing from this table.
pub fn required_for_create(kind: &str) -> &'static [&'static str] {
    match kind {
        "inboxes" => &["email_prefix"],
        _ => &[],
    }
}

/// The [`required_for_create`] fields missing from one create payload.
///
/// "Missing" means absent, `null`, or an empty/whitespace-only string — all
/// three fail the server's `blank=False` check identically, and treating an
/// empty string as present would let the doomed body through.
pub fn missing_required_for_create(kind: &str, body: &Value) -> Vec<&'static str> {
    let Some(obj) = body.as_object() else {
        return Vec::new();
    };
    required_for_create(kind)
        .iter()
        .copied()
        .filter(|field| match obj.get(*field) {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => s.trim().is_empty(),
            Some(_) => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_inbox_without_email_prefix_is_reported() {
        // The shape `migrate` produced for every brand-new inbox: no
        // `email_prefix` (dropped as env-specific) and no `email`
        // (`strip_for_create` removes it) — a body `POST /inboxes` rejects.
        let body = json!({ "name": "In", "queues": ["rdc://queues/invoices"] });
        assert_eq!(missing_required_for_create("inboxes", &body), vec!["email_prefix"]);
    }

    #[test]
    fn new_inbox_with_email_prefix_passes() {
        let body = json!({ "name": "In", "email_prefix": "acme" });
        assert!(missing_required_for_create("inboxes", &body).is_empty());
    }

    #[test]
    fn null_or_blank_email_prefix_counts_as_missing() {
        // Both reach the server as "no prefix": `null` fails the same
        // non_field_errors check, and an empty string fails `blank=False`.
        for v in [json!(null), json!(""), json!("   ")] {
            let body = json!({ "name": "In", "email_prefix": v });
            assert_eq!(
                missing_required_for_create("inboxes", &body),
                vec!["email_prefix"],
                "expected {v} to count as missing"
            );
        }
    }

    #[test]
    fn other_kinds_have_no_create_requirements() {
        // The table is opt-in per kind; nothing else may be blocked offline.
        for kind in ["hooks", "queues", "schemas", "workspaces", "rules", "labels"] {
            assert!(
                required_for_create(kind).is_empty(),
                "{kind} must not gain an offline create requirement"
            );
            assert!(missing_required_for_create(kind, &json!({})).is_empty());
        }
    }

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

    /// `prompt` and `memory.index_formula` stay inline in `schema.json`.
    #[test]
    fn schema_nested_prompt_and_index_formula_are_checked() {
        let body = json!({
            "content": [{
                "category": "section",
                "id": "invoice_details",
                "children": [{
                    "category": "datapoint",
                    "id": "invoice_id",
                    "prompt": "p".repeat(5001),
                    "memory": { "index_formula": "m".repeat(2001) }
                }]
            }]
        });
        let got = check_schema_content(&body);
        assert_eq!(
            got,
            vec![
                LimitViolation {
                    field: "prompt on datapoint 'invoice_id'".to_string(),
                    limit: 5000,
                    actual: 5001,
                },
                LimitViolation {
                    field: "memory.index_formula on datapoint 'invoice_id'".to_string(),
                    limit: 2000,
                    actual: 2001,
                },
            ],
        );
    }

    /// A multivalue's `children` is a single OBJECT, not an array. Missing
    /// that descent silently skips every line-item column — the most likely
    /// way to write this walk wrong.
    #[test]
    fn schema_walk_descends_into_line_item_columns() {
        let body = json!({
            "content": [{
                "category": "section",
                "id": "line_items_section",
                "children": [{
                    "category": "multivalue",
                    "id": "line_items",
                    "children": {
                        "category": "tuple",
                        "id": "line_item",
                        "children": [{
                            "category": "datapoint",
                            "id": "item_total",
                            "formula": "f".repeat(2001)
                        }]
                    }
                }]
            }]
        });
        assert_eq!(
            check_schema_content(&body),
            vec![LimitViolation {
                field: "formula on datapoint 'item_total'".to_string(),
                limit: 2000,
                actual: 2001,
            }],
        );
    }

    /// Boundaries, and the trailing-newline case the server trims.
    #[test]
    fn schema_nested_values_at_limit_are_accepted() {
        let body = json!({
            "content": [{
                "category": "section",
                "id": "s",
                "children": [{
                    "category": "datapoint",
                    "id": "d",
                    "prompt": "p".repeat(5000),
                    "formula": format!("{}\n", "f".repeat(2000)),
                    "memory": { "index_formula": "m".repeat(2000) }
                }]
            }]
        });
        assert_eq!(check_schema_content(&body), vec![]);
    }

    /// A schema with no content, or datapoints carrying none of these keys,
    /// must produce nothing and never panic.
    #[test]
    fn schema_walk_tolerates_missing_and_non_string_values() {
        assert_eq!(check_schema_content(&json!({})), vec![]);
        assert_eq!(check_schema_content(&json!({ "content": [] })), vec![]);
        let body = json!({
            "content": [{
                "category": "datapoint",
                "id": "d",
                "prompt": 42,
                "memory": "not-an-object"
            }]
        });
        assert_eq!(check_schema_content(&body), vec![]);
    }

    /// Real rules serialize each action FLAT with a `type` discriminator —
    /// `{"id","enabled","type","event","payload"}` — not under the
    /// polymorphic wrapper the OPTIONS metadata implies. A walker written
    /// from the metadata alone would match nothing.
    #[test]
    fn rule_action_payload_content_is_checked() {
        let body = json!({
            "name": "Example Rule",
            "actions": [
                {
                    "id": "b7d5856b-7990-4c8f-8048-ca3b8e68239a",
                    "enabled": true,
                    "type": "show_message",
                    "event": "validation",
                    "payload": { "type": "warning", "content": "ok", "schema_id": "total" }
                },
                {
                    "id": "cf3e8c84-552c-482c-b1cf-333ace397a8c",
                    "enabled": true,
                    "type": "add_automation_blocker",
                    "event": "validation",
                    "payload": { "content": "c".repeat(4097), "schema_id": "total" }
                }
            ]
        });
        assert_eq!(
            check_rule_actions(&body),
            vec![LimitViolation {
                field: "actions[1] (add_automation_blocker) payload.content".to_string(),
                limit: 4096,
                actual: 4097,
            }],
        );
    }

    #[test]
    fn rule_action_content_at_limit_is_accepted() {
        let body = json!({
            "actions": [{
                "type": "show_message",
                "payload": { "content": "c".repeat(4096) }
            }]
        });
        assert_eq!(check_rule_actions(&body), vec![]);
    }

    /// Rules with no actions, actions with no payload, and non-string
    /// content must all be tolerated without panicking.
    #[test]
    fn rule_actions_walk_tolerates_missing_and_malformed() {
        assert_eq!(check_rule_actions(&json!({})), vec![]);
        assert_eq!(check_rule_actions(&json!({ "actions": [] })), vec![]);
        assert_eq!(
            check_rule_actions(&json!({ "actions": [{ "type": "custom" }] })),
            vec![]
        );
        assert_eq!(
            check_rule_actions(&json!({ "actions": [{ "payload": { "content": 7 } }] })),
            vec![]
        );
    }

    /// Same invariant as `no_validated_field_is_stripped_before_push`, for
    /// the containers the nested walks descend into. If either were ever
    /// stripped before push, the walk would reject a value that never
    /// reaches the wire — a false positive strictly worse than the 400.
    #[test]
    fn nested_walk_containers_are_not_stripped_before_push() {
        for (kind, container) in [("schemas", "content"), ("rules", "actions")] {
            let mut body = json!({ container: [] });
            crate::snapshot::create::strip_for_create(&mut body, kind);
            assert!(
                body.get(container).is_some(),
                "{kind}.{container} is walked for nested limits but stripped before push",
            );
        }
    }

    fn org(cols: serde_json::Value) -> Value {
        serde_json::json!({ "settings": { "annotation_list_table": { "columns": cols } } })
    }

    #[test]
    fn org_settings_clean_body_has_no_problems() {
        let v = org(serde_json::json!([
            { "visible": true, "column_type": "schema", "width": 120.0,
              "schema_id": "document_id", "data_type": "string" },
            { "visible": false, "column_type": "meta", "width": 80.0, "meta_name": "status" },
        ]));
        assert_eq!(check_organization_settings(&v), Vec::new());
    }

    #[test]
    fn org_settings_without_settings_key_is_not_a_problem() {
        // Nothing to validate is not an error — the push driver decides what an
        // absent `settings` means.
        let v = serde_json::json!({ "id": 1, "name": "Acme" });
        assert_eq!(check_organization_settings(&v), Vec::new());
    }

    #[test]
    fn org_settings_rejects_the_options_wrapper_shape() {
        let v = org(serde_json::json!([
            { "schema": { "visible": true, "column_type": "schema", "width": 120.0,
                          "schema_id": "document_id", "data_type": "string" } }
        ]));
        let problems = check_organization_settings(&v);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].location, "settings.annotation_list_table.columns[0]");
        assert!(
            problems[0].problem.contains("flat"),
            "the message must point at the flat form: {}",
            problems[0].problem
        );
    }

    #[test]
    fn org_settings_rejects_unknown_data_type() {
        let v = org(serde_json::json!([
            { "visible": true, "column_type": "schema", "width": 120.0,
              "schema_id": "document_id", "data_type": "bogus" }
        ]));
        let problems = check_organization_settings(&v);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].location, "settings.annotation_list_table.columns[0].data_type");
    }

    #[test]
    fn org_settings_rejects_unknown_column_type() {
        let v = org(serde_json::json!([{ "visible": true, "column_type": "sideways", "width": 1.0 }]));
        let problems = check_organization_settings(&v);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].problem.contains("schema"), "{}", problems[0].problem);
    }

    #[test]
    fn org_settings_reports_each_missing_required_key() {
        let v = org(serde_json::json!([{ "column_type": "schema" }]));
        let problems = check_organization_settings(&v);
        let locs: Vec<&str> = problems.iter().map(|p| p.problem.as_str()).collect();
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(locs.iter().all(|p| p.contains("missing required key")), "{problems:?}");
    }

    #[test]
    fn org_settings_schema_id_at_the_limit_is_accepted_and_one_over_is_not() {
        let at = "a".repeat(50);
        let over = "a".repeat(51);
        let ok = org(serde_json::json!([
            { "visible": true, "column_type": "schema", "width": 1.0, "schema_id": at, "data_type": "string" }
        ]));
        assert_eq!(check_organization_settings(&ok), Vec::new());
        let bad = org(serde_json::json!([
            { "visible": true, "column_type": "schema", "width": 1.0, "schema_id": over, "data_type": "string" }
        ]));
        let problems = check_organization_settings(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].problem.contains("50"), "{}", problems[0].problem);
    }

    #[test]
    fn org_settings_checks_the_request_dashboard_table_too() {
        let v = serde_json::json!({ "settings": { "request_dashboard_table": { "columns": [
            { "visible": true, "column_type": "schema", "width": 1.0,
              "schema_id": "document_id", "data_type": "nope" }
        ] } } });
        let problems = check_organization_settings(&v);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].location.starts_with("settings.request_dashboard_table"), "{problems:?}");
    }
}
