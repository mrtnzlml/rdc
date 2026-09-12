//! The learned-facts layer: behaviors of the real Rossum API that `rdc` had to
//! discover in a live org, expressed once, executably.
//!
//! Every entry in [`QUIRKS`] carries evidence for the fact it names, in one
//! of two shapes distinguished by [`Quirk::citation`]'s own syntax — the
//! string [`Provenance`] holds, whichever of its three fields carries it:
//!
//! - A LIVE citation, `<scenario file>::<test fn>` (a double colon) — a live
//!   scenario actually asserts the fact. Checked by
//!   `every_live_citation_actually_proves_it`, which reads the cited file and
//!   confirms the cited test exists in it — not that the test proves the
//!   RIGHT thing, which is a human judgment call this guard cannot make, but
//!   at least that the citation cannot rot into a dangling reference.
//! - A SOURCE citation, `<repo file>:<line>` (a single colon) — no live
//!   scenario proves this fact; the evidence is a repo comment, a captured
//!   fixture, or (for a [`Provenance::NotModelled`] or
//!   [`Provenance::ChosenUnverified`] entry) the description of a gap or an
//!   unverified choice. Checked by `every_source_citation_names_a_real_file`,
//!   which confirms the cited file exists.
//!
//! [`Quirk::provenance`] is orthogonal to which citation shape is used above:
//! it says WHY this row exists, as one of three categories:
//!
//! - [`Provenance::Modelled`] — the fake reproduces a real API behaviour.
//!   Most quirks are this, with a LIVE citation; a `Modelled` quirk with a
//!   SOURCE citation means "the fake does this, but no live scenario proves
//!   it yet."
//! - [`Provenance::NotModelled`] — a real API fact the fake does NOT yet
//!   reproduce, recorded anyway so a known gap is visible in the same table
//!   as everything the fake gets right, rather than living only in a doc
//!   comment somewhere a reader has to already know to check. Its citation
//!   names what DOCUMENTS the fact, and is always SOURCE-shaped: an
//!   unimplemented behavior cannot have live proof of the fake's own
//!   conduct.
//! - [`Provenance::ChosenUnverified`] — the fake had to pick an answer and
//!   the real API's behaviour is UNKNOWN. Its `weighed_against` citation
//!   names whatever evidence exists for the CHOICE — explicitly NOT proof of
//!   it, and, like `NotModelled`, always SOURCE-shaped: a `::` here would
//!   claim the choice is proven, which it isn't by definition. A row MAY
//!   additionally carry `corroborated_by`, a list of LIVE citations that
//!   make the guess less arbitrary without proving it — see
//!   `an_engine_or_engine_field_carries_no_modified_at` below and
//!   [`Provenance::ChosenUnverified`]'s own doc comment. Checked by
//!   `every_corroborating_citation_names_a_real_test` for the same weak
//!   thing the LIVE check above verifies (the file and function are real),
//!   never for whether the cited test actually supports the claim — that
//!   stays a human judgment call, same as it does for `Modelled`. This
//!   distinction is not academic: this row's first version cited a real
//!   file at a real line that was simply the wrong one, which only a human
//!   reviewer caught.
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
/// implements the response-only half of quirk
/// `organization_patch_response_is_not_get_shaped` below — inserting
/// `rir_key`. Every other `(kind, method)` passes through unchanged — this
/// function is a no-op for them, not merely untested for them: `route()`
/// calls it unconditionally for every kind and method, so a rule added here
/// for one pair can never silently apply to another.
///
/// `settings` normalization is deliberately NOT here, even though it is the
/// same quirk's other documented difference — see
/// `normalize_organization_settings`'s doc comment below for why that half
/// belongs at the write path (`normalize_write`, below) instead of the
/// response seam. A first version of this seam put both halves here, which
/// a review caught: it made a PATCH response's `settings` look normalized
/// while the STORED value stayed raw, so a GET taken right after the PATCH
/// would still hand back the unnormalized shape — the opposite of what the
/// real server does.
pub fn shape_response(kind: &str, method: &str, body: &mut Value) {
    if (kind, method) == ("organizations", "PATCH") {
        insert_organization_rir_key(body);
    }
}

/// The write-path seam, symmetric with `shape_response` above: that function
/// is reached from `mod.rs::kind_response`, the ONE place `route()` builds a
/// response body; this one is reached from every place `state.rs` merges a
/// CLIENT-SENT body into stored state — `create_unchecked`, `patch`, and
/// `patch_organization` (organizations keep their own write function because
/// the org isn't stored in the generic `objects` map, but it is still a
/// write entrance and gets the same call). Before this existed, every "the
/// server does X when it writes" fact had to be hand-wired into whichever of
/// those functions discovered it first — `patch_organization`'s
/// `if k == "settings"` was exactly that. Two rules live here now:
///
/// - `("organizations", _)`: `settings` normalization
///   (`normalize_organization_settings`), moved out of `patch_organization`.
/// - `("inboxes", _)`: `email` re-derivation from `email_prefix`
///   (`derive_inbox_email`) — see that function's doc comment for why
///   `kinds::inbox_defaults` no longer derives `email` itself and keeps only
///   a narrower fallback for a body this rule has nothing to derive from.
///
/// **Not reached from `graph.rs`.** `relink`/`unlink` also update stored
/// object bodies — `add_ref`/`remove_ref` push and retract a back-ref url,
/// `set_field`/`remove_field` write or vacate a scalar field, `set_field`'s
/// `obj.insert(field.to_string(), value)` (`graph.rs:143`) being the plainest
/// case — and they do it as a side effect of somebody ELSE's write, entirely
/// outside `create_unchecked`/`patch`/`patch_organization`'s own bodies. None
/// of that goes through this seam. Today that is harmless, not by
/// coincidence but because the two write into disjoint (kind, field) space:
/// `relink`/`unlink` only ever write into a back-ref edge's TARGET kind, and
/// every such edge in `kinds::EDGES` targets `workspaces`, `schemas`, or
/// `queues` (`kinds.rs:226-235`) — never `organizations` or `inboxes`, the
/// only two kinds a rule here keys on. A future rule keyed to one of THOSE
/// three kinds (say, a rule reacting to `queues.hooks`, `queues.rules`,
/// `queues.inbox`, `workspaces.queues`, or `schemas.queues` — the exact
/// fields `add_ref`/`set_field` write) would silently miss every write
/// `relink`/`unlink` make, because nothing calls `normalize_write` from
/// `graph.rs`. Fixing that would mean adding that call to
/// `add_ref`/`remove_ref`/`set_field`/`remove_field` themselves, not adding a
/// fourth call site here — the same "one seam, not a growing set of hand-wired
/// call sites" reasoning the paragraph above already gives for why this
/// function exists at all.
///
/// Called unconditionally for every kind at every write, exactly like
/// `shape_response` is called for every response — so a rule added here for
/// one kind can never silently apply to another, and a future third rule has
/// exactly one place to be added rather than a choice of three call sites to
/// hand-wire it into (`graph.rs`'s back-ref writes aside, per the paragraph
/// above).
///
/// One structural limit, for whoever adds that third rule: unlike
/// `shape_response`, this signature carries no phase distinguisher —
/// `shape_response` gets `method` and can tell a `GET` from a `PATCH`
/// response; `normalize_write` gets only `(kind, body)`, the ALREADY-MERGED
/// result, with no way to tell a create from a patch or to see the
/// pre-merge value. Today's two rules don't need either: settings
/// normalization and email re-derivation are both pure functions of the
/// post-merge body. A rule that must act differently create-vs-patch, or
/// that needs what the body looked like BEFORE this write — e.g. a real fix
/// for quirk `id_and_url_survive_a_client_sent_patch` below, which would
/// need to know the id/url that stood in `patch`'s `slot` before the merge,
/// in order to restore them — will not fit this signature and needs a
/// different seam, not a third `match` arm here.
pub fn normalize_write(kind: &str, body: &mut Value) {
    match kind {
        "organizations" => {
            if let Some(settings) = body.get_mut("settings") {
                normalize_organization_settings(settings);
            }
        }
        "inboxes" => derive_inbox_email(body),
        _ => {}
    }
}

/// Quirk `inbox_email_is_re_derived_on_write`'s implementation:
/// `email` is server-derived from `email_prefix`
/// (`src/snapshot/limits.rs:466`), so a body that carries `email_prefix`
/// gets `email` (re-)computed from it — on create AND on every later PATCH.
/// A body with no `email_prefix` (the real API also accepts a direct
/// `email`, per the same `400 non_field_errors` this fake does not yet
/// enforce) is left untouched: this rule only ever DERIVES, it never
/// invents a fallback prefix.
///
/// A body carrying BOTH an explicit `email` and an `email_prefix` lets
/// `email_prefix` win — unconditionally overwriting whatever `email` the
/// body sent. That is deliberate, and it is a FIDELITY improvement over
/// what this fake used to do (create-time code that kept an explicit
/// `email` untouched), not an untested regression: `email` is
/// server-assigned on the real API, never client-decided, precisely
/// because it's computed FROM `email_prefix`
/// (`src/snapshot/limits.rs:466`: "`email` cannot satisfy it from rdc's
/// side... because it is server-derived"; `src/snapshot/create.rs:57`:
/// `strip_for_create` removes `email` for inboxes for the same reason). So
/// prefix-winning is what the real server would do too, given both.
///
/// This IS reachable in practice, not just a create-time corner: unlike
/// `POST`, an ordinary (non-migrate) `PATCH /inboxes/{id}` push routinely
/// sends `email` alongside `email_prefix` — `Inbox.email` is a plain
/// `String` that serializes whenever non-empty
/// (`src/cli/push/inboxes.rs:241-243`, `:363-370`). What makes prefix-wins
/// harmless rather than merely untested is a narrower fact: whenever `rdc`
/// sends `email_prefix`, any `email` it sends alongside is ALREADY
/// consistent with it — both are read off the same prior baseline. The one
/// path where `email_prefix` actually CHANGES is migrate's
/// `reconcile_email_prefix`, and that pipeline strips the source-host
/// `email` before it ever runs, so `email` ends up blank and therefore
/// OMITTED from the body entirely (`Inbox`'s
/// `skip_serializing_if = "String::is_empty"`, `src/model/inbox.rs:62-90`).
/// So this rule is reachable — a routine PATCH really does carry both
/// fields together — it just never has to arbitrate a genuine conflict
/// against what `rdc` sends today.
///
/// `kinds::inbox_defaults` still owns exactly one thing this rule
/// deliberately does not: a create-only fallback for a body sent with
/// NEITHER `email` nor `email_prefix` (a case the real API refuses, but
/// this fake doesn't enforce that yet either). That fallback calls
/// `inbox_email_for` below too, so the address FORMAT is defined in
/// exactly one place even though it now has two triggers — this rule
/// (re-derive whenever `email_prefix` is present) and that one
/// (invent `email_prefix: "inbox"` only when the body has nothing to
/// derive from at all).
fn derive_inbox_email(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else { return };
    let Some(prefix) = obj.get("email_prefix").and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    obj.insert("email".into(), json!(inbox_email_for(&prefix)));
}

/// The one formula behind a fake inbox's address, shared by
/// `derive_inbox_email` above (the write-path rule) and
/// `kinds::inbox_defaults`'s narrower create-only fallback, so the two
/// trigger conditions can never drift into computing different addresses
/// for the same prefix.
///
/// NOT a faithful reproduction of the real shape, on purpose: a real
/// address is `<email_prefix>-<hash>@<host>` (`src/snapshot/limits.rs:466`),
/// with a hash segment this fake has never modelled (a stage-1
/// simplification, predating this task). Anyone later asserting on the
/// exact *shape* of a fake inbox's address — rather than treating it as an
/// opaque string that must merely track `email_prefix` — would be testing
/// this fake's simplification, not the real server.
pub(super) fn inbox_email_for(prefix: &str) -> String {
    format!("{prefix}@fake.rossum.invalid")
}

/// Quirk `organization_patch_response_is_not_get_shaped`'s response-only
/// half: a real `PATCH /organizations/{id}` response carries `rir_key`,
/// which `GET /organizations/{id}` — before OR after that PATCH — never
/// returns. See `src/cli/push/organization.rs:161-177`.
///
/// The third documented difference in that source comment — `users`
/// returned in a different order — is deliberately NOT modelled: this
/// fake's organization always carries `users: []`
/// (`state.rs::OrgState::new`), so reversing an empty list is a no-op and an
/// assertion on it would be vacuous. Seeding synthetic users just to make
/// the reorder observable would change the organization body every pull
/// sees, for a purely cosmetic difference — and `rir_key` plus
/// `normalize_organization_settings` below already make a naive write-back
/// of a PATCH response detectably wrong on the next pull, which is the
/// property this whole exercise exists to protect.
///
/// `tests/live/scenarios/organization.rs::fake_organization_settings_push`
/// depends on this rule: its `assert_unprefixed_object_stable` check goes red
/// the moment `src/cli/push/organization.rs` writes a raw, non-GET-shaped
/// PATCH response to disk, and `rir_key` — present only here, never on a
/// GET — is what makes that body non-GET-shaped. That scenario pushes a
/// `width` and a non-empty `annotation_list_table`, so it never exercises
/// `normalize_organization_settings` below; `rir_key` alone carries the
/// scenario's entire protective property. Deleting this rule for the same
/// reason the `users` reorder above was dropped — "unmodellable, so drop
/// it" — would not fail that scenario; it would silently stop it from
/// protecting anything while it stayed green. Removing this quirk requires
/// also reckoning with that test.
fn insert_organization_rir_key(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else { return };
    // Presence is what matters, not the value.
    obj.entry("rir_key").or_insert_with(|| json!("fake-rir-key"));
}

/// Quirk `organization_patch_response_is_not_get_shaped`'s `settings` half —
/// called from `normalize_write` above, itself called from
/// `state.rs::patch_organization` AFTER it merges an incoming patch into
/// STORED state, not from `shape_response` above. This is a fact about what
/// the real server persists, not about how one response differs from what's
/// stored: `tests/cli_sync.rs:1996`'s mock comment records it directly — "A
/// real GET reflects the PATCH afterwards" with the NORMALIZED `settings`
/// already in place — so the normalized shape must be written into state at
/// merge time, or a GET taken after the PATCH would hand back the raw,
/// unnormalized value the real API never would.
///
/// Recurses through the whole `settings` subtree — an object, an array, or a
/// leaf at any depth — because the real server's normalization isn't scoped
/// to one fixed key path; it applies wherever `width` or an empty
/// `annotation_list_table` appear underneath `settings`.
pub(super) fn normalize_organization_settings(value: &mut Value) {
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
                normalize_organization_settings(v);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                normalize_organization_settings(v);
            }
        }
        _ => {}
    }
}

/// The three things a [`Quirk`] can be recording — see the module doc
/// comment for what each one means. Bundling each category's evidence INSIDE
/// its variant, rather than a separate `proven_by` field next to a
/// `modelled: bool`, makes the citation's FIELD NAME say which claim is
/// being made: `Modelled { proven_by }` says a real behaviour is proven,
/// `NotModelled { documented_at }` says a gap is documented,
/// `ChosenUnverified { weighed_against, .. }` says a guess is merely weighed
/// against something. A previous version of this registry squeezed
/// `NotModelled` and `ChosenUnverified` into one `modelled: false`, which is
/// exactly what let a `ChosenUnverified` row read as "the fake doesn't do
/// this real thing yet" when its own comment said the real thing was
/// unknown. The guards below still exist because Rust cannot check the
/// CONTENT of a `&'static str` — whether it is a live or source shape,
/// whether the file it names exists, or (`corroborated_by` specifically)
/// whether a cited test that exists actually supports the claim it's cited
/// for — only which field held it, and whether the referenced file and
/// function are real. That last gap is not hypothetical: the first version
/// of `an_engine_or_engine_field_carries_no_modified_at`'s `corroborated_by`
/// cited a real file at a real line that was simply the wrong block, and
/// separately asserted "no corroboration exists" for `engine_fields` when a
/// stronger one already did — both caught only by a human reviewer reading
/// the cited scenario, exactly the limit this paragraph is naming.
pub enum Provenance {
    /// The fake reproduces a real API behaviour. `proven_by`: see the module
    /// doc comment for the two citation shapes and which guard checks each.
    Modelled { proven_by: &'static str },
    /// A real API fact the fake does not yet reproduce. `documented_at`
    /// names what documents the fact — always a SOURCE citation (see the
    /// module doc comment for why), checked against a live-citation ban by
    /// `only_a_modelled_quirk_may_claim_a_live_citation` below.
    NotModelled { documented_at: &'static str },
    /// The fake had to pick an answer and the real API's behaviour is
    /// UNKNOWN. `weighed_against` names whatever evidence exists for the
    /// CHOICE — always a SOURCE citation, same reason and same guard as
    /// `NotModelled` — explicitly NOT a claim that the choice is correct:
    /// see the row's own doc comment for what the evidence actually shows
    /// and does not show.
    ///
    /// `corroborated_by` is a SEPARATE, optional list of LIVE citations
    /// (`<file>::<test>`, usually empty — most `ChosenUnverified` rows have
    /// none yet) that make the guess LESS ARBITRARY without proving it: a
    /// live scenario that doesn't assert the fact directly, but whose
    /// passing is only explicable if the fact holds. Checked by
    /// `every_corroborating_citation_names_a_real_test` below for the same
    /// weak thing `every_live_citation_actually_proves_it` checks for
    /// `Modelled` rows — that the file and function are real — which is
    /// deliberately NOT the same as checking that the cited test supports
    /// the claim; that half stays a human judgment call, spelled out in the
    /// row's own doc comment. Kept structurally distinct from
    /// `weighed_against` (rather than allowing `weighed_against` itself to
    /// be LIVE-shaped) so `only_a_modelled_quirk_may_claim_a_live_citation`'s
    /// ban on a `ChosenUnverified` row claiming proof stays simple: it only
    /// ever has to look at one field, and that field can never lie about
    /// being proof.
    ChosenUnverified {
        weighed_against: &'static str,
        corroborated_by: &'static [&'static str],
    },
}

pub struct Quirk {
    pub name: &'static str,
    /// Which of the three categories this row belongs to, bundled with its
    /// evidence. See [`Provenance`]'s doc comment.
    pub provenance: Provenance,
}

impl Quirk {
    /// The evidence string, whichever field its category stored it under —
    /// every guard below reads this instead of matching [`Provenance`]
    /// itself, so the citation-shape checks stay one piece of logic
    /// regardless of category. Does NOT include `ChosenUnverified`'s
    /// `corroborated_by` — that's a different kind of claim, read directly
    /// by `every_corroborating_citation_names_a_real_test` instead.
    fn citation(&self) -> &'static str {
        match self.provenance {
            Provenance::Modelled { proven_by } => proven_by,
            Provenance::NotModelled { documented_at } => documented_at,
            Provenance::ChosenUnverified { weighed_against, .. } => weighed_against,
        }
    }
}

pub const QUIRKS: &[Quirk] = &[
    Quirk {
        name: "queue_create_materializes_typed_email_template_defaults",
        provenance: Provenance::Modelled {
            proven_by: "email_templates.rs::live_email_templates_round_trip",
        },
    },
    Quirk {
        name: "queue_delete_is_async_and_cascades",
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
        provenance: Provenance::Modelled {
            proven_by: "ordering.rs::live_push_create_ordering",
        },
    },
    Quirk {
        name: "engine_delete_refused_while_a_queue_awaits_deletion",
        // NOT what `ordering.rs`'s cascade hits: `push::deletes` orders
        // engines BEFORE queues, so by the time an engine delete is
        // attempted there, its bound queue has not been asked to delete yet
        // — that is the SIBLING quirk below,
        // `engine_attached_to_active_queues`. This one only fires when an
        // engine outlives its queue's own `DELETE` — e.g. a later run's
        // `teardown.rs` best-effort sweep (`tests/live/support/teardown.rs:62`,
        // fired only after that run's queue delete already landed), which
        // asserts nothing about the outcome. No live scenario in this repo
        // exercises this branch over HTTP; the state-level proof is
        // `state.rs::an_engine_cannot_be_deleted_while_a_queue_awaits_deletion`,
        // which deletes the queue first and is not itself a `live_*` test.
        provenance: Provenance::Modelled {
            proven_by: "tests/live/support/teardown.rs:62",
        },
    },
    Quirk {
        name: "engine_attached_to_active_queues",
        // This IS what `ordering.rs`'s cascade hits: `push::deletes` orders
        // engines BEFORE queues, so the fixture queue is still fully live —
        // never asked to delete — when its engine's `DELETE` is attempted.
        // `live_push_create_ordering` proves the CONSEQUENCE `rdc` draws
        // from the refusal (warns by slug, the `expected_warning`
        // assertion; keeps the lockfile entry for retry, the `lf_after`
        // assertion) but never asserts this literal string — that exact
        // code is repo-documented, not live-asserted, at
        // `tests/live/support/teardown.rs:61`. State-level proof of the
        // fake's own rule:
        // `state.rs::an_engine_cannot_be_deleted_while_bound_to_an_active_queue`.
        provenance: Provenance::Modelled {
            proven_by: "ordering.rs::live_push_create_ordering",
        },
    },
    Quirk {
        name: "unresolvable_ref_is_an_invalid_hyperlink",
        // No live scenario provokes an unresolvable ref:
        // `cross_refs.rs::live_cross_refs` has no negative path, and the
        // only other mention of this exact string, `ordering.rs:110-112`,
        // documents why correct creation ORDER prevents the 400 from ever
        // firing live — it is never triggered, let alone asserted. The
        // evidence for the exact message is `src/snapshot/refs.rs:159`: the
        // refusal `rdc`'s whole deferred-relink path is built around.
        // `validate::on_write` matches it, and it is offline-tested at
        // `state.rs::a_ref_that_matches_no_object_is_an_invalid_hyperlink`.
        provenance: Provenance::Modelled {
            proven_by: "src/snapshot/refs.rs:159",
        },
    },
    Quirk {
        name: "over_length_field_is_refused_after_trailing_whitespace_trim",
        provenance: Provenance::Modelled {
            proven_by: "server_truth.rs::live_field_limits_match_the_server",
        },
    },
    Quirk {
        name: "queue_carries_one_engine_slot_only",
        provenance: Provenance::Modelled {
            proven_by: "server_truth.rs::live_queue_engine_slot_counts_values_not_keys",
        },
    },
    Quirk {
        name: "organization_patch_response_is_not_get_shaped",
        // Two of the three differences documented at
        // `src/cli/push/organization.rs:161-177` between a real
        // `PATCH /organizations/{id}` response and what `GET` on the same
        // id returns are now modelled, at two different layers, because
        // they are two different KINDS of fact:
        //
        // - `rir_key` is RESPONSE-only — it appears on the PATCH answer and
        //   on no GET, before or after. Modelled in `quirks::shape_response`
        //   (`insert_organization_rir_key`).
        // - `settings` normalization (`width: 140` comes back `140.0`; an
        //   empty `annotation_list_table` comes back `{ "columns": [] }`)
        //   is a STORAGE fact: the real server normalizes `settings` when
        //   it is WRITTEN, so the normalized value persists and a GET taken
        //   AFTER the PATCH returns it too. Modelled in
        //   `quirks::normalize_write` (the write-path seam, symmetric with
        //   `shape_response`), called from `state.rs::patch_organization`
        //   after it merges the patch — NOT in the response seam. An
        //   earlier version of this quirk put the normalization in the
        //   response seam, which a review caught: it made the PATCH
        //   response look normalized while the stored value stayed raw, so
        //   a GET right after the PATCH would still hand back the
        //   unnormalized shape. A later version hand-wired it as
        //   `if k == "settings"` directly inside `patch_organization`,
        //   which worked but meant every future "the server does X when it
        //   writes" fact needed its own hand-wiring; `normalize_write` is
        //   that fact's permanent home now.
        //
        // See `quirks::insert_organization_rir_key`'s doc comment for why
        // the third documented difference — `users` reordering — is
        // deliberately left unmodelled (this org's `users` is always `[]`,
        // so reversing it is vacuous).
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
        provenance: Provenance::Modelled {
            proven_by: "src/cli/push/organization.rs:161",
        },
    },
    Quirk {
        name: "inbox_email_is_re_derived_on_write",
        // `email` is server-derived from `email_prefix`
        // (`<email_prefix>-<hash>@<host>`) — `src/snapshot/limits.rs:468`
        // records this directly, and it's why `strip_for_create` removes a
        // hand-written `email` before every `POST /inboxes`. `rdc` DOES
        // PATCH `email_prefix` (migrate's `reconcile_email_prefix`), so a
        // fake that only derived `email` at create time would leave a
        // stale address after that PATCH, forever — the exact phantom-drift
        // shape this whole instrument exists to catch, and precisely why
        // this fact was named the DISCRIMINATING one when the missing
        // write-path seam was found. Modelled in `quirks::normalize_write`
        // (`derive_inbox_email`), called from both `state.rs::create_unchecked`
        // and `state.rs::patch` — the same seam the organization `settings`
        // rule above lives in.
        //
        // SOURCE, not LIVE: no live scenario patches an inbox's
        // `email_prefix` against a real org and re-reads `email` afterward
        // (a stage-2 inbox port may add one). The offline pin is
        // `an_inbox_email_is_re_derived_when_its_prefix_changes`
        // (`tests.rs`), which — like the organization quirk's offline pin
        // above — proves the FAKE's own behavior, not the real API's, so it
        // cannot serve as this quirk's citation either.
        provenance: Provenance::Modelled {
            proven_by: "src/snapshot/limits.rs:468",
        },
    },
    // --- category 2: a real API fact the fake does not yet reproduce -----
    Quirk {
        name: "inbox_patch_response_omits_fields_the_get_response_includes",
        // A real `PATCH /inboxes/{id}` response omits fields that `GET
        // /inboxes/{id}` on the same id includes — `bounce_email_to: null`
        // is the one example the source comment names, introduced with
        // "e.g.", so the full omitted-field set is not documented anywhere
        // in this repo. This is exactly why `push::inboxes::send_patch`
        // discards the PATCH response and re-baselines from a fresh GET
        // instead of trusting it: recording the PATCH-derived shape would
        // make the recorded base differ from what the next sync's
        // classifier reads off the GET, producing a spurious RemoteEdit.
        // See `src/cli/push/inboxes.rs:377` (the comment recording the
        // asymmetry) and `:725` (the offline mock test that reproduces it
        // on purpose,
        // `push_inboxes_records_the_refetched_body_not_the_patch_response`).
        //
        // NOT modelled here, on purpose. `quirks::shape_response` is called
        // from `mod.rs::kind_response` with only `(kind, method, body)` —
        // it never sees the incoming request payload — so it has no way to
        // know which fields a real partial PATCH response would have
        // omitted; and since the source only documents one example field
        // rather than an exhaustive list, shaping this now would mean
        // either inventing the rest of the set or narrowly modelling just
        // `bounce_email_to`, both of which risk the fake claiming more
        // precision than the evidence supports. Consequence for a stage-2
        // inbox port: a fake-backed inbox test will NOT reproduce this
        // asymmetry, so a test that ought to catch a naive write-back of an
        // inbox PATCH response will not catch it via the fake — only the
        // existing offline mock test above does, today.
        provenance: Provenance::NotModelled {
            documented_at: "src/cli/push/inboxes.rs:377",
        },
    },
    // --- category 3: the fake picked an answer; the real one is unknown --
    Quirk {
        name: "modified_at_does_not_move_when_a_back_reference_grows",
        // Renamed from `back_reference_growth_leaves_modified_at_unbumped`:
        // that name described the FAKE's own conduct ("[the fake] leaves
        // modified_at unbumped"), which read as a known real-API gap
        // (category 2) when it is actually a stance the fake had to invent
        // (category 3) — see the module doc comment. The name now states the
        // claim being made about the SERVER, however unverified.
        //
        // `graph.rs`'s `add_ref`/`set_field` — the functions `relink` calls
        // to grow a back-reference (e.g. a new hook pushing itself onto
        // `queue.hooks`) — touch only the target's own field, never
        // `modified_at`; only `state.rs`'s `create`/`patch`/`patch_organization`
        // stamp the clock, and only for the object THEY write, not for a
        // target that merely gained a back-ref as a side effect.
        //
        // This is `ChosenUnverified`, not `Modelled` or `NotModelled`,
        // because the real API's behavior here is UNKNOWN — nothing in this
        // repo says whether a real `queue.modified_at` moves when a hook
        // that names it is created. That is the defect this entry names: an
        // unrecorded choice, not a wrong one. The fake picked "no" silently;
        // this entry is what makes that a visible, deliberate placeholder
        // instead of a fact nobody could tell was ever decided.
        provenance: Provenance::ChosenUnverified {
            weighed_against: "tests/live/support/fake/graph.rs:82",
            corroborated_by: &[],
        },
    },
    Quirk {
        name: "id_and_url_survive_a_client_sent_patch",
        // Renamed from `patch_persists_client_sent_id_and_url`: same
        // problem as the row above — "[the fake's] patch persists" named
        // the fake's own merge function, not a claim about the server.
        //
        // `state.rs`'s `patch` is an unconditional shallow merge of every
        // key the request body carries (`dst.insert(k.clone(), v.clone())`
        // for each key, no exclusion list) — a PATCH body that happens to
        // carry `id` or `url` (both read-only, server-assigned fields)
        // would silently overwrite the stored ones. `rdc` DOES PATCH full
        // objects that carry both: `push`'s update paths serialize the
        // whole typed model, id and url included, trusting the real API to
        // ignore or reject them — but nothing in this repo pins WHICH of
        // those two the real API actually does, or confirms it does either
        // one. `ChosenUnverified`: the fake does not protect these keys, so
        // a hand-built request that sent a bogus `id` would corrupt the
        // store in a way a real org is assumed, but not shown, to refuse.
        provenance: Provenance::ChosenUnverified {
            weighed_against: "tests/live/support/fake/state.rs:280",
            corroborated_by: &[],
        },
    },
    Quirk {
        name: "an_engine_or_engine_field_carries_no_modified_at",
        // `kinds::MODELLED`'s `has_modified_at: false` for `"engines"` and
        // `"engine_fields"` (`kinds.rs`) was ORIGINALLY justified purely as
        // an internal-consistency argument with rdc's OWN drift-check code,
        // not as an observed server fact — see that field's doc comment,
        // corrected alongside this row. On its own, that argument says
        // nothing about the real server: `push::deletes::fetch_remote_modified_at`
        // (`src/cli/push/deletes.rs:481` / `:487`) unconditionally discards
        // whatever these two kinds' bodies carry (`.map(|_| None)`) when it
        // reads them back for the DELETE-time drift check, regardless of
        // what a real response would say.
        //
        // For ENGINES and ENGINE_FIELDS both, that is no longer the whole
        // story — the same asymmetry corroborates a real-server fact for
        // each, though at different strengths, because their CREATE-time
        // write-backs are not special-cased the way the delete-time reads
        // are: `push::engines` (`src/cli/push/engines.rs:111`) and
        // `push::engine_fields` (`src/cli/push/engine_fields.rs:100`) each
        // store whatever `.modified_at()` a real `POST /engines` or
        // `POST /engine_fields` response reports, `Some` or `None`, straight
        // into the lockfile. So `delete_one`'s drift comparison — remote
        // forced to `None` by the discard above, against whatever the
        // CREATE response actually put in the lockfile — only agrees (both
        // `None`, `drifted == false`, so `delete_one` falls through to
        // actually issuing the `DELETE`) if the real create response
        // carried no `modified_at` to begin with. A drifted comparison would
        // not fail the delete outright; non-interactively
        // (`resolve_delete_drift`, `src/cli/push/deletes.rs:370-381`) it
        // SKIPS the delete and warns, leaving the object very much alive.
        //
        // For ENGINES: `ordering.rs::live_push_create_ordering` asserts that
        // stderr contains the exact warning `"engines/{slug} delete failed
        // (skipped)"` — `push::deletes::run_deletes`'s catch for a
        // server-REFUSED delete, reachable only if the HTTP `DELETE` was
        // actually attempted, which by the chain above requires exactly
        // that. A green run is an OBSERVATION that a real engine's create
        // response carries no `modified_at` — CORROBORATION, not proof:
        // nothing in that scenario reads the create response's raw body
        // directly, and the same warning string could in principle be
        // produced by some other path.
        //
        // For ENGINE_FIELDS, the corroboration is DIFFERENT and stronger:
        // `engines.rs::live_engines_round_trip` creates a fresh, unbound
        // engine field, confirms it listed remotely (`engines.rs:169`),
        // deletes it through a tombstone (`sync --allow-deletes`), and
        // confirms it is NOT listed afterward (`engines.rs:180`) — a
        // directly OBSERVED successful `DELETE /engine_fields/{id}`, not an
        // inferred one. By the same drift-comparison chain above, that
        // delete could only have reached the server (rather than being
        // silently skipped on drift, which would have left the field
        // listed) if the real create response for that engine field also
        // carried no `modified_at`. Still CORROBORATION, not proof, for the
        // same reason as the engines half — but a directly observed delete
        // is stronger evidence than an inferred one from a refusal warning.
        //
        // An earlier version of this comment cited `ordering.rs:279` for an
        // "engine_fields has NO corroboration" claim. Both halves of that
        // were wrong: `ordering.rs:279` is an unrelated pre-flight
        // tombstone-widening check that skips `engine_fields` for a
        // slug/path-shape reason (its lockfile slug is compound,
        // `<engine>/<field>`, and doesn't appear verbatim in its on-disk
        // path); the "Everything that could go, went." sweep it was
        // confused with is at `ordering.rs:356` and doesn't mention
        // `engine_fields` at all, by inclusion or exclusion. And
        // `engine_fields` was never uncorroborated — `live_engines_round_trip`
        // corroborates it more directly than `ordering.rs` corroborates
        // `engines`. Caught in review, not by any guard: a citation naming
        // a real file at a real line is not the same as that line
        // supporting the claim, which is exactly why `corroborated_by`
        // below is checked only for existence, never for whether it proves
        // anything — that half stays a human's job.
        provenance: Provenance::ChosenUnverified {
            weighed_against: "src/cli/push/deletes.rs:481",
            corroborated_by: &[
                "ordering.rs::live_push_create_ordering",
                "engines.rs::live_engines_round_trip",
            ],
        },
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

/// Shared by `every_live_citation_actually_proves_it` (a `Modelled` quirk's
/// `proven_by`) and `every_corroborating_citation_names_a_real_test` (a
/// `ChosenUnverified` quirk's `corroborated_by`): confirm a LIVE citation
/// (`<file>::<test>`) names a real scenario file that really defines that
/// test function. `owner` is the quirk name, used only for the panic
/// message; `label` distinguishes which field is being checked so a failure
/// says which one.
///
/// Deliberately checks ONLY that the function exists, never that it proves
/// or corroborates the right thing — that half needs a human reading the
/// cited test, which is exactly the gap that let this row's own
/// `corroborated_by` cite a real function at a real location that turned
/// out to be the wrong one (see `Provenance`'s doc comment). A citation
/// naming real code is the mechanical half this function closes; whether
/// that code supports the claim is not mechanically checkable and is not
/// what this function is for.
fn assert_live_citation_resolves(owner: &str, label: &str, citation: &str, root: &std::path::Path) {
    let (file, test) = citation
        .split_once("::")
        .unwrap_or_else(|| panic!("quirk '{owner}' has a malformed {label} citation: {citation}"));
    assert!(
        is_plain_scenario_filename(file),
        "quirk '{owner}' cites '{file}' as its {label}, which is not a plain scenario filename \
         (no path separators, no `..`, must end in `.rs`)"
    );
    let path = root.join(file);
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("quirk '{owner}' cites '{file}' as its {label}, which cannot be read: {e}"));
    assert!(
        src.contains(&format!("fn {test}(")),
        "quirk '{owner}' cites '{test}' as its {label}, which '{file}' does not define"
    );
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
    /// Runs only on LIVE citations (`citation()` containing `::`) — see the
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
        let mut checked = 0;
        for q in QUIRKS.iter().filter(|q| q.citation().contains("::")) {
            checked += 1;
            assert_live_citation_resolves(q.name, "proven_by", q.citation(), &root);
        }
        // Every `Modelled` quirk must have a citation that resolves — this
        // loop is how a LIVE one gets checked. If nobody cited a live
        // scenario any more, this test would pass having verified nothing;
        // `QUIRKS` today carries several, so a regression to zero is a real
        // signal, not a false alarm.
        assert!(checked > 0, "no quirk claims a live citation — this guard would be checking nothing");
    }

    /// The `ChosenUnverified`-only sibling of the guard above, for
    /// `corroborated_by` rather than `proven_by`/`weighed_against`. Checks
    /// the exact same mechanical thing — the cited file exists and defines
    /// the cited test — via the same `assert_live_citation_resolves` helper,
    /// and is exactly as limited: it cannot tell whether the cited test
    /// actually corroborates the claim, only that it exists. That limit is
    /// not theoretical here. This guard was ADDED after
    /// `an_engine_or_engine_field_carries_no_modified_at`'s first version
    /// cited `ordering.rs::live_push_create_ordering` correctly (this guard
    /// would have passed) while ALSO claiming, in prose, that `engine_fields`
    /// had no corroboration at all — a claim this guard cannot check,
    /// because it isn't a citation-shape problem, it's a "did anyone verify
    /// what the prose says" problem. A human reviewer caught it; this guard
    /// exists only to make sure the citations that DO get added keep pointing
    /// at real code, so a future citation can't silently rot the way the
    /// `Modelled` guard above was written to prevent.
    #[test]
    fn every_corroborating_citation_names_a_real_test() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live/scenarios");
        let mut checked = 0;
        for q in QUIRKS.iter() {
            let Provenance::ChosenUnverified { corroborated_by, .. } = q.provenance else { continue };
            for citation in corroborated_by {
                checked += 1;
                assert!(
                    citation.contains("::"),
                    "quirk '{}' has a corroborated_by entry '{citation}' that isn't LIVE-shaped \
                     (`<file>::<test>`) — a source-only fact belongs in `weighed_against`, not here",
                    q.name
                );
                assert_live_citation_resolves(q.name, "corroborated_by", citation, &root);
            }
        }
        // Today exactly one row populates `corroborated_by` (with two
        // entries), so a naive "iterate and maybe assert" version of this
        // guard could still pass with zero real checks if that row's field
        // were ever emptied by accident — this pins that it isn't.
        assert!(
            checked > 0,
            "no quirk has a corroborated_by entry — this guard would be checking nothing"
        );
    }

    /// The other citation shape: a SOURCE citation (`<file>:<line>`, no
    /// `::`), used when no live scenario proves the fact — including every
    /// `NotModelled` or `ChosenUnverified` entry, neither of which can have
    /// live proof by definition (see `only_a_modelled_quirk_may_claim_a_live_citation`
    /// below). Weaker than the live check (there is no line-number or
    /// content verification, only that the file exists), but that asymmetry
    /// is honest: a source citation was never claiming live proof in the
    /// first place, only that a reader who follows it lands on a real file.
    #[test]
    fn every_source_citation_names_a_real_file() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut checked = 0;
        for q in QUIRKS.iter().filter(|q| !q.citation().contains("::")) {
            checked += 1;
            let (file, _loc) = q
                .citation()
                .split_once(':')
                .unwrap_or_else(|| panic!("quirk '{}' has a malformed citation: {}", q.name, q.citation()));
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
        assert!(checked > 0, "no quirk claims a source citation — this guard would be checking nothing");
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

    /// Every quirk is visible and self-consistent: only `Provenance::Modelled`
    /// may pair with a LIVE citation. `NotModelled` can't — an unimplemented
    /// behavior cannot have live proof of the fake's own conduct — and
    /// `ChosenUnverified` can't either, for the same underlying reason: a
    /// citation there is evidence WEIGHED AGAINST a choice, never proof of
    /// it, and a `::` would claim the stronger thing. This subsumes the
    /// registry's old single check (`modelled: false` never claims a live
    /// citation): both non-`Modelled` categories used to collapse into that
    /// one flag, which is exactly how a `ChosenUnverified` row got read as
    /// "the fake doesn't do this real thing yet" — see the module doc
    /// comment.
    ///
    /// Checks both non-`Modelled` categories explicitly (not just "any row
    /// that isn't `Modelled`") and counts each separately, so this cannot
    /// pass by iterating an empty set for either one — a real risk here:
    /// `QUIRKS` has exactly one `NotModelled` row today, so a filter bug
    /// that silently dropped that category would otherwise go unnoticed.
    #[test]
    fn only_a_modelled_quirk_may_claim_a_live_citation() {
        let (mut not_modelled_checked, mut chosen_unverified_checked) = (0, 0);
        for q in QUIRKS.iter() {
            match q.provenance {
                Provenance::Modelled { .. } => continue,
                Provenance::NotModelled { .. } => not_modelled_checked += 1,
                Provenance::ChosenUnverified { .. } => chosen_unverified_checked += 1,
            }
            assert!(
                !q.citation().contains("::"),
                "quirk '{}' is not Modelled but cites '{}' as if a live scenario proved it",
                q.name,
                q.citation()
            );
        }
        assert!(not_modelled_checked > 0, "no NotModelled quirk was checked — this guard covers that category vacuously");
        assert!(
            chosen_unverified_checked > 0,
            "no ChosenUnverified quirk was checked — this guard covers that category vacuously"
        );
    }

    /// Anchor for the guards above: if a future edit collapsed every row
    /// into one category (e.g. by mis-porting `provenance` during a
    /// refactor), `only_a_modelled_quirk_may_claim_a_live_citation`'s own
    /// non-vacuousness asserts would already catch a missing `NotModelled`
    /// or `ChosenUnverified` — this test names the same fact directly, at
    /// the registry level rather than inside one guard, so a reader
    /// scanning test names sees the invariant even before opening that
    /// guard's body.
    #[test]
    fn every_provenance_category_has_at_least_one_row() {
        let (mut modelled, mut not_modelled, mut chosen_unverified) = (0, 0, 0);
        for q in QUIRKS.iter() {
            match q.provenance {
                Provenance::Modelled { .. } => modelled += 1,
                Provenance::NotModelled { .. } => not_modelled += 1,
                Provenance::ChosenUnverified { .. } => chosen_unverified += 1,
            }
        }
        assert!(modelled > 0, "no Modelled quirk exists");
        assert!(not_modelled > 0, "no NotModelled quirk exists");
        assert!(chosen_unverified > 0, "no ChosenUnverified quirk exists");
    }
}
