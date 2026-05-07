//! Server-managed JSON fields stripped from content_hash inputs.
//!
//! Rossum's API stamps fields like `modified_at` and `modifier` on every
//! server-side touch. Including them in `content_hash` produces false-positive
//! conflicts on re-pull. This module strips them at hash-computation time
//! only; the on-disk JSON keeps every field (matches API output, useful
//! in editor and `rdc diff`).
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
}
