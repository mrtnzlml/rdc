//! Per-env overlays — declarative env-specific values that override the
//! canonical snapshot when pushing to that env. Per spec §9.
//!
//! M11: simple dotted-path keys, push-side only. JMESPath wildcards and
//! pull-side stripping deferred to a future milestone.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Overlay {
    pub version: u32,
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
    /// matching the lockfile key for queue-scoped email templates (M16).
    #[serde(default)]
    pub email_templates: BTreeMap<String, BTreeMap<String, Value>>,
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
}

/// Apply a flat dotted-path → value map onto a `serde_json::Value`. Creates
/// intermediate objects if missing. Existing values at the path are
/// overwritten unconditionally.
pub fn apply_overrides(value: &mut Value, overrides: &BTreeMap<String, Value>) {
    for (path, new_value) in overrides {
        set_at_path(value, path, new_value.clone());
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
}
