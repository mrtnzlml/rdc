//! `rdc diff <env>` — local snapshot vs remote (1 GET per edited object).
//! `rdc diff <a> <b>` — two local snapshots, no API calls (per spec §6).
//!
//! Both modes produce unified diffs on stdout. No file writes, no PATCHes.

use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::snapshot::email_template::read_email_template;
use crate::snapshot::hook::{read_hook, serialize_hook};
use crate::snapshot::inbox::read_inbox;
use crate::snapshot::queue::read_queue;
use crate::snapshot::schema::{read_schema, serialize_schema};
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use similar::TextDiff;
use std::path::Path;

pub async fn run(left: String, right: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;

    if !cfg.envs.contains_key(&left) {
        return Err(anyhow!("env '{left}' is not defined in rdc.toml"));
    }

    match right {
        None => diff_local_vs_remote(&cwd, &cfg, &left).await,
        Some(other) => {
            if !cfg.envs.contains_key(&other) {
                return Err(anyhow!("env '{other}' is not defined in rdc.toml"));
            }
            diff_snapshot_vs_snapshot(&cwd, &left, &other)
        }
    }
}

pub async fn diff_local_vs_remote(cwd: &Path, cfg: &ProjectConfig, env: &str) -> Result<()> {
    let env_cfg = cfg.envs.get(env).expect("env present in cfg");
    let paths = Paths::for_env(cwd, env);
    let lockfile_path = paths.lockfile();
    if !lockfile_path.exists() {
        return Err(anyhow!(
            "no lockfile for env '{env}' (run `rdc pull {env}` first)"
        ));
    }
    let lockfile = Lockfile::load(&lockfile_path)?;

    let token = resolve_token(cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing Rossum API client")?;

    let mut diffs_printed = 0usize;

    // Hooks: combined-form (.json + .py)
    if paths.hooks_dir().exists() {
        for entry in std::fs::read_dir(paths.hooks_dir())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(slug) = name.strip_suffix(".json") else { continue };
            if slug.ends_with(".remote") {
                continue;
            }

            let local = match read_hook(&paths.hooks_dir(), slug) {
                Ok(h) => h,
                Err(_) => continue,
            };
            let (local_json, local_code) = serialize_hook(&local)?;

            let id = match lookup_id(&lockfile, "hooks", slug) {
                Some(i) => i,
                None => continue,
            };
            let remote = client.get_hook(id, None).await
                .with_context(|| format!("fetching hook {id} for diff"))?;
            let (remote_json, remote_code) = serialize_hook(&remote)?;

            let lj = String::from_utf8_lossy(&local_json);
            let rj = String::from_utf8_lossy(&remote_json);
            print_unified(&format!("hooks/{slug}.json (local)"),
                          &format!("hooks/{slug}.json (remote)"),
                          &lj, &rj, &mut diffs_printed);

            // Code (.py) — present only if the hook has code.
            let lc = local_code.unwrap_or_default();
            let rc = remote_code.unwrap_or_default();
            if lc != rc {
                print_unified(&format!("hooks/{slug}.py (local)"),
                              &format!("hooks/{slug}.py (remote)"),
                              &lc, &rc, &mut diffs_printed);
            }
        }
    }

    // Rules: combined-form (.json + optional .py for trigger_condition).
    diff_rules(&paths, &lockfile, &client, &mut diffs_printed).await?;
    // Flat kinds with simple typed JSON.
    diff_flat_remote::<crate::model::Label>(
        &paths.labels_dir(), "labels", &lockfile, &client, &mut diffs_printed,
        |c, id| Box::pin(async move {
            let list = c.list_labels(None).await?;
            list.into_iter().find(|l| l.id == id)
                .ok_or_else(|| anyhow!("label id {id} not found on remote"))
        }),
    ).await?;
    diff_engines(&paths, &lockfile, &client, &mut diffs_printed).await?;
    diff_engine_fields(&paths, &lockfile, &client, &mut diffs_printed).await?;

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
                if queue_dir.join("queue.json").exists() {
                    let local = read_queue(&queue_dir)?;
                    let id = match lookup_id(&lockfile, "queues", &q_slug) {
                        Some(i) => i, None => continue,
                    };
                    let remotes = client.list_queues(None).await?;
                    if let Some(remote) = remotes.into_iter().find(|q| q.id == id) {
                        let lj = canonical_json_for_diff(&local)?;
                        let rj = canonical_json_for_diff(&remote)?;
                        print_unified(&format!("queues/{q_slug}/queue.json (local)"),
                                      &format!("queues/{q_slug}/queue.json (remote)"),
                                      &lj, &rj, &mut diffs_printed);
                    }
                }

                // schema.json + formulas/
                if queue_dir.join("schema.json").exists() {
                    let local = read_schema(&queue_dir)?;
                    let id = match lookup_id(&lockfile, "schemas", &q_slug) {
                        Some(i) => i, None => continue,
                    };
                    let remote = client.get_schema(id, None).await?;
                    let (lj, l_formulas) = serialize_schema(&local)?;
                    let (rj, r_formulas) = serialize_schema(&remote)?;
                    let ljs = String::from_utf8_lossy(&lj);
                    let rjs = String::from_utf8_lossy(&rj);
                    print_unified(&format!("schemas/{q_slug}/schema.json (local)"),
                                  &format!("schemas/{q_slug}/schema.json (remote)"),
                                  &ljs, &rjs, &mut diffs_printed);
                    diff_formulas(&q_slug, &l_formulas, &r_formulas, &mut diffs_printed);
                }

                // inbox.json
                if queue_dir.join("inbox.json").exists() {
                    let local = read_inbox(&queue_dir)?;
                    let id = match lookup_id(&lockfile, "inboxes", &q_slug) {
                        Some(i) => i, None => continue,
                    };
                    let remote = client.get_inbox(id, None).await?;
                    let lj = canonical_json_for_diff(&local)?;
                    let rj = canonical_json_for_diff(&remote)?;
                    print_unified(&format!("inboxes/{q_slug}/inbox.json (local)"),
                                  &format!("inboxes/{q_slug}/inbox.json (remote)"),
                                  &lj, &rj, &mut diffs_printed);
                }

                // email-templates/
                let templates_dir = paths.queue_email_templates_dir(&ws_slug, &q_slug);
                if templates_dir.exists() {
                    let mut remote_cache: Option<Vec<crate::model::EmailTemplate>> = None;
                    for t_entry in std::fs::read_dir(&templates_dir)? {
                        let t_entry = t_entry?;
                        let name = t_entry.file_name().to_string_lossy().to_string();
                        let Some(t_slug) = name.strip_suffix(".json") else { continue };
                        if t_slug.ends_with(".remote") {
                            continue;
                        }
                        let local = read_email_template(&templates_dir, t_slug)?;
                        let key = format!("{ws_slug}/{q_slug}/{t_slug}");
                        let id = match lookup_id(&lockfile, "email_templates", &key) {
                            Some(i) => i, None => continue,
                        };
                        if remote_cache.is_none() {
                            remote_cache = Some(client.list_email_templates(None).await?);
                        }
                        if let Some(remote) = remote_cache.as_ref().unwrap().iter().find(|t| t.id == id) {
                            let lj = canonical_json_for_diff(&local)?;
                            let rj = canonical_json_for_diff(remote)?;
                            print_unified(&format!("email_templates/{key}.json (local)"),
                                          &format!("email_templates/{key}.json (remote)"),
                                          &lj, &rj, &mut diffs_printed);
                        }
                    }
                }
            }
        }
    }

    if diffs_printed == 0 {
        println!("no diffs (local snapshot matches remote for env '{env}')");
    }

    Ok(())
}

fn diff_snapshot_vs_snapshot(cwd: &Path, src: &str, tgt: &str) -> Result<()> {
    let src_paths = Paths::for_env(cwd, src);
    let tgt_paths = Paths::for_env(cwd, tgt);

    let mut diffs_printed = 0usize;

    // Diff every JSON file pairwise. Walk both trees and union the file
    // set; for each file present in both, diff bytes; for added/removed,
    // print a single-sided header.
    let src_files = collect_snapshot_files(&src_paths)?;
    let tgt_files = collect_snapshot_files(&tgt_paths)?;

    let mut all_keys: std::collections::BTreeSet<String> = src_files.keys().cloned().collect();
    all_keys.extend(tgt_files.keys().cloned());

    for rel in all_keys {
        match (src_files.get(&rel), tgt_files.get(&rel)) {
            (Some(left), Some(right)) => {
                let l = String::from_utf8_lossy(left);
                let r = String::from_utf8_lossy(right);
                if l != r {
                    print_unified(
                        &format!("{rel} (in {src})"),
                        &format!("{rel} (in {tgt})"),
                        &l, &r, &mut diffs_printed,
                    );
                }
            }
            (Some(_), None) => {
                println!("--- {rel} (in {src})");
                println!("+++ {rel} (missing in {tgt})");
                diffs_printed += 1;
            }
            (None, Some(_)) => {
                println!("--- {rel} (missing in {src})");
                println!("+++ {rel} (in {tgt})");
                diffs_printed += 1;
            }
            (None, None) => unreachable!(),
        }
    }

    if diffs_printed == 0 {
        println!("no diffs (snapshots '{src}' and '{tgt}' are identical)");
    }
    Ok(())
}

/// Walk `envs/<env>/` and return a map of (relative-path → bytes) for every
/// JSON, .py, and .toml file (skipping `.remote` siblings and `_index.md`).
fn collect_snapshot_files(paths: &Paths) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let mut out: std::collections::BTreeMap<String, Vec<u8>> = std::collections::BTreeMap::new();
    let env_root = paths.env_root();
    if !env_root.exists() {
        return Ok(out);
    }
    walk_dir(&env_root, &env_root, &mut out)?;
    Ok(out)
}

fn walk_dir(
    base: &Path,
    dir: &Path,
    out: &mut std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir(base, &path, out)?;
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "_index.md" {
            continue;
        }
        if name.ends_with(".remote") || name.ends_with(".remote.json") {
            continue;
        }
        // Only diff text-y files.
        let is_text = name.ends_with(".json")
            || name.ends_with(".py")
            || name.ends_with(".toml")
            || name.ends_with(".md");
        if !is_text {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let rel = path.strip_prefix(base)?.to_string_lossy().to_string();
        out.insert(rel, bytes);
    }
    Ok(())
}

fn lookup_id(lockfile: &Lockfile, kind: &str, slug: &str) -> Option<u64> {
    lockfile.objects.get(kind).and_then(|m| m.get(slug)).map(|e| e.id)
}

fn canonical_json_for_diff<T: serde::Serialize>(v: &T) -> Result<String> {
    let mut bytes = serde_json::to_vec_pretty(v)?;
    bytes.push(b'\n');
    Ok(String::from_utf8(bytes)?)
}

/// Print a unified diff (3-line context) of two text blobs with custom
/// header labels. If the inputs are byte-equal, prints nothing and leaves
/// `counter` untouched — callers can pass a no-op counter (`&mut 0`) when
/// they only care about the side effect.
///
/// Used by `rdc diff` directly and by `rdc push --dry-run --diff` /
/// `rdc deploy --dry-run --diff` to surface per-object changes.
pub fn print_unified(
    left_label: &str,
    right_label: &str,
    left: &str,
    right: &str,
    counter: &mut usize,
) {
    if left == right {
        return;
    }
    let diff = TextDiff::from_lines(left, right);
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header(left_label, right_label)
        .to_string();
    print!("{unified}");
    *counter += 1;
}

/// Print a "new file" unified diff — the right side is empty so every line
/// of `body` shows up as a `+` insertion. Used by the dry-run preview when
/// reporting a would-be POST (no remote counterpart yet).
pub fn print_new_file_diff(label: &str, body: &str) {
    print_unified("/dev/null", label, "", body, &mut 0);
}

/// Mirror of `print_new_file_diff` for would-be deletions: every line of
/// `body` shows up as a `-` removal. Used by `rdc deploy --mirror
/// --dry-run --diff`.
pub fn print_deleted_file_diff(label: &str, body: &str) {
    print_unified(label, "/dev/null", body, "", &mut 0);
}

fn diff_formulas(
    q_slug: &str,
    local: &[(String, Vec<u8>)],
    remote: &[(String, Vec<u8>)],
    counter: &mut usize,
) {
    let local_map: std::collections::BTreeMap<String, &[u8]> =
        local.iter().map(|(k, v)| (k.clone(), v.as_slice())).collect();
    let remote_map: std::collections::BTreeMap<String, &[u8]> =
        remote.iter().map(|(k, v)| (k.clone(), v.as_slice())).collect();
    let mut all_keys: std::collections::BTreeSet<String> = local_map.keys().cloned().collect();
    all_keys.extend(remote_map.keys().cloned());
    for field_id in all_keys {
        let l = local_map.get(&field_id).copied().unwrap_or(&[]);
        let r = remote_map.get(&field_id).copied().unwrap_or(&[]);
        if l != r {
            let ls = String::from_utf8_lossy(l);
            let rs = String::from_utf8_lossy(r);
            print_unified(
                &format!("schemas/{q_slug}/formulas/{field_id}.py (local)"),
                &format!("schemas/{q_slug}/formulas/{field_id}.py (remote)"),
                &ls, &rs, counter,
            );
        }
    }
}

/// Walk `rules/<slug>.json` files and diff each against the fetched
/// remote rule. When the local or remote has a `trigger_condition`,
/// emit a second diff for the `.py` side too. Mirrors the hooks block.
async fn diff_rules(
    paths: &Paths,
    lockfile: &Lockfile,
    client: &RossumClient,
    counter: &mut usize,
) -> Result<()> {
    use crate::snapshot::rule::{read_rule, serialize_rule};
    let dir = paths.rules_dir();
    if !dir.exists() {
        return Ok(());
    }
    let mut remote_cache: Option<Vec<crate::model::Rule>> = None;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue;
        }
        let local = match read_rule(&dir, slug) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let (local_json, local_code) = serialize_rule(&local)?;

        let id = match lookup_id(lockfile, "rules", slug) {
            Some(i) => i,
            None => continue,
        };
        if remote_cache.is_none() {
            remote_cache = Some(client.list_rules(None).await?);
        }
        let Some(remote) = remote_cache.as_ref().unwrap().iter().find(|r| r.id == id).cloned() else {
            continue;
        };
        let (remote_json, remote_code) = serialize_rule(&remote)?;

        let lj = String::from_utf8_lossy(&local_json);
        let rj = String::from_utf8_lossy(&remote_json);
        print_unified(
            &format!("rules/{slug}.json (local)"),
            &format!("rules/{slug}.json (remote)"),
            &lj, &rj, counter,
        );

        let lc = local_code.unwrap_or_default();
        let rc = remote_code.unwrap_or_default();
        if lc != rc {
            print_unified(
                &format!("rules/{slug}.py (local)"),
                &format!("rules/{slug}.py (remote)"),
                &lc, &rc, counter,
            );
        }
    }
    Ok(())
}

/// Walk `engines/<slug>/engine.json` files and diff each against the
/// fetched remote engine.
async fn diff_engines(
    paths: &Paths,
    lockfile: &Lockfile,
    client: &RossumClient,
    counter: &mut usize,
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
        let path = e_entry.path().join("engine.json");
        if !path.exists() {
            continue;
        }
        let local: crate::model::Engine = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let local_canon = canonical_json_for_diff(&local)?;
        let Some(id) = lookup_id(lockfile, "engines", &e_slug) else { continue };
        let list = client.list_engines(None).await?;
        let remote = match list.into_iter().find(|e| e.id == id) {
            Some(r) => r,
            None => continue,
        };
        let remote_canon = canonical_json_for_diff(&remote)?;
        print_unified(
            &format!("engines/{e_slug}/engine.json (local)"),
            &format!("engines/{e_slug}/engine.json (remote)"),
            &local_canon, &remote_canon, counter,
        );
    }
    Ok(())
}

/// Walk `engines/<engine>/fields/<slug>.json` files and diff each
/// against the fetched remote engine field.
async fn diff_engine_fields(
    paths: &Paths,
    lockfile: &Lockfile,
    client: &RossumClient,
    counter: &mut usize,
) -> Result<()> {
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(());
    }
    let mut remote_cache: Option<Vec<crate::model::EngineField>> = None;
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
            let local: crate::model::EngineField = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
            let local_canon = canonical_json_for_diff(&local)?;
            let Some(id) = lookup_id(lockfile, "engine_fields", f_slug) else { continue };
            if remote_cache.is_none() {
                remote_cache = Some(client.list_engine_fields(None).await?);
            }
            let Some(remote) = remote_cache.as_ref().unwrap().iter().find(|f| f.id == id) else { continue };
            let remote_canon = canonical_json_for_diff(remote)?;
            print_unified(
                &format!("engines/{e_slug}/fields/{f_slug}.json (local)"),
                &format!("engines/{e_slug}/fields/{f_slug}.json (remote)"),
                &local_canon, &remote_canon, counter,
            );
        }
    }
    Ok(())
}

/// Generic helper for flat-list kinds (rules / labels). Reads each
/// `<kind>/<slug>.json`, fetches the matching remote via the supplied
/// future, and prints unified diff if they differ.
async fn diff_flat_remote<T>(
    dir: &Path,
    kind: &str,
    lockfile: &Lockfile,
    client: &RossumClient,
    counter: &mut usize,
    fetch: impl Fn(&RossumClient, u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + '_>>,
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
        let raw = std::fs::read_to_string(&path)?;
        let local: T = serde_json::from_str(&raw)?;
        let local_canon = canonical_json_for_diff(&local)?;

        let id = match lookup_id(lockfile, kind, slug) {
            Some(i) => i, None => continue,
        };
        let remote: T = fetch(client, id).await?;
        let remote_canon = canonical_json_for_diff(&remote)?;

        print_unified(
            &format!("{kind}/{slug}.json (local)"),
            &format!("{kind}/{slug}.json (remote)"),
            &local_canon, &remote_canon, counter,
        );
    }
    Ok(())
}
