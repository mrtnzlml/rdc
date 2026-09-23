//! Helpers for the resource-creation push path.
//!
//! When the push phase sees a local file with no lockfile entry, it treats it as
//! a new object and POSTs it. The POST body is the user-authored JSON minus
//! the server-managed fields. Stripping them client-side keeps the request
//! clean — the user's placeholder `id: 0` / `url: ""` (or missing fields)
//! never reach the server.

use indexmap::IndexMap;
use serde_json::Value;

/// Field names that the server assigns / computes on every kind. Always
/// stripped before POST regardless of kind.
pub(crate) const UNIVERSAL_SERVER_FIELDS: &[&str] = &[
    "id",
    "url",
    "created_at",
    "created_by",
    "modified_at",
    "modified_by",
    "status",
];

/// Field names the server computes per-kind from child relationships.
/// Stripped before POST so the request doesn't ship stale or empty
/// computed values.
fn kind_specific_strip(kind: &str) -> &'static [&'static str] {
    match kind {
        // server fills `queues` from each queue's `workspace` URL
        "workspaces" => &["queues"],
        // server fills `hooks`, `webhooks`, `rules` from each child's `queues` URL,
        // `inbox` is the back-ref from the inbox's `queues` URL, `counts` is
        // a runtime aggregate. `users` and `workflows` are likewise reverse
        // membership lists (every entry references this queue from the *other*
        // side), and a cross-env PATCH can't rewrite their src URLs reliably
        // because users/workflows aren't deployable kinds in rdc.
        //
        // `rir_url` is a server-managed, per-cluster internal RIR service URL
        // (e.g. `http://…svc.cluster.local`) that Rossum assigns; sending it
        // back 400s with "Invalid URL" (it doesn't resolve on another cluster
        // and is read-only), so strip it from POST and cross-env PATCH bodies
        // like `counts`. The server re-fills it on create.
        "queues" => &[
            "hooks",
            "webhooks",
            "rules",
            "inbox",
            "counts",
            "users",
            "workflows",
            "rir_url",
        ],
        // server fills `queues` from each queue's `schema` URL
        "schemas" => &["queues"],
        // server assigns the inbox's email address
        "inboxes" => &["email"],
        // server-managed sub-resource on hooks. `status` is a runtime
        // health field ("ready" / "failed" / etc.) that Rossum sets and
        // updates as the hook fires — read-only from the user's
        // perspective. Sending it back in PATCH/POST is at best ignored
        // and at worst 400s, so strip it like `counts` on queues.
        "hooks" => &["test", "status"],
        // `triggers` references a sub-resource kind (`/api/v1/triggers/<id>`)
        // that rdc doesn't pull or deploy; sending src trigger URLs to tgt
        // 400s with "Invalid hyperlink", so strip them. The remote keeps its
        // own triggers, which is the conservative outcome.
        "email_templates" => &["triggers"],
        // `agenda_id` is a per-env, server-generated identifier (an opaque
        // tenant-prefixed hash) that Rossum assigns when the engine is created
        // and refreshes on training cycles. It's read-only and changes often; strip it
        // from POST/PATCH bodies so cross-env deploys don't try to overwrite
        // the tgt's identifier with the src's, and so push doesn't echo a
        // value the API will ignore or reject.
        "engines" => &["agenda_id"],
        _ => &[],
    }
}

/// True if `field` is one [`strip_for_create`] removes for `kind` — a
/// server-managed field (universal self-identity like `url`, or a kind-specific
/// server-derived back-reference like a queue's `hooks`/`webhooks`/`rules`).
///
/// The deferred-relink pass uses this to never PATCH a field the create body
/// stripped: such a field only reaches the deferred set because it happened to
/// hold `rdc://` refs at create time, but the server owns it (it's populated
/// from the child side — e.g. `queue.webhooks` from each `hook.queues`). PATCHing
/// it back is at best a no-op and at worst a 400 — a queue's `webhooks` accepts
/// only `/webhooks/<id>` URLs, which the resolver never emits (it produces
/// `/hooks/<id>`), yielding "Invalid hyperlink - Incorrect URL match".
pub fn is_server_stripped(kind: &str, field: &str) -> bool {
    UNIVERSAL_SERVER_FIELDS.contains(&field) || kind_specific_strip(kind).contains(&field)
}

/// Mutate `body` to remove server-managed fields for the given kind.
/// Idempotent: calling twice is the same as once.
pub fn strip_for_create(body: &mut Value, kind: &str) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for f in UNIVERSAL_SERVER_FIELDS {
        obj.remove(*f);
    }
    for f in kind_specific_strip(kind) {
        obj.remove(*f);
    }
}

/// Per-kind fields that get *redacted* on pull — the key stays visible
/// in the on-disk JSON but the value is replaced with [`REDACTED_VALUE_SENTINEL`],
/// so noisy server-computed runtime data doesn't pollute git diffs.
///
/// Different intent from `kind_specific_strip` above: that removes a
/// key entirely from outgoing POST/PATCH bodies; this rewrites an
/// *incoming* value into a constant so the on-disk bytes are stable
/// across syncs. The two lists may overlap (a queue's `counts` and
/// `rir_url` appear in both — stripped from outbound payloads because
/// the server rejects them on PATCH, and redacted in inbound payloads
/// because they are noisy/non-portable server-set values), but they're
/// independent and the duplication is intentional.
///
/// Add a new field here when a runtime aggregate (or other server-set
/// "live" data) shows up in `git diff` noise.
///
/// `pub` so the cross-kind codec invariant test can introspect the spec and
/// assert that every codec's `disk_bytes` actually redacts these fields — the
/// systematic guard against a codec silently omitting redaction (as the hooks
/// codec did before the fix).
pub fn redact_on_pull(kind: &str) -> &'static [&'static str] {
    match kind {
        // `counts` is a runtime aggregate that churns on every document status
        // change. `rir_url` is a per-cluster, server-managed internal RIR
        // service URL (`http://…svc.cluster.local`) that Rossum assigns:
        // read-only, non-portable, and 400s if echoed back — so it's also
        // stripped from outbound bodies by `kind_specific_strip`. Redacting it
        // on pull keeps the concrete internal hostname out of tracked files
        // (a cross-env / cross-cluster leak) and stops it from producing a
        // spurious pull whenever a `migrate`d snapshot's `rir_url` disagrees
        // with the target remote's value.
        "queues" => &["counts", "rir_url"],
        // `status` is the runtime health of the hook; Rossum updates it
        // on every fire. Without redaction, every `rdc sync` rewrites
        // hooks/<slug>.json with a fresh status string and the git diff
        // is full of churn.
        "hooks" => &["status"],
        // `agenda_id` rotates on training; redact so the on-disk JSON
        // stays stable across syncs.
        "engines" => &["agenda_id"],
        _ => &[],
    }
}

/// The sentinel string that replaces redacted values on disk. Chosen
/// to be human-readable so anyone (or any agent) opening queue.json
/// sees both the field's existence and a one-line explanation, with
/// no need to consult external docs.
pub const REDACTED_VALUE_SENTINEL: &str = "<refreshed live in Rossum; not synced by rdc>";

/// Mutate `body` to redact noisy server-set fields per [`redact_on_pull`].
/// Each redacted key's value is replaced by [`REDACTED_VALUE_SENTINEL`];
/// keys that aren't present are left alone (no insertion). Idempotent.
pub fn redact_for_disk(body: &mut Value, kind: &str) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for field in redact_on_pull(kind) {
        if obj.contains_key(*field) {
            obj.insert(
                (*field).to_string(),
                Value::String(REDACTED_VALUE_SENTINEL.to_string()),
            );
        }
    }
}

/// Like `strip_for_create`, but also strips `organization` — used for
/// **cross-env PATCH bodies and cross-env idempotency comparisons**, where
/// the src snapshot's `organization` URL belongs to the src org and would
/// either be 400'd by the API or distort byte-equality against the tgt
/// remote (whose `organization` belongs to the tgt org).
///
/// Same field set as `strip_for_create` (so creates inside an env still get
/// to specify the org), plus `organization`.
pub fn strip_for_cross_env_patch(body: &mut Value, kind: &str) {
    strip_for_create(body, kind);
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    obj.remove("organization");
    // A hook's `token_owner` is a per-env user URL: each env's hooks point at
    // that env's users, which aren't a deployable kind in rdc (no cross-env
    // mapping). It always differs across envs and is never meaningful cross-env
    // drift, so strip it from cross-env comparisons — exactly like
    // `organization`. The value a hook actually keeps is controlled by `rdc
    // migrate`: when the target overlay configures one — a per-hook
    // `token_owner`, or the kind-wide `[hooks."*"] token_owner` default — that
    // owner is written into the snapshot; with no overlay entry the hook keeps
    // its as-migrated value. Either way this strip affects only the cross-env
    // *comparison*, not the value pushed.
    if kind == "hooks" {
        obj.remove("token_owner");
        // `hook_template` is a per-env store-template URL: the host is the
        // env's org subdomain (the template id itself is cluster-stable) and
        // the field is read-only — the API assigns it at `POST /hooks/create`
        // and ignores it on PATCH. Like `token_owner`, it never carries
        // cross-env drift, so strip it so a cross-env migrate restores the
        // TARGET's value instead of importing the source env's host. (The
        // store-extension install path reads `hook_template` straight off the
        // on-disk file via `build_install_body`, independent of this strip.)
        obj.remove("hook_template");
        // `guide` is read-only store-template HTML that embeds the env's
        // configurator URL (`https://<org>-<env>.rossum.app/svc/...`). Same
        // reasoning as `hook_template`: per-env and never writable, so it must
        // not cross envs.
        obj.remove("guide");
    }
    // The Rossum API treats an engine field's `name` as immutable after
    // create — `PATCH /engine_fields/<id>` with a changed `name` returns
    // 400 "You cannot change the name of an existing engine field." Strip
    // it from the cross-env compare and from cross-env PATCH bodies so a
    // slug mapping that pairs two differently-named fields (e.g.
    // `item-qty` -> `item-quantity`) doesn't trigger a doomed PATCH. The
    // name is still preserved for POSTs (strip_for_create — used by real
    // creates — does not remove it).
    if kind == "engine_fields" {
        obj.remove("name");
    }
    // A queue's `generic_engine` is a per-env URL: the host is the env's org
    // subdomain (`https://<org>-<env>.rossum.app/.../generic_engines/<id>`) while
    // the id is a cluster-stable built-in. Rossum rewrites the host to the
    // target org when the queue is stored, so a cross-env migrate that carries
    // the source host leaks it and conflicts forever (source host vs the
    // server-normalized target host). Like `organization`/`hook_template`, strip
    // it so the cross-env compare + migrate restore the TARGET's value instead of
    // importing the source env's host. Within-env create/PATCH still send it
    // (this only affects cross-env bodies + the migrate env-field set).
    if kind == "queues" {
        obj.remove("generic_engine");
    }
}

/// Partial-update analogue of [`strip_for_create`] / [`strip_for_cross_env_patch`].
///
/// The typed `update_<kind>` API endpoints take a typed struct whose
/// server-managed fields (`agenda_id`, `status`, `rir_url`, `counts`, …) land
/// in `#[serde(flatten)] extra`. CREATE bodies are stripped via the
/// Value-based [`strip_for_create`], but PATCH bodies built straight from the
/// typed struct were *not* — so the redacted sentinel (or a read-only server
/// value) was echoed back on every PATCH and at best ignored, at worst 400'd
/// (queue `rir_url` does the latter). This strips the same fields off `extra`
/// so a PATCH honours the identical contract as a CREATE.
///
/// Typed columns (`id`, `url`, `name`, and refs the caller has already remapped
/// such as queue `workspace`/`schema`) are intentionally left intact: the
/// server ignores `id`/`url` on PATCH, and remapped refs *must* be sent.
///
/// `cross_env` mirrors [`strip_for_cross_env_patch`]: it additionally drops
/// `organization` (and a hook's per-env `token_owner`).
pub fn strip_patch_extra(extra: &mut IndexMap<String, Value>, kind: &str, cross_env: bool) {
    for f in UNIVERSAL_SERVER_FIELDS {
        extra.shift_remove(*f);
    }
    for f in kind_specific_strip(kind) {
        extra.shift_remove(*f);
    }
    if cross_env {
        extra.shift_remove("organization");
        // Mirror `strip_for_cross_env_patch`: a hook's `token_owner` is a
        // per-env user URL with no cross-env mapping; `rdc migrate` writes the
        // target owner into the snapshot when the overlay configures one
        // (per-hook or `[hooks."*"]`).
        if kind == "hooks" {
            extra.shift_remove("token_owner");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_server_stripped_covers_universal_and_kind_specific() {
        // universal: self-identity `url` is stripped for every kind
        assert!(is_server_stripped("queues", "url"));
        assert!(is_server_stripped("engines", "url"));
        // kind-specific back-references on a queue
        assert!(is_server_stripped("queues", "webhooks"));
        assert!(is_server_stripped("queues", "hooks"));
        assert!(is_server_stripped("queues", "rules"));
        // a genuine cross-reference on a queue is NOT stripped (must relink)
        assert!(!is_server_stripped("queues", "engine"));
        // hooks' `queues` is a real forward ref, not stripped
        assert!(!is_server_stripped("hooks", "queues"));
    }

    #[test]
    fn cross_env_patch_strips_queue_generic_engine() {
        // A queue's `generic_engine` is a per-env host URL (Rossum rewrites the
        // host to the target org on store). Stripping it from cross-env bodies
        // lets migrate/reconcile restore the TARGET's value rather than leaking
        // the source env host, which otherwise conflicts forever.
        let mut q: Value = json!({
            "name": "Q",
            "generic_engine": "https://acme-dev.rossum.app/api/v1/generic_engines/5",
            "settings": { "x": 1 }
        });
        strip_for_cross_env_patch(&mut q, "queues");
        assert!(
            q.get("generic_engine").is_none(),
            "queue generic_engine must be stripped from cross-env bodies"
        );
        assert!(q.get("settings").is_some(), "real deployable content survives");
    }

    #[test]
    fn strip_patch_extra_removes_server_fields_like_create() {
        // engines: `agenda_id` (kind_specific) + `modified_at` (universal)
        // must go; `organization` is kept within-env, dropped cross-env; an
        // ordinary field survives untouched.
        let mut e: IndexMap<String, Value> = serde_json::from_value(json!({
            "type": "extractor",
            "agenda_id": "tnt_live_xyz",
            "modified_at": "2026-01-01T00:00:00Z",
            "organization": "https://x/api/v1/organizations/1",
        }))
        .unwrap();
        strip_patch_extra(&mut e, "engines", false);
        assert!(!e.contains_key("agenda_id"), "agenda_id must be stripped");
        assert!(
            !e.contains_key("modified_at"),
            "modified_at must be stripped"
        );
        assert!(
            e.contains_key("organization"),
            "within-env PATCH keeps organization"
        );
        assert!(e.contains_key("type"), "non-server field must survive");
        strip_patch_extra(&mut e, "engines", true);
        assert!(
            !e.contains_key("organization"),
            "cross-env PATCH drops organization"
        );

        // hooks: `status` (the field this whole change is about) must be
        // stripped in both modes; `token_owner` only cross-env.
        let mut h: IndexMap<String, Value> = serde_json::from_value(json!({
            "status": "ready",
            "token_owner": "https://x/api/v1/users/9",
        }))
        .unwrap();
        strip_patch_extra(&mut h, "hooks", false);
        assert!(
            !h.contains_key("status"),
            "hook status must be stripped from a PATCH body"
        );
        assert!(
            h.contains_key("token_owner"),
            "within-env PATCH keeps token_owner"
        );
        strip_patch_extra(&mut h, "hooks", true);
        assert!(
            !h.contains_key("token_owner"),
            "cross-env PATCH drops token_owner"
        );

        // queues: the proven-400 `rir_url` plus `counts` must be stripped.
        let mut q: IndexMap<String, Value> = serde_json::from_value(json!({
            "rir_url": "http://rir.svc.cluster.local",
            "counts": { "to_review": 1 },
        }))
        .unwrap();
        strip_patch_extra(&mut q, "queues", false);
        assert!(
            !q.contains_key("rir_url"),
            "queue rir_url must be stripped from a PATCH body"
        );
        assert!(
            !q.contains_key("counts"),
            "queue counts must be stripped from a PATCH body"
        );
    }

    #[test]
    fn strips_universal_fields() {
        let mut v = json!({
            "id": 42,
            "url": "https://x/api/v1/hooks/42",
            "name": "h",
            "type": "function",
            "events": [],
            "config": {},
            "created_at": "2026-01-01T00:00:00Z",
            "created_by": "u",
            "modified_at": "2026-01-02T00:00:00Z",
            "modified_by": "u",
            "status": "ready",
        });
        strip_for_create(&mut v, "hooks");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("id"));
        assert!(!obj.contains_key("url"));
        assert!(!obj.contains_key("created_at"));
        assert!(!obj.contains_key("created_by"));
        assert!(!obj.contains_key("modified_at"));
        assert!(!obj.contains_key("modified_by"));
        assert!(!obj.contains_key("status"));
        // User-meaningful fields preserved.
        assert!(obj.contains_key("name"));
        assert!(obj.contains_key("type"));
        assert!(obj.contains_key("events"));
        assert!(obj.contains_key("config"));
    }

    #[test]
    fn strips_kind_specific_hooks_test_and_status_fields() {
        let mut v = json!({
            "id": 0,
            "url": "",
            "name": "h",
            "test": {"some": "data"},
            "status": "ready",
        });
        strip_for_create(&mut v, "hooks");
        let obj = v.as_object().unwrap();
        assert!(
            !obj.contains_key("test"),
            "test sub-resource must be stripped"
        );
        assert!(
            !obj.contains_key("status"),
            "runtime status must be stripped (Rossum sets/updates it server-side)",
        );
    }

    #[test]
    fn strips_kind_specific_engines_agenda_id_field() {
        let mut v = json!({
            "id": 0,
            "url": "",
            "name": "e",
            "agenda_id": "tnt_abc123",
        });
        strip_for_create(&mut v, "engines");
        assert!(
            !v.as_object().unwrap().contains_key("agenda_id"),
            "engine agenda_id is per-env + read-only; must be stripped from POST/PATCH bodies",
        );
    }

    #[test]
    fn cross_env_strip_removes_hook_token_owner_but_create_keeps_it() {
        let body = json!({
            "id": 1, "url": "u", "name": "h", "type": "function",
            "token_owner": "https://x/api/v1/users/9",
        });
        // Within-env create/push must PRESERVE token_owner.
        let mut create = body.clone();
        strip_for_create(&mut create, "hooks");
        assert!(
            create.as_object().unwrap().contains_key("token_owner"),
            "within-env create must keep token_owner",
        );
        // Cross-env strip REMOVES it (per-env user URL, never cross-env drift).
        let mut cross = body.clone();
        strip_for_cross_env_patch(&mut cross, "hooks");
        assert!(
            !cross.as_object().unwrap().contains_key("token_owner"),
            "cross-env strip must remove hook token_owner",
        );
        // The token_owner strip is hooks-only — other kinds are untouched.
        let mut q = json!({ "id": 1, "url": "u", "name": "q", "token_owner": "keep-me" });
        strip_for_cross_env_patch(&mut q, "queues");
        assert!(
            q.as_object().unwrap().contains_key("token_owner"),
            "token_owner strip must be hooks-scoped",
        );
    }

    #[test]
    fn cross_env_strip_removes_hook_template_and_guide_but_create_keeps_them() {
        // A store-extension hook carries two per-env, read-only store-template
        // fields: `hook_template` (a URL whose host is the env's org subdomain;
        // the template id is cluster-stable) and `guide` (HTML embedding the
        // env's configurator URL). Both are assigned/refreshed server-side and
        // ignored on write — they never cross envs, exactly like `token_owner`.
        let body = json!({
            "id": 1, "url": "u", "name": "h", "type": "webhook",
            "extension_source": "rossum_store",
            "hook_template": "https://acme-dev.rossum.app/api/v1/hook_templates/39",
            "guide": "<div><form action=\"https://acme-dev.rossum.app/svc/x/\"></form></div>",
        });
        // Within-env create/push must PRESERVE both (same env → values valid).
        let mut create = body.clone();
        strip_for_create(&mut create, "hooks");
        let c = create.as_object().unwrap();
        assert!(
            c.contains_key("hook_template"),
            "within-env create must keep hook_template",
        );
        assert!(c.contains_key("guide"), "within-env create must keep guide");
        // Cross-env strip REMOVES both — a cross-env migrate must restore the
        // TARGET's values rather than import the source env's.
        let mut cross = body.clone();
        strip_for_cross_env_patch(&mut cross, "hooks");
        let x = cross.as_object().unwrap();
        assert!(
            !x.contains_key("hook_template"),
            "cross-env strip must remove hook_template",
        );
        assert!(
            !x.contains_key("guide"),
            "cross-env strip must remove guide",
        );
        // The strip is hooks-scoped — other kinds keep these keys untouched.
        let mut q = json!({ "id": 1, "url": "u", "name": "q", "hook_template": "x", "guide": "y" });
        strip_for_cross_env_patch(&mut q, "queues");
        let qo = q.as_object().unwrap();
        assert!(
            qo.contains_key("hook_template") && qo.contains_key("guide"),
            "hook_template/guide strip must be hooks-scoped",
        );
    }

    #[test]
    fn strips_workspace_server_fields() {
        let mut v = json!({
            "id": 1,
            "url": "u",
            "name": "ws",
            "organization": "https://x/api/v1/organizations/1",
            "queues": ["https://x/api/v1/queues/9"],
            "autopilot": true,
        });
        strip_for_create(&mut v, "workspaces");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("queues"));
        assert!(!obj.contains_key("id"));
        assert!(!obj.contains_key("url"));
        // Required user fields kept.
        assert!(obj.contains_key("name"));
        assert!(obj.contains_key("organization"));
        assert!(obj.contains_key("autopilot"));
    }

    #[test]
    fn strips_queue_computed_relationships() {
        let mut v = json!({
            "id": 0,
            "name": "q",
            "workspace": "https://x/api/v1/workspaces/1",
            "schema": "https://x/api/v1/schemas/9",
            "hooks": ["https://x/api/v1/hooks/1"],
            "webhooks": ["https://x/api/v1/webhooks/1"],
            "rules": [],
            "inbox": "https://x/api/v1/inboxes/1",
            "counts": {"to_review": 4},
            "automation_level": "never",
        });
        strip_for_create(&mut v, "queues");
        let obj = v.as_object().unwrap();
        for k in &["hooks", "webhooks", "rules", "inbox", "counts", "id"] {
            assert!(!obj.contains_key(*k), "should strip {k}");
        }
        for k in &["name", "workspace", "schema", "automation_level"] {
            assert!(obj.contains_key(*k), "should keep {k}");
        }
    }

    #[test]
    fn strips_inbox_email() {
        let mut v = json!({
            "id": 0,
            "name": "i",
            "email": "should-be-stripped@rossum.app",
            "queues": ["https://x/api/v1/queues/1"],
        });
        strip_for_create(&mut v, "inboxes");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("email"));
        assert!(obj.contains_key("queues"));
    }

    #[test]
    fn strips_queue_rir_url_on_create_and_cross_env() {
        // `rir_url` is a server-managed, per-cluster internal RIR service URL
        // ("http://…svc.cluster.local"). Sending it on POST/PATCH 400s with
        // "Invalid URL", and it's meaningless across orgs — strip it from both
        // the create body and the cross-env PATCH body.
        let body = json!({
            "id": 0,
            "name": "q",
            "workspace": "https://x/api/v1/workspaces/1",
            "schema": "https://x/api/v1/schemas/9",
            "rir_url": "http://rir.internal.svc.cluster.local",
        });
        let mut a = body.clone();
        strip_for_create(&mut a, "queues");
        assert!(
            !a.as_object().unwrap().contains_key("rir_url"),
            "create body must strip server-managed rir_url"
        );
        assert!(
            a.as_object().unwrap().contains_key("name"),
            "legit fields kept"
        );
        let mut b = body.clone();
        strip_for_cross_env_patch(&mut b, "queues");
        assert!(
            !b.as_object().unwrap().contains_key("rir_url"),
            "cross-env PATCH body must strip server-managed rir_url"
        );
    }

    #[test]
    fn idempotent() {
        let mut v = json!({"id": 1, "url": "u", "name": "x"});
        strip_for_create(&mut v, "hooks");
        let after1 = v.clone();
        strip_for_create(&mut v, "hooks");
        assert_eq!(v, after1);
    }

    #[test]
    fn unknown_kind_only_strips_universal() {
        let mut v = json!({"id": 1, "url": "u", "name": "x", "queues": ["q"]});
        strip_for_create(&mut v, "unknown_kind");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("id"));
        assert!(!obj.contains_key("url"));
        // queues kept because no kind-specific rule matched
        assert!(obj.contains_key("queues"));
    }

    #[test]
    fn redact_for_disk_replaces_queue_counts_with_sentinel() {
        let mut v = json!({
            "id": 1,
            "name": "q",
            "counts": {"importing": 5, "to_review": 2, "exported": 100},
        });
        redact_for_disk(&mut v, "queues");
        assert_eq!(
            v["counts"],
            Value::String(REDACTED_VALUE_SENTINEL.to_string())
        );
        // Other fields untouched.
        assert_eq!(v["id"], json!(1));
        assert_eq!(v["name"], json!("q"));
    }

    #[test]
    fn redact_for_disk_noop_when_counts_absent() {
        let mut v = json!({"id": 1, "name": "q"});
        let before = v.clone();
        redact_for_disk(&mut v, "queues");
        assert_eq!(v, before, "should not introduce a counts key");
    }

    #[test]
    fn redact_for_disk_noop_for_unredacted_kinds() {
        // Schemas / workspaces have no redact list; their on-disk values
        // are kept verbatim. (Hooks and engines DO redact — status and
        // agenda_id respectively — so they're excluded here.)
        let mut v = json!({"counts": {"importing": 5}, "status": "ready", "name": "x"});
        let before = v.clone();
        redact_for_disk(&mut v, "schemas");
        redact_for_disk(&mut v, "workspaces");
        assert_eq!(v, before);
    }

    #[test]
    fn redact_for_disk_replaces_hook_status_with_sentinel() {
        let mut v = json!({
            "id": 1,
            "name": "h",
            "status": "ready",
            "type": "function",
        });
        redact_for_disk(&mut v, "hooks");
        assert_eq!(
            v["status"],
            Value::String(REDACTED_VALUE_SENTINEL.to_string())
        );
        assert_eq!(v["name"], json!("h"));
        assert_eq!(v["type"], json!("function"));
    }

    #[test]
    fn redact_for_disk_replaces_engine_agenda_id_with_sentinel() {
        let mut v = json!({
            "id": 1,
            "name": "e",
            "agenda_id": "tnt_abc123",
            "type": "extractor",
        });
        redact_for_disk(&mut v, "engines");
        assert_eq!(
            v["agenda_id"],
            Value::String(REDACTED_VALUE_SENTINEL.to_string())
        );
        assert_eq!(v["name"], json!("e"));
    }

    #[test]
    fn redact_for_disk_is_idempotent() {
        let mut v = json!({"counts": {"importing": 5}, "name": "x"});
        redact_for_disk(&mut v, "queues");
        let after_first = v.clone();
        redact_for_disk(&mut v, "queues");
        assert_eq!(v, after_first);
    }

    #[test]
    fn queue_disk_bytes_make_counts_changes_invisible() {
        // Two queue snapshots that differ ONLY in the server-set `counts`
        // aggregate must serialize to identical on-disk bytes, so their content
        // hashes match and `rdc sync` never classifies live counts churn as a
        // conflict. Checked through the queues codec, which is what the pull
        // driver and the sync classifier/executor route through.
        let a = json!({
            "name": "invoices",
            "counts": {"to_review": 4, "exported": 100},
            "automation_level": "never",
        });
        let b = json!({
            "name": "invoices",
            "counts": {"to_review": 999, "exported": 0, "importing": 7},
            "automation_level": "never",
        });
        let queues = crate::snapshot::codec::codec("queues").unwrap();
        let ba = queues.disk_bytes(&a).unwrap().json;
        let bb = queues.disk_bytes(&b).unwrap().json;
        assert_eq!(
            ba, bb,
            "counts-only differences must redact to identical bytes"
        );
        assert!(String::from_utf8_lossy(&ba).contains(REDACTED_VALUE_SENTINEL));
        // Trailing newline like every on-disk snapshot file.
        assert_eq!(ba.last(), Some(&b'\n'));
    }
}
