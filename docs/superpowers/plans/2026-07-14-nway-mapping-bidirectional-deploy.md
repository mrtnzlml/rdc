# N-way Bidirectional Slug Mapping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace per-ordered-pair `.rdc/map/<src>-to-<tgt>.toml` files with a single generic `.rdc/mapping.toml` in which each row is a logical object and each environment names its own slug, so `migrate` is symmetric in both directions and round-trips stay safe.

**Architecture:** A new on-disk `GenericMapping` (array-of-tables per kind, env-name columns, divergences-only) is oriented for a requested `(src, tgt)` pair into the *existing* in-memory `Mapping` — so `migrate`, `build_subst`, `tgt_slug`, and `remap_relative` are untouched. Legacy pairwise files are converted once (union-find over their edges) on first access and deleted. The vestigial `hook_templates` mapping section is removed.

**Tech Stack:** Rust, `serde` + `toml`, `anyhow`. Tests are in-module `#[cfg(test)] mod tests`. Run with `cargo test`.

## Global Constraints

- **Customer confidentiality (STRICT):** no customer names or customer-specific identifiers (org/division/region codes, env names, queue/engine/hook slugs, hostnames, URLs, paths) anywhere — source, tests, docs, fixtures, **or git commit messages**. Use neutral placeholders (`dev`, `test`, `prod`, `invoices`, `master-data-hub`).
- **Never `git push`.** Commit locally only; the user publishes. End every commit message with `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
- **Verify empirically.** Reproduce real behavior; do not claim a fix from code reading alone. Confirm which `rdc` binary runs (Homebrew shadows local builds).
- **Do not run repo-wide `cargo fmt`.** The repo is not rustfmt-clean under local rustfmt; a `fmt-check` failure is pre-existing, not a regression. Format only the lines you touch, by hand.
- **The real multi-env acceptance repo holds the user's uncommitted work.** Never run bare `git stash`/`checkout`/`reset`/`clean` there; review diffs, no destructive git.
- **Backward compatibility:** `.rdc/map/` is committed; the format change must land as a clean, reviewable git diff and must never lose data that the current binary actually consumes.

---

## File Structure

- `src/mapping.rs` — **modify.** Keep `Mapping` (the oriented in-memory view); remove its vestigial `hook_templates` field; add `Mapping::kind_map_mut`. Add the new `GenericMapping` type with `load`/`save`/`is_empty`/`validate`/`orient`/`from_legacy` and its tests. (Both types change together and are tightly coupled — one file.)
- `src/paths.rs` — **modify.** `mapping_file()` → single `.rdc/mapping.toml` (no args); add `legacy_mapping_files()`.
- `src/cli/migrate/mod.rs` — **modify.** New private helpers `parse_legacy_env_pair`, `load_or_migrate_mapping`, `count_create_update`; rewire `run()` to load+orient+validate, drop the `auto_match`/stale/save path, and emit the new summary + empty-target hint.
- `src/cli/deploy/map.rs` — **modify.** Remove `auto_match`, `stale_mapping_sources`, `prune_stale_sources`, and their now-dead private helpers.
- `README.md` — **modify.** Add a "replicate an existing env" section; note the single mapping file.

---

## Task 1: Trim `Mapping` and add a mutable kind accessor

**Files:**
- Modify: `src/mapping.rs`
- Test: `src/mapping.rs` (in-module `tests`)

**Interfaces:**
- Produces: `Mapping::kind_map_mut(&mut self, kind: &str) -> Option<&mut BTreeMap<String, String>>`; `Mapping` no longer has a `hook_templates` field.

- [ ] **Step 1: Remove the `hook_templates` field and its `Default` entry.**

In `src/mapping.rs`, delete the field (the doc comment + `pub hook_templates: BTreeMap<String, String>,` — around lines 46–50) and the `hook_templates: BTreeMap::new(),` line inside `impl Default for Mapping`. Legacy files carrying `[hook_templates]` still load: serde ignores unknown fields on deserialize.

- [ ] **Step 2: Delete the now-invalid test.**

Remove the whole `hook_templates_section_round_trips` test (around lines 190–207).

- [ ] **Step 3: Add `kind_map_mut` next to `kind_map`.**

```rust
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
```

- [ ] **Step 4: Build and run the mapping tests.**

Run: `cargo test --lib mapping::`
Expected: PASS (the remaining `round_trip`, `load_*` tests compile and pass; the deleted test is gone).

- [ ] **Step 5: Commit.**

```bash
git add src/mapping.rs
git commit -m "$(cat <<'EOF'
refactor(mapping): drop vestigial hook_templates, add kind_map_mut

hook_templates in the Mapping had no consumer (absent from SUBST_KINDS;
kind_map returns None for it); cross-cluster retargeting goes through
retarget_hook_template. kind_map_mut is used by the upcoming orient().

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `GenericMapping` type — load / save / round-trip

**Files:**
- Modify: `src/mapping.rs`
- Test: `src/mapping.rs` (in-module `tests`)

**Interfaces:**
- Produces:
  - `struct GenericMapping { version: u32, workspaces/hooks/rules/labels/schemas/queues/inboxes/email_templates/engines/engine_fields: Vec<BTreeMap<String,String>> }`
  - `GenericMapping::load(&Path) -> Result<Self>` (default `version=2`, all-empty, when the file is absent)
  - `GenericMapping::save(&self, &Path) -> Result<()>` (creates parent dir; atomic write)
  - `GenericMapping::is_empty(&self) -> bool`
  - `GenericMapping::KINDS: [&'static str; 10]`
  - `GenericMapping::kind_rows(&self, kind: &str) -> Option<&Vec<BTreeMap<String,String>>>`
  - `GenericMapping::kind_rows_mut(&mut self, kind: &str) -> Option<&mut Vec<BTreeMap<String,String>>>`

- [ ] **Step 1: Write the failing round-trip test.**

Add to the `tests` module in `src/mapping.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify it fails.**

Run: `cargo test --lib mapping::tests::generic_mapping`
Expected: FAIL — `cannot find type GenericMapping`.

- [ ] **Step 3: Implement `GenericMapping`.**

Add to `src/mapping.rs` (after the `Mapping` impl):

```rust
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
}
```

- [ ] **Step 4: Run to verify it passes.**

Run: `cargo test --lib mapping::tests::generic_mapping`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add src/mapping.rs
git commit -m "$(cat <<'EOF'
feat(mapping): add GenericMapping on-disk type (N-way, divergences-only)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `GenericMapping::validate`

**Files:**
- Modify: `src/mapping.rs`
- Test: `src/mapping.rs` (in-module `tests`)

**Interfaces:**
- Consumes: `GenericMapping::KINDS`, `kind_rows`.
- Produces: `GenericMapping::validate(&self, known_envs: &BTreeSet<String>) -> Result<()>`.

- [ ] **Step 1: Write the failing tests.**

Add to `tests` (add `use std::collections::BTreeSet;` if not present):

```rust
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
```

- [ ] **Step 2: Run to verify it fails.**

Run: `cargo test --lib mapping::tests::validate_`
Expected: FAIL — `no method named validate`.

- [ ] **Step 3: Implement `validate`.**

Add to `impl GenericMapping` (ensure `use std::collections::BTreeSet;` at top of file — it currently imports only `BTreeMap`):

```rust
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
```

- [ ] **Step 4: Run to verify it passes.**

Run: `cargo test --lib mapping::tests::validate_`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add src/mapping.rs
git commit -m "$(cat <<'EOF'
feat(mapping): validate GenericMapping env columns and row uniqueness

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `GenericMapping::orient`

**Files:**
- Modify: `src/mapping.rs`
- Test: `src/mapping.rs` (in-module `tests`)

**Interfaces:**
- Consumes: `Mapping::kind_map_mut` (Task 1), `GenericMapping::KINDS`/`kind_rows` (Task 2).
- Produces: `GenericMapping::orient(&self, src_env: &str, tgt_env: &str) -> Mapping`.

- [ ] **Step 1: Write the failing tests.**

```rust
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
```

- [ ] **Step 2: Run to verify it fails.**

Run: `cargo test --lib mapping::tests::orient_`
Expected: FAIL — `no method named orient`.

- [ ] **Step 3: Implement `orient`.**

```rust
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
```

- [ ] **Step 4: Run to verify it passes.**

Run: `cargo test --lib mapping::tests::orient_`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add src/mapping.rs
git commit -m "$(cat <<'EOF'
feat(mapping): orient GenericMapping onto one direction

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `GenericMapping::from_legacy` (v1 → v2 conversion)

**Files:**
- Modify: `src/mapping.rs`
- Test: `src/mapping.rs` (in-module `tests`)

**Interfaces:**
- Consumes: `Mapping::kind_map` (existing), `GenericMapping::KINDS`/`kind_rows_mut`.
- Produces: `GenericMapping::from_legacy(files: &[(String, String, Mapping)]) -> Result<GenericMapping>` — each tuple is `(env_a, env_b, parsed legacy Mapping)`; builds connected components per kind, drops identity edges, hard-errors on inconsistency.

- [ ] **Step 1: Write the failing tests.**

```rust
#[test]
fn from_legacy_joins_transitively_and_drops_identity() {
    // dev->test renames a hook; test->prod renames it further. The two edges
    // share the test node and must join into one 3-env row. Identity pairs are
    // dropped entirely.
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
```

- [ ] **Step 2: Run to verify it fails.**

Run: `cargo test --lib mapping::tests::from_legacy`
Expected: FAIL — `no function named from_legacy`.

- [ ] **Step 3: Implement `from_legacy` + helpers.**

Add to `impl GenericMapping`:

```rust
/// Convert legacy per-pair mappings into one N-way table. Each input tuple is
/// `(env_a, env_b, mapping)` where `mapping` is a legacy `src -> tgt` map read
/// from `<env_a>-to-<env_b>.toml`. Identity pairs are dropped; non-identity
/// pairs become edges `(env_a, src) — (env_b, tgt)` joined into connected
/// components (one row per component). A component that forces one env to two
/// slugs is a hard error. `hook_templates` never enters (it is not a `KINDS`
/// entry and the legacy `Mapping` no longer carries it).
pub fn from_legacy(files: &[(String, String, Mapping)]) -> Result<GenericMapping> {
    let mut out = GenericMapping::default();
    for kind in Self::KINDS {
        let mut rows: Vec<BTreeMap<String, String>> = Vec::new();
        for (env_a, env_b, m) in files {
            let Some(map) = m.kind_map(kind) else { continue };
            for (src, tgt) in map {
                if src == tgt {
                    continue; // identity — no row needed
                }
                merge_legacy_edge(&mut rows, env_a, src, env_b, tgt, kind)?;
            }
        }
        *out.kind_rows_mut(kind).expect("KINDS entry is mappable") = rows;
    }
    Ok(out)
}
```

Add these free functions at module level (below the `impl`s):

```rust
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
```

- [ ] **Step 4: Run to verify it passes.**

Run: `cargo test --lib mapping::tests::from_legacy`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add src/mapping.rs
git commit -m "$(cat <<'EOF'
feat(mapping): convert legacy pairwise files to GenericMapping

Union-find over legacy edges; drop identity; hard-error on inconsistency.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `paths` — single mapping file + legacy discovery

**Files:**
- Modify: `src/paths.rs`
- Test: `src/paths.rs` (in-module `tests`)

**Interfaces:**
- Produces: `Paths::mapping_file(&self) -> PathBuf` (= `<root>/.rdc/mapping.toml`); `Paths::legacy_mapping_files(&self) -> Vec<PathBuf>` (existing `<root>/.rdc/map/*-to-*.toml`). `mapping_dir()` is unchanged.

- [ ] **Step 1: Update the existing `mapping_file` test and add a legacy test.**

In `src/paths.rs` tests, replace the `mapping_file_path` test body:

```rust
#[test]
fn mapping_file_path() {
    assert_eq!(
        p().mapping_file(),
        Path::new("/proj/.rdc/mapping.toml")
    );
}
```

- [ ] **Step 2: Run to verify it fails.**

Run: `cargo test --lib paths::tests::mapping_file_path`
Expected: FAIL — signature mismatch / wrong path.

- [ ] **Step 3: Change `mapping_file` and add `legacy_mapping_files`.**

Replace the current `mapping_file` (lines 123–126):

```rust
/// `<root>/.rdc/mapping.toml` — the single, direction-free slug map.
pub fn mapping_file(&self) -> PathBuf {
    self.root.join(".rdc").join("mapping.toml")
}

/// Legacy per-pair mapping files `<root>/.rdc/map/<a>-to-<b>.toml`, if any.
/// Superseded by [`mapping_file`]; enumerated only to migrate them once.
pub fn legacy_mapping_files(&self) -> Vec<PathBuf> {
    let dir = self.mapping_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("toml")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.contains("-to-"))
                    .unwrap_or(false)
        })
        .collect();
    out.sort();
    out
}
```

- [ ] **Step 4: Run to verify it passes.**

Run: `cargo test --lib paths::tests::mapping_file_path`
Expected: PASS. (The crate will not fully build yet — `migrate::run` still calls the old two-arg `mapping_file`. That is fixed in Task 8. Use `cargo test --lib paths::` which compiles the `paths` unit only if the crate compiles; if the crate fails to link, proceed — Task 8 restores the build and re-runs this test.)

> Note: because `mapping_file`'s signature changes, the crate does not compile until Task 8 updates the caller. Tasks 6–8 form one build-restoring sequence; commit Task 6 even though `cargo build` is red, then keep going.

- [ ] **Step 5: Commit.**

```bash
git add src/paths.rs
git commit -m "$(cat <<'EOF'
refactor(paths): single .rdc/mapping.toml + legacy file discovery

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: migrate helpers — load-or-migrate, env-pair parse, create/update count

**Files:**
- Modify: `src/cli/migrate/mod.rs`
- Test: `src/cli/migrate/mod.rs` (in-module `tests`)

**Interfaces:**
- Consumes: `GenericMapping` (Tasks 2–5), `Paths::mapping_file`/`legacy_mapping_files` (Task 6), `crate::mapping::Mapping`, `classify` (existing), `remap_relative` (existing).
- Produces (module-private):
  - `fn parse_legacy_env_pair(stem: &str, known_envs: &BTreeSet<String>) -> Option<(String, String)>`
  - `fn load_or_migrate_mapping(src_paths: &Paths, known_envs: &BTreeSet<String>, dry_run: bool, log: &crate::log::Log) -> Result<GenericMapping>`
  - `fn count_create_update(files: &[PathBuf], tgt_root: &Path, mapping: &Mapping, selection: &Option<crate::cli::deploy::selection::Selection>) -> (usize, usize)` — returns `(create, update)` counted over primary object JSONs.

- [ ] **Step 1: Write the failing tests.**

Add to the `tests` module in `src/cli/migrate/mod.rs` (import what you need: `use std::collections::BTreeSet;`):

```rust
#[test]
fn parse_legacy_env_pair_handles_hyphenated_envs() {
    let envs = BTreeSet::from([
        "dev-eu".to_string(),
        "test-eu".to_string(),
        "prod-eu".to_string(),
    ]);
    assert_eq!(
        parse_legacy_env_pair("dev-eu-to-test-eu", &envs),
        Some(("dev-eu".to_string(), "test-eu".to_string()))
    );
    assert_eq!(parse_legacy_env_pair("unrelated", &envs), None);
}

#[test]
fn count_create_update_counts_by_target_file_presence() {
    use crate::mapping::Mapping;
    let dir = tempfile::TempDir::new().unwrap();
    let tgt_root = dir.path().join("prod");
    // An existing target hook => update; a missing one => create.
    std::fs::create_dir_all(tgt_root.join("hooks")).unwrap();
    std::fs::write(tgt_root.join("hooks").join("existing.json"), b"{}").unwrap();

    let files = vec![
        PathBuf::from("hooks/existing.json"),
        PathBuf::from("hooks/brand-new.json"),
    ];
    let (create, update) =
        count_create_update(&files, &tgt_root, &Mapping::default(), &None);
    assert_eq!((create, update), (1, 1));
}
```

- [ ] **Step 2: Run to verify it fails.**

Run: `cargo test --lib cli::migrate::tests::parse_legacy_env_pair cli::migrate::tests::count_create_update`
Expected: FAIL — functions not defined.

- [ ] **Step 3: Implement the three helpers.**

Add near the top of `src/cli/migrate/mod.rs` (after the imports; ensure `use crate::mapping::{Mapping, GenericMapping};` and `use std::collections::BTreeSet;` are present):

```rust
/// Recover `(env_a, env_b)` from a legacy `<a>-to-<b>` file stem by matching
/// both sides against known envs. Robust to hyphens in env names: it accepts
/// the split where both halves are real envs.
fn parse_legacy_env_pair(stem: &str, known_envs: &BTreeSet<String>) -> Option<(String, String)> {
    for a in known_envs {
        if let Some(rest) = stem.strip_prefix(&format!("{a}-to-")) {
            if known_envs.contains(rest) {
                return Some((a.clone(), rest.to_string()));
            }
        }
    }
    None
}

/// Load `.rdc/mapping.toml`, or convert legacy per-pair files once if it is
/// absent. On conversion: parse each legacy file, union its edges, write the
/// generic file (unless empty), and delete the legacy files — a clean git diff.
/// A dry run reports the conversion without writing or deleting.
fn load_or_migrate_mapping(
    src_paths: &crate::paths::Paths,
    known_envs: &BTreeSet<String>,
    dry_run: bool,
    log: &crate::log::Log,
) -> Result<GenericMapping> {
    let generic_path = src_paths.mapping_file();
    if generic_path.exists() {
        return GenericMapping::load(&generic_path);
    }
    let legacy = src_paths.legacy_mapping_files();
    if legacy.is_empty() {
        return Ok(GenericMapping::default());
    }

    let mut parsed: Vec<(String, String, Mapping)> = Vec::new();
    for path in &legacy {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let Some((a, b)) = parse_legacy_env_pair(stem, known_envs) else {
            log.event(
                crate::log::Action::Info,
                &format!(
                    "skipping legacy mapping file {} (env pair not in rdc.toml)",
                    path.display()
                ),
            );
            continue;
        };
        parsed.push((a, b, Mapping::load(path)?));
    }

    let generic = GenericMapping::from_legacy(&parsed)?;

    let verb = if dry_run { "would migrate" } else { "migrated" };
    log.event(
        crate::log::Action::Info,
        &format!(
            "{verb} {} legacy mapping file(s) -> {}",
            legacy.len(),
            generic_path.display()
        ),
    );

    if !dry_run {
        if !generic.is_empty() {
            generic.save(&generic_path)?;
        }
        for path in &legacy {
            std::fs::remove_file(path)
                .with_context(|| format!("removing legacy mapping {}", path.display()))?;
        }
    }
    Ok(generic)
}

/// Count how many primary objects this migration will CREATE vs UPDATE in the
/// target, by whether the remapped target JSON already exists. Mirrors the
/// selection gate of the write loop. Used only for the migrate summary.
fn count_create_update(
    files: &[PathBuf],
    tgt_root: &Path,
    mapping: &Mapping,
    selection: &Option<crate::cli::deploy::selection::Selection>,
) -> (usize, usize) {
    let mut create = 0usize;
    let mut update = 0usize;
    for rel in files {
        let Some((kind, slug)) = classify(rel) else { continue };
        if let Some(sel) = selection {
            if !sel.contains(kind, &slug) {
                continue;
            }
        }
        let dst = remap_relative(rel, mapping);
        if tgt_root.join(&dst).exists() {
            update += 1;
        } else {
            create += 1;
        }
    }
    (create, update)
}
```

- [ ] **Step 4: Run to verify it passes.**

Run: `cargo test --lib cli::migrate::tests::parse_legacy_env_pair cli::migrate::tests::count_create_update`
Expected: PASS (the crate builds once Task 8's caller change is in; if red on the old `mapping_file` call, do Task 8 next, then re-run).

- [ ] **Step 5: Commit.**

```bash
git add src/cli/migrate/mod.rs
git commit -m "$(cat <<'EOF'
feat(migrate): helpers for generic-mapping load/convert + create-update count

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Wire `migrate::run` to the generic mapping

**Files:**
- Modify: `src/cli/migrate/mod.rs` (`run`, ~lines 1187–1390)

**Interfaces:**
- Consumes: `load_or_migrate_mapping`, `count_create_update` (Task 7), `GenericMapping::orient`/`validate` (Tasks 3–4).

- [ ] **Step 1: Move `ProjectConfig` load earlier and build `known_envs`.**

The target-org block currently loads `ProjectConfig` at ~line 1239. Move that load to just after the `log` is created (~line 1185) so `known_envs` is available for the mapping step:

```rust
let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());

let project_cfg = crate::config::ProjectConfig::load(&cwd.join("rdc.toml"))?;
let known_envs: std::collections::BTreeSet<String> =
    project_cfg.envs.keys().cloned().collect();
```

Then delete the later duplicate `let project_cfg = ...` line (keep the `tgt_env_cfg`/`tgt_org_url` derivation that uses it).

- [ ] **Step 2: Replace the mapping load/auto-match/prune/save block.**

Replace the whole block from `let mapping_file = src_paths.mapping_file(src, tgt);` through the `if !dry_run { ... mapping.save(&mapping_file)?; }` (current lines ~1188–1219) with:

```rust
// Slug map: load the generic .rdc/mapping.toml (converting legacy per-pair
// files once if needed), validate it, and orient onto this direction.
let generic = load_or_migrate_mapping(&src_paths, &known_envs, dry_run, &log)?;
generic.validate(&known_envs)?;
let mapping = generic.orient(src, tgt);
```

- [ ] **Step 3: Drop the `added` counter from the summary and add create/update + empty-target hint.**

Compute whether the target snapshot is empty, before the write loop (add just after `let files = enumerate_files(&src_root, src)?;`, ~line 1250):

```rust
let tgt_was_empty =
    !tgt_root.exists() || enumerate_files(&tgt_root, tgt)?.is_empty();
let (creates, updates) = count_create_update(&files, &tgt_root, &mapping, &selection);
```

Then change the final summary (current lines ~1375–1388) to:

```rust
let verb = if dry_run { "would migrate" } else { "migrated" };
log.event(
    crate::log::Action::Done,
    &format!(
        "{verb} {copied} file(s) ({renamed} renamed, {pruned} pruned) \
         envs/{src} -> envs/{tgt}"
    ),
);
log.event(
    crate::log::Action::Info,
    &format!("-> {tgt}: {updates} update, {creates} create"),
);
if tgt_was_empty {
    log.event(
        crate::log::Action::Info,
        &format!(
            "'{tgt}' is empty; this migrate creates {creates} object(s). \
             Author envs/{tgt}/overlay.toml for env-specific values."
        ),
    );
}
if !dry_run {
    log.event(
        crate::log::Action::Info,
        &format!("review `git diff`, then `rdc sync {tgt}` to push"),
    );
}
```

- [ ] **Step 4: Remove the `auto_match` call and its `added` binding.**

Delete the `let added = crate::cli::deploy::map::auto_match(...)?;` line (it no longer exists after Step 2's replacement — confirm no remaining reference to `added` compiles away). The `use` of `crate::cli::deploy::map` may become unused; remove any now-dead `use`.

- [ ] **Step 5: Build and run the full migrate + mapping + paths suites.**

Run: `cargo build 2>&1 | tail -20`
Expected: compiles (crate build restored).

Run: `cargo test --lib mapping:: paths:: cli::migrate::`
Expected: PASS.

- [ ] **Step 6: Commit.**

```bash
git add src/cli/migrate/mod.rs
git commit -m "$(cat <<'EOF'
feat(migrate): use generic .rdc/mapping.toml; drop auto-match persistence

migrate now loads the direction-free mapping (converting legacy pairwise
files once), orients it, and reports create-vs-update + an empty-target hint.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Remove dead `deploy/map.rs` auto-match + stale code

**Files:**
- Modify: `src/cli/deploy/map.rs`

- [ ] **Step 1: Confirm there are no remaining callers.**

Run: `grep -rn "auto_match\|stale_mapping_sources\|prune_stale_sources" src/ --include=*.rs | grep -v "src/cli/deploy/map.rs"`
Expected: no output (Task 8 removed the only callers).

- [ ] **Step 2: Delete the public functions and their now-private helpers.**

Remove `auto_match`, `stale_mapping_sources`, `prune_stale_sources`, and any private helper functions they exclusively used (`match_kind`, `match_queues`, `match_schemas`, `match_inboxes`, `match_email_templates`, `match_engines`, `match_engine_fields`, `match_workspaces`, `list_flat_slugs`, `collect_queue_slugs`, `collect_queue_slugs_with_file`, `collect_email_template_keys`, `list_engine_slugs`, `list_engine_field_slugs`, `list_workspace_slugs`) — plus the tests that exercise them. Let the compiler guide you: after deleting the public fns, run the build and delete whatever it now flags as unused (`dead_code`). If a helper is still used elsewhere in the crate, keep it.

- [ ] **Step 3: Build; delete the file if it is now empty.**

Run: `cargo build 2>&1 | tail -20`
Expected: compiles with no `dead_code` warnings for `map.rs`. If `map.rs` has no remaining items, delete it and remove its `mod map;` from `src/cli/deploy/mod.rs`.

- [ ] **Step 4: Run the full library test suite.**

Run: `cargo test --lib 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add -A src/cli/deploy/
git commit -m "$(cat <<'EOF'
refactor(migrate): remove obsolete auto-match/stale mapping code

Identity is now the lookup default; renames are hand-authored, so slug
auto-matching and stale-entry pruning have no purpose.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Documentation

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the `rdc migrate` section's mapping reference.**

Find the sentence describing the mapping (currently "…stored at `.rdc/map/<src>-to-<tgt>.toml`, hand-editable for renames…") and replace with a description of the single generic file:

```markdown
Slug alignment across envs lives in one hand-editable file,
`.rdc/mapping.toml`: each entry is a logical object, and each environment
names its own slug. Objects whose slug is identical everywhere need no entry
(they map 1:1). The file is often empty or absent.
```

- [ ] **Step 2: Add a "Replicate an existing env into a new one" subsection under `rdc migrate`.**

```markdown
### Replicate an existing env into a new one

Attending an existing project usually means bringing PROD *down* into a fresh
DEV/TEST to iterate safely — the reverse of promotion. It is the same two
steps, run backwards:

```sh
rdc init                 # add the target env (its org must already exist)
rdc migrate prod dev     # copy prod's snapshot into envs/dev/ locally
git diff                 # review
rdc sync dev             # create the objects in the dev org
```

`migrate` reports how many objects it will create vs update. When the target
is empty, every object is a create — author `envs/dev/overlay.toml` for the
values that must differ (token owners, external URLs, names) before syncing.

Because slug alignment is direction-free (`.rdc/mapping.toml`), promoting the
same objects back up later (`rdc migrate dev prod`) patches the originals
rather than duplicating them.
```

- [ ] **Step 3: Verify the README renders (no broken fences).**

Run: `grep -n "rdc migrate prod dev" README.md`
Expected: the new subsection lines are present.

- [ ] **Step 4: Commit.**

```bash
git add README.md
git commit -m "$(cat <<'EOF'
docs: single .rdc/mapping.toml + replicate-an-env (reverse) flow

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Acceptance verification (live + real project)

**Files:** none (verification only).

- [ ] **Step 1: Full suite + confirm the binary under test.**

Run: `cargo test --lib 2>&1 | tail -20`
Expected: PASS.

Build and note the local binary path so Homebrew's `rdc` does not shadow it:

Run: `cargo build --release && ls -l target/release/rdc`

- [ ] **Step 2: Live round-trip on the sandbox org (opt-in).**

Using the sandbox credentials (env vars only; nothing hardcoded), in a throwaway project:
1. `rdc migrate prod dev` into an empty `dev`, then `rdc sync dev` — confirm summary shows `N create, 0 update`.
2. Edit one object under `envs/dev/`, `rdc sync dev`.
3. `rdc migrate dev prod` — confirm summary shows `... update, 0 create` (no duplicate creates).
4. `rdc sync prod --dry-run` — confirm only PATCHes, no POSTs, for the round-tripped objects.

Run the existing live harness too:
`cargo test --test live -- --ignored --test-threads=1`
Expected: unchanged pass count (no regressions).

- [ ] **Step 3: Migrate the real multi-env acceptance repo (careful; no destructive git).**

In the real project repo (holds uncommitted work — **no bare `git stash`/`checkout`/`reset`/`clean`**):
1. Confirm which `rdc` runs (`which rdc`); use the freshly built binary explicitly if needed.
2. Run any `rdc migrate <a> <b> --dry-run` to trigger the lazy conversion preview; review the log line.
3. Run it for real; then `git status` / `git diff` and confirm: the three `.rdc/map/*-to-*.toml` files are deleted and `.rdc/mapping.toml` is either absent or contains only genuine renames (expected: absent — 0 slug renames after `hook_templates` is dropped).
4. Re-run `rdc migrate <a> <b>` and `rdc sync <b> --dry-run`; confirm byte-stable / idempotent (no churn, no unexpected creates).

- [ ] **Step 4: Record the outcome.**

Update the memory note `project_nway_mapping_bidirectional.md` with the live + real-project results (pass/fail, any surprises), and mark the feature implemented + verified.

---

## Self-Review

**1. Spec coverage:**
- Generic N-way file + format → Tasks 2, 10. ✓
- Orientation into existing `Mapping` → Task 4. ✓
- Identity default / divergences-only → Tasks 2 (skip empty), 4 (only differing pairs). ✓
- Drop `hook_templates` → Task 1. ✓
- Guardrails: create/update summary → Tasks 7, 8; validation → Task 3, 8; warn-not-prune on stale → covered by *removing* auto-prune (Task 9) and leaving hand-authored rows untouched; explicit stale-row warning is deferred (no code writes rows, so a stale row simply orients to nothing — acceptable; documented here as a known non-goal for v1). ✓ (note the deferral)
- Greenfield: docs + empty-target hint, no scaffold → Tasks 8, 10. ✓
- Backward-compat v1→v2 conversion (union-find, drop identity + hook_templates, hard-error, delete legacy) → Tasks 5, 7, 8. ✓
- Testing: unit across Tasks 1–7; live + real-project → Task 11. ✓

**2. Placeholder scan:** No TBD/TODO; every code step carries complete code; commands have expected output. The one deliberate flexibility is Task 9 Step 2 ("let the compiler guide you" for dead helpers) — this is standard dead-code removal, with the exact candidate list enumerated. ✓

**3. Type consistency:** `GenericMapping` field/method names (`kind_rows`, `kind_rows_mut`, `KINDS`, `orient`, `validate`, `from_legacy`, `is_empty`), `Mapping::kind_map_mut`, `Paths::mapping_file()`/`legacy_mapping_files()`, and the migrate helpers (`parse_legacy_env_pair`, `load_or_migrate_mapping`, `count_create_update`) are used identically wherever referenced across tasks. ✓

**Known deferral (surfaced, not hidden):** an explicit "this hand-authored row references a slug that no longer exists" warning is not implemented; with auto-match gone, nothing writes rows, and a stale row orients to nothing rather than causing harm. If desired it is a small follow-up (a `doctor` check), out of scope here.
