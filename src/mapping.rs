//! Env-pair mapping — connects src slug ↔ tgt slug per kind. Written by
//! `rdc map`, consumed by `rdc plan` / `rdc apply`. Per spec §10.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Mapping {
    pub version: u32,
    /// Workspace slug → workspace slug. Workspaces themselves are pull-only
    /// in `rdc apply` (we don't PATCH them across envs), but their URLs are
    /// referenced by queues, so the mapping is needed to rewrite
    /// `queue.workspace` from the src URL to the tgt URL.
    #[serde(default)]
    pub workspaces: BTreeMap<String, String>,
    #[serde(default)]
    pub hooks: BTreeMap<String, String>,
    #[serde(default)]
    pub rules: BTreeMap<String, String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Schema slug (= queue slug) → schema slug.
    #[serde(default)]
    pub schemas: BTreeMap<String, String>,
    /// Queue slug → queue slug.
    #[serde(default)]
    pub queues: BTreeMap<String, String>,
    /// Inbox slug (= queue slug) → inbox slug.
    #[serde(default)]
    pub inboxes: BTreeMap<String, String>,
    /// Email-template compound key `<ws>/<q>/<template>` → compound key.
    /// The `<ws>` and `<q>` segments may differ between src and tgt envs;
    /// auto-match in `rdc map` uses the full key, but the file is
    /// hand-editable for renames.
    #[serde(default)]
    pub email_templates: BTreeMap<String, String>,
    /// Engine slug → engine slug.
    #[serde(default)]
    pub engines: BTreeMap<String, String>,
    /// Engine field slug → engine field slug.
    #[serde(default)]
    pub engine_fields: BTreeMap<String, String>,
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            version: 1,
            workspaces: BTreeMap::new(),
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

impl Mapping {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let m: Mapping = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(m)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)
            .context("serializing mapping")?;
        crate::snapshot::writer::write_atomic(path, s.as_bytes())?;
        Ok(())
    }

    /// Look up the tgt slug for a `(kind, src_slug)` pair. Returns `None`
    /// if the kind isn't deployable or the pair isn't mapped. Used by
    /// `rdc apply`'s URL-rewrite step.
    pub fn lookup_tgt_slug(&self, kind: &str, src_slug: &str) -> Option<&str> {
        let map = match kind {
            "workspaces" => &self.workspaces,
            "hooks" => &self.hooks,
            "rules" => &self.rules,
            "labels" => &self.labels,
            "queues" => &self.queues,
            "schemas" => &self.schemas,
            "inboxes" => &self.inboxes,
            "email_templates" => &self.email_templates,
            "engines" => &self.engines,
            "engine_fields" => &self.engine_fields,
            _ => return None,
        };
        map.get(src_slug).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_returns_default_when_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.toml");
        let m = Mapping::load(&path).unwrap();
        assert_eq!(m, Mapping::default());
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_to_prod.toml");
        let mut m = Mapping::default();
        m.hooks.insert("validator-invoices".into(), "validator-invoices".into());
        m.hooks.insert("sftp-import".into(), "sftp-import-prod".into());
        m.rules.insert("validation-rule".into(), "validation-rule".into());
        m.labels.insert("priority-high".into(), "priority-high".into());
        m.save(&path).unwrap();
        let loaded = Mapping::load(&path).unwrap();
        assert_eq!(loaded, m);
    }
}
