use crate::model::Hook;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

/// Returns the file extension (without dot) for a hook's extracted code,
/// based on `config.runtime`. Default is `"py"` (Python).
///
/// Rossum hooks support Python and Node.js runtimes for function hooks.
/// The `runtime` field on the wire looks like `"python3.12"`,
/// `"python3.12-secure"`, `"nodejs18.x"`, `"nodejs20.x"`, etc. We match
/// "node" (case-insensitive) for JavaScript; everything else falls back
/// to Python so older snapshots without a recognized runtime keep working.
pub fn hook_code_extension(hook: &Hook) -> &'static str {
    runtime_to_extension(
        hook.config
            .get("runtime")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    )
}

/// Same as [`hook_code_extension`] but takes the raw JSON value (used by
/// callers that have the on-disk JSON, not a typed [`Hook`] — e.g. the
/// sync adapter and the push scanner).
pub fn hook_code_extension_from_value(value: &serde_json::Value) -> &'static str {
    let runtime = value
        .get("config")
        .and_then(|c| c.get("runtime"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    runtime_to_extension(runtime)
}

fn runtime_to_extension(runtime: &str) -> &'static str {
    if runtime.to_ascii_lowercase().starts_with("node") {
        "js"
    } else {
        "py"
    }
}

/// Write a hook to disk: a JSON file under `<dir>/<slug>.json` and, if the hook
/// has inline code, a sibling `<slug>.<ext>` file. The extension is derived
/// from `config.runtime` — `.js` for Node.js runtimes (anything starting
/// with `node`, case-insensitive), `.py` otherwise. The `code` field of
/// `config` is stripped from the JSON to avoid duplication; the sidecar
/// file becomes the source of truth.
///
/// If a stale sidecar with the *other* extension exists (e.g. a `.py`
/// from when the hook used Python), it is removed so the snapshot stays
/// canonical.
///
/// Returns the JSON bytes written (post-extraction, with trailing newline).
pub fn write_hook(dir: &Path, slug: &str, hook: &Hook) -> Result<Vec<u8>> {
    let (json_bytes, code) = serialize_hook(hook)?;
    write_atomic(&dir.join(format!("{slug}.json")), &json_bytes)?;
    let ext = hook_code_extension(hook);
    if let Some(code) = code {
        // Write code bytes exactly as received. Preserves byte-exact round-trip
        // through the codec (read_hook returns Hook with identical config.code).
        write_atomic(&dir.join(format!("{slug}.{ext}")), code.as_bytes())?;
    }
    // Sweep a stale sidecar with the opposite extension (runtime change,
    // or a botched manual edit). The current sidecar — if any — is the
    // canonical form for this runtime.
    let other_ext = other_extension(ext);
    let stale = dir.join(format!("{slug}.{other_ext}"));
    if stale.exists() {
        let _ = std::fs::remove_file(&stale);
    }
    Ok(json_bytes)
}

/// Remove `config.code` (a string) from a serialized hook Value and return
/// it for the sidecar.
fn split_hook_code(json_value: &mut Value) -> Option<String> {
    json_value
        .get_mut("config")
        .and_then(|c| c.as_object_mut())
        .and_then(|m| m.remove("code"))
        .and_then(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        })
}

/// Serialize a hook to its on-disk byte form WITHOUT writing. Returns the JSON
/// bytes (post-extraction, with trailing newline) and the optional extracted
/// code string. Used by pull/push drivers to compute `hook_combined_hash`
/// before deciding whether to write or send.
pub fn serialize_hook(hook: &Hook) -> Result<(Vec<u8>, Option<String>)> {
    let mut json_value = serde_json::to_value(hook).context("serializing hook to value")?;

    let code = split_hook_code(&mut json_value);

    // Redact the server-managed runtime `status` to a constant sentinel so it
    // doesn't churn on disk or surface as a spurious sync conflict — Rossum
    // flips it pending→ready→failed live, independent of any local edit.
    // Mirrors the queue `counts` redaction; the push path strips `status`
    // entirely via `strip_for_create`, so the sentinel is never sent back.
    crate::snapshot::create::redact_for_disk(&mut json_value, "hooks");

    sort_queues(&mut json_value);

    crate::snapshot::key_order::strip_hidden_fields_recursive(&mut json_value);
    crate::snapshot::key_order::reorder_top_level(
        &mut json_value,
        crate::snapshot::key_order::HOOK_KEY_ORDER,
    );

    let mut bytes = serde_json::to_vec_pretty(&json_value).context("serializing hook json")?;
    bytes.push(b'\n');
    Ok((bytes, code))
}

/// Sort the top-level `queues` URL array in place.
///
/// The Rossum API returns a hook's queue membership in the server's
/// internal numeric-id order, which is arbitrary from rdc's perspective
/// and not stable across pulls (a queue rebuilt or re-attached can
/// shift positions). Without normalising at serialize time, the on-disk
/// JSON would churn every time the server reshuffles, producing false
/// drift on sync and noisy git diffs. Hook→queue membership is a set,
/// not a list — order carries no meaning — so a deterministic
/// lexicographic sort is safe.
///
/// Defensive: only sorts when every element is a string. Mixed arrays
/// (which shouldn't happen for `queues` but might if someone hand-edits
/// the file) pass through untouched rather than panicking.
fn sort_queues(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Some(Value::Array(queues)) = obj.get_mut("queues") else {
        return;
    };
    if !queues.iter().all(|v| matches!(v, Value::String(_))) {
        return;
    }
    queues.sort_by(|a, b| match (a, b) {
        (Value::String(s1), Value::String(s2)) => s1.cmp(s2),
        _ => std::cmp::Ordering::Equal,
    });
}

/// Write only the hook's sidecar code file (extracted from `config.code`).
/// The `ext` argument is the file extension (`"py"` or `"js"`) and must be
/// derived from the hook's runtime by the caller via
/// [`hook_code_extension`] / [`hook_code_extension_from_value`]. Used by
/// pull drivers that compute the JSON write decision separately and only
/// need to overwrite the code file.
pub fn write_hook_code(dir: &Path, slug: &str, code: &str, ext: &str) -> Result<()> {
    let code_path = dir.join(format!("{slug}.{ext}"));
    write_atomic(&code_path, code.as_bytes())?;
    Ok(())
}

/// Read a hook back from disk as an untyped `Value`: load `<dir>/<slug>.json`,
/// then if `<dir>/<slug>.<ext>` exists (extension derived from
/// `config.runtime`), splice its contents back into `config.code`. The
/// Value is NOT yet deserialized into `Hook` — callers who need to apply
/// overlay overrides before typing call this directly, then
/// `serde_json::from_value(value)?`.
///
/// If the runtime-derived sidecar is missing, falls back to the *other*
/// extension. This is defensive — it handles the case where a user
/// switched `config.runtime` in JSON but hasn't renamed the sidecar yet.
/// The next `write_hook` normalizes the layout.
pub fn read_hook_value(dir: &Path, slug: &str) -> Result<Value> {
    let json_path = dir.join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let mut value: Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", json_path.display()))?;

    let ext = hook_code_extension_from_value(&value);
    let primary = dir.join(format!("{slug}.{ext}"));
    let code_path = if primary.exists() {
        Some(primary)
    } else {
        // Defensive fallback: check the other extension. This handles
        // the case where `config.runtime` was changed in JSON but the
        // sidecar wasn't renamed yet. Push/sync callers will normalize
        // on next write.
        let alt = dir.join(format!("{slug}.{}", other_extension(ext)));
        if alt.exists() { Some(alt) } else { None }
    };
    if let Some(p) = code_path {
        let code =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        // The sidecar file is the byte-exact canonical form (write_hook
        // preserves bytes). No trailing-newline normalization on read either.
        if let Some(config) = value.get_mut("config").and_then(|c| c.as_object_mut()) {
            config.insert("code".to_string(), Value::String(code));
        }
    }

    Ok(value)
}

/// Read a hook back from disk into a typed `Hook`. Splices the sidecar
/// code file (`.py` or `.js`) back into `config.code` first, so the
/// in-memory `Hook` is byte-for-byte equivalent to what was originally
/// serialized. Fails if required typed fields are missing —
/// overlay-stripping callers should use `read_hook_value` + apply
/// overlay + `from_value` instead.
pub fn read_hook(dir: &Path, slug: &str) -> Result<Hook> {
    let value = read_hook_value(dir, slug)?;
    let json_path = dir.join(format!("{slug}.json"));
    let hook: Hook = serde_json::from_value(value)
        .with_context(|| format!("deserializing hook from {}", json_path.display()))?;
    Ok(hook)
}

/// Helper: given one of `"py"` / `"js"`, return the other. Used when
/// sweeping stale sidecars after a runtime change.
fn other_extension(ext: &str) -> &'static str {
    if ext == "py" { "js" } else { "py" }
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

    fn js_hook() -> Hook {
        let v = json!({
            "id": 2,
            "url": "https://example/api/v1/hooks/2",
            "name": "Sample JS",
            "type": "function",
            "queues": [],
            "events": [],
            "config": {
                "runtime": "nodejs20.x",
                "code": "module.exports = (input) => input;\n"
            }
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn runtime_python_classic() {
        assert_eq!(runtime_to_extension("python3.12"), "py");
    }

    #[test]
    fn runtime_python_secure() {
        assert_eq!(runtime_to_extension("python3.12-secure"), "py");
    }

    #[test]
    fn runtime_nodejs18() {
        assert_eq!(runtime_to_extension("nodejs18.x"), "js");
    }

    #[test]
    fn runtime_nodejs20() {
        assert_eq!(runtime_to_extension("nodejs20.x"), "js");
    }

    #[test]
    fn runtime_is_case_insensitive() {
        assert_eq!(runtime_to_extension("NODEJS20.X"), "js");
        assert_eq!(runtime_to_extension("NodeJS18.x"), "js");
    }

    #[test]
    fn runtime_bare_node_is_js() {
        // Defensive: any string that begins with "node" maps to .js so
        // future runtime variants ("node22", "node-latest", …) keep
        // doing the right thing without code changes.
        assert_eq!(runtime_to_extension("node"), "js");
        assert_eq!(runtime_to_extension("node22"), "js");
    }

    #[test]
    fn runtime_empty_defaults_to_py() {
        assert_eq!(runtime_to_extension(""), "py");
    }

    #[test]
    fn runtime_garbage_defaults_to_py() {
        assert_eq!(runtime_to_extension("ruby3.2"), "py");
        assert_eq!(runtime_to_extension("???"), "py");
    }

    #[test]
    fn hook_code_extension_reads_config_runtime() {
        let py = sample_hook();
        assert_eq!(hook_code_extension(&py), "py");
        let js = js_hook();
        assert_eq!(hook_code_extension(&js), "js");
    }

    #[test]
    fn hook_code_extension_missing_runtime_field_defaults_to_py() {
        let v = json!({
            "id": 9, "url": "u", "name": "N", "type": "function",
            "config": {}
        });
        let h: Hook = serde_json::from_value(v).unwrap();
        assert_eq!(hook_code_extension(&h), "py");
    }

    #[test]
    fn hook_code_extension_from_value_handles_missing_config() {
        let v = json!({
            "id": 9, "url": "u", "name": "N", "type": "webhook"
        });
        assert_eq!(hook_code_extension_from_value(&v), "py");
    }

    #[test]
    fn hook_code_extension_from_value_reads_node_runtime() {
        let v = json!({
            "id": 9, "url": "u", "name": "N", "type": "function",
            "config": { "runtime": "nodejs18.x" }
        });
        assert_eq!(hook_code_extension_from_value(&v), "js");
    }

    #[test]
    fn writes_json_and_py() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &sample_hook()).unwrap();
        assert!(dir.path().join("sample.json").exists());
        assert!(dir.path().join("sample.py").exists());
        assert!(!dir.path().join("sample.js").exists());
    }

    #[test]
    fn write_hook_js_hook_writes_js_sidecar() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &js_hook()).unwrap();
        assert!(dir.path().join("sample.json").exists());
        assert!(
            dir.path().join("sample.js").exists(),
            "JS hook should write .js sidecar"
        );
        assert!(
            !dir.path().join("sample.py").exists(),
            "JS hook must not write a .py sidecar"
        );
        let body = std::fs::read_to_string(dir.path().join("sample.js")).unwrap();
        assert_eq!(body, "module.exports = (input) => input;\n");
    }

    #[test]
    fn write_hook_python_hook_unchanged() {
        // Regression: pre-existing Python sample_hook test continues to pass.
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &sample_hook()).unwrap();
        let py = std::fs::read_to_string(dir.path().join("sample.py")).unwrap();
        assert_eq!(py, "def x():\n    return 1\n");
    }

    #[test]
    fn write_hook_js_removes_stale_py_sidecar() {
        let dir = TempDir::new().unwrap();
        // Pre-create a leftover .py sidecar from a previous python run.
        std::fs::write(dir.path().join("sample.py"), b"# old\n").unwrap();
        write_hook(dir.path(), "sample", &js_hook()).unwrap();
        assert!(
            !dir.path().join("sample.py").exists(),
            ".py should be swept"
        );
        assert!(dir.path().join("sample.js").exists());
    }

    #[test]
    fn write_hook_python_removes_stale_js_sidecar() {
        let dir = TempDir::new().unwrap();
        // Pre-create a leftover .js sidecar from a previous nodejs run.
        std::fs::write(dir.path().join("sample.js"), b"// old\n").unwrap();
        write_hook(dir.path(), "sample", &sample_hook()).unwrap();
        assert!(
            !dir.path().join("sample.js").exists(),
            ".js should be swept"
        );
        assert!(dir.path().join("sample.py").exists());
    }

    #[test]
    fn unlisted_extras_preserve_api_order() {
        // The API returned extras in a non-alphabetical order:
        // sideload, guide, extension_image_url, metadata. None of these
        // are in HOOK_KEY_ORDER, so they should land on disk in the
        // same order the API delivered them — NOT alphabetical.
        let v = json!({
            "id": 1,
            "url": "u",
            "name": "n",
            "type": "function",
            "queues": [],
            "events": [],
            "config": { "runtime": "python3.12" },
            "sideload": [],
            "guide": null,
            "extension_image_url": null,
            "metadata": {},
        });
        let hook: Hook = serde_json::from_value(v).unwrap();
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "h", &hook).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("h.json")).unwrap();
        let p = |k: &str| {
            raw.find(&format!("\"{k}\":"))
                .unwrap_or_else(|| panic!("missing {k}"))
        };
        // Listed keys: id, name, events, queues come first (in HOOK_KEY_ORDER).
        // Then unlisted typed fields in struct decl order: url, type, config.
        // Then unlisted extras in API order: sideload, guide,
        // extension_image_url, metadata.
        let order = [
            "id",
            "name",
            "events",
            "queues",
            "url",
            "type",
            "config",
            "sideload",
            "guide",
            "extension_image_url",
            "metadata",
        ];
        let mut last = 0;
        for k in order {
            let pos = p(k);
            assert!(
                pos >= last,
                "key {k} out of order at byte {pos} (last was {last})\n--- json ---\n{raw}",
            );
            last = pos;
        }
    }

    #[test]
    fn modified_at_is_stripped_from_disk() {
        let dir = TempDir::new().unwrap();
        let v = json!({
            "id": 1,
            "url": "u",
            "name": "n",
            "type": "function",
            "queues": [],
            "events": [],
            "config": { "runtime": "python3.12" },
            "modified_at": "2026-05-22T08:42:15Z",
            "modifier": "https://x/api/v1/users/4",
        });
        let hook: Hook = serde_json::from_value(v).unwrap();
        write_hook(dir.path(), "h", &hook).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("h.json")).unwrap();
        assert!(
            !raw.contains("modified_at"),
            "modified_at must not appear on disk: {raw}"
        );
        // modifier stays — only modified_at is in HIDDEN_FIELDS today.
        assert!(
            raw.contains("modifier"),
            "modifier must still appear on disk: {raw}"
        );
    }

    #[test]
    fn json_keys_are_ordered_by_importance() {
        // Hook with all eight importance keys present + a few "rest"
        // keys (typed: url, type, config; extras: metadata).
        let v = json!({
            "id": 856489,
            "url": "https://example/api/v1/hooks/856489",
            "name": "Validator: invoices",
            "description": "Reject totals < 0",
            "type": "function",
            "active": true,
            "queues": ["https://example/api/v1/queues/1"],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" },
            "settings": { "tolerance": 0.01 },
            "run_after": [],
            "metadata": { "owner": "ap" },
        });
        let hook: Hook = serde_json::from_value(v).unwrap();
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "v-inv", &hook).unwrap();

        let raw = std::fs::read_to_string(dir.path().join("v-inv.json")).unwrap();
        // Find the byte offset of each key's quoted occurrence and
        // assert the prescribed ordering. Using `find` on quoted form
        // avoids matching the same substring inside an unrelated value.
        let q = |k: &str| format!("\"{k}\":");
        let pos = |k: &str| {
            raw.find(&q(k))
                .unwrap_or_else(|| panic!("missing key {k} in: {raw}"))
        };
        let order = [
            "id",
            "name",
            "description",
            "active",
            "events",
            "settings",
            "queues",
            "run_after",
            // Then unlisted-typed-fields in struct decl order,
            // then alphabetical extras.
            "url",
            "type",
            "config",
            "metadata",
        ];
        let mut last = 0;
        for k in order {
            let p = pos(k);
            assert!(
                p >= last,
                "key {k} appeared out of order at byte {p} (previous was at {last})\n--- json ---\n{raw}",
            );
            last = p;
        }
    }

    #[test]
    fn content_hash_invariant_under_top_level_reordering() {
        // The two byte streams below differ only in top-level key order;
        // canonicalize_for_hash must produce the same bytes for both.
        let ordered = br#"{"id":1,"name":"n","events":[],"queues":[]}"#;
        let scrambled = br#"{"queues":[],"name":"n","id":1,"events":[]}"#;
        let h1 = crate::state::content_hash(ordered, &crate::state::Lockfile::default());
        let h2 = crate::state::content_hash(scrambled, &crate::state::Lockfile::default());
        assert_eq!(
            h1, h2,
            "content_hash must be invariant under key reordering"
        );
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
    fn json_does_not_contain_code_field_for_js_hook() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &js_hook()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("sample.json")).unwrap();
        assert!(
            !raw.contains("module.exports"),
            "code should be in .js, not .json: {raw}"
        );
        assert!(raw.contains("nodejs20.x"), "runtime preserved: {raw}");
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
        assert!(!dir.path().join("sample.js").exists());
    }

    #[test]
    fn no_js_file_when_js_hook_has_no_code() {
        let mut hook = js_hook();
        if let Value::Object(map) = &mut hook.config {
            map.remove("code");
        }
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &hook).unwrap();
        assert!(dir.path().join("sample.json").exists());
        assert!(!dir.path().join("sample.js").exists());
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
    fn read_hook_round_trips_js() {
        let dir = TempDir::new().unwrap();
        let original = js_hook();
        write_hook(dir.path(), "sample-js", &original).unwrap();
        let read = read_hook(dir.path(), "sample-js").unwrap();
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
        assert!(
            msg.contains("nope.json"),
            "error should name the path: {msg}"
        );
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

    #[test]
    fn read_hook_value_falls_back_to_other_ext() {
        // Pathological state: JSON declares python runtime but the
        // on-disk sidecar is `.js`. The defensive fallback must still
        // splice the code so push/sync sees a complete object; the next
        // pull/write normalizes the layout.
        let dir = TempDir::new().unwrap();
        let json = json!({
            "id": 7, "url": "u", "name": "Mismatch",
            "type": "function",
            "queues": [], "events": [],
            "config": { "runtime": "python3.12" }
        });
        std::fs::write(
            dir.path().join("mismatch.json"),
            serde_json::to_vec_pretty(&json).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.path().join("mismatch.js"), b"// stray\n").unwrap();

        let v = read_hook_value(dir.path(), "mismatch").unwrap();
        let code = v
            .get("config")
            .and_then(|c| c.get("code"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        assert_eq!(
            code, "// stray\n",
            "defensive fallback should splice the existing sidecar regardless of declared runtime"
        );
    }

    #[test]
    fn read_hook_value_prefers_runtime_derived_ext() {
        // When both `.py` and `.js` are present (rare), prefer the one
        // matching the declared runtime. The other is treated as stale.
        let dir = TempDir::new().unwrap();
        let json = json!({
            "id": 8, "url": "u", "name": "Both",
            "type": "function",
            "queues": [], "events": [],
            "config": { "runtime": "nodejs20.x" }
        });
        std::fs::write(
            dir.path().join("both.json"),
            serde_json::to_vec_pretty(&json).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.path().join("both.py"), b"# stale\n").unwrap();
        std::fs::write(dir.path().join("both.js"), b"// canonical\n").unwrap();
        let v = read_hook_value(dir.path(), "both").unwrap();
        let code = v
            .get("config")
            .and_then(|c| c.get("code"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        assert_eq!(code, "// canonical\n");
    }

    fn hook_with_queues(urls: &[&str]) -> Hook {
        let v = json!({
            "id": 99,
            "url": "https://example/api/v1/hooks/99",
            "name": "QueuedHook",
            "type": "function",
            "queues": urls,
            "events": [],
            "config": { "runtime": "python3.12" }
        });
        serde_json::from_value(v).unwrap()
    }

    fn queues_array(bytes: &[u8]) -> Vec<String> {
        let v: Value = serde_json::from_slice(bytes).unwrap();
        v.get("queues")
            .and_then(|q| q.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|x| x.as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn serialize_hook_sorts_queues_lexicographically() {
        // Server returned this set in id-order which happens to be
        // reverse-lex. After serialize, on-disk order must be sorted.
        let h = hook_with_queues(&[
            "https://example/api/v1/queues/30",
            "https://example/api/v1/queues/10",
            "https://example/api/v1/queues/20",
        ]);
        let (bytes, _) = serialize_hook(&h).unwrap();
        assert_eq!(
            queues_array(&bytes),
            vec![
                "https://example/api/v1/queues/10".to_string(),
                "https://example/api/v1/queues/20".to_string(),
                "https://example/api/v1/queues/30".to_string(),
            ]
        );
    }

    #[test]
    fn serialize_hook_two_orderings_produce_identical_bytes() {
        // The whole point of the sort: two pulls returning the same set
        // of queues in different orders must produce byte-equal on-disk
        // JSON, so sync sees no drift and git stays quiet.
        let a = hook_with_queues(&[
            "https://example/api/v1/queues/2",
            "https://example/api/v1/queues/1",
            "https://example/api/v1/queues/3",
        ]);
        let b = hook_with_queues(&[
            "https://example/api/v1/queues/3",
            "https://example/api/v1/queues/2",
            "https://example/api/v1/queues/1",
        ]);
        let (bytes_a, _) = serialize_hook(&a).unwrap();
        let (bytes_b, _) = serialize_hook(&b).unwrap();
        assert_eq!(bytes_a, bytes_b);
    }

    #[test]
    fn serialize_hook_handles_empty_and_single_queue() {
        let empty = hook_with_queues(&[]);
        let (bytes, _) = serialize_hook(&empty).unwrap();
        assert_eq!(queues_array(&bytes), Vec::<String>::new());

        let single = hook_with_queues(&["https://example/api/v1/queues/42"]);
        let (bytes, _) = serialize_hook(&single).unwrap();
        assert_eq!(
            queues_array(&bytes),
            vec!["https://example/api/v1/queues/42".to_string()]
        );
    }

    #[test]
    fn serialize_hook_queue_sort_round_trips_through_read() {
        // After a write→read cycle, the in-memory Hook reflects the
        // sorted on-disk order — so push/diff downstream see stable
        // membership regardless of what order the server first
        // returned.
        let dir = TempDir::new().unwrap();
        let h = hook_with_queues(&[
            "https://example/api/v1/queues/9",
            "https://example/api/v1/queues/1",
            "https://example/api/v1/queues/5",
        ]);
        write_hook(dir.path(), "qh", &h).unwrap();
        let read = read_hook(dir.path(), "qh").unwrap();
        assert_eq!(
            read.queues,
            vec![
                "https://example/api/v1/queues/1".to_string(),
                "https://example/api/v1/queues/5".to_string(),
                "https://example/api/v1/queues/9".to_string(),
            ]
        );
    }
}
