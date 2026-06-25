use anyhow::{Context, Result};
use rdc::state::lockfile::Lockfile;
use std::path::Path;

#[allow(dead_code)]
pub fn load_lockfile(project: &Path, env: &str) -> Result<Lockfile> {
    let p = project.join(format!(".rdc/state/{env}.lock.json"));
    let raw = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

/// Sorted slugs recorded under `kind` in the lockfile (e.g. "queues").
#[allow(dead_code)]
pub fn lockfile_keys(lf: &Lockfile, kind: &str) -> Vec<String> {
    let mut v: Vec<String> = lf
        .objects
        .get(kind)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    v.sort();
    v
}

const VOLATILE: &[&str] = &[
    "id", "url", "modified_at", "created_at", "created_by", "modified_by", "email",
];

/// Recursively drop server-assigned / per-env fields so two snapshots from
/// different runs compare equal.
pub fn strip_volatile(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for k in VOLATILE {
                map.remove(*k);
            }
            for (_k, child) in map.iter_mut() {
                strip_volatile(child);
            }
        }
        serde_json::Value::Array(items) => {
            for it in items {
                strip_volatile(it);
            }
        }
        _ => {}
    }
}

/// Dotted-path getter, e.g. `field(&v, "config.runtime")`.
pub fn field<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_removes_volatile_recursively() {
        let mut v = json!({
            "id": 1, "name": "x",
            "nested": { "url": "u", "keep": true },
            "list": [{ "modified_at": "t", "ok": 1 }],
        });
        strip_volatile(&mut v);
        assert_eq!(v, json!({
            "name": "x",
            "nested": { "keep": true },
            "list": [{ "ok": 1 }],
        }));
    }

    #[test]
    fn dotted_field_getter() {
        let v = json!({ "config": { "runtime": "python3.12" } });
        assert_eq!(field(&v, "config.runtime").unwrap(), "python3.12");
        assert!(field(&v, "config.missing").is_none());
    }
}
