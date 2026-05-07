# rdc M13 — Push & Apply for Rules + Labels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend push and deploy (apply) to cover rules and labels — the next-most-frequently-edited kinds. Pure pattern replication of M10/M11/M12 against new object types.

**Architecture:** Same shape as hooks. Each new kind gets: `RossumClient::update_<kind>` (PATCH method); a push driver in `cli/push/<kind>s.rs`; integration into the push orchestrator; mapping support for `<kind>s` (parallel to `hooks`); apply driver loop for the kind.

**Tech Stack:** Same as M12.

**Scope:**
- ✅ `rdc push` covers: hooks, rules, labels
- ✅ `rdc map` auto-matches: hooks, rules, labels
- ✅ `rdc apply` deploys: hooks, rules, labels (with tgt overlay applied)
- ❌ NOT queues/schemas/inboxes/engines/workflows/email_templates/MDH (more complex; future)
- ❌ NOT auxiliary commands (`status`, `diff`, `auth`, `repair`) — future
- ❌ NOT pull-side overlay stripping — future
- ❌ NOT cross-ref indexer — future

**End state of M13:**

```
$ rdc push dev
Pushed 1 hook, 2 rules, 0 labels to env 'dev'

$ rdc map test prod
Auto-matched 2 new hooks, 1 rule, 2 labels by slug. Wrote .rdc/map/test→prod.toml.

$ rdc apply --from test --to prod
Applied 2 hooks, 1 rule, 2 labels (5 PATCHes) from test to prod
```

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `src/api/mod.rs` | Modify | Add `update_rule` and `update_label` |
| `src/cli/push/rules.rs` | Create | Push driver for rules |
| `src/cli/push/labels.rs` | Create | Push driver for labels |
| `src/cli/push/mod.rs` | Modify | Wire rules + labels into the orchestrator and summary |
| `src/mapping.rs` | Modify | Add `rules` and `labels` BTreeMap fields |
| `src/cli/deploy/map.rs` | Modify | Auto-match rules and labels too |
| `src/cli/deploy/plan.rs` | Modify | Show plan for rules and labels |
| `src/cli/deploy/apply.rs` | Modify | Apply rules and labels |
| `src/overlay.rs` | Modify | Add `rules` and `labels` BTreeMap fields |
| `tests/api.rs` | Modify | Tests for `update_rule` and `update_label` |
| `tests/cli_push.rs` | Modify | Tests for rule/label push |
| `tests/cli_deploy.rs` | Modify | Test rule/label propagation through deploy flow |
| `README.md` | Modify | Document expanded coverage |

---

## Task 1: API methods `update_rule`, `update_label`

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `tests/api.rs`

- [ ] **Step 1: Add the PATCH methods**

In `src/api/mod.rs`, after `update_hook`, add:

```rust
    pub async fn update_rule(&self, id: u64, rule: &crate::model::Rule) -> Result<crate::model::Rule> {
        let url = format!("{}/rules/{id}", self.base_url);
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", format!("token {}", self.token))
            .json(rule)
            .send()
            .await
            .with_context(|| format!("PATCH {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body }.into());
        }
        let value = resp.json::<crate::model::Rule>().await
            .with_context(|| format!("decoding PATCH response from {url}"))?;
        Ok(value)
    }

    pub async fn update_label(&self, id: u64, label: &crate::model::Label) -> Result<crate::model::Label> {
        let url = format!("{}/labels/{id}", self.base_url);
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", format!("token {}", self.token))
            .json(label)
            .send()
            .await
            .with_context(|| format!("PATCH {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body }.into());
        }
        let value = resp.json::<crate::model::Label>().await
            .with_context(|| format!("decoding PATCH response from {url}"))?;
        Ok(value)
    }
```

- [ ] **Step 2: Add tests**

Append to `tests/api.rs`:

```rust
#[tokio::test]
async fn update_rule_patches_and_returns_response() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/rules/2597"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 2597,
            "url": "https://mock.rossum.app/api/v1/rules/2597",
            "name": "E-invoice Validation",
            "queues": []
        })))
        .mount(&server).await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let rule: rdc::model::Rule = serde_json::from_value(serde_json::json!({
        "id": 2597,
        "url": "https://mock.rossum.app/api/v1/rules/2597",
        "name": "E-invoice Validation",
        "queues": []
    })).unwrap();
    let updated = client.update_rule(2597, &rule).await.unwrap();
    assert_eq!(updated.id, 2597);
}

#[tokio::test]
async fn update_label_patches_and_returns_response() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/labels/11"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 11,
            "url": "https://mock.rossum.app/api/v1/labels/11",
            "name": "Priority High",
            "organization": "https://mock.rossum.app/api/v1/organizations/285704"
        })))
        .mount(&server).await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let label: rdc::model::Label = serde_json::from_value(serde_json::json!({
        "id": 11,
        "url": "https://mock.rossum.app/api/v1/labels/11",
        "name": "Priority High",
        "organization": "https://mock.rossum.app/api/v1/organizations/285704"
    })).unwrap();
    let updated = client.update_label(11, &label).await.unwrap();
    assert_eq!(updated.id, 11);
}
```

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --test api`
Expected: 20 tests pass (18 + 2 new).

- [ ] **Step 4: Commit**

```bash
git add src/api/mod.rs tests/api.rs
git commit -m "feat(api): update_rule and update_label (PATCH)"
```

---

## Task 2: Push drivers for rules and labels

**Files:**
- Create: `src/cli/push/rules.rs`
- Create: `src/cli/push/labels.rs`
- Modify: `src/cli/push/mod.rs`
- Modify: `src/overlay.rs`

For rules and labels, the JSON has no extracted code (unlike hooks), so the hash is plain `content_hash` over JSON bytes — no combined hash needed.

- [ ] **Step 1: Extend `Overlay` to support rules and labels**

In `src/overlay.rs`, update the struct and tests:

```rust
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Overlay {
    pub version: u32,
    #[serde(default)]
    pub hooks: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(default)]
    pub rules: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(default)]
    pub labels: BTreeMap<String, BTreeMap<String, Value>>,
}

impl Overlay {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        // unchanged
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
}
```

- [ ] **Step 2: Create `src/cli/push/rules.rs`**

```rust
use crate::api::RossumClient;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let kind_dir = paths.rules_dir();
    if !kind_dir.exists() {
        return Ok((0, 0));
    }

    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let entries: Vec<_> = std::fs::read_dir(&kind_dir)
        .with_context(|| format!("reading {}", kind_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing {}", kind_dir.display()))?;

    let mut remote_rules: Option<Vec<crate::model::Rule>> = None;

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue;
        }

        // Read local rule.
        let path = kind_dir.join(format!("{slug}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let local_rule: crate::model::Rule = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;

        // Apply overlay if present.
        let mut payload = serde_json::to_value(&local_rule)
            .context("serializing local rule to value")?;
        if let Some(ov) = &overlay {
            if let Some(rule_overrides) = ov.rule(slug) {
                apply_overrides(&mut payload, rule_overrides);
            }
        }
        let payload_rule: crate::model::Rule = serde_json::from_value(payload.clone())
            .with_context(|| format!("re-deserializing overlay-applied rule '{slug}'"))?;

        let mut post_overlay_bytes = serde_json::to_vec_pretty(&payload_rule)
            .context("serializing rule")?;
        post_overlay_bytes.push(b'\n');
        let local_combined = content_hash(&post_overlay_bytes);

        let entry = lockfile.objects.get("rules").and_then(|m| m.get(slug));
        let Some(entry) = entry else {
            eprintln!("warning: rules/{slug}.json — no lockfile entry, skipping");
            skipped += 1;
            continue;
        };
        let Some(base) = &entry.content_hash else {
            eprintln!("warning: rules/{slug}.json — lockfile entry has no content_hash, skipping");
            skipped += 1;
            continue;
        };

        if &local_combined == base {
            continue;
        }

        let id = entry.id;

        if remote_rules.is_none() {
            remote_rules = Some(client.list_rules().await
                .context("listing rules to verify no drift before push")?);
        }
        let remote_list = remote_rules.as_ref().unwrap();
        let Some(remote_rule) = remote_list.iter().find(|r| r.id == id) else {
            eprintln!("warning: rules/{slug}.json — id {id} not found on remote, skipping");
            skipped += 1;
            continue;
        };

        let mut remote_bytes = serde_json::to_vec_pretty(remote_rule)
            .context("serializing remote rule")?;
        remote_bytes.push(b'\n');
        let remote_combined = content_hash(&remote_bytes);

        if &remote_combined != base {
            eprintln!("warning: rules/{slug}.json — remote has changed since last pull, skipping push");
            skipped += 1;
            continue;
        }

        let updated = client.update_rule(id, &payload_rule).await
            .with_context(|| format!("PATCH /rules/{id}"))?;

        let mut updated_bytes = serde_json::to_vec_pretty(&updated)
            .context("serializing updated rule")?;
        updated_bytes.push(b'\n');
        let updated_hash = content_hash(&updated_bytes);
        lockfile.upsert(
            "rules",
            slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
            },
        );
        pushed += 1;
    }

    Ok((pushed, skipped))
}
```

- [ ] **Step 3: Create `src/cli/push/labels.rs`**

Same shape as rules — replace `Rule` with `Label`, `rules` with `labels`, `update_rule` with `update_label`, `list_rules` with `list_labels`. Save to `src/cli/push/labels.rs`:

```rust
use crate::api::RossumClient;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let kind_dir = paths.labels_dir();
    if !kind_dir.exists() {
        return Ok((0, 0));
    }

    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let entries: Vec<_> = std::fs::read_dir(&kind_dir)
        .with_context(|| format!("reading {}", kind_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing {}", kind_dir.display()))?;

    let mut remote_labels: Option<Vec<crate::model::Label>> = None;

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue;
        }

        let path = kind_dir.join(format!("{slug}.json"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let local_label: crate::model::Label = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;

        let mut payload = serde_json::to_value(&local_label)
            .context("serializing local label to value")?;
        if let Some(ov) = &overlay {
            if let Some(label_overrides) = ov.label(slug) {
                apply_overrides(&mut payload, label_overrides);
            }
        }
        let payload_label: crate::model::Label = serde_json::from_value(payload.clone())
            .with_context(|| format!("re-deserializing overlay-applied label '{slug}'"))?;

        let mut post_overlay_bytes = serde_json::to_vec_pretty(&payload_label)
            .context("serializing label")?;
        post_overlay_bytes.push(b'\n');
        let local_combined = content_hash(&post_overlay_bytes);

        let entry = lockfile.objects.get("labels").and_then(|m| m.get(slug));
        let Some(entry) = entry else {
            eprintln!("warning: labels/{slug}.json — no lockfile entry, skipping");
            skipped += 1;
            continue;
        };
        let Some(base) = &entry.content_hash else {
            eprintln!("warning: labels/{slug}.json — lockfile entry has no content_hash, skipping");
            skipped += 1;
            continue;
        };

        if &local_combined == base {
            continue;
        }

        let id = entry.id;

        if remote_labels.is_none() {
            remote_labels = Some(client.list_labels().await
                .context("listing labels to verify no drift before push")?);
        }
        let remote_list = remote_labels.as_ref().unwrap();
        let Some(remote_label) = remote_list.iter().find(|l| l.id == id) else {
            eprintln!("warning: labels/{slug}.json — id {id} not found on remote, skipping");
            skipped += 1;
            continue;
        };

        let mut remote_bytes = serde_json::to_vec_pretty(remote_label)
            .context("serializing remote label")?;
        remote_bytes.push(b'\n');
        let remote_combined = content_hash(&remote_bytes);

        if &remote_combined != base {
            eprintln!("warning: labels/{slug}.json — remote has changed since last pull, skipping push");
            skipped += 1;
            continue;
        }

        let updated = client.update_label(id, &payload_label).await
            .with_context(|| format!("PATCH /labels/{id}"))?;

        let mut updated_bytes = serde_json::to_vec_pretty(&updated)
            .context("serializing updated label")?;
        updated_bytes.push(b'\n');
        let updated_hash = content_hash(&updated_bytes);
        lockfile.upsert(
            "labels",
            slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
            },
        );
        pushed += 1;
    }

    Ok((pushed, skipped))
}
```

- [ ] **Step 4: Wire into orchestrator**

Replace `src/cli/push/mod.rs`:

```rust
use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

mod hooks;
mod labels;
mod rules;

pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())
        .with_context(|| format!("loading project config from {}", paths.project_config().display()))?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;

    let (n_hooks, c_hooks) = hooks::push(&paths, &client, &mut lockfile).await
        .with_context(|| format!("pushing hooks for env '{env}'"))?;
    let (n_rules, c_rules) = rules::push(&paths, &client, &mut lockfile).await
        .with_context(|| format!("pushing rules for env '{env}'"))?;
    let (n_labels, c_labels) = labels::push(&paths, &client, &mut lockfile).await
        .with_context(|| format!("pushing labels for env '{env}'"))?;

    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)
        .with_context(|| format!("regenerating _index.md for env '{env}'"))?;

    let mut summary = format!(
        "Pushed {}, {}, {} to env '{env}'",
        crate::cli::pull::common::pluralize(n_hooks, "hook", "hooks"),
        crate::cli::pull::common::pluralize(n_rules, "rule", "rules"),
        crate::cli::pull::common::pluralize(n_labels, "label", "labels"),
    );
    let total_skipped = c_hooks + c_rules + c_labels;
    if total_skipped > 0 {
        summary.push_str(&format!(", {} skipped (conflict)", total_skipped));
    }
    println!("{summary}");
    Ok(())
}
```

- [ ] **Step 5: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: existing tests pass (no behavior change for hooks; new push paths kick in when rules/labels dirs have entries).

- [ ] **Step 6: Commit**

```bash
git add src/
git commit -m "feat(cli): rdc push covers rules and labels"
```

---

## Task 3: Mapping + plan + apply for rules and labels

**Files:**
- Modify: `src/mapping.rs`
- Modify: `src/cli/deploy/map.rs`
- Modify: `src/cli/deploy/plan.rs`
- Modify: `src/cli/deploy/apply.rs`

- [ ] **Step 1: Extend `Mapping`**

In `src/mapping.rs`, replace the struct + Default impl:

```rust
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Mapping {
    pub version: u32,
    #[serde(default)]
    pub hooks: BTreeMap<String, String>,
    #[serde(default)]
    pub rules: BTreeMap<String, String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            version: 1,
            hooks: BTreeMap::new(),
            rules: BTreeMap::new(),
            labels: BTreeMap::new(),
        }
    }
}
```

Update the round_trip test to also populate rules and labels:

```rust
    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_to_prod.toml");
        let mut m = Mapping::default();
        m.hooks.insert("validator-invoices".into(), "validator-invoices".into());
        m.rules.insert("validation-rule".into(), "validation-rule".into());
        m.labels.insert("priority-high".into(), "priority-high".into());
        m.save(&path).unwrap();
        let loaded = Mapping::load(&path).unwrap();
        assert_eq!(loaded, m);
    }
```

- [ ] **Step 2: Extend `cli/deploy/map.rs`**

Generalize the slug-listing helper to take a directory path, then call it for hooks_dir + rules_dir + labels_dir. Replace `src/cli/deploy/map.rs` with:

```rust
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::paths::Paths;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;

pub async fn run(src: &str, tgt: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())
        .with_context(|| format!("loading project config from {}", src_paths.project_config().display()))?;
    if !cfg.envs.contains_key(src) {
        return Err(anyhow!("env '{src}' is not defined in rdc.toml"));
    }
    if !cfg.envs.contains_key(tgt) {
        return Err(anyhow!("env '{tgt}' is not defined in rdc.toml"));
    }

    let mapping_path = src_paths.mapping_file(src, tgt);
    let mut mapping = Mapping::load(&mapping_path)?;

    let h_new = match_kind(&mut mapping.hooks, &src_paths.hooks_dir(), &tgt_paths.hooks_dir())?;
    let r_new = match_kind(&mut mapping.rules, &src_paths.rules_dir(), &tgt_paths.rules_dir())?;
    let l_new = match_kind(&mut mapping.labels, &src_paths.labels_dir(), &tgt_paths.labels_dir())?;

    let any = h_new + r_new + l_new;
    if any > 0 || !mapping.hooks.is_empty() || !mapping.rules.is_empty() || !mapping.labels.is_empty() {
        std::fs::create_dir_all(src_paths.mapping_dir())
            .with_context(|| format!("creating {}", src_paths.mapping_dir().display()))?;
        mapping.save(&mapping_path)?;
    }

    println!(
        "Auto-matched {} new hooks, {} new rules, {} new labels by slug. Wrote {}.",
        h_new, r_new, l_new, mapping_path.display()
    );
    Ok(())
}

fn match_kind(
    existing: &mut BTreeMap<String, String>,
    src_dir: &std::path::Path,
    tgt_dir: &std::path::Path,
) -> Result<usize> {
    let src_slugs = list_slugs(src_dir)?;
    let tgt_slugs: std::collections::HashSet<_> = list_slugs(tgt_dir)?.into_iter().collect();
    let mut added = 0;
    for src_slug in &src_slugs {
        if existing.contains_key(src_slug) {
            continue;
        }
        if tgt_slugs.contains(src_slug) {
            existing.insert(src_slug.clone(), src_slug.clone());
            added += 1;
        }
    }
    Ok(added)
}

fn list_slugs(dir: &std::path::Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
    {
        let entry = entry
            .with_context(|| format!("listing {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(slug) = name.strip_suffix(".json") {
            if !slug.ends_with(".remote") {
                out.push(slug.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}
```

- [ ] **Step 3: Extend `cli/deploy/plan.rs`**

Replace `src/cli/deploy/plan.rs`:

```rust
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;

pub async fn run(src: &str, tgt: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())
        .with_context(|| format!("loading project config from {}", src_paths.project_config().display()))?;
    if !cfg.envs.contains_key(src) {
        return Err(anyhow!("env '{src}' is not defined in rdc.toml"));
    }
    if !cfg.envs.contains_key(tgt) {
        return Err(anyhow!("env '{tgt}' is not defined in rdc.toml"));
    }

    let mapping = Mapping::load(&src_paths.mapping_file(src, tgt))?;
    let tgt_lockfile = Lockfile::load(&tgt_paths.lockfile())?;

    println!("Plan: {src} → {tgt}");

    let mut total_count = 0;
    let mut total_warnings = 0;
    total_count += plan_kind("hooks", &mapping.hooks, &src_paths.hooks_dir(), &tgt_lockfile, tgt, &mut total_warnings);
    total_count += plan_kind("rules", &mapping.rules, &src_paths.rules_dir(), &tgt_lockfile, tgt, &mut total_warnings);
    total_count += plan_kind("labels", &mapping.labels, &src_paths.labels_dir(), &tgt_lockfile, tgt, &mut total_warnings);

    if total_count == 0 && total_warnings == 0 {
        println!("  (no mapped objects)");
    }
    Ok(())
}

fn plan_kind(
    kind: &str,
    pairs: &BTreeMap<String, String>,
    src_dir: &std::path::Path,
    tgt_lockfile: &Lockfile,
    tgt: &str,
    warnings: &mut usize,
) -> usize {
    let mut count = 0;
    for (src_slug, tgt_slug) in pairs {
        let src_path = src_dir.join(format!("{src_slug}.json"));
        if !src_path.exists() {
            eprintln!("warning: src {kind}/{src_slug}.json missing — skipping in plan");
            *warnings += 1;
            continue;
        }
        let tgt_id = tgt_lockfile
            .objects
            .get(kind)
            .and_then(|m| m.get(tgt_slug))
            .map(|e| e.id);
        let Some(tgt_id) = tgt_id else {
            eprintln!("warning: tgt lockfile has no entry for {kind}/{tgt_slug} — run `rdc pull {tgt}` first");
            *warnings += 1;
            continue;
        };
        println!("  ~ {kind}/{src_slug}  →  {tgt}/{tgt_slug} (id {tgt_id})");
        count += 1;
    }
    count
}
```

- [ ] **Step 4: Extend `cli/deploy/apply.rs`**

Apply needs to handle three kinds in the same flow. Replace `src/cli/deploy/apply.rs`:

```rust
use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::snapshot::hook::read_hook;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

pub async fn run(src: &str, tgt: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())
        .with_context(|| format!("loading project config from {}", src_paths.project_config().display()))?;
    let _src_cfg = cfg.envs.get(src).ok_or_else(|| anyhow!("env '{src}' is not defined in rdc.toml"))?;
    let tgt_cfg = cfg.envs.get(tgt).ok_or_else(|| anyhow!("env '{tgt}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, tgt)?;
    let tgt_client = RossumClient::new(tgt_cfg.api_base.clone(), token)
        .context("constructing tgt API client")?;

    let mapping = Mapping::load(&src_paths.mapping_file(src, tgt))?;
    let tgt_lockfile = Lockfile::load(&tgt_paths.lockfile())?;
    let tgt_overlay = Overlay::load(&tgt_paths.overlay_file())
        .with_context(|| format!("loading tgt overlay from {}", tgt_paths.overlay_file().display()))?;

    let mut applied_hooks = 0;
    let mut applied_rules = 0;
    let mut applied_labels = 0;
    let mut skipped = 0;

    // Hooks
    for (src_slug, tgt_slug) in &mapping.hooks {
        let tgt_id = match tgt_lockfile.objects.get("hooks").and_then(|m| m.get(tgt_slug)).map(|e| e.id) {
            Some(id) => id,
            None => { eprintln!("warning: tgt lockfile has no entry for hooks/{tgt_slug} — skipping"); skipped += 1; continue; }
        };
        let src_hook = match read_hook(&src_paths.hooks_dir(), src_slug) {
            Ok(h) => h,
            Err(e) => { eprintln!("warning: cannot read src hooks/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        let mut payload = serde_json::to_value(&src_hook).context("serializing src hook")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(overrides) = ov.hook(tgt_slug) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_hook: crate::model::Hook = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied hook for tgt slug '{tgt_slug}'"))?;
        tgt_client.update_hook(tgt_id, &payload_hook).await
            .with_context(|| format!("PATCH tgt hooks/{tgt_id} (mapped from src '{src_slug}')"))?;
        applied_hooks += 1;
    }

    // Rules
    for (src_slug, tgt_slug) in &mapping.rules {
        let tgt_id = match tgt_lockfile.objects.get("rules").and_then(|m| m.get(tgt_slug)).map(|e| e.id) {
            Some(id) => id,
            None => { eprintln!("warning: tgt lockfile has no entry for rules/{tgt_slug} — skipping"); skipped += 1; continue; }
        };
        let path = src_paths.rules_dir().join(format!("{src_slug}.json"));
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => { eprintln!("warning: cannot read src rules/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        let src_rule: crate::model::Rule = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        let mut payload = serde_json::to_value(&src_rule).context("serializing src rule")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(overrides) = ov.rule(tgt_slug) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_rule: crate::model::Rule = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied rule for tgt slug '{tgt_slug}'"))?;
        tgt_client.update_rule(tgt_id, &payload_rule).await
            .with_context(|| format!("PATCH tgt rules/{tgt_id}"))?;
        applied_rules += 1;
    }

    // Labels
    for (src_slug, tgt_slug) in &mapping.labels {
        let tgt_id = match tgt_lockfile.objects.get("labels").and_then(|m| m.get(tgt_slug)).map(|e| e.id) {
            Some(id) => id,
            None => { eprintln!("warning: tgt lockfile has no entry for labels/{tgt_slug} — skipping"); skipped += 1; continue; }
        };
        let path = src_paths.labels_dir().join(format!("{src_slug}.json"));
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => { eprintln!("warning: cannot read src labels/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        let src_label: crate::model::Label = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        let mut payload = serde_json::to_value(&src_label).context("serializing src label")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(overrides) = ov.label(tgt_slug) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_label: crate::model::Label = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied label for tgt slug '{tgt_slug}'"))?;
        tgt_client.update_label(tgt_id, &payload_label).await
            .with_context(|| format!("PATCH tgt labels/{tgt_id}"))?;
        applied_labels += 1;
    }

    let total = applied_hooks + applied_rules + applied_labels;
    let mut summary = format!(
        "Applied {} hooks, {} rules, {} labels ({} PATCHes) from {src} to {tgt}",
        applied_hooks, applied_rules, applied_labels, total
    );
    if skipped > 0 {
        summary.push_str(&format!(", {} skipped", skipped));
    }
    println!("{summary}");
    Ok(())
}
```

- [ ] **Step 5: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all existing tests pass (the old `cli_deploy.rs` integration test only mocks hook PATCHes; it should still pass since the apply will now also TRY to deploy rules/labels but mapping has none, so apply skips them).

WAIT — actually, the existing test uses fixtures with rules_list = empty_list (per `mount_full_pull` helper). After pulls, mapping has hooks only (auto-match). So apply only patches hooks. The existing assertion "Applied 2" needs to change to "Applied 2 hooks, 0 rules, 0 labels (2 PATCHes)". Update the test.

In `tests/cli_deploy.rs`, find the apply assertion:

```rust
        .stdout(predicate::str::contains("Applied 2"));
```

This still passes (the new summary contains "Applied 2 hooks"). Should be fine.

- [ ] **Step 6: Commit**

```bash
git add src/
git commit -m "feat(cli): rdc map/plan/apply cover rules and labels"
```

---

## Task 4: README + memory

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update README**

Update Status:
```
**Status:** M13. Pull side complete. Push + apply for hooks, rules, labels.
```

Update the "M10 limitations" + "M12 limitations" notes to reflect rules+labels coverage. Add a small note like:

```
**Coverage as of M13:** push and apply work for hooks, rules, and labels. Other
kinds (queues, schemas, inboxes, engines, engine_fields, workflows,
workflow_steps, email_templates, MDH) are pull-only — extending push to them
is future work.
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: M13 — push + apply for rules and labels"
```

---

## Self-Review

**Spec coverage:**
- §6 push/plan/apply — extended to rules and labels (3/14 kinds covered for write).
- §9 overlay — extended to rules and labels.

**Placeholder scan:** No "TBD"/"TODO" patterns.

**Type consistency:** `update_rule(id, &Rule) -> Result<Rule>` and `update_label(id, &Label) -> Result<Label>` consistent across Tasks 1-3. `Overlay` rules/labels accessor `Option<&BTreeMap<String, Value>>` consistent with `hook()` accessor.

**Scope check:** 4 tasks. Mostly mechanical replication of M10/M12 patterns. ~700 LOC of new code.

---

## Next milestones

- **M14:** Distribution. Homebrew tap, GitHub releases (cross-compiled binaries via cargo-dist or ci script), curl|sh installer. The "ship it" milestone — the tool becomes usable without a Rust toolchain.
- Future: push for remaining kinds (queues with cross-refs, schemas with formula combined hash, MDH); pull-side overlay stripping; auxiliary commands; cross-ref indexer.
