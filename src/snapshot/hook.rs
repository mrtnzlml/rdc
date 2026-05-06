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
/// Returns the JSON path written.
pub fn write_hook(dir: &Path, slug: &str, hook: &Hook) -> Result<()> {
    let mut json_value = serde_json::to_value(hook)
        .context("serializing hook to value")?;

    // Extract `config.code` into a sibling .py file.
    let code = json_value
        .get_mut("config")
        .and_then(|c| c.as_object_mut())
        .and_then(|m| m.remove("code"))
        .and_then(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        });

    let json_path = dir.join(format!("{slug}.json"));
    let json_bytes = serde_json::to_vec_pretty(&json_value)
        .context("serializing hook json")?;
    let mut json_with_newline = json_bytes;
    json_with_newline.push(b'\n');
    write_atomic(&json_path, &json_with_newline)?;

    if let Some(code) = code {
        let py_path = dir.join(format!("{slug}.py"));
        let mut bytes = code.into_bytes();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        write_atomic(&py_path, &bytes)?;
    }

    Ok(())
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
    fn py_contains_code_with_trailing_newline() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &sample_hook()).unwrap();
        let py = std::fs::read_to_string(dir.path().join("sample.py")).unwrap();
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
}
