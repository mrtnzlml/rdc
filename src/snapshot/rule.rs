//! On-disk codec for rules.
//!
//! Rossum rules carry a `trigger_condition` field — a Python expression
//! evaluated to decide whether the rule fires. Storing it inside JSON
//! is awkward to edit and review (newlines escaped, no syntax
//! highlighting). We extract it on pull to a sibling `.py` file under
//! the same name, and splice it back into the JSON on push.
//!
//! Mirrors `src/snapshot/hook.rs` exactly except for the path: hook
//! code lives at `config.code` (nested), rule code lives at
//! `trigger_condition` (top-level).

use crate::model::Rule;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

/// Write a rule to disk: a JSON file under `<dir>/<slug>.json` and, if
/// the rule has a `trigger_condition`, a sibling `<dir>/<slug>.py` file.
/// The `trigger_condition` field is stripped from the JSON to avoid
/// duplication; the `.py` file becomes the source of truth.
///
/// Returns the JSON bytes written (post-extraction, with trailing newline).
pub fn write_rule(dir: &Path, slug: &str, r: &Rule) -> Result<Vec<u8>> {
    let (json_bytes, code) = serialize_rule(r)?;
    write_atomic(&dir.join(format!("{slug}.json")), &json_bytes)?;
    if let Some(code) = code {
        // Byte-exact: preserve the trigger_condition string as-is so the
        // round-trip is identity.
        write_atomic(&dir.join(format!("{slug}.py")), code.as_bytes())?;
    }
    Ok(json_bytes)
}

/// Serialize a rule to its on-disk byte form WITHOUT writing. Returns
/// the JSON bytes (post-extraction, with trailing newline) and the
/// optional extracted `trigger_condition` string. Used by pull/push
/// drivers to compute `rule_combined_hash` before deciding whether to
/// write or send.
pub fn serialize_rule(r: &Rule) -> Result<(Vec<u8>, Option<String>)> {
    let mut json_value = serde_json::to_value(r).context("serializing rule to value")?;

    // Only extract when the field is actually a string. If a tenant
    // ever emits a non-string trigger_condition (unexpected per the
    // Rossum API), leave it in the JSON so round-trip stays lossless.
    let code = if matches!(json_value.get("trigger_condition"), Some(Value::String(_))) {
        json_value
            .as_object_mut()
            .and_then(|m| m.remove("trigger_condition"))
            .and_then(|v| if let Value::String(s) = v { Some(s) } else { None })
    } else {
        None
    };

    crate::snapshot::key_order::strip_hidden_fields_recursive(&mut json_value);

    let mut bytes = serde_json::to_vec_pretty(&json_value).context("serializing rule json")?;
    bytes.push(b'\n');
    Ok((bytes, code))
}

/// Write only the rule's `.py` file (extracted from `trigger_condition`).
/// Used by pull drivers that compute the JSON write decision separately
/// and only need to overwrite the code file.
pub fn write_rule_code(dir: &Path, slug: &str, code: &str) -> Result<()> {
    let py_path = dir.join(format!("{slug}.py"));
    write_atomic(&py_path, code.as_bytes())?;
    Ok(())
}

/// Read a rule back from disk as an untyped `Value`: load `<dir>/<slug>.json`,
/// then if `<dir>/<slug>.py` exists, splice its contents back into
/// `trigger_condition`. The Value is NOT yet deserialized into `Rule`
/// — callers that want to apply overlay overrides before typing call
/// this directly, then `serde_json::from_value(value)?`.
pub fn read_rule_value(dir: &Path, slug: &str) -> Result<Value> {
    let json_path = dir.join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", json_path.display()))?;

    let py_path = dir.join(format!("{slug}.py"));
    if py_path.exists() {
        let code = std::fs::read_to_string(&py_path)
            .with_context(|| format!("reading {}", py_path.display()))?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("trigger_condition".to_string(), Value::String(code));
        }
    }

    Ok(value)
}

/// Read a rule back from disk into a typed `Rule`. Splices `<slug>.py`
/// back into `trigger_condition` first, so the in-memory `Rule` is
/// byte-for-byte equivalent to what was originally serialized.
pub fn read_rule(dir: &Path, slug: &str) -> Result<Rule> {
    let value = read_rule_value(dir, slug)?;
    let json_path = dir.join(format!("{slug}.json"));
    let rule: Rule = serde_json::from_value(value)
        .with_context(|| format!("deserializing rule from {}", json_path.display()))?;
    Ok(rule)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample_with_code() -> Rule {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/rules/1",
            "name": "Validate totals",
            "queues": [],
            "trigger_condition": "annotation_content.total > 1000\n"
        });
        serde_json::from_value(v).unwrap()
    }

    fn sample_without_code() -> Rule {
        let v = json!({
            "id": 2,
            "url": "https://x/api/v1/rules/2",
            "name": "No code rule",
            "queues": []
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn writes_json_and_py_when_trigger_condition_present() {
        let dir = TempDir::new().unwrap();
        write_rule(dir.path(), "r", &sample_with_code()).unwrap();
        assert!(dir.path().join("r.json").exists());
        assert!(dir.path().join("r.py").exists());
    }

    #[test]
    fn json_does_not_contain_trigger_condition() {
        let dir = TempDir::new().unwrap();
        write_rule(dir.path(), "r", &sample_with_code()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("r.json")).unwrap();
        assert!(!raw.contains("trigger_condition"),
            "trigger_condition should be in .py, not .json; got:\n{raw}");
        assert!(raw.contains("Validate totals"), "other fields preserved");
    }

    #[test]
    fn py_contains_exact_trigger_condition_bytes() {
        let dir = TempDir::new().unwrap();
        write_rule(dir.path(), "r", &sample_with_code()).unwrap();
        let py = std::fs::read_to_string(dir.path().join("r.py")).unwrap();
        assert_eq!(py, "annotation_content.total > 1000\n");
    }

    #[test]
    fn no_py_file_when_rule_has_no_trigger_condition() {
        let dir = TempDir::new().unwrap();
        write_rule(dir.path(), "r", &sample_without_code()).unwrap();
        assert!(dir.path().join("r.json").exists());
        assert!(!dir.path().join("r.py").exists());
    }

    #[test]
    fn serialize_returns_json_and_code() {
        let (bytes, code) = serialize_rule(&sample_with_code()).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(!s.contains("trigger_condition"));
        assert_eq!(code.as_deref(), Some("annotation_content.total > 1000\n"));
    }

    #[test]
    fn round_trip_with_trigger_condition() {
        let dir = TempDir::new().unwrap();
        let original = sample_with_code();
        write_rule(dir.path(), "r", &original).unwrap();
        let read = read_rule(dir.path(), "r").unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn round_trip_without_trigger_condition() {
        let dir = TempDir::new().unwrap();
        let original = sample_without_code();
        write_rule(dir.path(), "r", &original).unwrap();
        let read = read_rule(dir.path(), "r").unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn non_string_trigger_condition_stays_in_json() {
        // A rule whose `trigger_condition` is somehow a number/bool —
        // unexpected per the API but we don't want to silently drop it.
        let v = json!({
            "id": 1, "url": "https://x/api/v1/rules/1", "name": "Weird",
            "queues": [], "trigger_condition": 42
        });
        let r: Rule = serde_json::from_value(v).unwrap();
        let (bytes, code) = serialize_rule(&r).unwrap();
        assert!(code.is_none(), "non-string trigger_condition should not be extracted");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("\"trigger_condition\": 42"), "should stay in JSON; got:\n{s}");
    }

    #[test]
    fn read_with_unicode_trigger_condition() {
        let dir = TempDir::new().unwrap();
        let v = json!({
            "id": 1, "url": "https://x/api/v1/rules/1", "name": "Unicode",
            "queues": [], "trigger_condition": "# žluťoučký kůň\nx > 0"
        });
        let rule: Rule = serde_json::from_value(v).unwrap();
        write_rule(dir.path(), "u", &rule).unwrap();
        let read = read_rule(dir.path(), "u").unwrap();
        assert_eq!(rule, read);
    }
}
