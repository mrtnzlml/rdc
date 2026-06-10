//! Per-env overlays — declarative env-specific values applied when MIGRATING
//! a snapshot from one environment to another. Per spec §9.
//!
//! C-1 model (migrate-only): an overlay maps dotted-path keys to the value the
//! TARGET env should use. `migrate` applies them (via [`apply_overrides`]) so
//! the promoted snapshot carries that env's real values. Pull, push, and sync
//! treat overlay-managed fields as ordinary content — they are NOT stripped on
//! pull nor re-applied on push — so each env's snapshot shows its real values
//! on disk and the snapshot itself is the source of truth for what is pushed.
//!
//! (Previously the overlay was bidirectional — applied on push, stripped on
//! pull — which kept the snapshot env-agnostic but hid the value from disk.
//! C-1 trades that for visibility: snapshots are env-specific and self-evident.)
//!
//! The override format is simple dotted-path keys; JMESPath wildcards / array
//! filters are out of scope for v1.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub const OVERLAY_VERSION: u32 = 1;

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

impl Default for Overlay {
    fn default() -> Self {
        Self {
            version: OVERLAY_VERSION,
            defaults: Defaults::default(),
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

/// Idempotent write: load the overlay file (or create an empty one),
/// patch in the `token_owner` (per-hook if `slug` is `Some`, otherwise
/// into `[defaults] store_extension_token_owner`), atomically rewrite
/// the TOML. Preserves every other key.
pub fn write_store_extension_token_owner(
    path: &Path,
    slug: Option<&str>,
    user_url: &str,
) -> Result<()> {
    let mut overlay = Overlay::load(path)?.unwrap_or_default();
    match slug {
        Some(s) => {
            let entry = overlay.hooks.entry(s.to_string()).or_insert_with(BTreeMap::new);
            entry.insert("token_owner".into(), Value::String(user_url.into()));
        }
        None => {
            overlay.defaults.store_extension_token_owner = Some(user_url.into());
        }
    }
    let s = toml::to_string_pretty(&overlay).context("serializing overlay")?;
    crate::snapshot::writer::write_atomic(path, s.as_bytes())?;
    Ok(())
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
        let obj = current.as_object_mut().expect("set_at_path just initialized current as Value::Object");
        let entry = obj.entry((*segment).to_string()).or_insert(Value::Object(Default::default()));
        if !entry.is_object() {
            *entry = Value::Object(Default::default());
        }
        current = entry;
    }
    if !current.is_object() {
        *current = Value::Object(Default::default());
    }
    let obj = current.as_object_mut().expect("set_at_path just initialized current as Value::Object");
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

    #[test]
    fn write_token_owner_creates_per_hook_entry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");
        std::fs::write(&path, "version = 1\n\n[hooks.other-hook]\n\"name\" = \"Other\"\n").unwrap();

        write_store_extension_token_owner(&path, Some("master-data-hub"), "https://prod/api/v1/users/938493").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[hooks.master-data-hub]"));
        assert!(raw.contains("token_owner = \"https://prod/api/v1/users/938493\""));
        assert!(raw.contains("[hooks.other-hook]"), "existing entries must be preserved");
        assert!(raw.contains("Other"));
    }

    #[test]
    fn write_token_owner_creates_defaults_entry_when_slug_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");
        std::fs::write(&path, "version = 1\n").unwrap();

        write_store_extension_token_owner(&path, None, "https://prod/api/v1/users/938493").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[defaults]"));
        assert!(raw.contains("store_extension_token_owner = \"https://prod/api/v1/users/938493\""));
    }

    #[test]
    fn write_token_owner_creates_file_if_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");

        write_store_extension_token_owner(&path, None, "https://prod/api/v1/users/938493").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("version = 1"));
        assert!(raw.contains("[defaults]"));
        assert!(raw.contains("store_extension_token_owner"));
    }
}
