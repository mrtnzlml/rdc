//! Per-env overlays — declarative env-specific values that override the
//! canonical snapshot when pushing to that env. Per spec §9.
//!
//! Bidirectional: applied on push (merged into the outbound PATCH body)
//! and stripped on pull (the snapshot stays in canonical pre-overlay
//! form so cross-env diffs and deploys are quiet). The override format
//! is simple dotted-path keys; JMESPath wildcards / array filters are
//! out of scope for v1.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct Defaults {
    /// Fallback `token_owner` URL applied to every store extension that
    /// has no per-hook `token_owner` override. Set automatically by
    /// `rdc deploy`'s interactive picker on first deploy; hand-editable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_extension_token_owner: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Overlay {
    pub version: u32,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub hooks: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(default)]
    pub rules: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(default)]
    pub labels: BTreeMap<String, BTreeMap<String, Value>>,
    /// Schema overrides keyed by queue slug (schemas use the queue's slug
    /// since each queue has exactly one schema). Useful for per-env
    /// classifier thresholds, queue-specific defaults, etc.
    #[serde(default)]
    pub schemas: BTreeMap<String, BTreeMap<String, Value>>,
    /// Queue overrides keyed by queue slug. Useful for per-env automation
    /// levels, score thresholds, locale, etc.
    #[serde(default)]
    pub queues: BTreeMap<String, BTreeMap<String, Value>>,
    /// Inbox overrides keyed by queue slug (one inbox per queue).
    #[serde(default)]
    pub inboxes: BTreeMap<String, BTreeMap<String, Value>>,
    /// Email-template overrides keyed by `<ws_slug>/<q_slug>/<template_slug>`,
    /// matching the lockfile key for queue-scoped email templates.
    #[serde(default)]
    pub email_templates: BTreeMap<String, BTreeMap<String, Value>>,
    /// Engine overrides keyed by engine slug.
    #[serde(default)]
    pub engines: BTreeMap<String, BTreeMap<String, Value>>,
    /// Engine field overrides keyed by engine field slug.
    #[serde(default)]
    pub engine_fields: BTreeMap<String, BTreeMap<String, Value>>,
}

impl Overlay {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let overlay: Overlay = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(overlay))
    }

    pub fn hook(&self, slug: &str) -> Option<&BTreeMap<String, Value>> {
        self.hooks.get(slug)
    }

    pub fn rule(&self, slug: &str) -> Option<&BTreeMap<String, Value>> {
        self.rules.get(slug)
    }

    pub fn label(&self, slug: &str) -> Option<&BTreeMap<String, Value>> {
        self.labels.get(slug)
    }

    pub fn schema(&self, slug: &str) -> Option<&BTreeMap<String, Value>> {
        self.schemas.get(slug)
    }

    pub fn queue(&self, slug: &str) -> Option<&BTreeMap<String, Value>> {
        self.queues.get(slug)
    }

    pub fn inbox(&self, slug: &str) -> Option<&BTreeMap<String, Value>> {
        self.inboxes.get(slug)
    }

    pub fn email_template(&self, key: &str) -> Option<&BTreeMap<String, Value>> {
        self.email_templates.get(key)
    }

    pub fn engine(&self, slug: &str) -> Option<&BTreeMap<String, Value>> {
        self.engines.get(slug)
    }

    pub fn engine_field(&self, slug: &str) -> Option<&BTreeMap<String, Value>> {
        self.engine_fields.get(slug)
    }
}

/// Apply a flat dotted-path → value map onto a `serde_json::Value`. Creates
/// intermediate objects if missing. Existing values at the path are
/// overwritten unconditionally.
pub fn apply_overrides(value: &mut Value, overrides: &BTreeMap<String, Value>) {
    for (path, new_value) in overrides {
        set_at_path(value, path, new_value.clone());
    }
}

/// Remove the leaf key at each dotted path from `value`. Intermediate
/// objects that become empty are NOT cleaned up — the strip is shallow.
/// Missing paths are silently ignored.
///
/// Used on the pull side (per spec §9.3) so the snapshot reflects the
/// canonical form (without env-specific overlay values), keeping diffs
/// across envs quiet. The push side re-applies the overlay before
/// sending, so the round-trip is preserved.
pub fn strip_paths(value: &mut Value, paths: &BTreeMap<String, Value>) {
    for path in paths.keys() {
        delete_at_path(value, path);
    }
}

fn delete_at_path(value: &mut Value, path: &str) {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.is_empty() {
        return;
    }
    let mut current = value;
    for segment in &segments[..segments.len() - 1] {
        let Some(obj) = current.as_object_mut() else { return };
        let Some(next) = obj.get_mut(*segment) else { return };
        current = next;
    }
    if let Some(obj) = current.as_object_mut() {
        obj.remove(*segments.last().unwrap());
    }
}

fn set_at_path(value: &mut Value, path: &str, new_value: Value) {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.is_empty() {
        return;
    }
    let mut current = value;
    for segment in &segments[..segments.len() - 1] {
        if !current.is_object() {
            *current = Value::Object(Default::default());
        }
        let obj = current.as_object_mut().expect("just made object");
        let entry = obj.entry((*segment).to_string()).or_insert(Value::Object(Default::default()));
        if !entry.is_object() {
            *entry = Value::Object(Default::default());
        }
        current = entry;
    }
    if !current.is_object() {
        *current = Value::Object(Default::default());
    }
    let obj = current.as_object_mut().expect("just made object");
    obj.insert(segments.last().unwrap().to_string(), new_value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn load_returns_none_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");
        let res = Overlay::load(&path).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn load_parses_valid_overlay() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");
        std::fs::write(&path, r#"
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"config.runtime" = "python3.12-secure"
"#).unwrap();
        let overlay = Overlay::load(&path).unwrap().unwrap();
        assert_eq!(overlay.version, 1);
        let hook = overlay.hook("validator-invoices").unwrap();
        assert_eq!(hook.get("name").unwrap(), &Value::String("Validator (PROD)".into()));
        assert_eq!(hook.get("config.runtime").unwrap(), &Value::String("python3.12-secure".into()));
    }

    #[test]
    fn load_parses_schema_overrides() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");
        std::fs::write(&path, r#"
version = 1

[schemas.cost-invoices]
"settings.default_score_threshold" = 0.95
"#).unwrap();
        let overlay = Overlay::load(&path).unwrap().unwrap();
        let s = overlay.schema("cost-invoices").unwrap();
        let v = s.get("settings.default_score_threshold").unwrap();
        assert_eq!(v.as_f64().unwrap(), 0.95);
    }

    #[test]
    fn apply_simple_top_level_override() {
        let mut v = json!({ "name": "Original", "id": 1 });
        let mut overrides = BTreeMap::new();
        overrides.insert("name".to_string(), Value::String("Override".into()));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["name"], Value::String("Override".into()));
        assert_eq!(v["id"], Value::Number(1.into()));
    }

    #[test]
    fn apply_nested_dotted_override() {
        let mut v = json!({ "config": { "runtime": "old", "other": "kept" } });
        let mut overrides = BTreeMap::new();
        overrides.insert("config.runtime".to_string(), Value::String("new".into()));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["config"]["runtime"], Value::String("new".into()));
        assert_eq!(v["config"]["other"], Value::String("kept".into()));
    }

    #[test]
    fn apply_creates_intermediate_objects_when_missing() {
        let mut v = json!({ "name": "x" });
        let mut overrides = BTreeMap::new();
        overrides.insert("settings.deep.value".to_string(), Value::String("created".into()));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["settings"]["deep"]["value"], Value::String("created".into()));
        assert_eq!(v["name"], Value::String("x".into()));
    }

    #[test]
    fn apply_replaces_non_object_at_intermediate_path() {
        let mut v = json!({ "config": "scalar" });
        let mut overrides = BTreeMap::new();
        overrides.insert("config.runtime".to_string(), Value::String("py".into()));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["config"]["runtime"], Value::String("py".into()));
    }

    #[test]
    fn strip_removes_top_level_key() {
        let mut v = json!({ "name": "Validator (PROD)", "id": 1 });
        let mut paths = BTreeMap::new();
        paths.insert("name".to_string(), Value::Null);
        strip_paths(&mut v, &paths);
        assert!(v.get("name").is_none(), "name should be removed");
        assert_eq!(v["id"], Value::Number(1.into()), "other fields untouched");
    }

    #[test]
    fn strip_removes_nested_key_keeps_parent() {
        let mut v = json!({ "config": { "runtime": "py-prod", "timeout": 30 } });
        let mut paths = BTreeMap::new();
        paths.insert("config.runtime".to_string(), Value::Null);
        strip_paths(&mut v, &paths);
        assert!(v["config"].get("runtime").is_none());
        assert_eq!(v["config"]["timeout"], Value::Number(30.into()));
    }

    #[test]
    fn strip_silently_ignores_missing_path() {
        let mut v = json!({ "id": 1 });
        let mut paths = BTreeMap::new();
        paths.insert("config.missing.deep".to_string(), Value::Null);
        strip_paths(&mut v, &paths);
        assert_eq!(v, json!({ "id": 1 }));
    }

    #[test]
    fn strip_handles_multiple_paths() {
        let mut v = json!({
            "name": "Validator",
            "config": { "runtime": "py", "timeout": 30 }
        });
        let mut paths = BTreeMap::new();
        paths.insert("name".to_string(), Value::Null);
        paths.insert("config.runtime".to_string(), Value::Null);
        strip_paths(&mut v, &paths);
        assert!(v.get("name").is_none());
        assert!(v["config"].get("runtime").is_none());
        assert_eq!(v["config"]["timeout"], Value::Number(30.into()));
    }

    #[test]
    fn defaults_section_parses_store_extension_token_owner() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");
        std::fs::write(&path, r#"
version = 1

[defaults]
store_extension_token_owner = "https://prod/api/v1/users/938493"

[hooks.master-data-hub]
"name" = "MDH (PROD)"
"#).unwrap();
        let overlay = Overlay::load(&path).unwrap().unwrap();
        assert_eq!(
            overlay.defaults.store_extension_token_owner.as_deref(),
            Some("https://prod/api/v1/users/938493")
        );
        assert!(overlay.hook("master-data-hub").is_some());
    }

    #[test]
    fn defaults_section_is_optional() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");
        std::fs::write(&path, "version = 1\n").unwrap();
        let overlay = Overlay::load(&path).unwrap().unwrap();
        assert!(overlay.defaults.store_extension_token_owner.is_none());
    }
}
