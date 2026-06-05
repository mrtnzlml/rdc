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

/// Kinds whose object URLs are rewritten to `rdc://` on pull. A kind is
/// portable iff rdc snapshots it as a deployable object. `organization`
/// (per-env singleton) and `mdh_indexes` (no `/api/v1/` URL) are excluded;
/// their URLs stay verbatim. Non-snapshotted targets (users, hook_templates)
/// never resolve via the lockfile, so they are left alone regardless.
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

/// Recursively apply `f` to every string leaf in a JSON tree (objects and
/// array elements, at any depth). Lifted from `deploy/common.rs` so both
/// the portable-ref conversion and the (soon-removed) URL rewriter share it.
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
    lockfile.url_for_slug(kind, slug).map(|u| u.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Lockfile, ObjectEntry};

    fn lf_with(kind: &str, slug: &str, id: u64, url: &str) -> Lockfile {
        let mut lf = Lockfile::default();
        lf.upsert(
            kind,
            slug,
            ObjectEntry {
                id,
                url: Some(url.into()),
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
            parse_rdc_ref("rdc://engine_fields/mtr/code"),
            Some(("engine_fields", "mtr/code"))
        );
        assert_eq!(parse_rdc_ref("https://x.rossum.app/api/v1/queues/1"), None);
        assert_eq!(parse_rdc_ref("not a ref"), None);
        assert_eq!(parse_rdc_ref("rdc://queues"), None);
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
        let url = "https://ferguson-dev.rossum.app/api/v1/workspaces/1054061";
        let lf = lf_with("workspaces", "demo", 1054061, url);
        let rdc = url_to_rdc(url, &lf).unwrap();
        assert_eq!(rdc, "rdc://workspaces/demo");
        assert_eq!(rdc_to_url(&rdc, &lf).as_deref(), Some(url));
    }

    #[test]
    fn organization_and_unknown_urls_are_left_as_urls() {
        let org = "https://ferguson-dev.rossum.app/api/v1/organizations/418975";
        let mut lf = lf_with("organization", "self", 418975, org);
        let user = "https://ferguson-dev.rossum.app/api/v1/users/499604";
        assert_eq!(url_to_rdc(org, &lf), None);
        assert_eq!(url_to_rdc(user, &lf), None);
        lf.upsert(
            "queues",
            "invoices",
            ObjectEntry {
                id: 10,
                url: Some("https://x/api/v1/queues/10".into()),
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let mut v = serde_json::json!({
            "queue": "https://x/api/v1/queues/10",
            "organization": org,
            "actions": [{ "payload": { "queue": "https://x/api/v1/queues/10" } }],
        });
        portabilize_value(&mut v, &lf);
        assert_eq!(v["queue"], "rdc://queues/invoices");
        assert_eq!(v["actions"][0]["payload"]["queue"], "rdc://queues/invoices");
        assert_eq!(v["organization"], org);
    }
}
