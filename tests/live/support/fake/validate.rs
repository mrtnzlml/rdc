//! The fake's rejections.
//!
//! Strict on purpose: a mid-run 400 that wedges a real env is one of the three
//! symptoms this whole exercise exists to make reproducible offline. A
//! permissive fake would model the state and miss the failure.

use serde_json::Value;

use super::state::{ApiError, OrgState};

/// Refuse a delete the real API refuses.
pub fn on_delete(st: &OrgState, kind: &'static str, id: u64) -> Result<(), ApiError> {
    if kind == "engines" {
        let engine_url = st.url("engines", id);
        let blocked = st
            .queues_awaiting_deletion()
            .iter()
            .any(|q| q.get("engine").and_then(Value::as_str) == Some(engine_url.as_str()));
        if blocked {
            // "after up to 24 hours" with no unbind escape hatch — see
            // `tests/live/support/teardown.rs:62`.
            return Err(ApiError::bad_request("engine_attached_to_queues_waiting_for_deletion"));
        }
    }
    Ok(())
}

/// Per-kind field length caps, keyed by the field name they apply to.
///
/// Pinned here independently of `crate::snapshot::limits::field_limits` on
/// purpose, and NOT imported from it: the fake stands in for the SERVER, and
/// an independent pin is what lets `live_field_limits_match_the_server` catch
/// the fake and `limits.rs` drifting apart — importing the same table would
/// make that test tautological. Values verified live and recorded in
/// `src/snapshot/limits.rs::field_limits`'s own doc comments; a rule's
/// `description` cap (255) is far tighter than a hook's (2000), which is why
/// this is a table and not one constant.
pub fn field_caps(kind: &str) -> &'static [(&'static str, usize)] {
    match kind {
        "hooks" => &[
            ("name", 255),
            ("description", 2000),
            ("extension_image_url", 200),
            ("read_more_url", 200),
        ],
        "queues" => &[("name", 255), ("rir_params", 255)],
        "schemas" => &[("name", 255)],
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
        "saved_views" => &[("name", 255)],
        _ => &[],
    }
}

/// Types a queue may hold exactly one of.
const UNIQUE_TEMPLATE_TYPES: &[&str] =
    &["rejection_default", "email_with_no_processable_attachments"];

/// Fields whose value is a url that must resolve to a live object of the
/// third element's kind — not just resolve to SOMETHING. A well-formed,
/// existing url of the wrong resource kind (e.g. a queue's `schema` field
/// carrying a `workspace` url) is refused, matching the real API's ref-type
/// checking; see `OrgState::resolves_kind`.
const REF_FIELDS: &[(&str, &str, &str)] = &[
    ("queues", "workspace", "workspaces"),
    ("queues", "schema", "schemas"),
    ("queues", "engine", "engines"),
    ("queues", "generic_engine", "engines"),
    ("email_templates", "queue", "queues"),
    ("labels", "organization", "organizations"),
    ("workspaces", "organization", "organizations"),
];

/// Refuse a create the real API refuses.
///
/// Wired into `create` only — **PATCH validation is a deliberate, documented
/// gap**, not an oversight. Doing it properly means validating the MERGED
/// result (this patch applied on top of the object's current state) while
/// exempting the create-only rules: a partial PATCH legitimately carries no
/// `schema` key, so rule 2 below ("a queue needs a schema") would fire
/// wrongly on a PATCH that never touches `schema`. That is a design question
/// this task does not answer. Nothing in stage 1 needs it — the only PATCHes
/// any scenario sends are a valid label colour and a hook rename — so this
/// leaves stage 2 a documented limitation instead of a silent one.
pub fn on_write(st: &OrgState, kind: &'static str, body: &Value) -> Result<(), ApiError> {
    // 1. Every ref must resolve, AND resolve to the right kind. This is the
    //    refusal rdc's whole deferred-relink path is built around
    //    (`src/snapshot/refs.rs:159`).
    for &(k, field, expected_kind) in REF_FIELDS {
        if k != kind {
            continue;
        }
        if let Some(url) = body.get(field).and_then(Value::as_str)
            && !st.resolves_kind(url, expected_kind)
        {
            return Err(ApiError::bad_request("Invalid hyperlink - No URL match"));
        }
    }
    // Unlike `REF_FIELDS` above, this is intentionally NOT scoped per kind:
    // `queues` and `run_after` mean the same thing (a list of queue urls, a
    // list of hook urls) on whichever kind carries them, so there is
    // nothing to gain from re-listing them per kind the way `REF_FIELDS`'s
    // single-url fields differ per kind.
    for (field, expected_kind) in [("queues", "queues"), ("run_after", "hooks")] {
        if let Some(list) = body.get(field).and_then(Value::as_array) {
            for v in list {
                if let Some(url) = v.as_str()
                    && !st.resolves_kind(url, expected_kind)
                {
                    return Err(ApiError::bad_request("Invalid hyperlink - No URL match"));
                }
            }
        }
    }

    // 2. A queue is created with its schema, or not at all.
    if kind == "queues" {
        if body.get("schema").and_then(Value::as_str).is_none() {
            return Err(ApiError::bad_request("schema: This field is required."));
        }
        let slots = ["engine", "generic_engine"]
            .iter()
            .filter(|f| body.get(**f).map(|v| !v.is_null()).unwrap_or(false))
            .count();
        if slots > 1 {
            return Err(ApiError::non_field(
                "Only one of engine, generic_engine may be set.",
            ));
        }
    }

    // 3. Length caps, measured after the trailing-whitespace trim the server
    //    applies before validating, in `chars()` (Unicode code points, what
    //    Django's `MaxLengthValidator` counts), not bytes. The boundary is
    //    INCLUSIVE — reject only when strictly longer than the cap — which is
    //    load-bearing, not pedantry: a label's `color` is capped at 7 and the
    //    seed fixture carries `"#ff0000"`, exactly 7 characters. rdc itself
    //    treats caps as inclusive; see
    //    `hook_description_exactly_at_limit_is_accepted` in
    //    `src/snapshot/limits.rs`.
    for &(field, cap) in field_caps(kind) {
        if let Some(s) = body.get(field).and_then(Value::as_str)
            && s.trim_end().chars().count() > cap
        {
            return Err(ApiError::bad_request(format!(
                "{field}: Ensure this field has no more than {cap} characters."
            )));
        }
    }

    // 4. A queue holds one template of each unique type, and it already has
    //    the ones the server made (`src/cli/push/email_templates.rs:97`).
    if kind == "email_templates"
        && let Some(ty) = body.get("type").and_then(Value::as_str)
        && UNIQUE_TEMPLATE_TYPES.contains(&ty)
        && let Some(queue) = body.get("queue").and_then(Value::as_str)
        && st.has_template_of_type(queue, ty)
    {
        return Err(ApiError::bad_request(format!(
            "type: a '{ty}' template already exists on this queue."
        )));
    }

    // 5. `POST /queues` validates the queue's schema against the bound
    //    engine's field NAMES. This is the refusal that forced push to create
    //    engines and their fields BEFORE queues
    //    (`src/cli/push/mod.rs:88-96`), and it bites only when the engine
    //    already exists in the target.
    if kind == "queues"
        && let Some(engine_url) = body.get("engine").and_then(Value::as_str)
        && let Some(schema_url) = body.get("schema").and_then(Value::as_str)
    {
        let known = st.engine_field_names(engine_url);
        let mut extracted = Vec::new();
        // Kind-checked on purpose, not `get_by_url`: a mismatched `schema`
        // ref must not silently walk a non-schema object and extract
        // nothing from it, which would let this rule pass vacuously even if
        // rule 1 above were ever bypassed.
        if let Some(schema) = st.get_by_url_kind(schema_url, "schemas") {
            extracted_field_ids(schema.get("content").unwrap_or(&Value::Null), &mut extracted);
        }
        if let Some(missing) = extracted.iter().find(|f| !known.contains(*f)) {
            let engine_id = engine_url.rsplit('/').next().unwrap_or("?");
            return Err(ApiError::non_field(format!(
                "Engine (id: {engine_id}) restriction: extracted field \
                 '{missing}' is not present among names of engine fields"
            )));
        }
    }

    Ok(())
}

/// Every datapoint `id` a schema's content tree extracts.
///
/// Walks the same shape `snapshot::schema::extract_formulas` walks:
/// `children` is an ARRAY for sections and tuples but a single OBJECT for a
/// multivalue's element schema, so descending into both is what covers
/// line-item columns rather than only top-level datapoints.
fn extracted_field_ids(node: &Value, out: &mut Vec<String>) {
    match node {
        Value::Array(items) => {
            for item in items {
                extracted_field_ids(item, out);
            }
        }
        Value::Object(o) => {
            if o.get("category").and_then(Value::as_str) == Some("datapoint")
                && let Some(id) = o.get("id").and_then(Value::as_str)
            {
                out.push(id.to_string());
            }
            if let Some(children) = o.get("children") {
                extracted_field_ids(children, out);
            }
        }
        _ => {}
    }
}
