//! Recursive 3-way merge for JSON values.
//!
//! Algorithm (per node, dispatching by JSON kind):
//!
//! * **Trivial cases first.** If `local == base`, return `remote`.
//!   If `remote == base`, return `local`. If `local == remote`, return
//!   either (both sides agreed). These dominate the dispatch — a
//!   nested call that finds an unchanged side returns its answer
//!   without recursing further.
//! * **Objects.** Recurse per key, using the union of base / local /
//!   remote keys. A missing key on a side is treated as `Value::Null`
//!   so add-vs-edit and delete-vs-edit are observed correctly. If a
//!   key auto-resolves to `Value::Null` AND was absent on all
//!   resolved sides, it is dropped from the output (so a deletion
//!   doesn't leak back as an explicit null).
//! * **Arrays of `id`-keyed objects.** When every element of every
//!   side is an object with an `id` field (string or number), the
//!   array is merged as a *map keyed by id*: element identity follows
//!   the id, element body is merge3'd recursively. The output order
//!   is base order first (preserving the user's existing layout),
//!   then any added ids in (local-first, remote-second) discovery
//!   order. This is the schema.content / schema.children case.
//! * **Arrays of strings.** Treated as sets. The result is
//!   `(base ∪ local-adds ∪ remote-adds) \ (local-removes ∪
//!   remote-removes)`, ordered alphabetically for determinism. This
//!   is the `hook.queues` case (URL set).
//! * **Anything else.** A scalar (number / bool / string / null) that
//!   differs on both sides, OR an array shape mismatch (mixed
//!   element kinds, objects without `id`), returns
//!   `MergeOutcome::Conflict` with a dotted path of where the
//!   disagreement lives.
//!
//! The merge is **hash-invariant safe**: the canonical form used by
//! `content_hash` (alphabetical key sort + noise strip) produces the
//! same hash whether the merge keeps base ordering or any other
//! ordering, so a re-pull after auto-merge won't appear as drift.

use super::MergeOutcome;
use serde_json::{Map, Value};

/// 3-way merge entry point. `base` is the bytes that were on disk at
/// the last successful sync (the lockfile's `content_hash` pre-image,
/// recovered from `.rdc/state/<env>.base/`). `local` is the current
/// on-disk state, `remote` the just-fetched API state.
pub fn merge3_json(base: &Value, local: &Value, remote: &Value) -> MergeOutcome<Value> {
    let mut local_paths = Vec::new();
    let mut remote_paths = Vec::new();
    let mut conflicts = Vec::new();
    let merged = merge_node(
        "",
        base,
        local,
        remote,
        &mut local_paths,
        &mut remote_paths,
        &mut conflicts,
    );
    if conflicts.is_empty() {
        MergeOutcome::Merged {
            merged,
            local_paths,
            remote_paths,
        }
    } else {
        MergeOutcome::Conflict { reasons: conflicts }
    }
}

fn merge_node(
    path: &str,
    base: &Value,
    local: &Value,
    remote: &Value,
    local_paths: &mut Vec<String>,
    remote_paths: &mut Vec<String>,
    conflicts: &mut Vec<String>,
) -> Value {
    if local == base {
        // Pure remote change (or no change). Record the path only when
        // there's an actual edit so the log line stays useful.
        if remote != base && !path.is_empty() {
            remote_paths.push(path.to_string());
        }
        return remote.clone();
    }
    if remote == base {
        if local != base && !path.is_empty() {
            local_paths.push(path.to_string());
        }
        return local.clone();
    }
    if local == remote {
        // Both sides made the SAME change. No conflict, just record
        // it as one path (arbitrarily attribute to "local").
        if !path.is_empty() {
            local_paths.push(path.to_string());
        }
        return local.clone();
    }

    // Both diverged AND disagree. Try to recurse into structure.
    match (base, local, remote) {
        (Value::Object(b), Value::Object(l), Value::Object(r)) => {
            merge_objects(path, b, l, r, local_paths, remote_paths, conflicts)
        }
        (Value::Array(b), Value::Array(l), Value::Array(r)) => {
            merge_arrays(path, b, l, r, local_paths, remote_paths, conflicts)
        }
        _ => {
            // Type mismatch OR scalar conflict.
            conflicts.push(if path.is_empty() { "<root>".to_string() } else { path.to_string() });
            local.clone() // arbitrary; caller treats Conflict as discarded
        }
    }
}

fn merge_objects(
    path: &str,
    base: &Map<String, Value>,
    local: &Map<String, Value>,
    remote: &Map<String, Value>,
    local_paths: &mut Vec<String>,
    remote_paths: &mut Vec<String>,
    conflicts: &mut Vec<String>,
) -> Value {
    // Union of keys preserving base order, then local-only, then remote-only.
    let mut keys: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for k in base.keys().chain(local.keys()).chain(remote.keys()) {
        if seen.insert(k.clone()) {
            keys.push(k.clone());
        }
    }

    let mut out = Map::new();
    for k in keys {
        let b = base.get(&k).cloned().unwrap_or(Value::Null);
        let l = local.get(&k).cloned().unwrap_or(Value::Null);
        let r = remote.get(&k).cloned().unwrap_or(Value::Null);
        let child_path = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };

        let merged = merge_node(
            &child_path,
            &b,
            &l,
            &r,
            local_paths,
            remote_paths,
            conflicts,
        );

        // If the merge resolved to Null AND the key was absent from
        // BOTH local and remote, treat as "deleted on both sides" —
        // drop the key. Otherwise keep it (an explicit null is a
        // legitimate value).
        let key_absent_local = !local.contains_key(&k);
        let key_absent_remote = !remote.contains_key(&k);
        if merged.is_null() && key_absent_local && key_absent_remote {
            continue;
        }
        out.insert(k, merged);
    }
    Value::Object(out)
}

/// Array merge dispatcher. Picks the right strategy based on element shape.
fn merge_arrays(
    path: &str,
    base: &[Value],
    local: &[Value],
    remote: &[Value],
    local_paths: &mut Vec<String>,
    remote_paths: &mut Vec<String>,
    conflicts: &mut Vec<String>,
) -> Value {
    // All-strings: set-semantics merge.
    if all_strings(base) && all_strings(local) && all_strings(remote) {
        return merge_string_arrays(path, base, local, remote, local_paths, remote_paths);
    }
    // All-id-keyed-objects: keyed merge.
    if let (Some(b_map), Some(l_map), Some(r_map)) = (
        id_keyed(base),
        id_keyed(local),
        id_keyed(remote),
    ) {
        return merge_id_keyed_arrays(
            path,
            base,
            &b_map,
            &l_map,
            &r_map,
            local_paths,
            remote_paths,
            conflicts,
        );
    }
    // Anything else: shape mismatch / opaque array.
    conflicts.push(if path.is_empty() { "<root>".to_string() } else { path.to_string() });
    Value::Array(local.to_vec())
}

fn all_strings(arr: &[Value]) -> bool {
    arr.iter().all(|v| v.is_string())
}

/// If every element is an object with a present `id` (string or
/// number), return `id_string → element` for keyed lookup. Otherwise
/// `None`.
fn id_keyed(arr: &[Value]) -> Option<std::collections::BTreeMap<String, Value>> {
    let mut out = std::collections::BTreeMap::new();
    for el in arr {
        let obj = el.as_object()?;
        let id = obj.get("id")?;
        let key = match id {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return None,
        };
        out.insert(key, el.clone());
    }
    Some(out)
}

fn merge_string_arrays(
    path: &str,
    base: &[Value],
    local: &[Value],
    remote: &[Value],
    local_paths: &mut Vec<String>,
    remote_paths: &mut Vec<String>,
) -> Value {
    use std::collections::BTreeSet;
    let to_set = |arr: &[Value]| -> BTreeSet<String> {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    };
    let b: BTreeSet<String> = to_set(base);
    let l: BTreeSet<String> = to_set(local);
    let r: BTreeSet<String> = to_set(remote);

    // Removed by either side (was in base, not in local) loses.
    let removed_local: BTreeSet<&String> = b.difference(&l).collect();
    let removed_remote: BTreeSet<&String> = b.difference(&r).collect();
    let added_local: BTreeSet<&String> = l.difference(&b).collect();
    let added_remote: BTreeSet<&String> = r.difference(&b).collect();

    let mut result: BTreeSet<String> = b.clone();
    for x in &removed_local { result.remove(*x); }
    for x in &removed_remote { result.remove(*x); }
    for x in &added_local { result.insert((*x).clone()); }
    for x in &added_remote { result.insert((*x).clone()); }

    // Record the side(s) that changed the array (one path per side
    // since string-set granularity below the array isn't useful).
    if !path.is_empty() {
        if !added_local.is_empty() || !removed_local.is_empty() {
            local_paths.push(path.to_string());
        }
        if !added_remote.is_empty() || !removed_remote.is_empty() {
            remote_paths.push(path.to_string());
        }
    }

    // Deterministic alphabetical order (set semantics, no positional meaning).
    let out: Vec<Value> = result.into_iter().map(Value::String).collect();
    Value::Array(out)
}

fn merge_id_keyed_arrays(
    path: &str,
    base_order: &[Value],
    base: &std::collections::BTreeMap<String, Value>,
    local: &std::collections::BTreeMap<String, Value>,
    remote: &std::collections::BTreeMap<String, Value>,
    local_paths: &mut Vec<String>,
    remote_paths: &mut Vec<String>,
    conflicts: &mut Vec<String>,
) -> Value {
    use std::collections::BTreeSet;
    // Union of all ids, but emit in: base order first, then local-only
    // (sorted), then remote-only (sorted). Preserves user's existing
    // schema layout while giving deterministic placement for additions.
    let mut out = Vec::new();
    let mut emitted: BTreeSet<String> = BTreeSet::new();

    let id_of = |v: &Value| -> Option<String> {
        v.as_object()
            .and_then(|o| o.get("id"))
            .and_then(|i| match i {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
    };

    // Pass 1: base order.
    for b_el in base_order {
        let Some(id) = id_of(b_el) else { continue };
        if !emitted.insert(id.clone()) {
            continue;
        }
        let merged = merge_single_id(
            path, &id, base, local, remote, local_paths, remote_paths, conflicts,
        );
        if let Some(m) = merged {
            out.push(m);
        }
    }
    // Pass 2: local-only.
    let mut local_only: Vec<String> = local
        .keys()
        .filter(|k| !base.contains_key(k.as_str()))
        .cloned()
        .collect();
    local_only.sort();
    for id in local_only {
        if !emitted.insert(id.clone()) { continue; }
        let merged = merge_single_id(
            path, &id, base, local, remote, local_paths, remote_paths, conflicts,
        );
        if let Some(m) = merged {
            out.push(m);
        }
    }
    // Pass 3: remote-only.
    let mut remote_only: Vec<String> = remote
        .keys()
        .filter(|k| !base.contains_key(k.as_str()) && !local.contains_key(k.as_str()))
        .cloned()
        .collect();
    remote_only.sort();
    for id in remote_only {
        if !emitted.insert(id.clone()) { continue; }
        let merged = merge_single_id(
            path, &id, base, local, remote, local_paths, remote_paths, conflicts,
        );
        if let Some(m) = merged {
            out.push(m);
        }
    }

    Value::Array(out)
}

fn merge_single_id(
    path: &str,
    id: &str,
    base: &std::collections::BTreeMap<String, Value>,
    local: &std::collections::BTreeMap<String, Value>,
    remote: &std::collections::BTreeMap<String, Value>,
    local_paths: &mut Vec<String>,
    remote_paths: &mut Vec<String>,
    conflicts: &mut Vec<String>,
) -> Option<Value> {
    let element_path = if path.is_empty() {
        format!("[id={id}]")
    } else {
        format!("{path}[id={id}]")
    };
    let b = base.get(id);
    let l = local.get(id);
    let r = remote.get(id);
    match (b, l, r) {
        // Caller's union enumeration guarantees at least one side has the id.
        (None, None, None) => None,

        // Pure-add cases.
        (None, None, Some(rv)) => {
            remote_paths.push(element_path);
            Some(rv.clone())
        }
        (None, Some(lv), None) => {
            local_paths.push(element_path);
            Some(lv.clone())
        }
        (None, Some(lv), Some(rv)) => {
            if lv == rv {
                local_paths.push(element_path);
                Some(lv.clone())
            } else {
                // Both sides added a new element at the same id with
                // different bodies — genuine concurrent-add conflict.
                conflicts.push(element_path);
                None
            }
        }

        // Both-delete and clean delete cases.
        (Some(_), None, None) => None,
        (Some(bv), None, Some(rv)) => {
            if rv == bv {
                // Local deleted, remote unchanged → honor local intent.
                local_paths.push(element_path);
                None
            } else {
                conflicts.push(element_path);
                None
            }
        }
        (Some(bv), Some(lv), None) => {
            if lv == bv {
                remote_paths.push(element_path);
                None
            } else {
                conflicts.push(element_path);
                None
            }
        }

        // All three present → recurse into the element body.
        (Some(bv), Some(lv), Some(rv)) => {
            let merged = merge_node(
                &element_path,
                bv,
                lv,
                rv,
                local_paths,
                remote_paths,
                conflicts,
            );
            Some(merged)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merged_value(out: MergeOutcome<Value>) -> Value {
        match out {
            MergeOutcome::Merged { merged, .. } => merged,
            MergeOutcome::Conflict { reasons } => {
                panic!("unexpected conflict: {reasons:?}");
            }
        }
    }

    fn conflict_paths(out: MergeOutcome<Value>) -> Vec<String> {
        match out {
            MergeOutcome::Merged { merged, .. } => {
                panic!("expected conflict, got merged: {merged}");
            }
            MergeOutcome::Conflict { reasons } => reasons,
        }
    }

    #[test]
    fn identical_inputs_return_base() {
        let v = json!({"a": 1});
        let out = merge3_json(&v, &v, &v);
        assert_eq!(merged_value(out), v);
    }

    #[test]
    fn only_local_changed_returns_local() {
        let base = json!({"a": 1, "b": 2});
        let local = json!({"a": 10, "b": 2});
        let remote = json!({"a": 1, "b": 2});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(merged_value(out), local);
    }

    #[test]
    fn only_remote_changed_returns_remote() {
        let base = json!({"a": 1, "b": 2});
        let local = json!({"a": 1, "b": 2});
        let remote = json!({"a": 1, "b": 20});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(merged_value(out), remote);
    }

    #[test]
    fn disjoint_top_level_edits_auto_merge() {
        let base = json!({"name": "X", "active": true, "queues": []});
        let local = json!({"name": "X edited", "active": true, "queues": []});
        let remote = json!({"name": "X", "active": false, "queues": []});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(
            merged_value(out),
            json!({"name": "X edited", "active": false, "queues": []})
        );
    }

    #[test]
    fn disjoint_top_level_edits_record_paths() {
        let base = json!({"name": "X", "active": true});
        let local = json!({"name": "X edited", "active": true});
        let remote = json!({"name": "X", "active": false});
        match merge3_json(&base, &local, &remote) {
            MergeOutcome::Merged { local_paths, remote_paths, .. } => {
                assert_eq!(local_paths, vec!["name".to_string()]);
                assert_eq!(remote_paths, vec!["active".to_string()]);
            }
            MergeOutcome::Conflict { reasons } => panic!("unexpected conflict: {reasons:?}"),
        }
    }

    #[test]
    fn same_field_changed_on_both_sides_is_conflict() {
        let base = json!({"name": "X"});
        let local = json!({"name": "X local"});
        let remote = json!({"name": "X remote"});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(conflict_paths(out), vec!["name".to_string()]);
    }

    #[test]
    fn nested_disjoint_edits_recurse() {
        let base = json!({"settings": {"a": 1, "b": 2}});
        let local = json!({"settings": {"a": 10, "b": 2}});
        let remote = json!({"settings": {"a": 1, "b": 20}});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(merged_value(out), json!({"settings": {"a": 10, "b": 20}}));
    }

    #[test]
    fn additions_on_both_sides_kept() {
        let base = json!({"a": 1});
        let local = json!({"a": 1, "b": 2});
        let remote = json!({"a": 1, "c": 3});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(merged_value(out), json!({"a": 1, "b": 2, "c": 3}));
    }

    #[test]
    fn delete_vs_edit_is_conflict() {
        let base = json!({"a": 1, "x": "keep"});
        let local = json!({"a": 1});                // deleted x
        let remote = json!({"a": 1, "x": "edited"}); // edited x
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(conflict_paths(out), vec!["x".to_string()]);
    }

    #[test]
    fn both_deleted_same_key_drops_it() {
        let base = json!({"a": 1, "x": "old"});
        let local = json!({"a": 1});
        let remote = json!({"a": 1});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(merged_value(out), json!({"a": 1}));
    }

    #[test]
    fn string_set_array_union_of_additions() {
        let base = json!({"queues": ["a", "b"]});
        let local = json!({"queues": ["a", "b", "c"]});
        let remote = json!({"queues": ["a", "b", "d"]});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(merged_value(out), json!({"queues": ["a", "b", "c", "d"]}));
    }

    #[test]
    fn string_set_array_removed_by_one_side_kept_out() {
        let base = json!({"queues": ["a", "b", "c"]});
        let local = json!({"queues": ["a", "c"]}); // removed b
        let remote = json!({"queues": ["a", "b", "c"]}); // unchanged
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(merged_value(out), json!({"queues": ["a", "c"]}));
    }

    #[test]
    fn id_keyed_array_disjoint_element_edits() {
        let base = json!({"content": [
            {"id": "f1", "label": "Field 1"},
            {"id": "f2", "label": "Field 2"},
        ]});
        let local = json!({"content": [
            {"id": "f1", "label": "Field 1 edited"},
            {"id": "f2", "label": "Field 2"},
        ]});
        let remote = json!({"content": [
            {"id": "f1", "label": "Field 1"},
            {"id": "f2", "label": "Field 2 edited"},
        ]});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(
            merged_value(out),
            json!({"content": [
                {"id": "f1", "label": "Field 1 edited"},
                {"id": "f2", "label": "Field 2 edited"},
            ]})
        );
    }

    #[test]
    fn id_keyed_array_same_element_field_changed_both_sides_is_conflict() {
        let base = json!({"content": [{"id": "f1", "label": "Field 1"}]});
        let local = json!({"content": [{"id": "f1", "label": "Field 1 local"}]});
        let remote = json!({"content": [{"id": "f1", "label": "Field 1 remote"}]});
        let out = merge3_json(&base, &local, &remote);
        let cs = conflict_paths(out);
        assert!(
            cs.iter().any(|c| c.contains("f1") && c.contains("label")),
            "expected conflict path mentioning f1.label, got {cs:?}"
        );
    }

    #[test]
    fn id_keyed_array_local_adds_new_element() {
        let base = json!({"content": [{"id": "f1", "label": "Field 1"}]});
        let local = json!({"content": [
            {"id": "f1", "label": "Field 1"},
            {"id": "f2", "label": "Field 2 new"},
        ]});
        let remote = json!({"content": [{"id": "f1", "label": "Field 1"}]});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(
            merged_value(out),
            json!({"content": [
                {"id": "f1", "label": "Field 1"},
                {"id": "f2", "label": "Field 2 new"},
            ]})
        );
    }

    #[test]
    fn id_keyed_array_both_sides_add_different_new_elements() {
        let base = json!({"content": [{"id": "f1", "label": "Field 1"}]});
        let local = json!({"content": [
            {"id": "f1", "label": "Field 1"},
            {"id": "f2", "label": "Local"},
        ]});
        let remote = json!({"content": [
            {"id": "f1", "label": "Field 1"},
            {"id": "f3", "label": "Remote"},
        ]});
        let out = merge3_json(&base, &local, &remote);
        // f1 (base order), then f2 (local-only), then f3 (remote-only).
        assert_eq!(
            merged_value(out),
            json!({"content": [
                {"id": "f1", "label": "Field 1"},
                {"id": "f2", "label": "Local"},
                {"id": "f3", "label": "Remote"},
            ]})
        );
    }

    #[test]
    fn id_keyed_array_local_deletes_unchanged_element() {
        let base = json!({"content": [
            {"id": "f1", "label": "Field 1"},
            {"id": "f2", "label": "Field 2"},
        ]});
        let local = json!({"content": [{"id": "f1", "label": "Field 1"}]}); // deleted f2
        let remote = json!({"content": [
            {"id": "f1", "label": "Field 1"},
            {"id": "f2", "label": "Field 2"},
        ]});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(
            merged_value(out),
            json!({"content": [{"id": "f1", "label": "Field 1"}]})
        );
    }

    #[test]
    fn id_keyed_array_delete_vs_edit_is_conflict() {
        let base = json!({"content": [{"id": "f1", "label": "Field 1"}]});
        let local = json!({"content": []});  // deleted
        let remote = json!({"content": [{"id": "f1", "label": "Field 1 edited"}]}); // edited
        let out = merge3_json(&base, &local, &remote);
        let cs = conflict_paths(out);
        assert!(cs.iter().any(|c| c.contains("f1")), "got {cs:?}");
    }

    #[test]
    fn schema_local_adds_top_level_table_while_remote_edits_sibling() {
        // Repro of the reported scenario: the user adds a top-level table
        // (multivalue → tuple → column datapoints) into a section's children
        // locally, while the remote edits a SIBLING datapoint's label. The
        // merge must keep BOTH the remote edit AND the local table.
        let base = json!({"content": [
            {"category": "section", "id": "basic", "children": [
                {"category": "datapoint", "id": "invoice_id", "label": "Invoice"}
            ]}
        ]});
        let local = json!({"content": [
            {"category": "section", "id": "basic", "children": [
                {"category": "datapoint", "id": "invoice_id", "label": "Invoice"},
                {"category": "multivalue", "id": "line_items", "label": "Line items",
                 "children": {"category": "tuple", "id": "line_items_tuple", "children": [
                    {"category": "datapoint", "id": "item_desc", "label": "Desc"}
                 ]}}
            ]}
        ]});
        let remote = json!({"content": [
            {"category": "section", "id": "basic", "children": [
                {"category": "datapoint", "id": "invoice_id", "label": "Invoice No."}
            ]}
        ]});
        let out = merge3_json(&base, &local, &remote);
        assert_eq!(
            merged_value(out),
            json!({"content": [
                {"category": "section", "id": "basic", "children": [
                    {"category": "datapoint", "id": "invoice_id", "label": "Invoice No."},
                    {"category": "multivalue", "id": "line_items", "label": "Line items",
                     "children": {"category": "tuple", "id": "line_items_tuple", "children": [
                        {"category": "datapoint", "id": "item_desc", "label": "Desc"}
                     ]}}
                ]}
            ]}),
            "local table must survive a simultaneous remote edit to a sibling"
        );
    }

    #[test]
    fn schema_local_adds_new_top_level_section_while_remote_edits_other_section() {
        // EXACT reported shape: local adds a brand-new top-level SECTION
        // (containing a table) at the end of `content`, while the remote edits
        // a field in a DIFFERENT existing section. The new section must survive.
        let base = json!({"content": [
            {"category": "section", "id": "sec_a", "children": [
                {"category": "datapoint", "id": "a1", "label": "A1"}
            ]},
            {"category": "section", "id": "sec_b", "children": [
                {"category": "datapoint", "id": "b1", "label": "B1"}
            ]}
        ]});
        let local = json!({"content": [
            {"category": "section", "id": "sec_a", "children": [
                {"category": "datapoint", "id": "a1", "label": "A1"}
            ]},
            {"category": "section", "id": "sec_b", "children": [
                {"category": "datapoint", "id": "b1", "label": "B1"}
            ]},
            {"category": "section", "id": "sec_new", "children": [
                {"category": "multivalue", "id": "my_table", "label": "My table",
                 "children": {"category": "tuple", "id": "my_table_tuple", "children": [
                    {"category": "datapoint", "id": "col1", "label": "Col"}
                 ]}}
            ]}
        ]});
        let remote = json!({"content": [
            {"category": "section", "id": "sec_a", "children": [
                {"category": "datapoint", "id": "a1", "label": "A1 EDITED"}
            ]},
            {"category": "section", "id": "sec_b", "children": [
                {"category": "datapoint", "id": "b1", "label": "B1"}
            ]}
        ]});
        let (merged, local_paths) = match merge3_json(&base, &local, &remote) {
            MergeOutcome::Merged { merged, local_paths, .. } => (merged, local_paths),
            MergeOutcome::Conflict { reasons } => panic!("unexpected conflict: {reasons:?}"),
        };
        let ids: Vec<String> = merged["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect();
        assert!(
            ids.contains(&"sec_new".to_string()),
            "the locally-added top-level section must survive; got section ids {ids:?}"
        );
        assert_eq!(
            merged["content"][0]["children"][0]["label"], "A1 EDITED",
            "remote's sibling-section edit must also be kept"
        );
        // The added section MUST be reported as a local-side change. This is
        // the trigger the auto-merge caller relies on (commit c863229): a
        // non-empty `local_paths` promotes the merged result to the push
        // pipeline, so the new section reaches the remote instead of being
        // reverted on the next sync. Without this flag the table silently
        // dropped (the reported bug).
        assert!(
            local_paths.iter().any(|p| p.contains("sec_new")),
            "the local section-add must be recorded in local_paths so it gets pushed; got {local_paths:?}"
        );
    }

    #[test]
    fn opaque_array_without_ids_is_conflict_on_divergence() {
        let base = json!([1, 2, 3]);
        let local = json!([1, 2, 3, 4]);
        let remote = json!([1, 2, 3, 5]);
        let out = merge3_json(&base, &local, &remote);
        let cs = conflict_paths(out);
        assert_eq!(cs, vec!["<root>".to_string()]);
    }

    #[test]
    fn scalar_type_mismatch_is_conflict() {
        let base = json!(1);
        let local = json!("changed");
        let remote = json!(true);
        let out = merge3_json(&base, &local, &remote);
        let cs = conflict_paths(out);
        assert_eq!(cs, vec!["<root>".to_string()]);
    }
}
