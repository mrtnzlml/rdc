//! On-disk JSON presentation: key ordering AND field hiding.
//!
//! Two concerns share this module because both are pure cosmetic
//! transforms applied at write time to make the on-disk files easier
//! for users to read and to diff:
//!
//! * **Key ordering** — `reorder_top_level` moves "important" keys
//!   (per-kind constants like [`HOOK_KEY_ORDER`]) to the front of the
//!   top-level object.
//! * **Field hiding** — [`strip_hidden_fields`] removes
//!   server-managed fields ([`HIDDEN_FIELDS`]) so they don't churn the
//!   on-disk JSON. `modified_at` and `modified_by` are hidden (both
//!   already tracked in the lockfile, as `ObjectEntry::modified_at` /
//!   `ObjectEntry::modified_by`).
//!
//! Requires the `preserve_order` feature on `serde_json` so
//! `Value::Object` is an `IndexMap` (insertion-order preserving). With
//! that, calling `serde_json::to_vec_pretty` over a transformed object
//! emits keys in the order this module imposed.
//!
//! Hash invariance: `content_hash` canonicalises through
//! `snapshot::noise::canonicalize_for_hash`, which sorts all keys
//! alphabetically AND strips noise fields before hashing. Reordering
//! and hiding fields on disk therefore does NOT produce drift on the
//! next sync.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

/// Server-managed keys removed from on-disk JSON before write. Both are
/// preserved in the lockfile (`ObjectEntry::modified_at` /
/// `ObjectEntry::modified_by`), so dropping them from the JSON loses no
/// information.
///
/// **Top-level only, deliberately.** An earlier version recursed, which is a
/// silent-corruption path in two places rdc does not own:
///
/// - `metadata` is a user-writable free-form object that push sends WHOLESALE,
///   so a stripped `metadata.modified_by` would be deleted server-side on the
///   next push.
/// - an MDH index spec keys each index by COLUMN name (`{"key": {"col": 1}}`),
///   so a dataset column named `modified_by` would lose its index.
///
/// rdc owns the object's own top-level stamps and nothing below them. The
/// content hash still ignores these names at any depth (see
/// [`crate::snapshot::noise::NOISE_FIELDS`]), so a nested occurrence stays on
/// disk without ever churning.
pub const HIDDEN_FIELDS: &[&str] = &["modified_at", "modified_by"];

/// Hook top-level key importance. Listed keys land first in this order;
/// remaining keys (typed fields not listed, then any flattened extras)
/// keep their current relative emission order.
pub const HOOK_KEY_ORDER: &[&str] = &[
    "id",
    "name",
    "description",
    "active",
    "events",
    "settings",
    "queues",
    "run_after",
];

/// Strip every key in [`HIDDEN_FIELDS`] from the TOP LEVEL of `value`.
/// Nested objects and arrays are left untouched — see [`HIDDEN_FIELDS`] for
/// why. Mutates in place; a no-op for a non-object.
pub fn strip_hidden_fields(value: &mut Value) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    for f in HIDDEN_FIELDS {
        map.shift_remove(*f);
    }
}

/// True iff `bytes` parse as a JSON object carrying a [`HIDDEN_FIELDS`] key at
/// the TOP LEVEL. Used by the pull driver to detect a legacy on-disk format and
/// force a one-time rewrite even when the canonical content hash is unchanged.
/// Nested occurrences are data rdc leaves alone, so they must not nudge a
/// rewrite — that would be permanent churn.
pub fn contains_hidden_fields(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return false;
    };
    value
        .as_object()
        .is_some_and(|map| HIDDEN_FIELDS.iter().any(|f| map.contains_key(*f)))
}

/// Serialize a typed value to canonical on-disk JSON bytes: convert via
/// `to_value`, strip hidden fields, write pretty + trailing newline.
/// Used by snapshot writers that don't need any per-kind extras (key
/// ordering, code extraction) on top.
pub fn serialize_for_disk(typed: &impl Serialize) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(typed).context("serializing typed value")?;
    strip_hidden_fields(&mut value);
    let mut bytes = serde_json::to_vec_pretty(&value).context("serializing JSON")?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Reorder the top-level object's keys: `important` keys first, in the
/// listed order (skipping any that aren't present); then every other
/// key in its current order.
///
/// No-op if `value` is not an object or `important` is empty.
pub fn reorder_top_level(value: &mut Value, important: &[&str]) {
    let Some(map) = value.as_object_mut() else { return; };
    if important.is_empty() {
        return;
    }
    let mut old_map = std::mem::take(map);
    let mut new_map = serde_json::Map::with_capacity(old_map.len());
    for key in important {
        if let Some(v) = old_map.shift_remove(*key) {
            new_map.insert((*key).to_string(), v);
        }
    }
    // Drain the rest in insertion order (preserve_order is enabled).
    for (k, v) in old_map {
        new_map.insert(k, v);
    }
    *value = Value::Object(new_map);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn keys_in_order(v: &Value) -> Vec<String> {
        v.as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    #[test]
    fn lists_important_first_then_rest_in_current_order() {
        let mut v = json!({
            "id": 1,
            "url": "u",
            "name": "n",
            "type": "function",
            "queues": [],
            "events": [],
            "config": {},
            "active": true,
            "description": "d",
            "run_after": [],
            "settings": {},
            "metadata": {},
        });
        reorder_top_level(&mut v, HOOK_KEY_ORDER);
        // Important: id, name, description, active, events, settings,
        // queues, run_after. Then the rest in their previous order:
        // url, type, config, metadata.
        assert_eq!(
            keys_in_order(&v),
            vec![
                "id", "name", "description", "active",
                "events", "settings", "queues", "run_after",
                "url", "type", "config", "metadata",
            ]
        );
    }

    #[test]
    fn missing_important_keys_are_skipped() {
        // A minimal hook without description/active/settings/run_after
        // should still get id, name, events, queues at the front in
        // order, with the remaining typed fields after.
        let mut v = json!({
            "id": 1,
            "url": "u",
            "name": "n",
            "type": "webhook",
            "queues": [],
            "events": [],
            "config": {},
        });
        reorder_top_level(&mut v, HOOK_KEY_ORDER);
        assert_eq!(
            keys_in_order(&v),
            vec!["id", "name", "events", "queues", "url", "type", "config"]
        );
    }

    #[test]
    fn noop_on_non_object_values() {
        let mut v = json!([1, 2, 3]);
        reorder_top_level(&mut v, HOOK_KEY_ORDER);
        assert_eq!(v, json!([1, 2, 3]));

        let mut v = json!("string");
        reorder_top_level(&mut v, HOOK_KEY_ORDER);
        assert_eq!(v, json!("string"));
    }

    #[test]
    fn noop_on_empty_important_list() {
        let mut v = json!({ "b": 2, "a": 1 });
        let before: Vec<String> = keys_in_order(&v);
        reorder_top_level(&mut v, &[]);
        assert_eq!(keys_in_order(&v), before);
    }

    #[test]
    fn nested_objects_are_not_touched() {
        let mut v = json!({
            "name": "outer",
            "id": 1,
            "config": { "z": 1, "a": 2 },
        });
        reorder_top_level(&mut v, HOOK_KEY_ORDER);
        // Top level reordered (id before name); nested config keeps order.
        assert_eq!(keys_in_order(&v), vec!["id", "name", "config"]);
        let config_keys: Vec<String> =
            v.get("config").unwrap().as_object().unwrap().keys().cloned().collect();
        assert_eq!(config_keys, vec!["z", "a"]);
    }

    #[test]
    fn strip_hidden_removes_top_level_modified_at() {
        let mut v = json!({"id": 1, "modified_at": "2026-05-22T08:42:15Z", "name": "n"});
        strip_hidden_fields(&mut v);
        assert_eq!(v, json!({"id": 1, "name": "n"}));
    }

    #[test]
    fn strip_hidden_leaves_modifier_and_other_server_fields_alone() {
        // Only `modified_at` / `modified_by` are hidden — `modifier`,
        // `created_at`, `created_by`, `status`, etc. must NOT be stripped from
        // on-disk JSON by this helper. They live elsewhere if needed.
        let mut v = json!({
            "id": 1,
            "modified_at": "t",
            "modifier": "https://x/api/v1/users/4",
            "created_at": "t0",
            "created_by": "u",
            "status": "ready",
        });
        strip_hidden_fields(&mut v);
        assert_eq!(
            v,
            json!({
                "id": 1,
                "modifier": "https://x/api/v1/users/4",
                "created_at": "t0",
                "created_by": "u",
                "status": "ready",
            })
        );
    }

    #[test]
    fn contains_hidden_fields_returns_false_on_non_json() {
        assert!(!contains_hidden_fields(b"not json"));
        assert!(!contains_hidden_fields(b""));
    }

    #[test]
    fn serialize_for_disk_strips_modified_at() {
        let v = json!({
            "id": 1,
            "modified_at": "2026-05-22T08:42:15Z",
            "name": "n",
            "modifier": "u",
        });
        let bytes = serialize_for_disk(&v).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            out,
            json!({"id": 1, "name": "n", "modifier": "u"}),
        );
        // And the trailing newline is added.
        assert_eq!(bytes.last(), Some(&b'\n'));
    }
    #[test]
    fn strips_modified_by_from_top_level() {
        let mut v = json!({
            "id": 1,
            "modified_at": "t",
            "modified_by": "https://x/api/v1/users/4",
            "name": "n",
        });
        strip_hidden_fields(&mut v);
        assert_eq!(v, json!({"id": 1, "name": "n"}));
    }

    #[test]
    fn keeps_nested_stamps_that_may_be_user_data() {
        // `metadata` is user-writable and PATCHed wholesale, and an MDH index
        // spec keys index definitions by COLUMN name (`{"key": {"col": 1}}`).
        // Stripping a nested `modified_at`/`modified_by` would delete data rdc
        // does not own — server-side, on the next push.
        let mut v = json!({
            "modified_by": "https://x/api/v1/users/4",
            "metadata": {"modified_by": "ops", "modified_at": "t", "kept": true},
            "key": {"modified_by": 1},
            "items": [{"modified_at": "t"}],
        });
        strip_hidden_fields(&mut v);
        assert_eq!(
            v,
            json!({
                "metadata": {"modified_by": "ops", "modified_at": "t", "kept": true},
                "key": {"modified_by": 1},
                "items": [{"modified_at": "t"}],
            })
        );
    }

    #[test]
    fn contains_hidden_fields_is_top_level_only() {
        assert!(contains_hidden_fields(b"{\"id\":1,\"modified_by\":\"u\"}"));
        assert!(!contains_hidden_fields(
            b"{\"metadata\":{\"modified_at\":\"t\"}}"
        ));
        assert!(!contains_hidden_fields(
            b"{\"items\":[{\"modified_by\":\"u\"}]}"
        ));
    }
}
