//! Env-pair mapping — connects src slug ↔ tgt slug per kind. Built and
//! consumed by `rdc deploy` (auto-matched on each run, then persisted to
//! disk so subsequent deploys keep the same alignment).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Mapping {
    pub version: u32,
    /// Workspace slug → workspace slug. Workspaces themselves are pull-only
    /// in `rdc deploy` (we don't PATCH them across envs), but their URLs are
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
        let mut m: Mapping = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        m.migrate_legacy_nested_keys();
        Ok(m)
    }

    /// Drop legacy flat-key entries from `engine_fields` and
    /// `workflow_steps` so a mapping file written before per-parent
    /// scoping doesn't carry stale slugs forward. The auto-match in
    /// `cli::deploy::map` repopulates these sections with composite
    /// `<parent>/<child>` keys on the next `rdc deploy` run.
    fn migrate_legacy_nested_keys(&mut self) {
        self.engine_fields.retain(|src, tgt| {
            src.contains('/') && tgt.contains('/')
        });
        self.workflow_steps_passthrough(); // present in lockfile, not in Mapping
    }

    /// Reserved for parity with `engine_fields` if/when `workflow_steps`
    /// is added to `Mapping` (currently pull-only at the Rossum API, so
    /// no slug pairing is needed).
    fn workflow_steps_passthrough(&mut self) {}

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)
            .context("serializing mapping")?;
        crate::snapshot::writer::write_atomic(path, s.as_bytes())?;
        Ok(())
    }

    /// Look up the tgt slug for a `(kind, src_slug)` pair. Returns `None`
    /// if the kind isn't deployable or the pair isn't mapped. Used by
    /// the URL-rewrite step inside `rdc deploy`.
    pub fn lookup_tgt_slug(&self, kind: &str, src_slug: &str) -> Option<&str> {
        self.kind_map(kind)
            .and_then(|m| m.get(src_slug).map(|s| s.as_str()))
    }

    /// Borrow the slug-pair map for a given deployable kind. Returns
    /// `None` for non-deployable kinds (e.g. `hook_templates`, which
    /// uses a URL-pair map instead). Used by callers that need to
    /// iterate values (e.g. compute_plan's mirror-delete branch, which
    /// must exclude mapped tgt slugs).
    pub fn kind_map(&self, kind: &str) -> Option<&BTreeMap<String, String>> {
        Some(match kind {
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
        })
    }

    /// Mutable sibling of [`kind_map`], used by [`GenericMapping::orient`] to
    /// populate a fresh oriented `Mapping`. Same kind set; `None` for
    /// non-deployable kinds.
    pub fn kind_map_mut(&mut self, kind: &str) -> Option<&mut BTreeMap<String, String>> {
        Some(match kind {
            "workspaces" => &mut self.workspaces,
            "hooks" => &mut self.hooks,
            "rules" => &mut self.rules,
            "labels" => &mut self.labels,
            "queues" => &mut self.queues,
            "schemas" => &mut self.schemas,
            "inboxes" => &mut self.inboxes,
            "email_templates" => &mut self.email_templates,
            "engines" => &mut self.engines,
            "engine_fields" => &mut self.engine_fields,
            _ => return None,
        })
    }
}

/// On-disk, direction-free slug map for a whole project. Each entry is a
/// logical object; each environment names its own slug. Only objects whose
/// slug DIFFERS across envs are stored — identical slugs map 1:1 by default.
/// Oriented into a per-pair [`Mapping`] via [`orient`] for `migrate`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct GenericMapping {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schemas: Vec<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queues: Vec<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inboxes: Vec<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub email_templates: Vec<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engines: Vec<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engine_fields: Vec<BTreeMap<String, String>>,
}

impl Default for GenericMapping {
    fn default() -> Self {
        Self {
            version: 2,
            workspaces: Vec::new(),
            hooks: Vec::new(),
            rules: Vec::new(),
            labels: Vec::new(),
            schemas: Vec::new(),
            queues: Vec::new(),
            inboxes: Vec::new(),
            email_templates: Vec::new(),
            engines: Vec::new(),
            engine_fields: Vec::new(),
        }
    }
}

impl GenericMapping {
    pub const KINDS: [&'static str; 10] = [
        "workspaces", "hooks", "rules", "labels", "schemas",
        "queues", "inboxes", "email_templates", "engines", "engine_fields",
    ];

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let g: GenericMapping = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(g)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let s = toml::to_string_pretty(self).context("serializing mapping")?;
        crate::snapshot::writer::write_atomic(path, s.as_bytes())?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        Self::KINDS
            .iter()
            .all(|k| self.kind_rows(k).map(|r| r.is_empty()).unwrap_or(true))
    }

    pub fn kind_rows(&self, kind: &str) -> Option<&Vec<BTreeMap<String, String>>> {
        Some(match kind {
            "workspaces" => &self.workspaces,
            "hooks" => &self.hooks,
            "rules" => &self.rules,
            "labels" => &self.labels,
            "schemas" => &self.schemas,
            "queues" => &self.queues,
            "inboxes" => &self.inboxes,
            "email_templates" => &self.email_templates,
            "engines" => &self.engines,
            "engine_fields" => &self.engine_fields,
            _ => return None,
        })
    }

    pub fn kind_rows_mut(&mut self, kind: &str) -> Option<&mut Vec<BTreeMap<String, String>>> {
        Some(match kind {
            "workspaces" => &mut self.workspaces,
            "hooks" => &mut self.hooks,
            "rules" => &mut self.rules,
            "labels" => &mut self.labels,
            "schemas" => &mut self.schemas,
            "queues" => &mut self.queues,
            "inboxes" => &mut self.inboxes,
            "email_templates" => &mut self.email_templates,
            "engines" => &mut self.engines,
            "engine_fields" => &mut self.engine_fields,
            _ => return None,
        })
    }

    /// Reject rows that reference an env not in `rdc.toml`, or that map the same
    /// `(env, slug)` in more than one row of a kind (which would make orientation
    /// ambiguous). Both are hard errors so a hand-edit typo fails loudly.
    pub fn validate(&self, known_envs: &std::collections::BTreeSet<String>) -> Result<()> {
        for kind in Self::KINDS {
            let rows = self.kind_rows(kind).expect("KINDS entry has rows");
            let mut seen: std::collections::BTreeSet<(String, String)> =
                std::collections::BTreeSet::new();
            for row in rows {
                for (env, slug) in row {
                    if !known_envs.contains(env) {
                        anyhow::bail!(
                            ".rdc/mapping.toml: {kind} row references unknown env \
                             '{env}' (not defined in rdc.toml)"
                        );
                    }
                    if !seen.insert((env.clone(), slug.clone())) {
                        anyhow::bail!(
                            ".rdc/mapping.toml: {kind} maps ({env}, {slug}) in more \
                             than one row — each (env, slug) must be unique per kind"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Project the N-way table onto one ordered pair, yielding the per-kind
    /// `src_slug -> tgt_slug` [`Mapping`] that `migrate` consumes. Only rows that
    /// name BOTH envs with DIFFERING slugs contribute; everything else is left to
    /// the identity fallback in `tgt_slug`. Rows are matched by the SOURCE env's
    /// column so same-slug objects in other tracks never cross-match.
    pub fn orient(&self, src_env: &str, tgt_env: &str) -> Mapping {
        let mut m = Mapping::default();
        for kind in Self::KINDS {
            let rows = self.kind_rows(kind).expect("KINDS entry has rows");
            let dest = m.kind_map_mut(kind).expect("KINDS entry is a mappable kind");
            for row in rows {
                if let (Some(s), Some(t)) = (row.get(src_env), row.get(tgt_env)) {
                    if s != t {
                        dest.insert(s.clone(), t.clone());
                    }
                }
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
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

    #[test]
    fn load_drops_legacy_flat_engine_field_keys() {
        // A pre-migration mapping wrote `[engine_fields]` with bare field
        // slugs. After the schema change to composite `<engine>/<field>`
        // keys those entries are useless — auto-match regenerates them
        // on the next deploy. Dropping silently on load is the
        // user's chosen migration path.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.toml");
        std::fs::write(&path, r#"
version = 1

[engine_fields]
"amount" = "amount"
"item-qty" = "item-quantity"
"my-engine/total" = "my-engine/total"
"#).unwrap();
        let loaded = Mapping::load(&path).unwrap();
        assert_eq!(loaded.engine_fields.len(), 1);
        assert_eq!(loaded.engine_fields.get("my-engine/total"), Some(&"my-engine/total".to_string()));
        assert!(!loaded.engine_fields.contains_key("amount"));
        assert!(!loaded.engine_fields.contains_key("item-qty"));
    }

    #[test]
    fn generic_mapping_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mapping.toml");
        let mut g = GenericMapping::default();
        g.queues.push(BTreeMap::from([
            ("dev".to_string(), "invoices".to_string()),
            ("prod".to_string(), "invoices-prod".to_string()),
        ]));
        g.hooks.push(BTreeMap::from([
            ("dev".to_string(), "master-data-hub".to_string()),
            ("prod".to_string(), "mdh-prod".to_string()),
        ]));
        g.save(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("version = 2"));
        assert!(raw.contains("[[queues]]"));
        assert!(!raw.contains("[[labels]]"), "empty kinds are omitted");

        let loaded = GenericMapping::load(&path).unwrap();
        assert_eq!(loaded, g);
    }

    #[test]
    fn generic_mapping_load_defaults_when_missing() {
        let dir = TempDir::new().unwrap();
        let g = GenericMapping::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(g.version, 2);
        assert!(g.is_empty());
    }

    #[test]
    fn validate_rejects_unknown_env() {
        let mut g = GenericMapping::default();
        g.hooks.push(BTreeMap::from([
            ("dev".to_string(), "h".to_string()),
            ("staging".to_string(), "h2".to_string()),
        ]));
        let envs = BTreeSet::from(["dev".to_string(), "prod".to_string()]);
        let err = g.validate(&envs).unwrap_err();
        assert!(format!("{err}").contains("staging"));
    }

    #[test]
    fn validate_rejects_duplicate_env_slug_across_rows() {
        let mut g = GenericMapping::default();
        g.queues.push(BTreeMap::from([
            ("dev".to_string(), "invoices".to_string()),
            ("prod".to_string(), "invoices-a".to_string()),
        ]));
        g.queues.push(BTreeMap::from([
            ("dev".to_string(), "invoices".to_string()),
            ("prod".to_string(), "invoices-b".to_string()),
        ]));
        let envs = BTreeSet::from(["dev".to_string(), "prod".to_string()]);
        let err = g.validate(&envs).unwrap_err();
        assert!(format!("{err}").contains("invoices"));
    }

    #[test]
    fn validate_accepts_well_formed() {
        let mut g = GenericMapping::default();
        g.queues.push(BTreeMap::from([
            ("dev".to_string(), "invoices".to_string()),
            ("prod".to_string(), "invoices-prod".to_string()),
        ]));
        let envs = BTreeSet::from(["dev".to_string(), "prod".to_string()]);
        assert!(g.validate(&envs).is_ok());
    }

    #[test]
    fn orient_emits_only_divergent_pairs_for_the_direction() {
        let mut g = GenericMapping::default();
        g.hooks.push(BTreeMap::from([
            ("dev".to_string(), "master-data-hub".to_string()),
            ("test".to_string(), "mdh-test".to_string()),
            ("prod".to_string(), "mdh-prod".to_string()),
        ]));
        // dev -> prod
        let m = g.orient("dev", "prod");
        assert_eq!(m.lookup_tgt_slug("hooks", "master-data-hub"), Some("mdh-prod"));
        // reverse prod -> dev reads the same row
        let r = g.orient("prod", "dev");
        assert_eq!(r.lookup_tgt_slug("hooks", "mdh-prod"), Some("master-data-hub"));
        // a pair not present in the row (dev->test uses different value) still works
        let t = g.orient("dev", "test");
        assert_eq!(t.lookup_tgt_slug("hooks", "master-data-hub"), Some("mdh-test"));
    }

    #[test]
    fn orient_is_keyed_by_source_env_column_not_any_column() {
        // A row describing the eu track must NOT match a lookup for the us track,
        // even if a slug value coincides.
        let mut g = GenericMapping::default();
        g.queues.push(BTreeMap::from([
            ("dev-eu".to_string(), "invoices".to_string()),
            ("prod-eu".to_string(), "invoices-prod".to_string()),
        ]));
        // Migrating dev-us -> test-us: no dev-us column in the row => no match =>
        // absent from the oriented map => tgt_slug falls back to identity.
        let m = g.orient("dev-us", "test-us");
        assert_eq!(m.lookup_tgt_slug("queues", "invoices"), None);
    }
}
