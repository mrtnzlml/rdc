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
    /// Saved-view slug → saved-view slug.
    #[serde(default)]
    pub saved_views: BTreeMap<String, String>,
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
            saved_views: BTreeMap::new(),
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
            "saved_views" => &self.saved_views,
            _ => return None,
        })
    }

    /// Mutable sibling of [`Self::kind_map`], used by [`GenericMapping::orient`] to
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
            "saved_views" => &mut self.saved_views,
            _ => return None,
        })
    }
}

/// On-disk, direction-free slug map for a whole project. Each entry is a
/// logical object; each environment names its own slug. Only objects whose
/// slug DIFFERS across envs are stored — identical slugs map 1:1 by default.
/// Oriented into a per-pair [`Mapping`] via [`GenericMapping::orient`] for
/// `migrate`.
/// Default for [`GenericMapping::version`] when a hand-authored
/// `mapping.toml` omits the field entirely.
fn default_generic_mapping_version() -> u32 {
    1
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub saved_views: Vec<BTreeMap<String, String>>,
}

impl Default for GenericMapping {
    fn default() -> Self {
        Self {
            version: 1,
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
            saved_views: Vec::new(),
        }
    }
}

impl GenericMapping {
    pub const KINDS: [&'static str; 11] = [
        "workspaces", "hooks", "rules", "labels", "schemas",
        "queues", "inboxes", "email_templates", "engines", "engine_fields",
        "saved_views",
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
            "saved_views" => &self.saved_views,
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
            "saved_views" => &mut self.saved_views,
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
                if let (Some(s), Some(t)) = (row.get(src_env), row.get(tgt_env))
                    && s != t
                {
                    dest.insert(s.clone(), t.clone());
                }
            }
        }
        m
    }

    /// Rename an environment across every mapping row (each row is
    /// `BTreeMap<env_name, slug>`). No-op for rows that don't reference `old`,
    /// and a no-op overall when `old == new`. Local, non-behavioral: `migrate`
    /// reads the same rows under the new env name afterward.
    pub fn rename_env(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        for kind in Self::KINDS {
            if let Some(rows) = self.kind_rows_mut(kind) {
                for row in rows.iter_mut() {
                    if let Some(slug) = row.remove(old) {
                        row.insert(new.to_string(), slug);
                    }
                }
            }
        }
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

// ---------------------------------------------------------------------------
// Surgical edits to the hand-authored `.rdc/mapping.toml`.
//
// `rdc doctor` records a slug rename here the moment it applies one, because
// that is the only moment the correspondence is known: `rdc migrate` strips
// `id` and `url` from an object it CREATES in the target env, so a target tree
// holds nothing at all tying a renamed slug to the object that env already
// has. Without a row the next promotion prunes the old target object and
// creates a new one — which for a queue is a `DELETE` that purges its
// documents.
//
// The file is hand-authored, so nothing here parses-and-re-serializes it: an
// edit rewrites one column in place or appends one row, leaving every comment,
// key order and formatting choice byte-identical. Mirrors the surgical
// `overlay.toml` header rewrite in `cli::deploy::realign`.
// ---------------------------------------------------------------------------

impl GenericMapping {
    /// Index of the `kind` row that names `(env, slug)`, if any. [`validate`]
    /// makes `(env, slug)` unique per kind, so there is at most one.
    ///
    /// [`validate`]: GenericMapping::validate
    pub fn row_naming(&self, kind: &str, env: &str, slug: &str) -> Option<usize> {
        let rows = self.kind_rows(kind)?;
        rows.iter()
            .position(|r| r.get(env).is_some_and(|s| s == slug))
    }
}

/// Split a mapping-file line into `(code, trailing)` at the first `#` that is
/// not inside a quoted string. `trailing` keeps the `#` and everything after
/// it, so rejoining the two halves reproduces the line byte-for-byte.
fn split_off_comment(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' && q == b'"' {
                    i += 1; // skip the escaped char
                } else if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b'#' {
                    return (&line[..i], &line[i..]);
                }
            }
        }
        i += 1;
    }
    (line, "")
}

/// The kind named by an array-of-tables header line (`[[queues]]`), if this
/// line is one.
fn array_table_header(line: &str) -> Option<&str> {
    let code = split_off_comment(line).0.trim();
    let inner = code.strip_prefix("[[")?.strip_suffix("]]")?;
    let name = inner.trim();
    // A dotted header (`[[a.b]]`) is not a mapping kind; leave it alone.
    if name.is_empty() || name.contains('.') || name.contains('[') {
        return None;
    }
    Some(name)
}

/// The key a `key = "value"` line assigns, unquoted, plus the byte range of
/// the value's quoted literal within `line`. `None` for any line that is not a
/// simple quoted-string assignment (a blank line, a header, an inline table).
fn key_and_value_span(line: &str) -> Option<(String, std::ops::Range<usize>, String)> {
    let (code, _) = split_off_comment(line);
    // The first `=` outside quotes separates key from value.
    let bytes = code.as_bytes();
    let mut quote: Option<u8> = None;
    let mut eq: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' && q == b'"' {
                    i += 1;
                } else if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b'=' {
                    eq = Some(i);
                    break;
                }
            }
        }
        i += 1;
    }
    let eq = eq?;
    let raw_key = code[..eq].trim();
    let key = raw_key
        .strip_prefix('"')
        .and_then(|k| k.strip_suffix('"'))
        .or_else(|| raw_key.strip_prefix('\'').and_then(|k| k.strip_suffix('\'')))
        .unwrap_or(raw_key)
        .to_string();
    if key.is_empty() {
        return None;
    }
    // The value must be a quoted string; anything else (a number, an array, an
    // inline table) is not a slug column and is left untouched.
    let after_eq = &code[eq + 1..];
    let lead = after_eq.len() - after_eq.trim_start().len();
    let vstart = eq + 1 + lead;
    let vbytes = code.as_bytes();
    let q = *vbytes.get(vstart)?;
    if q != b'"' && q != b'\'' {
        return None;
    }
    let mut j = vstart + 1;
    while j < vbytes.len() {
        let b = vbytes[j];
        if b == b'\\' && q == b'"' {
            j += 2;
            continue;
        }
        if b == q {
            let value = code[vstart + 1..j].to_string();
            return Some((key, vstart..j + 1, value));
        }
        j += 1;
    }
    None
}

/// Rewrite `env`'s column from `old` to `new` in the first `[[kind]]` row that
/// names it, preserving the line's indentation, quote style and any trailing
/// comment. `None` when no such line exists — the caller appends a row instead.
pub(crate) fn rewrite_row_column(
    text: &str,
    kind: &str,
    env: &str,
    old: &str,
    new: &str,
) -> Option<String> {
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    let mut in_kind = false;
    for line in lines.iter_mut() {
        if let Some(header) = array_table_header(line) {
            in_kind = header == kind;
            continue;
        }
        if !in_kind {
            continue;
        }
        let Some((key, span, value)) = key_and_value_span(line) else {
            continue;
        };
        if key != env || value != old {
            continue;
        }
        let quote = &line[span.start..span.start + 1];
        let replacement = format!("{quote}{}{quote}", escape_toml_str(new, quote));
        let mut updated = String::with_capacity(line.len() + new.len());
        updated.push_str(&line[..span.start]);
        updated.push_str(&replacement);
        updated.push_str(&line[span.end..]);
        *line = updated;
        return Some(lines.join("\n"));
    }
    None
}

/// Append a `[[kind]]` row carrying `cols` (env → slug). An empty `text` (no
/// mapping file yet) gets the `version` header first. The existing text is
/// never reflowed — the row is appended after a single blank line.
pub(crate) fn append_row(text: &str, kind: &str, cols: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    if text.trim().is_empty() {
        out.push_str(&format!("version = {}\n", default_generic_mapping_version()));
    } else {
        out.push_str(text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&format!("[[{kind}]]\n"));
    for (env, slug) in cols {
        out.push_str(&format!("{} = \"{}\"\n", toml_key(env), escape_toml_str(slug, "\"")));
    }
    out
}

/// A TOML bare key when the name allows it (`A-Za-z0-9_-`), quoted otherwise.
fn toml_key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        name.to_string()
    } else {
        format!("\"{}\"", escape_toml_str(name, "\""))
    }
}

/// Escape a string for the given quote style. Slugs are `[a-z0-9-]` in
/// practice, so this only ever has to cover a hand-edited pathological value.
fn escape_toml_str(s: &str, quote: &str) -> String {
    if quote == "'" {
        // Literal strings have no escapes at all; a value carrying the quote
        // itself cannot be represented, so fall back to dropping it rather
        // than emitting a file that no longer parses.
        return s.replace('\'', "");
    }
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
        assert!(raw.contains("version = 1"));
        assert!(raw.contains("[[queues]]"));
        assert!(!raw.contains("[[labels]]"), "empty kinds are omitted");

        let loaded = GenericMapping::load(&path).unwrap();
        assert_eq!(loaded, g);
    }

    #[test]
    fn generic_mapping_load_defaults_when_missing() {
        let dir = TempDir::new().unwrap();
        let g = GenericMapping::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(g.version, 1);
        assert!(g.is_empty());
    }

    #[test]
    fn generic_mapping_deserializes_hand_authored_file_without_version() {
        // A hand-authored mapping.toml is expected to omit boilerplate like
        // `version` entirely; it must still parse (defaulting to the current
        // schema version) instead of hard-failing on a missing field.
        let g: GenericMapping = toml::from_str("[[hooks]]\ndev = \"a\"\nprod = \"b\"\n").unwrap();
        assert_eq!(g.version, 1);
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

    #[test]
    fn rename_env_rewrites_row_keys_across_kinds() {
        let mut g = GenericMapping::default();
        let mut row = std::collections::BTreeMap::new();
        row.insert("dev".to_string(), "cost-dev".to_string());
        row.insert("prod".to_string(), "cost-prod".to_string());
        g.queues.push(row);
        let mut hrow = std::collections::BTreeMap::new();
        hrow.insert("dev".to_string(), "validator".to_string());
        g.hooks.push(hrow);
        g.rename_env("dev", "sandbox");
        assert_eq!(g.queues[0].get("sandbox"), Some(&"cost-dev".to_string()));
        assert_eq!(g.queues[0].get("dev"), None);
        assert_eq!(g.queues[0].get("prod"), Some(&"cost-prod".to_string()));
        assert_eq!(g.hooks[0].get("sandbox"), Some(&"validator".to_string()));
    }

    /// Drift guard for the parallel 10-kind lists (struct fields, `Default`,
    /// `KINDS`, `kind_rows`, `kind_rows_mut`). Adding a deployable kind means
    /// touching all of them; miss `KINDS` and that kind silently drops out of
    /// `validate`/`orient`/`is_empty`/`from_legacy` with no compile error. This
    /// ties the struct's serialized fields to `KINDS` in BOTH directions:
    ///   - the struct literal below names every field, so adding a Vec field
    ///     fails to compile here until the author updates it (a prompt to also
    ///     touch `KINDS`);
    ///   - the assertions then fail unless the new field is in `KINDS` too, and
    ///     every `KINDS` entry resolves through `kind_rows`/`kind_rows_mut`.
    #[test]
    fn kinds_list_matches_struct_fields_and_accessors() {
        let row = || {
            let mut m = BTreeMap::new();
            m.insert("dev".to_string(), "a".to_string());
            m
        };
        // Exhaustive struct literal: adding a mapping Vec field to
        // GenericMapping breaks THIS line until the author updates it.
        let full = GenericMapping {
            version: 1,
            workspaces: vec![row()],
            hooks: vec![row()],
            rules: vec![row()],
            labels: vec![row()],
            schemas: vec![row()],
            queues: vec![row()],
            inboxes: vec![row()],
            email_templates: vec![row()],
            engines: vec![row()],
            engine_fields: vec![row()],
            saved_views: vec![row()],
        };

        let value = serde_json::to_value(&full).expect("serialize GenericMapping");
        let obj = value.as_object().expect("GenericMapping serializes to a map");
        let serialized: std::collections::BTreeSet<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "version")
            .collect();
        let declared: std::collections::BTreeSet<&str> =
            GenericMapping::KINDS.iter().copied().collect();
        assert_eq!(
            serialized, declared,
            "GenericMapping kind fields and KINDS have drifted: a new mapping \
             field must be added to KINDS (and kind_rows/kind_rows_mut), and \
             KINDS must not name a nonexistent field"
        );

        for kind in GenericMapping::KINDS {
            assert!(
                full.kind_rows(kind).is_some(),
                "kind_rows has no arm for KINDS entry '{kind}'"
            );
            assert!(
                full.clone().kind_rows_mut(kind).is_some(),
                "kind_rows_mut has no arm for KINDS entry '{kind}'"
            );
        }
    }

    #[test]
    fn saved_views_is_a_mapping_kind() {
        assert!(GenericMapping::KINDS.contains(&"saved_views"));
        let g = GenericMapping::default();
        assert!(g.kind_rows("saved_views").is_some());
        let m = Mapping::default();
        assert!(m.kind_map("saved_views").is_some());
    }

    #[test]
    fn a_mapping_toml_without_saved_views_still_parses() {
        // Backward compatibility: an existing project's mapping file predates
        // the kind and must load unchanged. It also says `version = 2`, the
        // number the format carried before it was renumbered to 1 — nothing
        // reads the field, so both keep loading.
        let g: GenericMapping = toml::from_str("version = 2\n").unwrap();
        assert!(g.kind_rows("saved_views").unwrap().is_empty());
    }

    /// A hand-authored file, with the shapes a human actually writes: comments
    /// above and beside rows, blank lines, and two kinds.
    const HAND_AUTHORED: &str = r#"# Cross-env slug names. Hand-authored.
version = 1

# The invoice queue is named differently in prod.
[[queues]]
dev = "invoices"   # renamed in prod, see ticket
prod = "invoices-prod"

[[hooks]]
dev = "invoices"
prod = "invoices"
"#;

    #[test]
    fn row_naming_finds_the_row_that_names_env_and_slug() {
        let g: GenericMapping = toml::from_str(HAND_AUTHORED).unwrap();
        assert_eq!(g.row_naming("queues", "dev", "invoices"), Some(0));
        assert_eq!(g.row_naming("queues", "prod", "invoices-prod"), Some(0));
        assert_eq!(g.row_naming("queues", "prod", "invoices"), None);
        assert_eq!(g.row_naming("labels", "dev", "invoices"), None);
    }

    /// The whole point of the surgical edit: one value changes, every comment,
    /// blank line, alignment and unrelated row survives byte-for-byte.
    #[test]
    fn rewrite_row_column_changes_only_that_column() {
        let out =
            rewrite_row_column(HAND_AUTHORED, "queues", "dev", "invoices", "vendor-invoices")
                .expect("the row names (dev, invoices)");
        assert!(
            out.contains(r#"dev = "vendor-invoices"   # renamed in prod, see ticket"#),
            "value replaced in place, comment and spacing kept: {out}"
        );
        // Everything else identical: diff the two line lists.
        let before: Vec<&str> = HAND_AUTHORED.lines().collect();
        let after: Vec<&str> = out.lines().collect();
        assert_eq!(before.len(), after.len(), "no lines added or removed");
        let changed: Vec<usize> = before
            .iter()
            .zip(&after)
            .enumerate()
            .filter(|(_, (b, a))| b != a)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(changed.len(), 1, "exactly one line changed: {changed:?}");
    }

    /// The hooks row names (dev, invoices) too. Rewriting `queues` must not
    /// touch it — kind scoping is what keeps two objects that happen to share
    /// a slug independent.
    #[test]
    fn rewrite_row_column_ignores_rows_of_another_kind() {
        let out =
            rewrite_row_column(HAND_AUTHORED, "queues", "dev", "invoices", "vendor-invoices")
                .unwrap();
        let g: GenericMapping = toml::from_str(&out).unwrap();
        assert_eq!(g.hooks[0].get("dev").map(String::as_str), Some("invoices"));
        assert_eq!(
            g.queues[0].get("dev").map(String::as_str),
            Some("vendor-invoices")
        );
    }

    #[test]
    fn rewrite_row_column_preserves_a_literal_quote_style() {
        let text = "version = 1\n\n[[queues]]\ndev = 'invoices'\nprod = \"invoices\"\n";
        let out = rewrite_row_column(text, "queues", "dev", "invoices", "vendor").unwrap();
        assert!(out.contains("dev = 'vendor'"), "{out}");
    }

    #[test]
    fn rewrite_row_column_is_none_when_no_row_names_the_slug() {
        assert!(rewrite_row_column(HAND_AUTHORED, "queues", "dev", "nope", "x").is_none());
        assert!(rewrite_row_column(HAND_AUTHORED, "labels", "dev", "invoices", "x").is_none());
    }

    /// A `#` inside a slug value must not be read as a comment, and a quoted
    /// key must still match its env.
    #[test]
    fn rewrite_row_column_handles_quoted_keys_and_hashes_in_values() {
        let text = "version = 1\n\n[[hooks]]\n\"dev-eu\" = \"a#b\"\nprod = \"a\"\n";
        let out = rewrite_row_column(text, "hooks", "dev-eu", "a#b", "c").unwrap();
        assert!(out.contains("\"dev-eu\" = \"c\""), "{out}");
    }

    #[test]
    fn append_row_creates_a_versioned_file_from_nothing() {
        let cols = BTreeMap::from([
            ("dev".to_string(), "vendor-invoices".to_string()),
            ("prod".to_string(), "invoices".to_string()),
        ]);
        let out = append_row("", "queues", &cols);
        assert_eq!(
            out,
            "version = 1\n\n[[queues]]\ndev = \"vendor-invoices\"\nprod = \"invoices\"\n"
        );
        let g: GenericMapping = toml::from_str(&out).unwrap();
        assert_eq!(g.row_naming("queues", "dev", "vendor-invoices"), Some(0));
    }

    #[test]
    fn append_row_leaves_existing_bytes_untouched() {
        let cols = BTreeMap::from([
            ("dev".to_string(), "vendor".to_string()),
            ("prod".to_string(), "v".to_string()),
        ]);
        let out = append_row(HAND_AUTHORED, "labels", &cols);
        assert!(out.starts_with(HAND_AUTHORED), "existing text is a prefix");
        assert!(out.ends_with("[[labels]]\ndev = \"vendor\"\nprod = \"v\"\n"), "{out}");
    }

    /// An env name that is not a bare TOML key still produces a loadable file.
    #[test]
    fn append_row_quotes_an_env_name_that_needs_it() {
        let cols = BTreeMap::from([
            ("dev eu".to_string(), "a".to_string()),
            ("prod".to_string(), "b".to_string()),
        ]);
        let out = append_row("", "hooks", &cols);
        assert!(out.contains("\"dev eu\" = \"a\""), "{out}");
        let g: GenericMapping = toml::from_str(&out).unwrap();
        assert_eq!(g.row_naming("hooks", "dev eu", "a"), Some(0));
    }

    /// The end state a doctor rename produces must be a file `migrate` can
    /// use: it loads, it validates, and it orients to the rename.
    #[test]
    fn recorded_rows_load_validate_and_orient() {
        let cols = BTreeMap::from([
            ("dev".to_string(), "vendor-invoices".to_string()),
            ("test".to_string(), "invoices".to_string()),
            ("prod".to_string(), "invoices".to_string()),
        ]);
        let text = append_row("", "queues", &cols);
        let g: GenericMapping = toml::from_str(&text).unwrap();
        let envs = BTreeSet::from([
            "dev".to_string(),
            "test".to_string(),
            "prod".to_string(),
        ]);
        assert!(g.validate(&envs).unwrap().is_empty(), "no warnings");
        let m = g.orient("dev", "prod");
        assert_eq!(
            m.queues.get("vendor-invoices").map(String::as_str),
            Some("invoices"),
            "the promotion must target prod's existing slug"
        );
    }
}
