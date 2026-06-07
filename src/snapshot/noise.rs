//! Server-managed JSON fields stripped from content_hash inputs.
//!
//! Rossum's API stamps fields like `modified_at` and `modifier` on every
//! server-side touch. Including them in `content_hash` produces false-positive
//! conflicts on re-pull. This module strips them at hash-computation time
//! only; the on-disk JSON keeps every field (matches API output, useful
//! in editor and dry-run previews).
//!
//! The list is intentionally a code constant. Adding a field requires a
//! one-line code change with a rationale comment.

/// Top-level and nested JSON keys removed from the canonical projection
/// before content_hash is computed.
pub const NOISE_FIELDS: &[&str] = &["modified_at", "modifier"];

/// Walk `value` and remove any object key whose name is in NOISE_FIELDS.
/// Recurses into nested objects and arrays. Mutates in place.
pub fn strip_noise_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for field in NOISE_FIELDS {
                map.remove(*field);
            }
            for (_, child) in map.iter_mut() {
                strip_noise_fields(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                strip_noise_fields(item);
            }
        }
        _ => {}
    }
}

/// Produce a canonical byte projection of `bytes` for hashing:
/// parse as JSON, strip noise fields, sort set-like URL arrays, sort keys,
/// re-serialize. Returns `bytes` unchanged if parsing fails (e.g., non-JSON
/// inputs from tests or raw formula bytes used inside combined hashes).
///
/// With `preserve_order` enabled on serde_json, `Value::Object` is an
/// IndexMap, so the bytes we serialize would reflect input order — which
/// makes the hash sensitive to key reordering. Sorting every object's
/// keys alphabetically (recursively) ensures two byte streams representing
/// the same logical content hash to the same value.
pub fn canonicalize_for_hash(bytes: &[u8], lockfile: &crate::state::Lockfile) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    // Reference-form normalization (the keystone of env-portable snapshots):
    // rewrite every cross-reference to its canonical `rdc://<kind>/<slug>` form
    // using `lockfile`, so a URL-form body (fresh from the API) and the
    // `rdc://`-form body (on disk) of the SAME object hash IDENTICALLY. This is
    // what lets portable snapshots coexist with the URL-based three-way merge
    // without portabilizing at every comparison site. An empty lockfile (e.g.
    // `Lockfile::default()` in tests, or ref-free sidecar bytes) makes this a
    // no-op, preserving prior behavior.
    crate::snapshot::refs::portabilize_value(&mut value, lockfile);
    strip_noise_fields(&mut value);
    sort_url_arrays(&mut value);
    sort_keys_recursive(&mut value);
    serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec())
}

/// Recursively sort the keys of every JSON object alphabetically. Used
/// to canonicalise for hashing so on-disk key order doesn't affect
/// `content_hash` — the Rossum API doesn't guarantee stable key order
/// across endpoints, so two byte-different bodies with the same content
/// would otherwise produce different hashes.
pub(crate) fn sort_keys_recursive(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let taken = std::mem::take(map);
            let mut entries: Vec<(String, serde_json::Value)> = taken.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, mut v) in entries {
                sort_keys_recursive(&mut v);
                map.insert(k, v);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                sort_keys_recursive(item);
            }
        }
        _ => {}
    }
}

/// Recursively sort arrays whose elements are **all absolute http(s) URLs**.
///
/// This is deliberately conservative: it only
/// reorders an array when *every* element is a URL. Rossum returns a queue's
/// server-computed back-reference arrays (`hooks`, `webhooks`, `rules`,
/// `users`, `workflows`, `queues`, `run_after`, `triggers`, …) in
/// non-deterministic per-env / per-endpoint order, so without this the queue
/// `content_hash` churns on every fetch and the object perpetually "drifts
/// from baseline". Sorting these set-like URL arrays makes the hash
/// order-insensitive (symmetrically for the recorded baseline and the drift
/// check, since both route through [`canonicalize_for_hash`]).
///
/// String arrays that are **not** all URLs are left untouched on purpose,
/// because their order is frequently meaningful: formula operands
/// (`$subtract`, `$divide`, `$gte`), `selectors`, multiline template bodies,
/// mime-type preference lists, etc. The predicate keys on element *content*
/// (is-URL), never on the array's name — `$in`, for instance, appears both as
/// a URL set and as a value set in real payloads.
pub(crate) fn sort_url_arrays(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(arr) => {
            let all_urls = !arr.is_empty()
                && arr.iter().all(|v| match v {
                    serde_json::Value::String(s) => is_url(s),
                    _ => false,
                });
            if all_urls {
                arr.sort_by(|a, b| match (a, b) {
                    (serde_json::Value::String(s1), serde_json::Value::String(s2)) => s1.cmp(s2),
                    _ => std::cmp::Ordering::Equal,
                });
            } else {
                for v in arr.iter_mut() {
                    sort_url_arrays(v);
                }
            }
        }
        serde_json::Value::Object(obj) => {
            for v in obj.values_mut() {
                sort_url_arrays(v);
            }
        }
        _ => {}
    }
}

/// An element counts as a URL when it is an absolute http(s) URL — the form a
/// raw Rossum cross-reference takes (`https://<host>/api/v1/<kind>/<id>`) — or a
/// portable `rdc://<kind>/<slug>` reference (the form refs take on disk after
/// pull converts them). Both must be recognized so set-like reference arrays
/// (`hook.queues`, `engine.training_queues`) stay order-insensitive in the hash
/// before and after conversion.
fn is_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://") || s.starts_with("rdc://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_removes_top_level_modified_at() {
        let mut v = json!({"name": "x", "modified_at": "2026-01-01T00:00:00Z"});
        strip_noise_fields(&mut v);
        assert_eq!(v, json!({"name": "x"}));
    }

    #[test]
    fn strip_removes_modifier() {
        let mut v = json!({"name": "x", "modifier": "https://x/api/v1/users/1"});
        strip_noise_fields(&mut v);
        assert_eq!(v, json!({"name": "x"}));
    }

    #[test]
    fn strip_removes_nested_modified_at() {
        let mut v = json!({
            "name": "x",
            "child": {"modified_at": "2026-01-01T00:00:00Z", "kept": true}
        });
        strip_noise_fields(&mut v);
        assert_eq!(v, json!({"name": "x", "child": {"kept": true}}));
    }

    #[test]
    fn strip_handles_array_of_objects() {
        let mut v = json!({
            "items": [
                {"id": 1, "modified_at": "t1"},
                {"id": 2, "modified_at": "t2"}
            ]
        });
        strip_noise_fields(&mut v);
        assert_eq!(v, json!({"items": [{"id": 1}, {"id": 2}]}));
    }

    #[test]
    fn strip_leaves_other_fields_alone() {
        let mut v = json!({
            "id": 42,
            "url": "https://x/api/v1/labels/42",
            "name": "Audit",
            "metadata": {"foo": "bar"}
        });
        let original = v.clone();
        strip_noise_fields(&mut v);
        assert_eq!(v, original);
    }

    #[test]
    fn strip_no_op_on_primitives_and_empty() {
        let mut v = json!(42);
        strip_noise_fields(&mut v);
        assert_eq!(v, json!(42));
        let mut v = json!({});
        strip_noise_fields(&mut v);
        assert_eq!(v, json!({}));
        let mut v = json!([]);
        strip_noise_fields(&mut v);
        assert_eq!(v, json!([]));
    }

    #[test]
    fn canonicalize_strips_modified_at() {
        let with = b"{\"name\":\"x\",\"modified_at\":\"t\"}";
        let without = b"{\"name\":\"x\"}";
        let c1 = canonicalize_for_hash(with, &crate::state::Lockfile::default());
        let c2 = canonicalize_for_hash(without, &crate::state::Lockfile::default());
        assert_eq!(c1, c2);
    }

    #[test]
    fn canonicalize_falls_back_on_non_json_bytes() {
        let raw = b"hello";
        let out = canonicalize_for_hash(raw, &crate::state::Lockfile::default());
        assert_eq!(out, raw.to_vec());
    }

    #[test]
    fn canonicalize_real_content_change_differs() {
        let a = b"{\"name\":\"foo\",\"modified_at\":\"t\"}";
        let b = b"{\"name\":\"bar\",\"modified_at\":\"t\"}";
        assert_ne!(
            canonicalize_for_hash(a, &crate::state::Lockfile::default()),
            canonicalize_for_hash(b, &crate::state::Lockfile::default())
        );
    }

    #[test]
    fn canonicalize_modifier_only_difference_collapses() {
        let a = b"{\"name\":\"x\",\"modifier\":\"u1\"}";
        let b = b"{\"name\":\"x\",\"modifier\":\"u2\"}";
        assert_eq!(
            canonicalize_for_hash(a, &crate::state::Lockfile::default()),
            canonicalize_for_hash(b, &crate::state::Lockfile::default())
        );
    }

    #[test]
    fn sort_url_arrays_sorts_only_url_arrays() {
        let mut v = json!({
            // set-like URL back-reference: must be sorted
            "hooks": ["https://x/api/v1/hooks/9", "https://x/api/v1/hooks/1"],
            // formula operands: ($unitCost - $amount); order is SEMANTIC
            "$subtract": ["$unitCost", "$amount"],
            // non-URL strings (event types): order left as-is
            "events": ["b.event", "a.event"],
            // mixed (one URL, one literal) -> NOT all-URL -> untouched
            "mixed": ["https://x/api/v1/hooks/2", "literal"],
            // empty array -> untouched
            "empty": [],
        });
        sort_url_arrays(&mut v);
        assert_eq!(
            v["hooks"],
            json!(["https://x/api/v1/hooks/1", "https://x/api/v1/hooks/9"]),
            "all-URL array must be sorted"
        );
        assert_eq!(
            v["$subtract"],
            json!(["$unitCost", "$amount"]),
            "formula operands must keep their order"
        );
        assert_eq!(
            v["events"],
            json!(["b.event", "a.event"]),
            "non-URL string array must keep its order"
        );
        assert_eq!(
            v["mixed"],
            json!(["https://x/api/v1/hooks/2", "literal"]),
            "mixed array (not all URLs) must keep its order"
        );
        assert_eq!(v["empty"], json!([]), "empty array stays empty");
    }

    #[test]
    fn sort_url_arrays_recurses_into_nested_structures() {
        let mut v = json!({
            "q": { "webhooks": ["https://x/api/v1/webhooks/5", "https://x/api/v1/webhooks/2"] },
            "list": [{ "users": ["https://x/api/v1/users/8", "https://x/api/v1/users/3"] }],
        });
        sort_url_arrays(&mut v);
        assert_eq!(
            v["q"]["webhooks"],
            json!(["https://x/api/v1/webhooks/2", "https://x/api/v1/webhooks/5"])
        );
        assert_eq!(
            v["list"][0]["users"],
            json!(["https://x/api/v1/users/3", "https://x/api/v1/users/8"])
        );
    }

    #[test]
    fn canonicalize_is_url_array_order_insensitive() {
        // The bug: a queue's `hooks`/`webhooks` back-references come back in a
        // different order each fetch, churning the content hash -> phantom drift.
        let a = br#"{"hooks":["https://x/api/v1/hooks/9","https://x/api/v1/hooks/1"]}"#;
        let b = br#"{"hooks":["https://x/api/v1/hooks/1","https://x/api/v1/hooks/9"]}"#;
        assert_eq!(
            canonicalize_for_hash(a, &crate::state::Lockfile::default()),
            canonicalize_for_hash(b, &crate::state::Lockfile::default()),
            "set-like URL back-reference order must not affect the content hash"
        );
    }

    #[test]
    fn canonicalize_is_rdc_ref_array_order_insensitive() {
        // After pull converts refs to rdc://, set-like reference arrays must
        // stay order-insensitive in the hash, exactly as raw URL arrays do.
        let a = br#"{"queues":["rdc://queues/b","rdc://queues/a"]}"#;
        let b = br#"{"queues":["rdc://queues/a","rdc://queues/b"]}"#;
        assert_eq!(
            canonicalize_for_hash(a, &crate::state::Lockfile::default()),
            canonicalize_for_hash(b, &crate::state::Lockfile::default())
        );
    }

    #[test]
    fn canonicalize_preserves_non_url_array_order() {
        // Guard against over-sorting: `$subtract` is (a - b); swapping the
        // operands is a REAL change and must stay visible in the hash.
        let a = br#"{"$subtract":["$unitCost","$amount"]}"#;
        let b = br#"{"$subtract":["$amount","$unitCost"]}"#;
        assert_ne!(
            canonicalize_for_hash(a, &crate::state::Lockfile::default()),
            canonicalize_for_hash(b, &crate::state::Lockfile::default()),
            "non-URL array order is semantic and must remain significant in the hash"
        );
    }
}
