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
/// parse as JSON, strip noise fields, sort keys, re-serialize. Returns
/// `bytes` unchanged if parsing fails (e.g., non-JSON inputs from tests
/// or raw formula bytes used inside combined hashes).
///
/// With `preserve_order` enabled on serde_json, `Value::Object` is an
/// IndexMap, so the bytes we serialize would reflect input order — which
/// makes the hash sensitive to key reordering. Sorting every object's
/// keys alphabetically (recursively) ensures two byte streams representing
/// the same logical content hash to the same value.
pub fn canonicalize_for_hash(bytes: &[u8]) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    strip_noise_fields(&mut value);
    sort_keys_recursive(&mut value);
    serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec())
}

/// Recursively sort the keys of every JSON object alphabetically. Used
/// to canonicalise for hashing so on-disk key order doesn't affect
/// `content_hash`. Also reused by `normalize_for_cross_env_compare` so
/// the deploy idempotency check and `rdc deploy --dry-run` previews are
/// equally insensitive to key order — the Rossum API doesn't guarantee stable
/// key order across endpoints, so two byte-different bodies with the
/// same content would otherwise diff (or trigger a spurious PATCH on
/// re-deploy).
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

/// Recursively sort every all-string array in the tree alphabetically.
/// Set-like URL arrays (a hook's `queues`, `events`, `run_after`) come
/// back from Rossum in per-env id order; sorting makes cross-env compares
/// order-insensitive. Mixed-type arrays (objects/numbers) are left alone —
/// `content[]` field order is meaningful.
pub(crate) fn sort_string_arrays(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(arr) => {
            let all_strings = arr
                .iter()
                .all(|v| matches!(v, serde_json::Value::String(_)));
            if all_strings {
                arr.sort_by(|a, b| match (a, b) {
                    (serde_json::Value::String(s1), serde_json::Value::String(s2)) => s1.cmp(s2),
                    _ => std::cmp::Ordering::Equal,
                });
            } else {
                for v in arr.iter_mut() {
                    sort_string_arrays(v);
                }
            }
        }
        serde_json::Value::Object(obj) => {
            for v in obj.values_mut() {
                sort_string_arrays(v);
            }
        }
        _ => {}
    }
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
        let c1 = canonicalize_for_hash(with);
        let c2 = canonicalize_for_hash(without);
        assert_eq!(c1, c2);
    }

    #[test]
    fn canonicalize_falls_back_on_non_json_bytes() {
        let raw = b"hello";
        let out = canonicalize_for_hash(raw);
        assert_eq!(out, raw.to_vec());
    }

    #[test]
    fn canonicalize_real_content_change_differs() {
        let a = b"{\"name\":\"foo\",\"modified_at\":\"t\"}";
        let b = b"{\"name\":\"bar\",\"modified_at\":\"t\"}";
        assert_ne!(canonicalize_for_hash(a), canonicalize_for_hash(b));
    }

    #[test]
    fn canonicalize_modifier_only_difference_collapses() {
        let a = b"{\"name\":\"x\",\"modifier\":\"u1\"}";
        let b = b"{\"name\":\"x\",\"modifier\":\"u2\"}";
        assert_eq!(canonicalize_for_hash(a), canonicalize_for_hash(b));
    }
}
