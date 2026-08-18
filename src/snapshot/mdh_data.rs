//! Canonical on-disk form for MDH row data (`envs/<env>/mdh/<slug>/data.jsonl`).
//!
//! `data.jsonl` is not JSON, so `canonicalize_for_hash` hashes it VERBATIM
//! (`snapshot/noise.rs:57-60`). Every byte therefore has to be canonical or
//! pull and push would rewrite the file on every cycle. The canonical form:
//!
//! 1. one JSON object per line, compact (no insignificant whitespace);
//! 2. object keys sorted lexicographically, recursively (array element order
//!    is data and is preserved);
//! 3. lines sorted by their canonical bytes;
//! 4. LF endings, trailing newline; zero rows is a 0-byte file;
//! 5. `_id` dropped if and only if it is the EJSON `{"$oid": …}` wrapper —
//!    a server-generated ObjectId (live-verified marker). Any other `_id`
//!    (string, int, …) is a business key the user authored and is preserved.
//!
//! Rule 5 is applied to BOTH directions — rows read from the env and rows read
//! from disk — so a hand-written `$oid` `_id` is ignored for comparison and
//! removed the next time the file is written. Without that symmetry the two
//! sides could never compare equal.

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// On-disk filename for a manual dataset's rows.
pub const DATA_FILE: &str = "data.jsonl";

/// Row count above which rdc warns that a dataset is large for git.
pub const ROW_WARN_THRESHOLD: usize = 1_000;

/// Row count above which rdc refuses to version a dataset at all.
pub const ROW_HARD_LIMIT: usize = 10_000;

/// True when `v` is the EJSON wrapper for a server-generated ObjectId: an
/// object whose single key is `$oid`.
pub fn is_oid(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == 1 && o.contains_key("$oid"))
}

/// Reduce a row to its canonical form: keys sorted recursively (via
/// [`crate::snapshot::noise::sort_keys_recursive`], which preserves array
/// element order as data), a server-generated `_id` dropped.
pub fn canonicalize_row(row: &Value) -> Value {
    let mut out = row.clone();
    crate::snapshot::noise::sort_keys_recursive(&mut out);
    if let Value::Object(obj) = &mut out
        && obj.get("_id").is_some_and(is_oid)
    {
        obj.shift_remove("_id");
    }
    out
}

/// Serialize rows to the canonical `data.jsonl` bytes.
pub fn to_jsonl(rows: &[Value]) -> Result<Vec<u8>> {
    let mut lines: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
    for row in rows {
        let canonical = canonicalize_row(row);
        if !canonical.is_object() {
            bail!("MDH row is not a JSON object: {canonical}");
        }
        lines.push(serde_json::to_vec(&canonical).context("serializing an MDH row")?);
    }
    lines.sort();
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(&line);
        out.push(b'\n');
    }
    Ok(out)
}

/// Parse canonical `data.jsonl` bytes back into rows, canonicalizing each one.
/// `display_path` appears in error messages (`<path>:<line>`), so a bad hand
/// edit points at the exact line.
pub fn from_jsonl(bytes: &[u8], display_path: &str) -> Result<Vec<Value>> {
    let text = std::str::from_utf8(bytes)
        .with_context(|| format!("{display_path} is not valid UTF-8"))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .with_context(|| format!("{display_path}:{} is not valid JSON", i + 1))?;
        if !value.is_object() {
            bail!("{display_path}:{} is not a JSON object", i + 1);
        }
        out.push(canonicalize_row(&value));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    /// The `$oid` wrapper is the live-verified marker of a server-generated
    /// ObjectId (A2) — the one `_id` shape that is safe to strip.
    #[test]
    fn is_oid_matches_only_the_single_key_wrapper() {
        assert!(is_oid(&json!({ "$oid": "6a8403a6070b60eaa348d173" })));
        assert!(!is_oid(&json!("natural-key-1")));
        assert!(!is_oid(&json!(42)));
        assert!(!is_oid(&json!({ "$oid": "x", "other": 1 })));
        assert!(!is_oid(&json!({})));
    }

    #[test]
    fn canonicalize_sorts_keys_recursively_and_leaves_arrays_ordered() {
        let row = json!({
            "zeta": 1,
            "alpha": { "b": 2, "a": { "d": 4, "c": 3 } },
            "arr": [ { "y": 1, "x": 2 }, "second", "first" ]
        });
        // Arrays keep their element order (order is data); only object keys sort.
        assert_eq!(
            serde_json::to_string(&canonicalize_row(&row)).unwrap(),
            r#"{"alpha":{"a":{"c":3,"d":4},"b":2},"arr":[{"x":2,"y":1},"second","first"],"zeta":1}"#
        );
    }

    #[test]
    fn canonicalize_drops_server_id_but_keeps_business_key() {
        let server = json!({ "_id": { "$oid": "6a8403a6070b60eaa348d173" }, "code": "1000" });
        assert_eq!(canonicalize_row(&server), json!({ "code": "1000" }));

        let business = json!({ "_id": "gl-1000", "code": "1000" });
        assert_eq!(canonicalize_row(&business), business);

        let numeric = json!({ "_id": 7, "code": "1000" });
        assert_eq!(canonicalize_row(&numeric), numeric);
    }

    #[test]
    fn to_jsonl_is_line_sorted_compact_and_newline_terminated() {
        let rows = vec![
            json!({ "code": "2000", "label": "Travel" }),
            json!({ "code": "1000", "label": "Office supplies" }),
        ];
        let out = to_jsonl(&rows).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"code\":\"1000\",\"label\":\"Office supplies\"}\n\
             {\"code\":\"2000\",\"label\":\"Travel\"}\n"
        );
    }

    #[test]
    fn to_jsonl_of_no_rows_is_empty_not_a_blank_line() {
        // A manual dataset with zero rows is a 0-byte file — that is what
        // distinguishes "manual and empty" from "not manual" (no file).
        assert!(to_jsonl(&[]).unwrap().is_empty());
    }

    /// The whole point of a canonical form: feeding it back through the
    /// pipeline must be a fixed point, or every sync rewrites the file (C1).
    #[test]
    fn jsonl_round_trip_is_idempotent_across_the_verified_type_matrix() {
        // Exactly the types verified to round-trip byte-identically through the
        // Data Storage API (A4).
        let rows = vec![json!({
            "_id": "natural-key-1",
            "int": 42,
            "long": 9007199254740993i64,
            "float": 3.14,
            "bool": true,
            "nil": null,
            "uni": "Přílöhá — ünïcode",
            "trailws": "sep ",
            "empty_obj": {},
            "empty_arr": [],
            "arr": [1, "two", { "three": 3 }],
            "nested": { "a": { "b": "c" } },
            "ejson_date": { "$date": "2026-01-15T00:00:00Z" },
            "dotted.key": "dotted",
            "dollar_in_value": "$gt"
        })];
        let first = to_jsonl(&rows).unwrap();
        let parsed = from_jsonl(&first, "data.jsonl").unwrap();
        let second = to_jsonl(&parsed).unwrap();
        assert_eq!(first, second, "canonical form must be a fixed point");
        assert_eq!(parsed[0]["long"], json!(9007199254740993i64));
        assert_eq!(parsed[0]["trailws"], json!("sep "), "trailing ws is data");
    }

    /// A hand-written `$oid` `_id` must be dropped on READ too. Without that
    /// symmetry the two sides could never compare equal and the dataset would
    /// churn forever (spec, on-disk rule 5).
    #[test]
    fn from_jsonl_drops_a_hand_written_server_id() {
        let line = b"{\"_id\":{\"$oid\":\"6a8403a6070b60eaa348d173\"},\"code\":\"1000\"}\n";
        assert_eq!(from_jsonl(line, "data.jsonl").unwrap(), vec![json!({ "code": "1000" })]);
    }

    #[test]
    fn from_jsonl_skips_blank_lines_and_reports_the_offending_line_number() {
        let ok = b"{\"a\":1}\n\n{\"b\":2}\n";
        assert_eq!(from_jsonl(ok, "data.jsonl").unwrap().len(), 2);

        let bad = b"{\"a\":1}\nnot json\n";
        let err = format!("{:#}", from_jsonl(bad, "envs/dev/mdh/gl-codes/data.jsonl").unwrap_err());
        assert!(err.contains("data.jsonl:2"), "must name the line: {err}");

        let scalar = b"\"just a string\"\n";
        let err = format!("{:#}", from_jsonl(scalar, "data.jsonl").unwrap_err());
        assert!(err.contains("data.jsonl:1"), "must name the line: {err}");
        assert!(err.contains("object"), "must say what was wrong: {err}");
    }
}
