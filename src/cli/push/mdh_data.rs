//! Push driver for MDH row data (`data.jsonl`) — the hybrid keyed diff.
//!
//! The local file is authoritative: after a push the env holds exactly the rows
//! the file lists. How that is achieved matters, though, so this is NOT a
//! delete-everything-then-reinsert:
//!
//! - a row carrying an explicit (non-ObjectId) `_id` is identified BY that id,
//!   so editing it is one in-place `replace_one` and its identity survives;
//! - a row without one is identified by its canonical content, as a multiset,
//!   so only genuinely new rows are inserted and only genuinely surplus rows
//!   are deleted. Untouched rows are never rewritten — no empty window, no
//!   ObjectId churn.
//!
//! Deletes are applied BEFORE inserts so a unique index survives a row edit:
//! the outgoing row releases its key before the incoming row claims it.

use crate::snapshot::mdh_data::canonicalize_row;
use serde_json::Value;
use std::collections::BTreeMap;

/// The operations that make a collection match `data.jsonl`.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RowDiff {
    /// Documents to insert, canonical form (may carry an explicit `_id`).
    pub insert: Vec<Value>,
    /// `(id, replacement)` pairs for in-place replace. The replacement never
    /// contains `_id` — the filter carries identity and the field is immutable.
    pub replace: Vec<(Value, Value)>,
    /// RAW remote `_id` values to delete.
    pub delete: Vec<Value>,
}

/// The explicit, user-authored `_id` of a canonical row, if any. A
/// server-generated ObjectId is not one — `canonicalize_row` has already
/// dropped it, so this returns `None` for those.
fn explicit_id(canonical: &Value) -> Option<Value> {
    canonical.get("_id").cloned()
}

/// Canonical JSON text of a value — the map key for both identity flavors.
fn key_of(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// A canonical row with its `_id` removed: the shape `replace_one` wants.
fn without_id(canonical: &Value) -> Value {
    let mut out = canonical.clone();
    if let Value::Object(obj) = &mut out {
        obj.shift_remove("_id");
    }
    out
}

/// Diff local rows (from `data.jsonl`) against raw remote rows (from
/// `find_all`, `_id`s intact). Output order is derived from sorted maps, so it
/// is deterministic and independent of input order.
#[allow(dead_code)] // Called from tests only until Task 6 adds the push driver that consumes it; Task 6 removes this attribute.
pub(crate) fn diff_rows(local: &[Value], remote: &[Value]) -> RowDiff {
    // key → canonical row
    let mut local_keyed: BTreeMap<String, Value> = BTreeMap::new();
    // canonical json → (row, wanted count)
    let mut local_unkeyed: BTreeMap<String, (Value, usize)> = BTreeMap::new();
    for row in local {
        let canonical = canonicalize_row(row);
        match explicit_id(&canonical) {
            Some(id) => {
                local_keyed.insert(key_of(&id), canonical);
            }
            None => {
                let k = key_of(&canonical);
                local_unkeyed.entry(k).or_insert((canonical, 0)).1 += 1;
            }
        }
    }

    // key → (raw id, canonical row)
    let mut remote_keyed: BTreeMap<String, (Value, Value)> = BTreeMap::new();
    // canonical json → raw ids, sorted so the surplus we drop is deterministic
    let mut remote_unkeyed: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in remote {
        let raw_id = row.get("_id").cloned().unwrap_or(Value::Null);
        let canonical = canonicalize_row(row);
        match explicit_id(&canonical) {
            Some(id) => {
                remote_keyed.insert(key_of(&id), (raw_id, canonical));
            }
            None => remote_unkeyed
                .entry(key_of(&canonical))
                .or_default()
                .push(raw_id),
        }
    }
    for ids in remote_unkeyed.values_mut() {
        ids.sort_by_key(key_of);
    }

    let mut diff = RowDiff::default();

    for (key, local_row) in &local_keyed {
        match remote_keyed.get(key) {
            None => diff.insert.push(local_row.clone()),
            Some((_, remote_row)) if remote_row != local_row => {
                let id = explicit_id(local_row).expect("a keyed row always carries its _id");
                diff.replace.push((id, without_id(local_row)));
            }
            Some(_) => {}
        }
    }
    for (key, (raw_id, _)) in &remote_keyed {
        if !local_keyed.contains_key(key) {
            diff.delete.push(raw_id.clone());
        }
    }

    for (key, (row, wanted)) in &local_unkeyed {
        let have = remote_unkeyed.get(key).map_or(0, |ids| ids.len());
        for _ in have..*wanted {
            diff.insert.push(row.clone());
        }
    }
    for (key, ids) in &remote_unkeyed {
        let wanted = local_unkeyed.get(key).map_or(0, |(_, n)| *n);
        for raw_id in ids.iter().skip(wanted) {
            diff.delete.push(raw_id.clone());
        }
    }

    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn oid(hex: &str) -> Value {
        json!({ "$oid": hex })
    }

    #[test]
    fn identical_sides_produce_no_operations() {
        let local = vec![json!({ "code": "1000" }), json!({ "_id": "gl-2", "code": "2000" })];
        let remote = vec![
            json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000" }),
            json!({ "_id": "gl-2", "code": "2000" }),
        ];
        assert_eq!(diff_rows(&local, &remote), RowDiff::default());
    }

    /// Key order and nesting must not register as a change — both sides are
    /// reduced to the same canonical form first.
    #[test]
    fn key_order_difference_is_not_a_change() {
        let local = vec![json!({ "a": 1, "b": { "d": 4, "c": 3 } })];
        let remote = vec![json!({ "_id": oid("aa"), "b": { "c": 3, "d": 4 }, "a": 1 })];
        assert_eq!(diff_rows(&local, &remote), RowDiff::default());
    }

    #[test]
    fn keyed_row_edit_becomes_an_in_place_replace_without_id() {
        let local = vec![json!({ "_id": "gl-1000", "code": "1000", "label": "new" })];
        let remote = vec![json!({ "_id": "gl-1000", "code": "1000", "label": "old" })];
        let d = diff_rows(&local, &remote);
        assert!(d.insert.is_empty() && d.delete.is_empty());
        assert_eq!(
            d.replace,
            vec![(json!("gl-1000"), json!({ "code": "1000", "label": "new" }))],
            "identity is carried by the filter; the replacement must omit _id"
        );
    }

    #[test]
    fn keyed_row_missing_remotely_is_inserted_with_its_id() {
        let local = vec![json!({ "_id": "gl-1000", "code": "1000" })];
        let d = diff_rows(&local, &[]);
        assert_eq!(d.insert, vec![json!({ "_id": "gl-1000", "code": "1000" })]);
        assert!(d.replace.is_empty() && d.delete.is_empty());
    }

    #[test]
    fn keyed_row_missing_locally_is_deleted_by_its_raw_id() {
        let remote = vec![json!({ "_id": "gl-1000", "code": "1000" })];
        let d = diff_rows(&[], &remote);
        assert_eq!(d.delete, vec![json!("gl-1000")]);
        assert!(d.insert.is_empty() && d.replace.is_empty());
    }

    #[test]
    fn unkeyed_row_edit_becomes_a_delete_plus_an_insert() {
        // Without a business key there is nothing to pair old and new by, so an
        // edit is expressed as removing the old row and adding the new one.
        let local = vec![json!({ "code": "1000", "label": "new" })];
        let remote = vec![json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000", "label": "old" })];
        let d = diff_rows(&local, &remote);
        assert_eq!(d.insert, vec![json!({ "code": "1000", "label": "new" })]);
        assert_eq!(d.delete, vec![oid("6a8403a6070b60eaa348d173")]);
        assert!(d.replace.is_empty());
    }

    /// Unkeyed rows are a MULTISET: two identical rows are two rows. The diff
    /// moves only the surplus, in either direction.
    #[test]
    fn unkeyed_duplicates_are_diffed_by_count() {
        let row = json!({ "code": "1000" });
        // local wants 3, remote has 1 → insert 2.
        let d = diff_rows(
            &[row.clone(), row.clone(), row.clone()],
            &[json!({ "_id": oid("a1"), "code": "1000" })],
        );
        assert_eq!(d.insert, vec![row.clone(), row.clone()]);
        assert!(d.delete.is_empty());

        // local wants 1, remote has 3 → delete 2 (the surplus, deterministically).
        let d = diff_rows(
            &[row.clone()],
            &[
                json!({ "_id": oid("a1"), "code": "1000" }),
                json!({ "_id": oid("a2"), "code": "1000" }),
                json!({ "_id": oid("a3"), "code": "1000" }),
            ],
        );
        assert!(d.insert.is_empty());
        assert_eq!(d.delete, vec![oid("a2"), oid("a3")]);
    }

    /// A hand-written `$oid` `_id` on the local side is NOT a business key: it
    /// is dropped by canonicalization, so the row diffs as unkeyed content and
    /// does not churn against the identical remote row.
    #[test]
    fn hand_written_server_id_locally_is_not_treated_as_a_key() {
        let local = vec![json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000" })];
        let remote = vec![json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000" })];
        assert_eq!(diff_rows(&local, &remote), RowDiff::default());
    }

    #[test]
    fn output_is_deterministic_regardless_of_input_order() {
        let a = json!({ "code": "1000" });
        let b = json!({ "code": "2000" });
        let one = diff_rows(&[a.clone(), b.clone()], &[]);
        let two = diff_rows(&[b.clone(), a.clone()], &[]);
        assert_eq!(one, two, "op order must not depend on input order");
    }
}
