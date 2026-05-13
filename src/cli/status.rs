//! `rdc status [env]` — read-only health check (per spec §6).
//!
//! For each env (or all envs if no arg):
//! 1. Token presence: env var or secrets file? Loud if missing.
//! 2. Auth: GET /organizations/{org_id} succeeds?
//! 3. Lockfile: present + readable + version + object counts.
//! 4. Local edits: per writable kind, count of files whose hash differs
//!    from the lockfile's `content_hash` (= what `rdc push` would send).
//!
//! No side effects. No file writes. No PATCHes.

use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::snapshot::hook::serialize_hook;
use crate::snapshot::schema::serialize_schema;
use crate::state::{
    content_hash, hook_combined_hash, schema_combined_hash, Lockfile,
};
use anyhow::{anyhow, Context, Result};
use std::path::Path;

pub async fn run(env_filter: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;

    let envs_to_check: Vec<String> = match &env_filter {
        Some(e) => {
            if !cfg.envs.contains_key(e) {
                return Err(anyhow!("env '{e}' is not defined in rdc.toml"));
            }
            vec![e.clone()]
        }
        None => cfg.envs.keys().cloned().collect(),
    };

    for (i, env) in envs_to_check.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let env_cfg = cfg.envs.get(env).expect("env present in cfg");
        let paths = Paths::for_env(&cwd, env);

        println!("Env '{env}'");
        println!("  api_base: {}", env_cfg.api_base);
        println!("  org_id:   {}", env_cfg.org_id);

        // Token + auth.
        match resolve_token(&cwd, env) {
            Err(e) => {
                println!("  token:    missing ({e})");
            }
            Ok(token) => {
                println!("  token:    present");
                let client = match RossumClient::new(env_cfg.api_base.clone(), token) {
                    Ok(c) => c,
                    Err(e) => {
                        println!("  auth:     failed (HTTP client init: {e:#})");
                        continue;
                    }
                };
                match client.get_organization(env_cfg.org_id, None).await {
                    Ok(org) => println!("  auth:     ok (org '{}', id {})", org.name, org.id),
                    Err(e) => println!("  auth:     failed ({})", first_line(&format!("{e:#}"))),
                }
            }
        }

        // Lockfile.
        let lockfile_path = paths.lockfile();
        if !lockfile_path.exists() {
            println!("  lockfile: missing (run `rdc pull {env}`)");
            continue;
        }
        let lockfile = match Lockfile::load(&lockfile_path) {
            Ok(l) => l,
            Err(e) => {
                println!("  lockfile: unreadable ({e:#})");
                continue;
            }
        };
        let total_objects: usize = lockfile.objects.values().map(|m| m.len()).sum();
        println!(
            "  lockfile: v{}, {total_objects} objects across {} kinds",
            lockfile.version,
            lockfile.objects.len()
        );

        // Local edits per writable kind. For each, compare local-on-disk
        // hash to lockfile.content_hash. The snapshot is authoritative
        // (this is exactly what `rdc push` would compare against).
        let edits = local_edits(&paths, &lockfile)?;
        if edits.is_empty() {
            println!("  edits:    none (push would be a no-op)");
        } else {
            println!("  edits:    {} file(s) differ from lockfile:", edits.len());
            for (kind, slug) in &edits {
                println!("            {kind}/{slug}");
            }
        }

        // Pending deletes: lockfile entries whose local file is missing.
        // Surfaced here so users see the destructive side of a future
        // `rdc push` before invoking it.
        let tombstones = crate::cli::push::scan::detect_tombstones(&paths, &lockfile);
        if tombstones.is_empty() {
            println!("  deletes:  none");
        } else {
            let total = tombstones.total();
            println!(
                "  deletes:  {total} file(s) tracked but missing locally \
                 (run `rdc push {env} --allow-deletes` to remove from remote):"
            );
            let listing: [(&str, &std::collections::BTreeMap<String, u64>); 10] = [
                ("workspaces", &tombstones.workspaces),
                ("schemas", &tombstones.schemas),
                ("queues", &tombstones.queues),
                ("inboxes", &tombstones.inboxes),
                ("email_templates", &tombstones.email_templates),
                ("hooks", &tombstones.hooks),
                ("rules", &tombstones.rules),
                ("labels", &tombstones.labels),
                ("engines", &tombstones.engines),
                ("engine_fields", &tombstones.engine_fields),
            ];
            for (name, m) in listing {
                for slug in m.keys() {
                    println!("            {name}/{slug}");
                }
            }
        }

        // Pending renames: stale local slugs vs current JSON name fields.
        // Surfaced here so it's discoverable outside of pull-summary
        // scrollback. Run `rdc map <env>` to apply.
        let pending = crate::cli::deploy::realign::detect(&paths, &lockfile);
        if pending.is_empty() {
            println!("  renames:  none");
        } else {
            println!(
                "  renames:  {} pending (run `rdc repair {env} --rename-slugs` to apply):",
                pending.len()
            );
            for p in &pending {
                println!("            {}", p.describe());
            }
        }
    }

    Ok(())
}

/// Walk every writable kind in the snapshot and return a sorted list of
/// `(kind, slug)` entries whose local file's hash != lockfile.content_hash.
/// Read-only.
fn local_edits(paths: &Paths, lockfile: &Lockfile) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();

    // Hooks: combined hash (json + py).
    if paths.hooks_dir().exists() {
        for entry in std::fs::read_dir(paths.hooks_dir())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(slug) = name.strip_suffix(".json") else { continue };
            if slug.ends_with(".remote") {
                continue;
            }
            let hook = match crate::snapshot::hook::read_hook(&paths.hooks_dir(), slug) {
                Ok(h) => h,
                Err(_) => continue,
            };
            let (json_bytes, code) = serialize_hook(&hook)?;
            let local = hook_combined_hash(&json_bytes, &code);
            if differs("hooks", slug, lockfile, &local) {
                out.push(("hooks".into(), slug.into()));
            }
        }
    }

    // Flat-list kinds with plain content_hash. Each goes through typed
    // deserialize+serialize so the canonical bytes match what pull wrote
    // (struct-order typed fields, BTreeMap-sorted extra).
    rules_edits(paths, lockfile, &mut out)?;
    flat_kind_edits::<crate::model::Label>(&paths.labels_dir(), "labels", lockfile, &mut out)?;
    engines_edits(paths, lockfile, &mut out)?;
    engine_fields_edits(paths, lockfile, &mut out)?;
    // Workflows + workflow_steps are pull-only (Rossum API read-only); we
    // intentionally skip them in edit detection.

    // Queue-nested kinds.
    if paths.workspaces_dir().exists() {
        for ws_entry in std::fs::read_dir(paths.workspaces_dir())? {
            let ws_entry = ws_entry?;
            if !ws_entry.file_type()?.is_dir() {
                continue;
            }
            let ws_slug = ws_entry.file_name().to_string_lossy().to_string();
            let queues_dir = paths.queues_dir(&ws_slug);
            if !queues_dir.exists() {
                continue;
            }
            for q_entry in std::fs::read_dir(&queues_dir)? {
                let q_entry = q_entry?;
                if !q_entry.file_type()?.is_dir() {
                    continue;
                }
                let q_slug = q_entry.file_name().to_string_lossy().to_string();
                let queue_dir = paths.queue_dir(&ws_slug, &q_slug);

                // queue.json
                let queue_path = queue_dir.join("queue.json");
                if queue_path.exists() {
                    let bytes = read_pretty_canonical_bytes::<crate::model::Queue>(&queue_path)?;
                    let h = content_hash(&bytes);
                    if differs("queues", &q_slug, lockfile, &h) {
                        out.push(("queues".into(), q_slug.clone()));
                    }
                }

                // schema (combined hash json + formulas/*.py)
                let schema_path = queue_dir.join("schema.json");
                if schema_path.exists() {
                    if let Ok(schema) = crate::snapshot::schema::read_schema(&queue_dir) {
                        let (json, formulas) = serialize_schema(&schema)?;
                        let local = schema_combined_hash(&json, &formulas);
                        if differs("schemas", &q_slug, lockfile, &local) {
                            out.push(("schemas".into(), q_slug.clone()));
                        }
                    }
                }

                // inbox.json
                let inbox_path = queue_dir.join("inbox.json");
                if inbox_path.exists() {
                    let bytes = read_pretty_canonical_bytes::<crate::model::Inbox>(&inbox_path)?;
                    let h = content_hash(&bytes);
                    if differs("inboxes", &q_slug, lockfile, &h) {
                        out.push(("inboxes".into(), q_slug.clone()));
                    }
                }

                // email-templates/*.json
                let templates_dir = paths.queue_email_templates_dir(&ws_slug, &q_slug);
                if templates_dir.exists() {
                    for t_entry in std::fs::read_dir(&templates_dir)? {
                        let t_entry = t_entry?;
                        let name = t_entry.file_name().to_string_lossy().to_string();
                        let Some(t_slug) = name.strip_suffix(".json") else { continue };
                        if t_slug.ends_with(".remote") {
                            continue;
                        }
                        let path = templates_dir.join(format!("{t_slug}.json"));
                        let bytes = read_pretty_canonical_bytes::<crate::model::EmailTemplate>(&path)?;
                        let h = content_hash(&bytes);
                        let key = format!("{ws_slug}/{q_slug}/{t_slug}");
                        if differs("email_templates", &key, lockfile, &h) {
                            out.push(("email_templates".into(), key));
                        }
                    }
                }
            }
        }
    }

    out.sort();
    Ok(out)
}

/// Read a JSON file, deserialize into `T`, then re-serialize via
/// `to_vec_pretty + \n`. This mirrors the canonical-form bytes that pull
/// writes (and therefore matches the lockfile's content_hash).
fn read_pretty_canonical_bytes<T>(path: &Path) -> Result<Vec<u8>>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let value: T = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    let mut bytes = serde_json::to_vec_pretty(&value)
        .with_context(|| format!("serializing {}", path.display()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn differs(kind: &str, slug: &str, lockfile: &Lockfile, local_hash: &str) -> bool {
    let base = lockfile
        .objects
        .get(kind)
        .and_then(|m| m.get(slug))
        .and_then(|e| e.content_hash.as_deref());
    match base {
        Some(b) => b != local_hash,
        // No lockfile entry for an on-disk file → treat as edit so the user
        // sees it (even though `rdc push` will skip create).
        None => true,
    }
}

fn flat_kind_edits<T>(
    dir: &Path,
    kind: &str,
    lockfile: &Lockfile,
    out: &mut Vec<(String, String)>,
) -> Result<()>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue;
        }
        let path = dir.join(format!("{slug}.json"));
        let bytes = read_pretty_canonical_bytes::<T>(&path)?;
        let h = content_hash(&bytes);
        if differs(kind, slug, lockfile, &h) {
            out.push((kind.into(), slug.into()));
        }
    }
    Ok(())
}

/// Walk `rules/<slug>.json` files and surface any whose combined
/// hash (JSON + extracted trigger_condition .py) differs from the
/// lockfile. Mirrors hook edit detection.
fn rules_edits(
    paths: &crate::paths::Paths,
    lockfile: &Lockfile,
    out: &mut Vec<(String, String)>,
) -> Result<()> {
    use crate::state::rule_combined_hash;
    let dir = paths.rules_dir();
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue;
        }
        // Re-read via splice + typed-canonical so the bytes match what
        // pull would write next.
        let rule = crate::snapshot::rule::read_rule(&dir, slug)?;
        let (json_bytes, code) = crate::snapshot::rule::serialize_rule(&rule)?;
        let h = rule_combined_hash(&json_bytes, &code);
        if differs("rules", slug, lockfile, &h) {
            out.push(("rules".into(), slug.into()));
        }
    }
    Ok(())
}

/// Walk `engines/<slug>/engine.json` files and surface any whose
/// content_hash differs from the lockfile.
fn engines_edits(
    paths: &crate::paths::Paths,
    lockfile: &Lockfile,
    out: &mut Vec<(String, String)>,
) -> Result<()> {
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(());
    }
    for e_entry in std::fs::read_dir(&engines_dir)? {
        let e_entry = e_entry?;
        if !e_entry.file_type()?.is_dir() {
            continue;
        }
        let e_slug = e_entry.file_name().to_string_lossy().to_string();
        let e_json_path = e_entry.path().join("engine.json");
        if !e_json_path.exists() {
            continue;
        }
        let bytes = read_pretty_canonical_bytes::<crate::model::Engine>(&e_json_path)?;
        let h = content_hash(&bytes);
        if differs("engines", &e_slug, lockfile, &h) {
            out.push(("engines".into(), e_slug));
        }
    }
    Ok(())
}

/// Walk `engines/<engine>/fields/<field>.json` files. Each field's
/// lockfile key is the field slug alone (not compound).
fn engine_fields_edits(
    paths: &crate::paths::Paths,
    lockfile: &Lockfile,
    out: &mut Vec<(String, String)>,
) -> Result<()> {
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(());
    }
    for e_entry in std::fs::read_dir(&engines_dir)? {
        let e_entry = e_entry?;
        if !e_entry.file_type()?.is_dir() {
            continue;
        }
        let e_slug = e_entry.file_name().to_string_lossy().to_string();
        let fields_dir = paths.engine_fields_dir(&e_slug);
        if !fields_dir.exists() {
            continue;
        }
        for f_entry in std::fs::read_dir(&fields_dir)? {
            let f_entry = f_entry?;
            let name = f_entry.file_name().to_string_lossy().to_string();
            let Some(f_slug) = name.strip_suffix(".json") else { continue };
            if f_slug.ends_with(".remote") {
                continue;
            }
            let path = fields_dir.join(format!("{f_slug}.json"));
            let bytes = read_pretty_canonical_bytes::<crate::model::EngineField>(&path)?;
            let h = content_hash(&bytes);
            if differs("engine_fields", f_slug, lockfile, &h) {
                out.push(("engine_fields".into(), f_slug.into()));
            }
        }
    }
    Ok(())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}
