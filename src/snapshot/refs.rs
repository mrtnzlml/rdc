//! The `rdc://<kind>/<slug>` portable-reference scheme.
//!
//! On disk, an internal cross-reference (e.g. `queue.workspace`) is stored as
//! `rdc://<kind>/<slug>` instead of a live API URL, making the snapshot
//! environment-agnostic. `<kind>/<slug>` is exactly the lockfile coordinate
//! `objects[kind][slug]`. Resolution is purely mechanical and needs no
//! per-field schema: walk every string, act on any `rdc://…`.

use crate::state::Lockfile;
use serde_json::Value;

/// `rdc://` URI prefix.
pub const RDC_SCHEME: &str = "rdc://";

/// Returns `true` for every kind whose URLs are rewritten to portable `rdc://`
/// refs on pull. The two excluded kinds are `organization` (per-env singleton)
/// and `mdh_indexes` (no `/api/v1/` URL); their URLs stay verbatim. All other
/// kinds are portable. Non-snapshotted targets (users, hook_templates) never
/// resolve via the lockfile, so they are left alone regardless.
pub fn is_portable_kind(kind: &str) -> bool {
    !matches!(kind, "organization" | "mdh_indexes")
}

/// Parse `rdc://<kind>/<slug>` into `(kind, slug)`. `kind` is the first path
/// segment; `slug` is the remainder (may itself contain `/` for composite
/// keys like `engine_fields`/`email_templates`). Returns `None` for any
/// string that is not a well-formed `rdc://` ref.
pub fn parse_rdc_ref(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix(RDC_SCHEME)?;
    let (kind, slug) = rest.split_once('/')?;
    if kind.is_empty() || slug.is_empty() {
        return None;
    }
    Some((kind, slug))
}

/// Recursively apply `f` to every string leaf in a JSON tree (object values
/// and array elements, at any depth). Object keys are not visited. Shared by the
/// portable-ref conversion here and `deploy/common.rs`'s URL rewriter.
pub fn walk_strings_mut(value: &mut Value, f: &mut dyn FnMut(&mut String)) {
    match value {
        Value::String(s) => f(s),
        Value::Array(items) => {
            for item in items {
                walk_strings_mut(item, f);
            }
        }
        Value::Object(map) => {
            for (_k, v) in map.iter_mut() {
                walk_strings_mut(v, f);
            }
        }
        _ => {}
    }
}

/// URL → `rdc://<kind>/<slug>` if the URL belongs to a portable kind tracked
/// in `lockfile`. Returns `None` (leave unchanged) otherwise — externals,
/// `organization`, and unknown URLs all fall here.
pub fn url_to_rdc(url: &str, lockfile: &Lockfile) -> Option<String> {
    let (kind, slug) = lockfile.lookup_url(url)?;
    if !is_portable_kind(kind) {
        return None;
    }
    Some(format!("{RDC_SCHEME}{kind}/{slug}"))
}

/// `rdc://<kind>/<slug>` → the env URL for that object, via the lockfile.
/// Returns `None` if the string is not an `rdc://` ref or the slug is not in
/// the lockfile (a dangling ref — left as-is so the API surfaces a clear error).
pub fn rdc_to_url(s: &str, lockfile: &Lockfile) -> Option<String> {
    let (kind, slug) = parse_rdc_ref(s)?;
    lockfile.url_for_slug(kind, slug)
}

/// Pull side: rewrite every portable-kind URL in `value` to `rdc://` form.
pub fn portabilize_value(value: &mut Value, lockfile: &Lockfile) {
    walk_strings_mut(value, &mut |s| {
        if let Some(rdc) = url_to_rdc(s, lockfile) {
            *s = rdc;
        }
    });
}

/// Push side: resolve every `rdc://` ref in `value` to the env URL.
pub fn resolve_value(value: &mut Value, lockfile: &Lockfile) {
    walk_strings_mut(value, &mut |s| {
        if let Some(url) = rdc_to_url(s, lockfile) {
            *s = url;
        }
    });
}

/// Push side, two-phase. Resolve every `rdc://` ref that CAN be resolved
/// (rewriting it to an env URL in place), then DEFER every **top-level object
/// field** whose value still contains a residual `rdc://` ref — remove it from
/// `value` and return `(field_name, original_value)` so the caller can PATCH it
/// later, once the referenced object exists. The returned value is the ORIGINAL
/// (pre-resolution) field so re-resolving it later is straightforward.
/// Non-object inputs defer nothing.
pub fn resolve_value_deferring(value: &mut Value, lockfile: &Lockfile) -> Vec<(String, Value)> {
    let Some(obj) = value.as_object() else {
        resolve_value(value, lockfile);
        return Vec::new();
    };
    // Snapshot the keys that have any rdc:// refs so we can save the originals.
    let candidate_keys: Vec<String> = obj
        .iter()
        .filter(|(_, v)| !residual_rdc_refs(v).is_empty())
        .map(|(k, _)| k.clone())
        .collect();
    // Save originals BEFORE resolution (caller re-resolves them later).
    let originals: Vec<(String, Value)> = candidate_keys
        .iter()
        .filter_map(|k| obj.get(k).map(|v| (k.clone(), v.clone())))
        .collect();
    // Resolve the whole body in place.
    resolve_value(value, lockfile);
    // Now check which fields STILL have residual refs after resolution and defer them.
    let obj = value.as_object_mut().expect("checked above");
    let mut deferred = Vec::new();
    for (k, orig) in originals {
        if let Some(resolved_v) = obj.get(&k)
            && !residual_rdc_refs(resolved_v).is_empty()
        {
            obj.remove(&k);
            deferred.push((k, orig));
        }
    }
    deferred
}

/// Recursively apply `f` to every string leaf in a JSON tree, read-only.
/// The immutable mirror of [`walk_strings_mut`] (object keys are not visited).
pub fn walk_strings(value: &Value, f: &mut dyn FnMut(&str)) {
    match value {
        Value::String(s) => f(s),
        Value::Array(items) => {
            for item in items {
                walk_strings(item, f);
            }
        }
        Value::Object(map) => {
            for (_k, v) in map.iter() {
                walk_strings(v, f);
            }
        }
        _ => {}
    }
}

/// Collect every `rdc://` portable reference still present in `value`, sorted
/// and de-duplicated. After [`resolve_value`] has run, the only refs that
/// remain are *dangling* — their target slug isn't in the lockfile, so they
/// could not be rewritten to an env URL. A non-empty result means the body is
/// NOT safe to send: the Rossum API parses an `rdc://…` value as a URL whose
/// path matches no object and rejects it with `"Invalid hyperlink - No URL
/// match."`. Callers use this to fail loud (naming the ref) instead of letting
/// that opaque 400 surface mid-push.
pub fn residual_rdc_refs(value: &Value) -> Vec<String> {
    let mut refs = Vec::new();
    walk_strings(value, &mut |s| {
        if parse_rdc_ref(s).is_some() {
            refs.push(s.to_string());
        }
    });
    refs.sort();
    refs.dedup();
    refs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Lockfile, ObjectEntry};

    /// Build a single-entry lockfile whose `api_base` is set so derived URLs
    /// resolve. The URL is no longer stored — it is `{api_base}/{kind}/{id}`
    /// (or `/organizations/{id}` for `organization`).
    fn lf_with(api_base: &str, kind: &str, slug: &str, id: u64) -> Lockfile {
        let mut lf = Lockfile {
            api_base: api_base.to_string(),
            ..Lockfile::default()
        };
        lf.upsert(
            kind,
            slug,
            ObjectEntry {
                id,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lf
    }

    #[test]
    fn parse_rdc_ref_splits_kind_and_slug() {
        assert_eq!(
            parse_rdc_ref("rdc://queues/invoices"),
            Some(("queues", "invoices"))
        );
        assert_eq!(
            parse_rdc_ref("rdc://engine_fields/extractor/code"),
            Some(("engine_fields", "extractor/code"))
        );
        assert_eq!(parse_rdc_ref("https://x.rossum.app/api/v1/queues/1"), None);
        assert_eq!(parse_rdc_ref("not a ref"), None);
        assert_eq!(parse_rdc_ref("rdc://queues"), None); // no slug
        assert_eq!(parse_rdc_ref("rdc:///slug"), None); // empty kind
    }

    #[test]
    fn portable_kinds_exclude_externals() {
        assert!(is_portable_kind("queues"));
        assert!(is_portable_kind("workspaces"));
        assert!(is_portable_kind("hooks"));
        assert!(!is_portable_kind("organization"));
        assert!(!is_portable_kind("mdh_indexes"));
    }

    #[test]
    fn url_round_trips_through_rdc() {
        let api_base = "https://example.rossum.app/api/v1";
        let url = "https://example.rossum.app/api/v1/workspaces/1054061";
        let lf = lf_with(api_base, "workspaces", "demo", 1054061);
        let rdc = url_to_rdc(url, &lf).unwrap();
        assert_eq!(rdc, "rdc://workspaces/demo");
        assert_eq!(rdc_to_url(&rdc, &lf).as_deref(), Some(url));
    }

    #[test]
    fn organization_and_unknown_urls_are_left_as_urls() {
        let api_base = "https://example.rossum.app/api/v1";
        let org = "https://example.rossum.app/api/v1/organizations/418975";
        let mut lf = lf_with(api_base, "organization", "self", 418975);
        let user = "https://example.rossum.app/api/v1/users/499604";
        assert_eq!(url_to_rdc(org, &lf), None);
        assert_eq!(url_to_rdc(user, &lf), None);
        lf.upsert(
            "queues",
            "invoices",
            ObjectEntry {
                id: 10,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let queue_url = "https://example.rossum.app/api/v1/queues/10";
        let mut v = serde_json::json!({
            "queue": queue_url,
            "organization": org,
            "actions": [{ "payload": { "queue": queue_url } }],
        });
        portabilize_value(&mut v, &lf);
        assert_eq!(v["queue"], "rdc://queues/invoices");
        assert_eq!(v["actions"][0]["payload"]["queue"], "rdc://queues/invoices");
        assert_eq!(v["organization"], org);
    }

    #[test]
    fn resolve_value_rewrites_rdc_refs_and_leaves_dangling_intact() {
        let api_base = "https://example.rossum.app/api/v1";
        let url = "https://example.rossum.app/api/v1/queues/10";
        let lf = lf_with(api_base, "queues", "invoices", 10);
        let mut v = serde_json::json!({
            "queue": "rdc://queues/invoices",
            "other": "rdc://queues/unknown-slug", // dangling — must survive
            "plain": "https://example.com/",
        });
        resolve_value(&mut v, &lf);
        assert_eq!(v["queue"], url);
        assert_eq!(v["other"], "rdc://queues/unknown-slug");
        assert_eq!(v["plain"], "https://example.com/");
    }

    #[test]
    fn residual_rdc_refs_finds_every_unresolved_ref_deduped_and_sorted() {
        // A body that has already been through `resolve_value`: resolvable
        // refs are now env URLs, only the dangling ones survive as `rdc://`.
        let v = serde_json::json!({
            "engine": "rdc://engines/1-inbox-sorting",          // dangling
            "schema": "https://example.rossum.app/api/v1/schemas/5", // resolved
            "training_queues": [
                "https://example.rossum.app/api/v1/queues/777325",   // resolved
                "rdc://queues/2-pipe-and-fitting-legacy",        // dangling
                "rdc://queues/2-pipe-and-fitting-legacy",        // dup of above
            ],
            "plain": "not a ref",
            "nested": { "deep": "rdc://engines/1-inbox-sorting" } // dup, nested
        });
        assert_eq!(
            residual_rdc_refs(&v),
            vec![
                "rdc://engines/1-inbox-sorting".to_string(),
                "rdc://queues/2-pipe-and-fitting-legacy".to_string(),
            ],
        );
    }

    #[test]
    fn residual_rdc_refs_empty_for_fully_resolved_body() {
        let v = serde_json::json!({
            "engine": "https://example.rossum.app/api/v1/engines/383",
            "name": "Inbox & Sorting",
            "settings": { "x": 1 },
        });
        assert!(residual_rdc_refs(&v).is_empty());
    }

    #[test]
    fn resolve_value_deferring_defers_unresolved_top_level_fields_only() {
        let api_base = "https://example.rossum.app/api/v1";
        let lf = lf_with(api_base, "queues", "invoices", 10);
        let mut body = serde_json::json!({
            "name": "Q",
            "workspace": "rdc://queues/invoices",            // resolvable -> stays, rewritten
            "engine": "rdc://engines/1-inbox-sorting",    // dangling -> deferred + removed
        });
        let deferred = resolve_value_deferring(&mut body, &lf);
        assert_eq!(body["workspace"], format!("{api_base}/queues/10"));
        assert!(body.get("engine").is_none(), "deferred field must be removed: {body}");
        assert_eq!(deferred, vec![("engine".to_string(),
            serde_json::json!("rdc://engines/1-inbox-sorting"))]);
    }

    #[test]
    fn resolve_value_deferring_defers_array_field_with_any_unresolved_member() {
        let api_base = "https://example.rossum.app/api/v1";
        let lf = lf_with(api_base, "queues", "invoices", 10);
        let mut body = serde_json::json!({
            "training_queues": ["rdc://queues/invoices", "rdc://queues/missing"],
        });
        let deferred = resolve_value_deferring(&mut body, &lf);
        assert!(body.get("training_queues").is_none());
        assert_eq!(deferred, vec![("training_queues".to_string(),
            serde_json::json!(["rdc://queues/invoices", "rdc://queues/missing"]))]);
    }

    #[test]
    fn resolve_value_deferring_defers_nothing_when_fully_resolvable() {
        let api_base = "https://example.rossum.app/api/v1";
        let lf = lf_with(api_base, "queues", "invoices", 10);
        let mut body = serde_json::json!({ "workspace": "rdc://queues/invoices" });
        let deferred = resolve_value_deferring(&mut body, &lf);
        assert!(deferred.is_empty());
        assert_eq!(body["workspace"], format!("{api_base}/queues/10"));
    }
}
