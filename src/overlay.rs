//! Per-env overlays — declarative env-specific values applied when MIGRATING
//! a snapshot from one environment to another. Per spec §9.
//!
//! C-1 model (migrate-only): an overlay maps each object's fields to the value
//! the TARGET env should use. `migrate` applies them (via [`apply_overrides`]) so
//! the promoted snapshot carries that env's real values. Pull, push, and sync
//! treat overlay-managed fields as ordinary content — they are NOT stripped on
//! pull nor re-applied on push — so each env's snapshot shows its real values
//! on disk and the snapshot itself is the source of truth for what is pushed.
//!
//! (Previously the overlay was bidirectional — applied on push, stripped on
//! pull — which kept the snapshot env-agnostic but hid the value from disk.
//! C-1 trades that for visibility: snapshots are env-specific and self-evident.)
//!
//! Overrides are native TOML: each value is deep-merged onto the object, so a
//! nested table (or unquoted dotted key) sets a nested field while preserving
//! its siblings, and a key is taken literally (a dot in a key is part of the
//! name, not a path). JMESPath wildcards / array filters are out of scope for v1.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub const OVERLAY_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Overlay {
    pub version: u32,
    /// Hook overrides keyed by hook slug. The reserved slug `"*"` is a
    /// kind-wide default applied to EVERY hook (e.g. `[hooks."*"] token_owner`),
    /// with a real per-slug entry winning over it. `"*"` is not a valid Rossum
    /// slug, so it never collides with a real hook. The same `"*"` convention
    /// applies to every other kind below; for the compound-key kinds
    /// (`engine_fields` = `<engine>/<field>`, `email_templates` =
    /// `<ws>/<q>/<template>`) `"*"` matches the WHOLE key — all objects of that
    /// kind — not a single path segment.
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

impl Default for Overlay {
    fn default() -> Self {
        Self {
            version: OVERLAY_VERSION,
            hooks: BTreeMap::new(),
            rules: BTreeMap::new(),
            labels: BTreeMap::new(),
            schemas: BTreeMap::new(),
            queues: BTreeMap::new(),
            inboxes: BTreeMap::new(),
            email_templates: BTreeMap::new(),
            engines: BTreeMap::new(),
            engine_fields: BTreeMap::new(),
        }
    }
}

/// Deep-merge an override map onto a `serde_json::Value`, following native TOML
/// semantics. Each entry is a field of the target object: a nested object value
/// (from a TOML sub-table or an unquoted dotted key) is merged recursively so
/// the target's sibling keys are preserved; any other value (scalar/array, or
/// an object replacing a non-object) overwrites the field. Keys are taken
/// literally — a key containing a dot is a single literal field name (TOML's
/// `"a.b"`), NOT a path. To target a nested field, use native TOML nesting
/// (`[table.sub]` or `a.b = …`), which the parser turns into nested objects.
pub fn apply_overrides(value: &mut Value, overrides: &BTreeMap<String, Value>) {
    if !value.is_object() {
        *value = Value::Object(serde_json::Map::new());
    }
    let target = value
        .as_object_mut()
        .expect("just ensured value is a JSON object");
    for (key, new_value) in overrides {
        merge_field(target, key, new_value);
    }
}

/// Set `key` on `target`, recursively merging when BOTH the existing value and
/// `new_value` are objects (so the target's other keys survive); otherwise
/// replace the field wholesale.
fn merge_field(target: &mut serde_json::Map<String, Value>, key: &str, new_value: &Value) {
    match (target.get_mut(key), new_value) {
        (Some(Value::Object(existing)), Value::Object(incoming)) => {
            for (k, v) in incoming {
                merge_field(existing, k, v);
            }
        }
        _ => {
            target.insert(key.to_string(), new_value.clone());
        }
    }
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
        // Native TOML nesting: an unquoted dotted key builds a sub-table.
        std::fs::write(&path, r#"
version = 1

[schemas.cost-invoices]
settings.default_score_threshold = 0.95
"#).unwrap();
        let overlay = Overlay::load(&path).unwrap().unwrap();
        let s = overlay.schema("cost-invoices").unwrap();
        let settings = s.get("settings").unwrap();
        assert_eq!(settings["default_score_threshold"].as_f64().unwrap(), 0.95);
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
    fn apply_merges_nested_object_preserving_siblings() {
        // Native TOML: a nested table deep-merges into the target object, leaving
        // the target's other keys intact (the corrected, non-destructive behavior).
        let mut v = json!({ "config": { "runtime": "old", "other": "kept" } });
        let mut overrides = BTreeMap::new();
        overrides.insert("config".to_string(), json!({ "runtime": "new" }));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["config"]["runtime"], Value::String("new".into()));
        assert_eq!(v["config"]["other"], Value::String("kept".into()), "siblings preserved");
    }

    #[test]
    fn apply_creates_missing_nested_objects() {
        let mut v = json!({ "name": "x" });
        let mut overrides = BTreeMap::new();
        overrides.insert("settings".to_string(), json!({ "deep": { "value": "created" } }));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["settings"]["deep"]["value"], Value::String("created".into()));
        assert_eq!(v["name"], Value::String("x".into()));
    }

    #[test]
    fn apply_replaces_non_object_target_with_object() {
        // Object-over-scalar at a leaf: the scalar is replaced wholesale.
        let mut v = json!({ "config": "scalar" });
        let mut overrides = BTreeMap::new();
        overrides.insert("config".to_string(), json!({ "runtime": "py" }));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["config"]["runtime"], Value::String("py".into()));
    }

    #[test]
    fn apply_treats_dotted_key_as_literal_field_not_a_path() {
        // Native TOML: a key literally containing a dot (TOML's `"a.b"`) is a
        // single literal field name, NOT a path. rdc no longer splits on '.'.
        let mut v = json!({ "config": { "url": "keep" } });
        let mut overrides = BTreeMap::new();
        overrides.insert("config.url".to_string(), Value::String("literal".into()));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["config.url"], Value::String("literal".into()), "set as a literal field");
        assert_eq!(v["config"]["url"], Value::String("keep".into()), "nested config.url untouched");
    }

    #[test]
    fn wildcard_slug_is_parsed_as_an_ordinary_kind_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");
        std::fs::write(&path, r#"
version = 1

[hooks."*"]
token_owner = "https://prod/api/v1/users/938493"

[hooks.master-data-hub]
"name" = "MDH (PROD)"
"#).unwrap();
        let overlay = Overlay::load(&path).unwrap().unwrap();
        // The reserved "*" wildcard is just another key in the hooks map; the
        // migrate driver gives it kind-wide-default semantics (applied to every
        // hook, with a real per-slug entry winning over it).
        let star = overlay.hook("*").unwrap();
        assert_eq!(
            star.get("token_owner").unwrap(),
            &Value::String("https://prod/api/v1/users/938493".into())
        );
        assert!(overlay.hook("master-data-hub").is_some());
    }
}
