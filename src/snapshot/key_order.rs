//! Per-kind key-ordering for on-disk JSON. Reorders top-level object
//! keys so the listed "important" keys appear first in the given order;
//! remaining keys keep their current relative order.
//!
//! Requires the `preserve_order` feature on `serde_json` so `Value::Object`
//! is an `IndexMap` (insertion-order preserving). With that, calling
//! `serde_json::to_vec_pretty` over a reordered object emits keys in the
//! order this module imposed.
//!
//! Hash invariance: `content_hash` canonicalises through
//! `snapshot::noise::canonicalize_for_hash`, which sorts all keys
//! alphabetically before hashing, so changing key order on disk does NOT
//! produce drift on the next sync.

use serde_json::Value;

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
}
