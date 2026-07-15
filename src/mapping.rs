//! Env-pair mapping — connects src slug ↔ tgt slug per kind. [`Mapping`] is
//! the in-memory, oriented view of one (src, tgt) pair, projected via
//! [`GenericMapping::orient`] from the hand-authored, N-way `.rdc/mapping.toml`.
//! Consumed by `rdc migrate` to rewrite slugs and `rdc://` refs between envs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Mapping {
    pub version: u32,
    /// Workspace slug → workspace slug. Workspaces themselves are pull-only
    /// at the Rossum API (we never PATCH them across envs), but their URLs
    /// are referenced by queues, so the mapping is needed to rewrite
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
    /// orientation matches rows by the full key, and `.rdc/mapping.toml`
    /// is hand-editable for renames.
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
    /// scoping doesn't carry stale slugs forward. Stale flat-key entries
    /// are simply dropped on load; the N-way mapping is hand-authored so
    /// nothing repopulates them.
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
    /// the URL-rewrite step inside `rdc migrate`.
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
/// Default for [`GenericMapping::version`] when a hand-authored
/// `mapping.toml` omits the field entirely.
fn default_generic_mapping_version() -> u32 {
    2
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct GenericMapping {
    #[serde(default = "default_generic_mapping_version")]
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

    /// Validate mapping rows against known envs and row uniqueness.
    ///
    /// A row referencing an env not in `rdc.toml` is NOT a hard error — envs get
    /// renamed/retired and a hand-authored mapping can lag `rdc.toml` briefly —
    /// so it is reported back as a warning string for the caller to log instead.
    /// Mapping the same `(env, slug)` in more than one row of a kind IS a hard
    /// error: it would make orientation ambiguous (which row does a lookup by
    /// that env/slug mean?), and there is no safe automatic resolution.
    pub fn validate(
        &self,
        known_envs: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        for kind in Self::KINDS {
            let rows = self.kind_rows(kind).expect("KINDS entry has rows");
            let mut seen: std::collections::BTreeSet<(String, String)> =
                std::collections::BTreeSet::new();
            for row in rows {
                for (env, slug) in row {
                    if !known_envs.contains(env) {
                        warnings.push(format!(
                            ".rdc/mapping.toml: {kind} row references unknown env \
                             '{env}' (not defined in rdc.toml)"
                        ));
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
        Ok(warnings)
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

    /// Convert legacy per-pair mappings into one N-way table. Each input tuple is
    /// `(env_a, env_b, mapping)` where `mapping` is a legacy `src -> tgt` map read
    /// from `<env_a>-to-<env_b>.toml`. EVERY edge — including identity
    /// (`src == tgt`) — is fed into the join so an env whose only link to a
    /// diverged object is an identity edge (e.g. `dev-to-test` has `mdh`->`mdh`
    /// while `test-to-prod` has `mdh`->`mdh-prod`) still ends up connected into
    /// that object's row; dropping identity edges up front would silently omit
    /// `dev` from the row and make `orient` fall back to the wrong identity slug.
    /// Edges join into connected components (one row per component); a component
    /// that forces one env to two slugs is a hard error. Once every kind's rows
    /// are built, rows that end up carrying only ONE distinct slug value (i.e.
    /// no real divergence — every env in the row agrees) are dropped, so the
    /// persisted file stays renames-only while every genuinely-diverged object
    /// keeps its full set of env columns. `hook_templates` never enters (it is
    /// not a `KINDS` entry and the legacy `Mapping` no longer carries it).
    pub fn from_legacy(files: &[(String, String, Mapping)]) -> Result<GenericMapping> {
        let mut out = GenericMapping::default();
        for kind in Self::KINDS {
            let mut rows: Vec<BTreeMap<String, String>> = Vec::new();
            for (env_a, env_b, m) in files {
                let Some(map) = m.kind_map(kind) else { continue };
                for (src, tgt) in map {
                    merge_legacy_edge(&mut rows, env_a, src, env_b, tgt, kind)?;
                }
            }
            // Keep only rows expressing a real divergence — pure-identity
            // connectivity rows (every env column holds the same slug) add
            // nothing over the identity fallback and would just bloat the file.
            rows.retain(|row| row.values().collect::<std::collections::BTreeSet<_>>().len() > 1);
            *out.kind_rows_mut(kind).expect("KINDS entry is mappable") = rows;
        }
        Ok(out)
    }
}

/// Insert `(env -> slug)` into `row`, hard-erroring if `env` already holds a
/// different slug (an inconsistent legacy edge set).
fn insert_legacy_node(
    row: &mut BTreeMap<String, String>,
    env: &str,
    slug: &str,
    kind: &str,
) -> Result<()> {
    match row.get(env) {
        Some(existing) if existing != slug => anyhow::bail!(
            "inconsistent legacy mapping for {kind}: env '{env}' maps to both \
             '{existing}' and '{slug}'; resolve the conflict in the legacy \
             .rdc/map/*.toml files before migrating"
        ),
        _ => {
            row.insert(env.to_string(), slug.to_string());
            Ok(())
        }
    }
}

/// Union the edge `(a_env, a_slug) — (b_env, b_slug)` into `rows`, keyed by
/// `(env, slug)` nodes. Merges the two endpoints' rows when both already exist.
fn merge_legacy_edge(
    rows: &mut Vec<BTreeMap<String, String>>,
    a_env: &str,
    a_slug: &str,
    b_env: &str,
    b_slug: &str,
    kind: &str,
) -> Result<()> {
    let ai = rows
        .iter()
        .position(|r| r.get(a_env).map(String::as_str) == Some(a_slug));
    let bi = rows
        .iter()
        .position(|r| r.get(b_env).map(String::as_str) == Some(b_slug));
    match (ai, bi) {
        (Some(i), Some(j)) if i == j => Ok(()),
        (Some(i), Some(j)) => {
            let jrow = rows.remove(j);
            let i2 = if j < i { i - 1 } else { i };
            for (env, slug) in &jrow {
                insert_legacy_node(&mut rows[i2], env, slug, kind)?;
            }
            Ok(())
        }
        (Some(i), None) => insert_legacy_node(&mut rows[i], b_env, b_slug, kind),
        (None, Some(j)) => insert_legacy_node(&mut rows[j], a_env, a_slug, kind),
        (None, None) => {
            let mut row = BTreeMap::new();
            row.insert(a_env.to_string(), a_slug.to_string());
            row.insert(b_env.to_string(), b_slug.to_string());
            rows.push(row);
            Ok(())
        }
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
    fn generic_mapping_deserializes_hand_authored_file_without_version() {
        // A hand-authored mapping.toml is expected to omit boilerplate like
        // `version` entirely; it must still parse (defaulting to the current
        // schema version) instead of hard-failing on a missing field.
        let g: GenericMapping = toml::from_str("[[hooks]]\ndev = \"a\"\nprod = \"b\"\n").unwrap();
        assert_eq!(g.version, 2);
        assert_eq!(
            g.hooks,
            vec![BTreeMap::from([
                ("dev".to_string(), "a".to_string()),
                ("prod".to_string(), "b".to_string()),
            ])]
        );
    }

    #[test]
    fn validate_warns_on_unknown_env_instead_of_erroring() {
        let mut g = GenericMapping::default();
        g.hooks.push(BTreeMap::from([
            ("dev".to_string(), "h".to_string()),
            ("staging".to_string(), "h2".to_string()),
        ]));
        let envs = BTreeSet::from(["dev".to_string(), "prod".to_string()]);
        let warnings = g.validate(&envs).expect("unknown env is a warning, not an error");
        assert!(
            warnings.iter().any(|w| w.contains("staging")),
            "warnings must name the unknown env: {warnings:?}"
        );
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
        assert_eq!(g.validate(&envs).unwrap(), Vec::<String>::new());
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

    #[test]
    fn from_legacy_joins_transitively_and_drops_identity() {
        // dev->test renames a hook; test->prod renames it further. The two edges
        // share the test node and must join into one 3-env row. The separate,
        // pure-identity `validator` row (every env agrees on the slug) carries no
        // real divergence and is dropped by the post-join `retain`.
        let mut dev_test = Mapping::default();
        dev_test.hooks.insert("mdh".into(), "mdh-test".into());
        dev_test.hooks.insert("validator".into(), "validator".into()); // identity
        let mut test_prod = Mapping::default();
        test_prod.hooks.insert("mdh-test".into(), "mdh-prod".into());

        let g = GenericMapping::from_legacy(&[
            ("dev".to_string(), "test".to_string(), dev_test),
            ("test".to_string(), "prod".to_string(), test_prod),
        ])
        .unwrap();

        assert_eq!(g.hooks.len(), 1, "identity dropped, one joined row");
        let row = &g.hooks[0];
        assert_eq!(row.get("dev").map(String::as_str), Some("mdh"));
        assert_eq!(row.get("test").map(String::as_str), Some("mdh-test"));
        assert_eq!(row.get("prod").map(String::as_str), Some("mdh-prod"));
    }

    #[test]
    fn from_legacy_preserves_env_linked_only_by_identity_edge() {
        // dev-to-test has only an IDENTITY edge for `mdh` (dev and test agree on
        // the slug); test-to-prod diverges it to `mdh-prod`. `dev` is connected to
        // the diverged object ONLY via that identity edge — dropping identity
        // edges up front (the old behavior) would omit `dev` from the joined row
        // entirely, and `orient("dev", "prod")` would then fall back to identity
        // and hand back the WRONG (source) slug instead of `mdh-prod`.
        let mut dev_test = Mapping::default();
        dev_test.hooks.insert("mdh".into(), "mdh".into()); // identity
        let mut test_prod = Mapping::default();
        test_prod.hooks.insert("mdh".into(), "mdh-prod".into());

        let g = GenericMapping::from_legacy(&[
            ("dev".to_string(), "test".to_string(), dev_test),
            ("test".to_string(), "prod".to_string(), test_prod),
        ])
        .unwrap();

        assert_eq!(g.hooks.len(), 1, "the diverged object's row must survive retain");
        let row = &g.hooks[0];
        assert_eq!(
            row,
            &BTreeMap::from([
                ("dev".to_string(), "mdh".to_string()),
                ("test".to_string(), "mdh".to_string()),
                ("prod".to_string(), "mdh-prod".to_string()),
            ]),
            "dev must be PRESENT in the row even though it only ever appears via \
             an identity edge"
        );

        let oriented = g.orient("dev", "prod");
        assert_eq!(
            oriented.lookup_tgt_slug("hooks", "mdh"),
            Some("mdh-prod"),
            "dev->prod must resolve through the joined row, not fall back to identity"
        );
    }

    #[test]
    fn from_legacy_hard_errors_on_inconsistency() {
        // dev->prod maps x to y; dev->test maps x to z; prod->test says y to w
        // (not z) — inconsistent: node y (== object x) forced to test=z and test=w.
        let mut dev_prod = Mapping::default();
        dev_prod.queues.insert("x".into(), "y".into());
        let mut dev_test = Mapping::default();
        dev_test.queues.insert("x".into(), "z".into());
        let mut prod_test = Mapping::default();
        prod_test.queues.insert("y".into(), "w".into());

        let err = GenericMapping::from_legacy(&[
            ("dev".to_string(), "prod".to_string(), dev_prod),
            ("dev".to_string(), "test".to_string(), dev_test),
            ("prod".to_string(), "test".to_string(), prod_test),
        ])
        .unwrap_err();
        assert!(format!("{err}").contains("inconsistent"));
    }
}
