//! The fake's rejections.
//!
//! Strict on purpose: a mid-run 400 that wedges a real env is one of the three
//! symptoms this whole exercise exists to make reproducible offline. A
//! permissive fake would model the state and miss the failure.
//!
//! ## Known-unmodelled rejections
//!
//! Declared here, the same way the PATCH-validation gap is declared on
//! `on_write` below, rather than left to be discovered by a stage-2 author
//! the hard way:
//!
//! - A schema DELETE is never refused for being referenced by a queue. The
//!   real API answers `409 conflict_referenced`
//!   (`tests/live/support/teardown.rs:36-44`, and the `assert_before`
//!   rationale at `tests/live/scenarios/ordering.rs:311-315`) while a queue
//!   still points at the schema; the fake has no such check.
//! - An `engine_fields` DELETE is never refused for being referenced by a
//!   schema either — the sibling this list lost when the engine-delete
//!   refusal above was modelled (`on_delete`'s `kind == "engines"` branch)
//!   but never gained. The real API answers `409 conflict_referenced`
//!   ("Cannot delete engine field used in a schema",
//!   `tests/live/support/teardown.rs:55-57`) while a schema still names the
//!   field; the fake has no such check. Not merely theoretical: a real
//!   `DELETE /engine_fields/{id}` for exactly this gap reaches this fake on
//!   every `fake_push_create_ordering` run, and it comes out of **rdc's own
//!   delete pass**, not out of the test harness.
//!   `tests/live/scenarios/ordering.rs` binds an engine field to its fixture
//!   queue's schema — the schema's single datapoint `id` IS the engine
//!   field's `name`, which is what buys that scenario its server-side
//!   ordering oracle — and then tombstones `envs/test/engines/<prefix>engine`
//!   as a whole directory, so the field is tombstoned along with its engine.
//!   `push::deletes::reverse_dep_order_iter` lists `engine_fields` FIRST and
//!   `schemas` TENTH, so `sync test --allow-deletes` issues
//!   `DELETE /engine_fields/{id}` while the schema naming the field is still
//!   very much alive — exactly the 409 case. `push::deletes::delete_one`'s
//!   drift gate waves it through rather than skipping it, because
//!   `KindSpec::has_modified_at` is `false` for this kind and
//!   `fetch_remote_modified_at` discards the remote value anyway, leaving
//!   both sides of the comparison `None`. The fake answers `204`.
//!   The scenario's own `Teardown` sweep of `engine_field` by name prefix
//!   (`tests/live/support/teardown.rs:68-78`) is NOT the source: by the time
//!   it runs, rdc has already deleted the field, so the prefix lookup comes
//!   back empty and the sweep issues no request at all.
//! - PATCH validation is only PARTLY modelled. [`on_patch`] applies the two
//!   rules that are decidable from the patch body alone — the length caps of
//!   [`field_caps`] and the queue engine-slot exclusion of
//!   [`check_queue_engine_slots`]. Everything else [`on_write`] checks is
//!   create-only here, and each omission is a real gap:
//!   - A PATCH's REFS are not resolved. `src/snapshot/refs.rs`'s
//!     `residual_rdc_refs` documents the refusal as a property of any body
//!     that is SENT ("the body is NOT safe to send … that opaque 400 …
//!     mid-push"), not of creates specifically, and rdc's deferred-relink
//!     path delivers refs by PATCH — so a PATCH almost certainly answers
//!     `Invalid hyperlink - No URL match` the same way. Almost: no live
//!     scenario provokes one on either verb, so this is a gap declared
//!     against repo documentation, not against an observation. The fake
//!     accepts them.
//!   - The schema-vs-engine-fields rule and the unique-typed-template rule
//!     (rules 5 and 4 of [`on_write`]) are not re-checked on a PATCH.
//!   - Nothing validates the MERGED result. Both modelled rules read only the
//!     keys the patch carries, so a partial PATCH that sets `engine` on a
//!     queue already holding a `generic_engine` is accepted here. What the
//!     real API does with that body is unknown: every live observation of the
//!     rule (`server_truth.rs::live_queue_engine_slot_counts_values_not_keys`)
//!     sends all three keys at once, which is also the only shape rdc's push
//!     ever sends — see quirk `queue_engine_slots_are_counted_on_the_patch_body`.

use serde_json::Value;

use super::kinds::{self, RefShape};
use super::state::{ApiError, OrgState};

/// Refuse a delete the real API refuses.
pub fn on_delete(st: &OrgState, kind: &'static str, id: u64) -> Result<(), ApiError> {
    if kind == "engines" {
        let engine_url = st.url("engines", id);
        // Two distinct refusals, told apart by whether the bound queue has
        // already been asked to delete (`tests/live/support/teardown.rs:59-61`):
        // a DRAINING queue (answered its own `202`, not yet actually gone)
        // refuses with "waiting_for_deletion"; a still-LIVE queue refuses
        // with "active_queues". Checked in that order because `push::deletes`
        // cascades engines BEFORE queues — an engine bound to this run's
        // queue is refused with the ACTIVE message every time this fires
        // from that cascade, since the queue's own `DELETE` hasn't landed
        // yet. The draining branch exists for the case an engine is deleted
        // AFTER its queue already got a `202` (e.g. a later run's janitor
        // sweep, `teardown.rs`'s own engine cleanup).
        if st
            .queues_awaiting_deletion()
            .iter()
            .any(|q| q.get("engine").and_then(Value::as_str) == Some(engine_url.as_str()))
        {
            // "after up to 24 hours" with no unbind escape hatch — see
            // `tests/live/support/teardown.rs:62`.
            return Err(ApiError::bad_request("engine_attached_to_queues_waiting_for_deletion"));
        }
        if !st.queues_bound_to_engine(&engine_url).is_empty() {
            return Err(ApiError::bad_request("engine_attached_to_active_queues"));
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

/// The three mutually exclusive fields a queue names its extraction engine
/// through.
///
/// Pinned here and NOT imported from
/// `crate::snapshot::limits::QUEUE_ENGINE_FIELDS`, for exactly the reason
/// [`field_caps`] is not imported from `field_limits`: the fake stands in for
/// the SERVER, and a shared constant would make any test of this rule compare
/// rdc's belief with itself. The names are quoted from the refusal
/// `src/cli/migrate/mod.rs`'s `reconcile_engine_slot` documents.
const QUEUE_ENGINE_SLOTS: [&str; 3] = ["engine", "dedicated_engine", "generic_engine"];

/// Refuse a queue body that binds more than one engine.
///
/// Counts VALUES, not KEYS: a body carrying all three keys with two of them
/// explicitly `null` binds one engine and is accepted. That asymmetry is
/// load-bearing for rdc twice over — `cli::migrate::reconcile_engine_slot`
/// clears a losing binding by nulling it rather than removing the key, and
/// every within-env queue PATCH re-serializes the whole on-disk body, which
/// always carries all three keys. A fake that counted keys would refuse both.
///
/// The message is the one `src/cli/migrate/mod.rs`'s `reconcile_engine_slot`
/// and `src/snapshot/limits.rs`'s `QUEUE_ENGINE_FIELDS` both quote verbatim
/// from the real API.
fn check_queue_engine_slots(body: &Value) -> Result<(), ApiError> {
    let bound = QUEUE_ENGINE_SLOTS
        .iter()
        .filter(|f| body.get(**f).is_some_and(|v| !v.is_null()))
        .count();
    if bound > 1 {
        return Err(ApiError::non_field(
            "Only one of dedicated_engine, generic_engine or engine can be set.",
        ));
    }
    Ok(())
}

/// Refuse a body whose string fields outrun [`field_caps`].
///
/// Measured after the trailing-whitespace trim the server applies before
/// validating, in `chars()` (Unicode code points, what Django's
/// `MaxLengthValidator` counts), not bytes. The boundary is INCLUSIVE —
/// reject only when strictly longer than the cap — which is load-bearing, not
/// pedantry: a label's `color` is capped at 7 and the seed fixture carries
/// `"#ff0000"`, exactly 7 characters. rdc itself treats caps as inclusive; see
/// `hook_description_exactly_at_limit_is_accepted` in `src/snapshot/limits.rs`.
///
/// Shared by [`on_write`] and [`on_patch`] so the trim-then-count rule cannot
/// drift between the two paths.
fn check_field_caps(kind: &str, body: &Value) -> Result<(), ApiError> {
    for &(field, cap) in field_caps(kind) {
        if let Some(s) = body.get(field).and_then(Value::as_str)
            && s.trim_end().chars().count() > cap
        {
            return Err(ApiError::bad_request(format!(
                "{field}: Ensure this field has no more than {cap} characters."
            )));
        }
    }
    Ok(())
}

/// Refuse a PATCH the real API refuses.
///
/// A deliberate SUBSET of [`on_write`]: only the rules decidable from the
/// patch body alone, because a partial PATCH legitimately omits most keys and
/// re-running the create-only rules against it would refuse bodies the server
/// takes (rule 2's "a queue needs a schema" would fire on every PATCH that
/// doesn't touch `schema`). The module doc comment lists what is therefore
/// still unmodelled here and what the real API does instead.
///
/// Takes no `OrgState`: neither modelled rule consults the object's current
/// state, which is itself the modelling choice recorded as quirk
/// `queue_engine_slots_are_counted_on_the_patch_body`.
pub fn on_patch(kind: &'static str, body: &Value) -> Result<(), ApiError> {
    if kind == "queues" {
        check_queue_engine_slots(body)?;
    }
    check_field_caps(kind, body)?;
    Ok(())
}

/// Refuse a create the real API refuses.
///
/// Wired into `create` only. The PATCH path has its own, narrower entry
/// point, [`on_patch`], rather than reusing this one, because most of what is
/// checked below is CREATE-only by construction: a partial PATCH legitimately
/// carries no `schema` key, so rule 2 ("a queue needs a schema") would fire
/// wrongly on a PATCH that never touches `schema`. Doing it in full means
/// validating the MERGED result — this patch applied on top of the object's
/// current state — which is a design question neither function answers; what
/// PATCH does and does not check today, and what the real API is known to do
/// instead, is listed in the module doc comment.
pub fn on_write(st: &OrgState, kind: &'static str, body: &Value) -> Result<(), ApiError> {
    // 1. Every ref must resolve, AND resolve to the right kind. This is the
    //    refusal rdc's whole deferred-relink path is built around
    //    (`src/snapshot/refs.rs:159`). Walks `kinds::EDGES`: an edge with an
    //    owner is checked only against a body of that kind; a universal edge
    //    (no owner) is checked against every kind, because `queues` and
    //    `run_after` mean the same thing (a list of queue urls, a list of
    //    hook urls) on whichever kind carries them — narrowing them to
    //    declared owners would silently stop validating refs on a kind with
    //    no row of its own.
    for edge in kinds::EDGES.iter().filter(|e| e.owner.is_none_or(|o| o == kind)) {
        match edge.shape {
            RefShape::Single => {
                if let Some(url) = body.get(edge.field).and_then(Value::as_str)
                    && !st.resolves_kind(url, edge.target)
                {
                    return Err(ApiError::bad_request("Invalid hyperlink - No URL match"));
                }
            }
            RefShape::Array => {
                if let Some(list) = body.get(edge.field).and_then(Value::as_array) {
                    for v in list {
                        if let Some(url) = v.as_str()
                            && !st.resolves_kind(url, edge.target)
                        {
                            return Err(ApiError::bad_request("Invalid hyperlink - No URL match"));
                        }
                    }
                }
            }
        }
    }

    // 2. A queue is created with its schema, or not at all — and binds at
    //    most one engine ([`check_queue_engine_slots`], shared with
    //    [`on_patch`]).
    if kind == "queues" {
        if body.get("schema").and_then(Value::as_str).is_none() {
            return Err(ApiError::bad_request("schema: This field is required."));
        }
        check_queue_engine_slots(body)?;
    }

    // 3. Length caps ([`check_field_caps`], shared with [`on_patch`]).
    check_field_caps(kind, body)?;

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
