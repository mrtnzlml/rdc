use crate::model::Hook;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

/// Write a hook to disk: a JSON file under `<dir>/<slug>.json` and, if the hook
/// has Python code, a sibling `<slug>.py` file. The `code` field of `config` is
/// stripped from the JSON to avoid duplication; the `.py` file becomes the
/// source of truth.
///
/// Returns the JSON bytes written (post-extraction, with trailing newline).
pub fn write_hook(dir: &Path, slug: &str, hook: &Hook) -> Result<Vec<u8>> {
    let (json_bytes, code) = serialize_hook(hook)?;
    write_atomic(&dir.join(format!("{slug}.json")), &json_bytes)?;
    if let Some(code) = code {
        // Write code bytes exactly as received. Preserves byte-exact round-trip
        // through the codec (read_hook returns Hook with identical config.code).
        write_atomic(&dir.join(format!("{slug}.py")), code.as_bytes())?;
    }
    Ok(json_bytes)
}

/// Serialize a hook to its on-disk byte form WITHOUT writing. Returns the JSON
/// bytes (post-extraction, with trailing newline) and the optional extracted
/// code string. Used by pull/push drivers to compute `hook_combined_hash`
/// before deciding whether to write or send.
pub fn serialize_hook(hook: &Hook) -> Result<(Vec<u8>, Option<String>)> {
    let mut json_value = serde_json::to_value(hook)
        .context("serializing hook to value")?;

    let code = json_value
        .get_mut("config")
        .and_then(|c| c.as_object_mut())
        .and_then(|m| m.remove("code"))
        .and_then(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        });

    let mut bytes = serde_json::to_vec_pretty(&json_value)
        .context("serializing hook json")?;
    bytes.push(b'\n');
    Ok((bytes, code))
}

/// Write only the hook's `.py` file (extracted from `config.code`). Used by
/// pull drivers that compute the JSON write decision separately and only need
/// to overwrite the code file.
pub fn write_hook_code(dir: &Path, slug: &str, code: &str) -> Result<()> {
    let py_path = dir.join(format!("{slug}.py"));
    write_atomic(&py_path, code.as_bytes())?;
    Ok(())
}

/// Read a hook back from disk as an untyped `Value`: load `<dir>/<slug>.json`,
/// then if `<dir>/<slug>.py` exists, splice its contents back into
/// `config.code`. The Value is NOT yet deserialized into `Hook` — callers
/// who need to apply overlay overrides before typing call this directly,
/// then `serde_json::from_value(value)?`.
pub fn read_hook_value(dir: &Path, slug: &str) -> Result<Value> {
    let json_path = dir.join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", json_path.display()))?;

    let py_path = dir.join(format!("{slug}.py"));
    if py_path.exists() {
        let code = std::fs::read_to_string(&py_path)
            .with_context(|| format!("reading {}", py_path.display()))?;
        // The .py file is the byte-exact canonical form (write_hook preserves
        // bytes). No trailing-newline normalization on read either.
        if let Some(config) = value.get_mut("config").and_then(|c| c.as_object_mut()) {
            config.insert("code".to_string(), Value::String(code));
        }
    }

    Ok(value)
}

/// Read a hook back from disk into a typed `Hook`. Splices `<slug>.py` back
/// into `config.code` first, so the in-memory `Hook` is byte-for-byte
/// equivalent to what was originally serialized. Fails if required typed
/// fields are missing — overlay-stripping callers should use
/// `read_hook_value` + apply overlay + `from_value` instead.
pub fn read_hook(dir: &Path, slug: &str) -> Result<Hook> {
    let value = read_hook_value(dir, slug)?;
    let json_path = dir.join(format!("{slug}.json"));
    let hook: Hook = serde_json::from_value(value)
        .with_context(|| format!("deserializing hook from {}", json_path.display()))?;
    Ok(hook)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Hook;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample_hook() -> Hook {
        let v = json!({
            "id": 1,
            "url": "https://example/api/v1/hooks/1",
            "name": "Sample",
            "type": "function",
            "queues": [],
            "events": [],
            "config": { "runtime": "python3.12", "code": "def x():\n    return 1\n" }
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn writes_json_and_py() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &sample_hook()).unwrap();
        assert!(dir.path().join("sample.json").exists());
        assert!(dir.path().join("sample.py").exists());
    }

    #[test]
    fn json_does_not_contain_code_field() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &sample_hook()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("sample.json")).unwrap();
        assert!(!raw.contains("def x"), "code should be in .py, not .json");
        assert!(raw.contains("python3.12"), "other config preserved");
    }

    #[test]
    fn py_contains_exact_code_bytes() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &sample_hook()).unwrap();
        let py = std::fs::read_to_string(dir.path().join("sample.py")).unwrap();
        // The sample's code already ends with \n; write preserves bytes exactly.
        assert_eq!(py, "def x():\n    return 1\n");
    }

    #[test]
    fn no_py_file_when_hook_has_no_code() {
        let mut hook = sample_hook();
        // Remove the code field
        if let Value::Object(map) = &mut hook.config {
            map.remove("code");
        }
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &hook).unwrap();
        assert!(dir.path().join("sample.json").exists());
        assert!(!dir.path().join("sample.py").exists());
    }

    #[test]
    fn serialize_hook_returns_json_and_code() {
        let h = sample_hook();
        let (bytes, code) = serialize_hook(&h).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(!s.contains("def x"), "code should be extracted from json");
        assert_eq!(code.as_deref(), Some("def x():\n    return 1\n"));
    }

    #[test]
    fn round_trip_with_code() {
        let dir = TempDir::new().unwrap();
        let original = sample_hook();
        write_hook(dir.path(), "sample", &original).unwrap();
        let read = read_hook(dir.path(), "sample").unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn round_trip_without_code() {
        let mut hook = sample_hook();
        if let Value::Object(map) = &mut hook.config {
            map.remove("code");
        }
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "no-code", &hook).unwrap();
        let read = read_hook(dir.path(), "no-code").unwrap();
        assert_eq!(hook, read);
    }

    #[test]
    fn read_missing_file_errors_with_path() {
        let dir = TempDir::new().unwrap();
        let err = read_hook(dir.path(), "nope").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nope.json"), "error should name the path: {msg}");
    }

    #[test]
    fn read_with_unicode_code() {
        let dir = TempDir::new().unwrap();
        let mut hook = sample_hook();
        if let Value::Object(map) = &mut hook.config {
            map.insert(
                "code".to_string(),
                Value::String("# žluťoučký kůň\nprint('ok')".to_string()),
            );
        }
        write_hook(dir.path(), "unicode", &hook).unwrap();
        let read = read_hook(dir.path(), "unicode").unwrap();
        assert_eq!(hook, read);
    }
}
