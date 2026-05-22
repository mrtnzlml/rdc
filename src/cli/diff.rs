//! `rdc diff <env>` — local snapshot vs remote (1 GET per edited object).
//! `rdc diff <a> <b>` — two local snapshots, no API calls (per spec §6).
//!
//! Both modes produce unified diffs on stdout. No file writes, no PATCHes.

use crate::api::RossumClient;
use crate::cli::resolve::line_diff;
use crate::config::ProjectConfig;
use crate::log::{Action, Log};
use crate::mapping::Mapping;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::snapshot::email_template::read_email_template;
use crate::snapshot::hook::{read_hook, serialize_hook};
use crate::snapshot::inbox::read_inbox;
use crate::snapshot::queue::read_queue;
use crate::snapshot::schema::{read_schema, serialize_schema};
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::sync::Arc;

/// Context for src-side canonicalisation in `rdc diff <src> <tgt>`. When
/// present, cross-reference URL strings on the src side are rewritten into
/// their tgt equivalents (per the deploy-managed mapping) so the line-diff
/// reports semantic drift only, not deploy-managed ID renumbering.
///
/// All three fields must be present for rewriting to be useful — the
/// mapping pairs slugs, the src lockfile resolves URLs → slugs, the tgt
/// lockfile resolves slugs → URLs. Missing any one falls back to noise
/// stripping alone.
struct RewriteCtx<'a> {
    mapping: &'a Mapping,
    src_lockfile: &'a Lockfile,
    tgt_lockfile: &'a Lockfile,
}

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
            "no lockfile for env '{env}' (run `rdc sync {env}` first)"
        ));
    }
    let lockfile = Lockfile::load(&lockfile_path)?;

    let token = resolve_token(cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing Rossum API client")?;

    // Wire a Log so every remote-API call emits a timestamped event line.
    let progress = Log::new(crate::cli::resolve::detect_color_mode(false));

    let mut diffs_printed = 0usize;

    // Hooks: combined-form (.json + .py). Parallel GETs with one batch
    // spinner; output is sorted by slug so the log is deterministic.
    if paths.hooks_dir().exists() {
        diff_hooks(&paths, &lockfile, &client, &mut diffs_printed, &progress).await?;
    }

    // Rules: combined-form (.json + optional .py for trigger_condition).
    diff_rules(&paths, &lockfile, &client, &mut diffs_printed, &progress).await?;
    // Flat kinds with simple typed JSON. The remote list is fetched ONCE
    // and reused across all per-slug diffs.
    diff_labels(&paths, &lockfile, &client, &mut diffs_printed, &progress).await?;
    diff_engines(&paths, &lockfile, &client, &mut diffs_printed, &progress).await?;
    diff_engine_fields(&paths, &lockfile, &client, &mut diffs_printed, &progress).await?;

    // Queue-nested kinds. Three list calls are hoisted (`queues`,
    // `email_templates`, and the queue tree-walk uses them by id), and
    // per-queue schema + inbox GETs run in a single bounded-concurrency
    // batch.
    if paths.workspaces_dir().exists() {
        diff_queue_tree(&paths, &lockfile, &client, &mut diffs_printed, &progress).await?;
    }

    if diffs_printed == 0 {
        progress.event(Action::Diff, &format!("done envs/{env} clean"));
        println!("no diffs (local snapshot matches remote for env '{env}')");
    } else {
        progress.event(Action::Diff, &format!("done envs/{env} ({diffs_printed} differing)"));
    }

    Ok(())
}

/// Bounded concurrency for parallel per-object GETs. Matches
/// `pull::common::prefetch_queue_children`'s cap — Rossum rate-limits
/// (429s) above this and 8 saturates upstream for read-only fan-outs.
const DIFF_FANOUT: usize = 8;

/// Parallel per-hook GET, then sort by slug and print diffs sequentially.
/// Hooks with missing local files, missing lockfile entries, or unreadable
/// JSON are skipped before the fan-out so they never burn a fetch slot.
async fn diff_hooks(
    paths: &Paths,
    lockfile: &Lockfile,
    client: &RossumClient,
    counter: &mut usize,
    progress: &Arc<Log>,
) -> Result<()> {
    use futures::stream::{StreamExt, TryStreamExt};

    // Collect work items deterministically (one local file → one fetch).
    let mut items: Vec<(String, crate::model::Hook, u64)> = Vec::new();
    for entry in std::fs::read_dir(paths.hooks_dir())? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if crate::paths::is_shadow_artifact(&name, paths.env()) {
            continue;
        }
        let Some(slug) = name.strip_suffix(".json") else { continue };
        let local = match read_hook(&paths.hooks_dir(), slug) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let id = match lookup_id(lockfile, "hooks", slug) {
            Some(i) => i,
            None => continue,
        };
        items.push((slug.to_string(), local, id));
    }
    if items.is_empty() {
        return Ok(());
    }
    let total = items.len();

    // Parallel fetch phase.
    progress.event(Action::Diff, "hooks start");
    let fetched_result: Result<Vec<(String, crate::model::Hook, crate::model::Hook)>> =
        futures::stream::iter(items)
            .map(|(slug, local, id)| {
                let progress = progress.clone();
                async move {
                    let remote = client
                        .get_hook(id, Some(progress.clone()))
                        .await
                        .with_context(|| format!("fetching hook {slug} for diff"))?;
                    Ok::<_, anyhow::Error>((slug, local, remote))
                }
            })
            .buffer_unordered(DIFF_FANOUT)
            .try_collect()
            .await;

    let mut fetched = fetched_result?;
    progress.event(Action::Diff, &format!("hooks ({total} fetched)"));

    // Sort by slug, print diffs sequentially.
    fetched.sort_by(|a, b| a.0.cmp(&b.0));
    for (slug, local, remote) in fetched {
        let (local_json, local_code) = serialize_hook(&local)?;
        let (remote_json, remote_code) = serialize_hook(&remote)?;

        let lj = String::from_utf8_lossy(&local_json);
        let rj = String::from_utf8_lossy(&remote_json);
        let before = *counter;
        print_unified(
            &format!("hooks/{slug}.json (local)"),
            &format!("hooks/{slug}.json (remote)"),
            &lj, &rj, counter,
        );

        // Code (.py or .js depending on runtime) — present only if the
        // hook has code. Use the *local* runtime for the filename
        // label; a runtime change between local and remote surfaces in
        // the JSON diff above.
        let lc = local_code.unwrap_or_default();
        let rc = remote_code.unwrap_or_default();
        if lc != rc {
            let ext = crate::snapshot::hook::hook_code_extension(&local);
            print_unified(
                &format!("hooks/{slug}.{ext} (local)"),
                &format!("hooks/{slug}.{ext} (remote)"),
                &lc, &rc, counter,
            );
        }
        if *counter > before {
            progress.event(Action::Diff, &format!("hook/{slug} differs"));
        } else {
            progress.event(Action::Diff, &format!("hook/{slug} clean"));
        }
    }
    Ok(())
}

/// Diff `labels/<slug>.json` against the remote label list. The list is
/// fetched ONCE and looked up by id for each local slug — previously the
/// caller invoked `list_labels` per local file (O(N²)).
async fn diff_labels(
    paths: &Paths,
    lockfile: &Lockfile,
    client: &RossumClient,
    counter: &mut usize,
    progress: &Arc<Log>,
) -> Result<()> {
    let dir = paths.labels_dir();
    if !dir.exists() {
        return Ok(());
    }
    let remotes = client.list_labels(Some(progress.clone())).await?;
    progress.event(Action::Diff, &format!("labels ({} remote)", remotes.len()));
    let by_id: std::collections::BTreeMap<u64, &crate::model::Label> =
        remotes.iter().map(|l| (l.id, l)).collect();
    diff_flat_remote::<crate::model::Label>(
        &dir, "label", paths.env(), lockfile, counter, progress, &by_id,
    )
}

fn diff_snapshot_vs_snapshot(cwd: &Path, src: &str, tgt: &str) -> Result<()> {
    let src_paths = Paths::for_env(cwd, src);
    let tgt_paths = Paths::for_env(cwd, tgt);

    let mut diffs_printed = 0usize;

    // Try to load the deploy-managed mapping and both lockfiles so URL
    // cross-references on the src side can be rewritten into their tgt
    // equivalents before line-diffing. Any missing piece degrades to
    // noise-stripping only — the diff still works, it just shows
    // deploy-managed URLs as differences.
    let mapping_path = src_paths.mapping_file(src, tgt);
    let mapping = if mapping_path.exists() {
        Mapping::load(&mapping_path).ok()
    } else {
        None
    };
    let src_lockfile = Lockfile::load(&src_paths.lockfile()).ok();
    let tgt_lockfile = Lockfile::load(&tgt_paths.lockfile()).ok();
    let ctx = match (mapping.as_ref(), src_lockfile.as_ref(), tgt_lockfile.as_ref()) {
        (Some(m), Some(s), Some(t)) => Some(RewriteCtx { mapping: m, src_lockfile: s, tgt_lockfile: t }),
        _ => None,
    };

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
                // Strip cross-env noise (`id`, `url`, `organization`,
                // server-managed timestamps, server-computed back-refs)
                // before diffing — those fields always differ between
                // envs and bury the real changes. When the deploy-managed
                // mapping is available, also rewrite src-side
                // cross-reference URLs (queue.workspace, hook.queues, …)
                // into their tgt form so paired objects compare equal.
                let left_norm = normalize_for_diff(&rel, left, ctx.as_ref());
                let right_norm = normalize_for_diff(&rel, right, None);
                if left_norm != right_norm {
                    let ls = String::from_utf8_lossy(&left_norm);
                    let rs = String::from_utf8_lossy(&right_norm);
                    print_unified(
                        &format!("{rel} (in {src})"),
                        &format!("{rel} (in {tgt})"),
                        &ls, &rs, &mut diffs_printed,
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
        println!("no diffs (snapshots '{src}' and '{tgt}' are identical after stripping cross-env noise)");
    }
    Ok(())
}

/// Normalise bytes for a cross-env diff display.
/// - JSON files of a recognised kind go through `normalize_for_cross_env_compare`,
///   which strips `id`, `url`, `organization`, server-managed timestamps,
///   and kind-specific computed back-references.
/// - JSON files of an unrecognised kind get pretty-printed so per-field
///   changes diff on their own lines.
/// - Non-JSON files (`.py`, `.toml`, `.md`) pass through unchanged.
///
/// When `rewrite_ctx` is `Some`, cross-reference URL strings in the JSON
/// are also rewritten src → tgt using the deploy-managed mapping. Pass
/// `Some(...)` on the src side and `None` on the tgt side so paired
/// objects compare byte-equal after canonicalisation.
fn normalize_for_diff(rel: &str, bytes: &[u8], rewrite_ctx: Option<&RewriteCtx<'_>>) -> Vec<u8> {
    if !rel.ends_with(".json") {
        return bytes.to_vec();
    }
    if let Some(kind) = kind_from_relative_path(rel) {
        if let Ok(normalized) =
            crate::cli::deploy::common::normalize_for_cross_env_compare(bytes, kind)
        {
            // If we have a mapping context, rewrite cross-reference URLs
            // on this side (only called on the src side for two-snapshot
            // diff) so paired objects compare byte-equal.
            if let Some(ctx) = rewrite_ctx {
                return rewrite_urls_in_bytes(&normalized, ctx);
            }
            return normalized;
        }
    }
    // Fall back to pretty-print so the diff is still per-field readable.
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        if let Ok(mut pretty) = serde_json::to_vec_pretty(&value) {
            if !pretty.ends_with(b"\n") {
                pretty.push(b'\n');
            }
            return pretty;
        }
    }
    bytes.to_vec()
}

/// Parse `bytes` as JSON, rewrite every URL string that matches a known
/// src object into its tgt equivalent (via the deploy-managed mapping),
/// and re-serialise. Returns `bytes` unchanged if parsing or re-serialising
/// fails — the caller still gets a readable, noise-stripped diff in that
/// case, just without URL rewriting.
fn rewrite_urls_in_bytes(bytes: &[u8], ctx: &RewriteCtx<'_>) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    crate::cli::deploy::common::rewrite_urls(
        &mut value,
        ctx.src_lockfile,
        ctx.tgt_lockfile,
        ctx.mapping,
        &std::collections::BTreeMap::new(),
    );
    let Ok(mut out) = serde_json::to_vec_pretty(&value) else {
        return bytes.to_vec();
    };
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out
}

/// Map a snapshot-relative path to its Rossum kind. Returns `None` for
/// paths that don't correspond to a known kind (e.g. `overlay.toml`,
/// `.py` formula files).
fn kind_from_relative_path(rel: &str) -> Option<&'static str> {
    if rel.starts_with("hooks/") && rel.ends_with(".json") {
        return Some("hooks");
    }
    if rel.starts_with("rules/") && rel.ends_with(".json") {
        return Some("rules");
    }
    if rel.starts_with("labels/") && rel.ends_with(".json") {
        return Some("labels");
    }
    if rel.starts_with("workspaces/") {
        if rel.ends_with("/workspace.json") {
            return Some("workspaces");
        }
        if rel.ends_with("/queue.json") {
            return Some("queues");
        }
        if rel.ends_with("/schema.json") {
            return Some("schemas");
        }
        if rel.ends_with("/inbox.json") {
            return Some("inboxes");
        }
        if rel.contains("/email-templates/") && rel.ends_with(".json") {
            return Some("email_templates");
        }
    }
    if rel.starts_with("engines/") {
        if rel.ends_with("/engine.json") {
            return Some("engines");
        }
        if rel.contains("/fields/") && rel.ends_with(".json") {
            return Some("engine_fields");
        }
    }
    None
}

/// Walk `envs/<env>/` and return a map of (relative-path → bytes) for every
/// JSON, .py, and .toml file (skipping shadow artifacts and `_index.md`).
fn collect_snapshot_files(paths: &Paths) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let mut out: std::collections::BTreeMap<String, Vec<u8>> = std::collections::BTreeMap::new();
    let env_root = paths.env_root();
    if !env_root.exists() {
        return Ok(out);
    }
    walk_dir(&env_root, &env_root, paths.env(), &mut out)?;
    Ok(out)
}

fn walk_dir(
    base: &Path,
    dir: &Path,
    env: &str,
    out: &mut std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir(base, &path, env, out)?;
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "_index.md" {
            continue;
        }
        if crate::paths::is_shadow_artifact(&name, env) {
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
/// Output is colorized for TTY stdout (red `-`, green `+`, cyan `@@`),
/// plain otherwise (respects `NO_COLOR` and the global `--no-color` flag).
///
/// Used by `rdc diff` directly and by `rdc push --dry-run --diff` /
/// `rdc deploy --dry-run` to surface per-object changes.
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
    let diff = line_diff(left, right);
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header(left_label, right_label)
        .to_string();
    let mode = crate::cli::resolve::detect_color_mode(false);
    for line in unified.lines() {
        println!("{}", crate::cli::resolve::colorize_diff_line(line, mode));
    }
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

/// Walk `rules/<slug>.json` files and diff each against the remote
/// rule list. When the local or remote has a `trigger_condition`, emit
/// a second diff for the `.py` side too. Mirrors the hooks block.
///
/// The remote rule list is fetched ONCE up front and looked up by id
/// per local slug — previously this lived inside the per-slug loop as
/// a lazy cache, which is fine for correctness but the new pattern
/// makes the single-list invariant explicit.
async fn diff_rules(
    paths: &Paths,
    lockfile: &Lockfile,
    client: &RossumClient,
    counter: &mut usize,
    progress: &Arc<Log>,
) -> Result<()> {
    use crate::snapshot::rule::{read_rule, serialize_rule};
    let dir = paths.rules_dir();
    if !dir.exists() {
        return Ok(());
    }

    let remotes = client.list_rules(Some(progress.clone())).await?;
    progress.event(Action::Diff, &format!("rules ({} remote)", remotes.len()));
    let by_id: std::collections::BTreeMap<u64, &crate::model::Rule> =
        remotes.iter().map(|r| (r.id, r)).collect();

    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if crate::paths::is_shadow_artifact(&name, paths.env()) {
            continue;
        }
        let Some(slug) = name.strip_suffix(".json") else { continue };
        let local = match read_rule(&dir, slug) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let (local_json, local_code) = serialize_rule(&local)?;

        let id = match lookup_id(lockfile, "rules", slug) {
            Some(i) => i,
            None => continue,
        };
        let Some(remote) = by_id.get(&id) else {
            progress.event(Action::Warn, &format!("rule/{slug} not on remote"));
            continue;
        };
        let (remote_json, remote_code) = serialize_rule(remote)?;

        let lj = String::from_utf8_lossy(&local_json);
        let rj = String::from_utf8_lossy(&remote_json);
        let before = *counter;
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
        if *counter > before {
            progress.event(Action::Diff, &format!("rule/{slug} differs"));
        } else {
            progress.event(Action::Diff, &format!("rule/{slug} clean"));
        }
    }
    Ok(())
}

/// Walk `engines/<slug>/engine.json` files and diff each against the
/// remote engine list. The list is fetched ONCE up front (previously
/// inside the loop — one redundant LIST per engine).
async fn diff_engines(
    paths: &Paths,
    lockfile: &Lockfile,
    client: &RossumClient,
    counter: &mut usize,
    progress: &Arc<Log>,
) -> Result<()> {
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(());
    }

    let remotes = client.list_engines(Some(progress.clone())).await?;
    progress.event(Action::Diff, &format!("engines ({} remote)", remotes.len()));
    let by_id: std::collections::BTreeMap<u64, &crate::model::Engine> =
        remotes.iter().map(|e| (e.id, e)).collect();

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
        let Some(remote) = by_id.get(&id) else {
            progress.event(Action::Warn, &format!("engine/{e_slug} not on remote"));
            continue;
        };
        let remote_canon = canonical_json_for_diff(remote)?;
        let before = *counter;
        print_unified(
            &format!("engines/{e_slug}/engine.json (local)"),
            &format!("engines/{e_slug}/engine.json (remote)"),
            &local_canon, &remote_canon, counter,
        );
        if *counter > before {
            progress.event(Action::Diff, &format!("engine/{e_slug} differs"));
        } else {
            progress.event(Action::Diff, &format!("engine/{e_slug} clean"));
        }
    }
    Ok(())
}

/// Walk `engines/<engine>/fields/<slug>.json` files and diff each
/// against the remote engine-fields list. The list is fetched ONCE up
/// front and looked up by id per local file.
async fn diff_engine_fields(
    paths: &Paths,
    lockfile: &Lockfile,
    client: &RossumClient,
    counter: &mut usize,
    progress: &Arc<Log>,
) -> Result<()> {
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(());
    }

    let remotes = client.list_engine_fields(Some(progress.clone())).await?;
    progress.event(Action::Diff, &format!("engine_fields ({} remote)", remotes.len()));
    let by_id: std::collections::BTreeMap<u64, &crate::model::EngineField> =
        remotes.iter().map(|f| (f.id, f)).collect();

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
            if crate::paths::is_shadow_artifact(&name, paths.env()) {
                continue;
            }
            let Some(f_slug) = name.strip_suffix(".json") else { continue };
            let path = fields_dir.join(format!("{f_slug}.json"));
            let local: crate::model::EngineField = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
            let local_canon = canonical_json_for_diff(&local)?;
            let Some(id) = lookup_id(lockfile, "engine_fields", f_slug) else { continue };
            let Some(remote) = by_id.get(&id) else {
                progress.event(Action::Warn, &format!("engine_field/{e_slug}/{f_slug} not on remote"));
                continue;
            };
            let remote_canon = canonical_json_for_diff(remote)?;
            let before = *counter;
            print_unified(
                &format!("engines/{e_slug}/fields/{f_slug}.json (local)"),
                &format!("engines/{e_slug}/fields/{f_slug}.json (remote)"),
                &local_canon, &remote_canon, counter,
            );
            if *counter > before {
                progress.event(Action::Diff, &format!("engine_field/{e_slug}/{f_slug} differs"));
            } else {
                progress.event(Action::Diff, &format!("engine_field/{e_slug}/{f_slug} clean"));
            }
        }
    }
    Ok(())
}

/// Generic helper for flat-list kinds. Reads each `<kind_plural>/<slug>.json`,
/// looks up the matching remote in the supplied `by_id` map (built from
/// a single up-front LIST call), and prints a unified diff when the
/// canonicalized bytes differ.
///
/// The list is fetched ONCE by the caller; this helper performs no API
/// calls of its own. `kind_singular` is used for per-resource event lines
/// (e.g. `"label"`); `kind_plural` is used for directory/path resolution
/// (e.g. `"labels"`).
fn diff_flat_remote<T>(
    dir: &Path,
    kind_singular: &str,
    env: &str,
    lockfile: &Lockfile,
    counter: &mut usize,
    progress: &Arc<Log>,
    by_id: &std::collections::BTreeMap<u64, &T>,
) -> Result<()>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    if !dir.exists() {
        return Ok(());
    }
    // The lockfile and path use the plural form (e.g. "labels"); derive it
    // from the dir name for lookups and display paths.
    let kind_plural = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("{kind_singular}s"));
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if crate::paths::is_shadow_artifact(&name, env) {
            continue;
        }
        let Some(slug) = name.strip_suffix(".json") else { continue };
        let path = dir.join(format!("{slug}.json"));
        let raw = std::fs::read_to_string(&path)?;
        let local: T = serde_json::from_str(&raw)?;
        let local_canon = canonical_json_for_diff(&local)?;

        let id = match lookup_id(lockfile, &kind_plural, slug) {
            Some(i) => i, None => continue,
        };
        let Some(remote) = by_id.get(&id) else {
            progress.event(Action::Warn, &format!("{kind_singular}/{slug} not on remote"));
            continue;
        };
        let remote_canon = canonical_json_for_diff(remote)?;

        let before = *counter;
        print_unified(
            &format!("{kind_plural}/{slug}.json (local)"),
            &format!("{kind_plural}/{slug}.json (remote)"),
            &local_canon, &remote_canon, counter,
        );
        if *counter > before {
            progress.event(Action::Diff, &format!("{kind_singular}/{slug} differs"));
        } else {
            progress.event(Action::Diff, &format!("{kind_singular}/{slug} clean"));
        }
    }
    Ok(())
}

/// Diff the queue tree (`envs/<env>/workspaces/<ws>/queues/<q>/{queue,schema,inbox}.json`
/// and `email-templates/`) against the remote.
///
/// Hot path: previously this issued one `list_queues` per local queue
/// (O(N²)) and serial `get_schema` / `get_inbox` calls. The new shape:
///
/// 1. Hoist `list_queues` and `list_email_templates` to ONE call each.
/// 2. Walk the local tree once to collect work items (queue / schema /
///    inbox / template diffs) with their (slug, id) pairs.
/// 3. Run the per-queue schema and inbox GETs in a single parallel
///    batch with `DIFF_FANOUT` concurrency, behind one base-named
///    spinner per kind that updates a `(N/M)` counter as fetches land.
/// 4. Print all diffs in deterministic order (sorted by slug).
async fn diff_queue_tree(
    paths: &Paths,
    lockfile: &Lockfile,
    client: &RossumClient,
    counter: &mut usize,
    progress: &Arc<Log>,
) -> Result<()> {
    // Phase A: walk the local tree, classify items by kind. No API
    // calls yet.
    let mut queue_items: Vec<(String, crate::model::Queue, u64)> = Vec::new(); // q_slug, local, id
    let mut schema_items: Vec<(String, crate::model::Schema, u64)> = Vec::new(); // q_slug, local, id
    let mut inbox_items: Vec<(String, crate::model::Inbox, u64)> = Vec::new(); // q_slug, local, id
    let mut template_items: Vec<(String, crate::model::EmailTemplate, u64)> = Vec::new(); // key, local, id

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

            if queue_dir.join("queue.json").exists() {
                if let (Ok(local), Some(id)) =
                    (read_queue(&queue_dir), lookup_id(lockfile, "queues", &q_slug))
                {
                    queue_items.push((q_slug.clone(), local, id));
                }
            }
            if queue_dir.join("schema.json").exists() {
                if let (Ok(local), Some(id)) =
                    (read_schema(&queue_dir), lookup_id(lockfile, "schemas", &q_slug))
                {
                    schema_items.push((q_slug.clone(), local, id));
                }
            }
            if queue_dir.join("inbox.json").exists() {
                if let (Ok(local), Some(id)) =
                    (read_inbox(&queue_dir), lookup_id(lockfile, "inboxes", &q_slug))
                {
                    inbox_items.push((q_slug.clone(), local, id));
                }
            }
            let templates_dir = paths.queue_email_templates_dir(&ws_slug, &q_slug);
            if templates_dir.exists() {
                for t_entry in std::fs::read_dir(&templates_dir)? {
                    let t_entry = t_entry?;
                    let name = t_entry.file_name().to_string_lossy().to_string();
                    if crate::paths::is_shadow_artifact(&name, paths.env()) {
                        continue;
                    }
                    let Some(t_slug) = name.strip_suffix(".json") else { continue };
                    let Ok(local) = read_email_template(&templates_dir, t_slug) else { continue };
                    let key = format!("{ws_slug}/{q_slug}/{t_slug}");
                    let Some(id) = lookup_id(lockfile, "email_templates", &key) else { continue };
                    template_items.push((key, local, id));
                }
            }
        }
    }

    // Phase B: hoisted LIST calls. queues + email-templates each fetched
    // exactly once, no matter how many local queues / templates exist.
    let queues_remote: std::collections::BTreeMap<u64, crate::model::Queue> = if !queue_items.is_empty() {
        let list = client.list_queues(Some(progress.clone())).await?;
        progress.event(Action::Diff, &format!("queues ({} remote)", list.len()));
        list.into_iter().map(|q| (q.id, q)).collect()
    } else {
        std::collections::BTreeMap::new()
    };
    let templates_remote: std::collections::BTreeMap<u64, crate::model::EmailTemplate> =
        if !template_items.is_empty() {
            let list = client.list_email_templates(Some(progress.clone())).await?;
            progress.event(Action::Diff, &format!("email_templates ({} remote)", list.len()));
            list.into_iter().map(|t| (t.id, t)).collect()
        } else {
            std::collections::BTreeMap::new()
        };

    // Phase C: parallel per-queue schema GETs.
    let schemas_fetched = parallel_fetch_by_id(
        "schemas",
        schema_items,
        progress,
        |id, prog| {
            let client = client;
            async move { client.get_schema(id, Some(prog)).await }
        },
    )
    .await?;

    // Phase D: parallel per-queue inbox GETs.
    let inboxes_fetched = parallel_fetch_by_id(
        "inboxes",
        inbox_items,
        progress,
        |id, prog| {
            let client = client;
            async move { client.get_inbox(id, Some(prog)).await }
        },
    )
    .await?;

    // Phase E: print diffs in deterministic order.
    // queue.json — sorted by slug
    let mut queues_sorted = queue_items;
    queues_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (q_slug, local, id) in queues_sorted {
        let before = *counter;
        if let Some(remote) = queues_remote.get(&id) {
            let lj = canonical_json_for_diff(&local)?;
            let rj = canonical_json_for_diff(remote)?;
            print_unified(
                &format!("queues/{q_slug}/queue.json (local)"),
                &format!("queues/{q_slug}/queue.json (remote)"),
                &lj, &rj, counter,
            );
        } else {
            progress.event(Action::Warn, &format!("queue/{q_slug} not on remote"));
            continue;
        }
        if *counter > before {
            progress.event(Action::Diff, &format!("queue/{q_slug} differs"));
        } else {
            progress.event(Action::Diff, &format!("queue/{q_slug} clean"));
        }
    }

    // schema.json + formulas/ — sorted by slug
    let mut schemas_sorted = schemas_fetched;
    schemas_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (q_slug, local, remote) in schemas_sorted {
        let (lj, l_formulas) = serialize_schema(&local)?;
        let (rj, r_formulas) = serialize_schema(&remote)?;
        let ljs = String::from_utf8_lossy(&lj);
        let rjs = String::from_utf8_lossy(&rj);
        let before = *counter;
        print_unified(
            &format!("schemas/{q_slug}/schema.json (local)"),
            &format!("schemas/{q_slug}/schema.json (remote)"),
            &ljs, &rjs, counter,
        );
        diff_formulas(&q_slug, &l_formulas, &r_formulas, counter);
        if *counter > before {
            progress.event(Action::Diff, &format!("schema/{q_slug} differs"));
        } else {
            progress.event(Action::Diff, &format!("schema/{q_slug} clean"));
        }
    }

    // inbox.json — sorted by slug
    let mut inboxes_sorted = inboxes_fetched;
    inboxes_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (q_slug, local, remote) in inboxes_sorted {
        let lj = canonical_json_for_diff(&local)?;
        let rj = canonical_json_for_diff(&remote)?;
        let before = *counter;
        print_unified(
            &format!("inboxes/{q_slug}/inbox.json (local)"),
            &format!("inboxes/{q_slug}/inbox.json (remote)"),
            &lj, &rj, counter,
        );
        if *counter > before {
            progress.event(Action::Diff, &format!("inbox/{q_slug} differs"));
        } else {
            progress.event(Action::Diff, &format!("inbox/{q_slug} clean"));
        }
    }

    // email-templates — sorted by composite key (ws/q/template)
    let mut templates_sorted = template_items;
    templates_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, local, id) in templates_sorted {
        let before = *counter;
        if let Some(remote) = templates_remote.get(&id) {
            let lj = canonical_json_for_diff(&local)?;
            let rj = canonical_json_for_diff(remote)?;
            print_unified(
                &format!("email_templates/{key}.json (local)"),
                &format!("email_templates/{key}.json (remote)"),
                &lj, &rj, counter,
            );
        } else {
            progress.event(Action::Warn, &format!("email_template/{key} not on remote"));
            continue;
        }
        if *counter > before {
            progress.event(Action::Diff, &format!("email_template/{key} differs"));
        } else {
            progress.event(Action::Diff, &format!("email_template/{key} clean"));
        }
    }

    Ok(())
}

/// Generic parallel batch fetch for one kind. Spawns up to `DIFF_FANOUT`
/// in-flight GETs and returns the fetched results for the caller to sort
/// and print.
async fn parallel_fetch_by_id<T, F, Fut>(
    kind_label: &str,
    items: Vec<(String, T, u64)>,
    progress: &Arc<Log>,
    fetch: F,
) -> Result<Vec<(String, T, T)>>
where
    T: Send + 'static,
    F: Fn(u64, Arc<Log>) -> Fut + Sync,
    Fut: std::future::Future<Output = Result<T>> + Send,
{
    use futures::stream::{StreamExt, TryStreamExt};

    if items.is_empty() {
        return Ok(Vec::new());
    }
    let total = items.len();
    progress.event(Action::Diff, &format!("{kind_label} start"));

    let fetched_result: Result<Vec<(String, T, T)>> = futures::stream::iter(items)
        .map(|(slug, local, id)| {
            let progress = progress.clone();
            let fetch = &fetch;
            let kind_label = kind_label;
            async move {
                let remote = fetch(id, progress.clone())
                    .await
                    .with_context(|| format!("fetching {kind_label} for {slug}"))?;
                Ok::<_, anyhow::Error>((slug, local, remote))
            }
        })
        .buffer_unordered(DIFF_FANOUT)
        .try_collect()
        .await;

    let fetched = fetched_result?;
    progress.event(Action::Diff, &format!("{kind_label} ({total} fetched)"));
    Ok(fetched)
}
