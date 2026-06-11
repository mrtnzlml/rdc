//! `rdc migrate <src> <tgt>` — a pure-local snapshot→snapshot transform.
//!
//! Because snapshots are environment-portable (cross-object references are
//! stored as `rdc://<kind>/<slug>`, with slugs stable across environments),
//! promoting a source env to a target env is a file transform with **zero
//! remote calls**:
//!
//! 1. copy every file under `envs/<src>/` into `envs/<tgt>/`,
//! 2. rename the slug path-components per the [`Mapping`],
//! 3. substitute `rdc://<kind>/<src_slug>` → `rdc://<kind>/<tgt_slug>` in
//!    file contents (identity for same-slug auto-matched pairs),
//! 4. apply the target env's overlay to each object.
//!
//! Afterwards the user reviews `git diff` and runs `rdc sync <tgt>` to push.
//! `rdc deploy` is untouched; `migrate` is added alongside it.

use crate::mapping::Mapping;
use crate::overlay::{Overlay, apply_overrides};
use crate::snapshot::refs::{RDC_SCHEME, walk_strings_mut};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Every portable kind that can appear as a `rdc://` reference target, paired
/// with its `Mapping` accessor. `engine_fields`/`email_templates` are never
/// reference targets (nothing points at them), so they are omitted from the
/// substitution dictionary — only the placement remap (`remap_relative`) uses
/// them.
const SUBST_KINDS: &[&str] = &[
    "workspaces",
    "queues",
    "schemas",
    "inboxes",
    "hooks",
    "rules",
    "labels",
    "engines",
];

/// Look up the tgt slug for `(kind, src_slug)`, falling back to identity
/// (same slug) when the pair isn't mapped — the common auto-matched case.
fn tgt_slug(mapping: &Mapping, kind: &str, src_slug: &str) -> String {
    mapping
        .lookup_tgt_slug(kind, src_slug)
        .map(str::to_string)
        .unwrap_or_else(|| src_slug.to_string())
}

/// Build the `rdc://<kind>/<src>` → `rdc://<kind>/<tgt>` substitution map for
/// every non-identity mapped pair across the reference-target kinds. Identity
/// pairs (auto-matched same-slug) are skipped — they need no rewrite, and
/// including them would only bloat the dict.
fn build_subst(mapping: &Mapping) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for kind in SUBST_KINDS {
        let Some(map) = mapping.kind_map(kind) else {
            continue;
        };
        for (src, tgt) in map {
            if src == tgt {
                continue;
            }
            out.insert(
                format!("{RDC_SCHEME}{kind}/{src}"),
                format!("{RDC_SCHEME}{kind}/{tgt}"),
            );
        }
    }
    out
}

/// Classify a snapshot-relative path into the `(kind, src_slug)` coordinate of
/// the *primary* object the leaf file represents, when it has one. Returns
/// `None` for files carrying no overlay-able slug (workflows, mdh,
/// organization, overlay.toml, …).
///
/// `src_slug` is the source-env lockfile/overlay coordinate: flat for
/// hooks/labels/rules/workspaces/queues, and the compound `<engine>/<field>`
/// (engine_fields) / `<ws>/<q>/<template>` (email_templates).
pub(crate) fn classify(rel: &Path) -> Option<(&'static str, String)> {
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let leaf = comps.last()?;

    match comps.first().map(String::as_str) {
        Some("hooks") if comps.len() == 2 => {
            leaf.strip_suffix(".json").map(|s| ("hooks", s.to_string()))
        }
        Some("labels") if comps.len() == 2 => leaf
            .strip_suffix(".json")
            .map(|s| ("labels", s.to_string())),
        Some("rules") if comps.len() == 2 => {
            leaf.strip_suffix(".json").map(|s| ("rules", s.to_string()))
        }
        Some("engines") => {
            if comps.len() == 3 && leaf == "engine.json" {
                Some(("engines", comps[1].clone()))
            } else if comps.len() == 4 && comps[2] == "fields" {
                leaf.strip_suffix(".json")
                    .map(|f| ("engine_fields", format!("{}/{}", comps[1], f)))
            } else {
                None
            }
        }
        Some("workspaces") => classify_workspace(&comps),
        _ => None,
    }
}

fn classify_workspace(comps: &[String]) -> Option<(&'static str, String)> {
    let ws = comps.get(1)?;
    let leaf = comps.last()?;
    match comps.len() {
        3 if leaf == "workspace.json" => Some(("workspaces", ws.clone())),
        5 if comps[2] == "queues" => {
            let q = &comps[3];
            match leaf.as_str() {
                "queue.json" => Some(("queues", q.clone())),
                "schema.json" => Some(("schemas", q.clone())),
                "inbox.json" => Some(("inboxes", q.clone())),
                _ => None,
            }
        }
        6 if comps[2] == "queues" && comps[4] == "email-templates" => {
            let q = &comps[3];
            leaf.strip_suffix(".json")
                .map(|t| ("email_templates", format!("{ws}/{q}/{t}")))
        }
        _ => None,
    }
}

/// Look up the tgt overlay overrides for a classified `(kind, src_slug)` pair.
/// The src slug is first remapped to its tgt slug (overlays are keyed by the
/// target-env slug, since the file lands under that slug), then dispatched to
/// the matching `Overlay` accessor. Returns `None` when no overlay applies.
fn overlay_for<'a>(
    overlay: &'a Overlay,
    mapping: &Mapping,
    kind: &str,
    src_slug: &str,
) -> Option<&'a BTreeMap<String, serde_json::Value>> {
    let tgt = tgt_slug(mapping, kind, src_slug);
    match kind {
        "hooks" => overlay.hook(&tgt),
        "rules" => overlay.rule(&tgt),
        "labels" => overlay.label(&tgt),
        "schemas" => overlay.schema(&tgt),
        "queues" => overlay.queue(&tgt),
        "inboxes" => overlay.inbox(&tgt),
        "email_templates" => overlay.email_template(&tgt),
        "engines" => overlay.engine(&tgt),
        "engine_fields" => overlay.engine_field(&tgt),
        _ => None,
    }
}

/// Transform one source file into its target location.
///
/// - `.json`: parse → substitute whole-string `rdc://` refs via `subst` →
///   apply the tgt overlay for the file's `(kind, tgt_slug)` → `write_atomic`.
/// - anything else (`.py`, …): copied verbatim.
///
/// `rel` is relative to the source env root; the destination is `remap_relative`
/// joined onto `tgt_root`.
fn transform_file(
    rel: &Path,
    src_root: &Path,
    tgt_root: &Path,
    mapping: &Mapping,
    subst: &BTreeMap<String, String>,
    overlay: Option<&Overlay>,
    tgt_org_url: &str,
) -> Result<()> {
    let src_path = src_root.join(rel);
    let dst_path = tgt_root.join(remap_relative(rel, mapping));

    let is_json = rel
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if !is_json {
        let bytes =
            std::fs::read(&src_path).with_context(|| format!("reading {}", src_path.display()))?;
        crate::snapshot::writer::write_atomic(&dst_path, &bytes)?;
        return Ok(());
    }

    let raw =
        std::fs::read(&src_path).with_context(|| format!("reading {}", src_path.display()))?;
    let mut value: serde_json::Value = serde_json::from_slice(&raw)
        .with_context(|| format!("parsing JSON {}", src_path.display()))?;

    // Whole-string rdc:// substitution: a ref is always a standalone field
    // value, never a substring of a template/formula, so we only replace when
    // the entire string is a subst key.
    walk_strings_mut(&mut value, &mut |s| {
        if let Some(replacement) = subst.get(s.as_str()) {
            *s = replacement.clone();
        }
    });

    // Reconcile env-specific identity: the source body carries the SOURCE
    // env's `id`, `created_by`/`modified_by`, `organization`, etc. — which are
    // wrong for the target. For an object that already exists in tgt (matched),
    // restore the TARGET's values for every field `cross_env_body` strips
    // (id/url/created_*/modified_*/status/organization + per-kind server-managed
    // / reverse-ref fields); for a new object (no tgt file), strip them to a
    // clean create payload so `rdc sync` POSTs and the server assigns identity.
    // Runs BEFORE the overlay so an explicit overlay override still wins.
    if let Some((kind, _)) = classify(rel)
        && let Some(codec) = crate::snapshot::codec::codec(kind)
    {
        reconcile_target_identity(&mut value, &dst_path, codec, tgt_org_url);
    }

    // Apply the tgt overlay for this object, if any.
    if let Some((kind, src_slug)) = classify(rel)
        && let Some(ov) = overlay
        && let Some(overrides) = overlay_for(ov, mapping, kind, &src_slug)
    {
        apply_overrides(&mut value, overrides);
    }

    // Canonicalize a hook's `run_after` to a stable, env-independent order. The
    // refs are now in portable `rdc://<slug>` form (post-subst), and `run_after`
    // is an unordered dependency set, so sorting the slugs makes migrate emit no
    // spurious reorder regardless of the source env's API ordering. No-op for
    // non-hook files (they carry no `run_after`).
    crate::snapshot::hook::sort_run_after(&mut value);
    // Same for a hook's `queues` membership set: keep the portable slugs in
    // sorted order so source envs pulled before the post-pass sort existed
    // don't leak their id-ordering into the target snapshot. Unlike
    // `run_after`, a `queues` array also exists on inboxes/schemas/workspaces
    // where pull preserves API order — so this must stay hooks-only.
    if rel.starts_with("hooks") {
        crate::snapshot::hook::sort_queues(&mut value);
    }

    let mut json = serde_json::to_vec_pretty(&value)?;
    json.push(b'\n');
    crate::snapshot::writer::write_atomic(&dst_path, &json)?;
    Ok(())
}

/// Replace the source body's env-specific identity with the target's.
///
/// `value` is the source object (post-`rdc://` subst). `tgt_path` is where it
/// will land in the target env. The "env-specific" field set is defined
/// authoritatively by the codec's `cross_env_body` (the same fields a cross-env
/// PATCH strips because they never cross envs: id/url/created_*/modified_*/
/// status/organization, plus per-kind server-managed and reverse-ref fields).
///
/// - **Matched** (a target file already exists): for each env-specific field,
///   take the TARGET file's value (or drop the field if the target lacks it).
///   The deployable content — everything `cross_env_body` keeps, including
///   forward refs like a hook's `queues` — stays the source's.
/// - **New** (no target file): strip the server-assigned fields to a clean
///   create payload (`create_body`) so the subsequent `rdc sync` POSTs.
fn reconcile_target_identity(
    value: &mut serde_json::Value,
    tgt_path: &Path,
    codec: &'static dyn crate::snapshot::codec::KindCodec,
    tgt_org_url: &str,
) {
    if !value.is_object() {
        return;
    }

    // The env-specific field set = top-level keys `cross_env_body` removes
    // (id/url/created_*/modified_*/status/organization + per-kind server-managed
    // and reverse-ref fields). Probe on a clone so `value`'s content is intact.
    let mut probe = value.clone();
    codec.cross_env_body(&mut probe);
    let kept: std::collections::BTreeSet<String> = probe
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let env_fields: Vec<String> = value
        .as_object()
        .map(|m| m.keys().filter(|k| !kept.contains(*k)).cloned().collect())
        .unwrap_or_default();

    let tgt: Option<serde_json::Value> = std::fs::read(tgt_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());

    // Does the source object carry an `organization`? (Used only on the
    // new-object path to decide whether to set the target org.)
    let had_org = value.get("organization").is_some();

    let tgt_obj = tgt.as_ref().and_then(|t| t.as_object());
    let obj = value.as_object_mut().expect("checked is_object above");

    match tgt_obj {
        Some(tobj) => {
            // Matched: take the TARGET's value for every env field (or drop it
            // if the target lacks it). Deployable content stays the source's.
            for field in env_fields {
                match tobj.get(&field) {
                    Some(tgt_field) => {
                        obj.insert(field, tgt_field.clone());
                    }
                    None => {
                        obj.remove(&field);
                    }
                }
            }
        }
        None => {
            // New in tgt: there's no target identity to inherit. Strip only the
            // universally server-assigned fields (id/url/created_*/modified_*/
            // status) so `rdc sync` POSTs a clean create — the server assigns
            // them. Set `organization` to the TARGET org (the object is created
            // in tgt; src's org would be wrong/rejected). Leave the rest as the
            // source's transformed content, including portable `rdc://` ref
            // lists — the subst already remapped them to tgt slugs, and the
            // server reconciles reverse-ref lists on create.
            for field in crate::snapshot::create::UNIVERSAL_SERVER_FIELDS {
                obj.remove(*field);
            }
            if had_org {
                obj.insert(
                    "organization".to_string(),
                    serde_json::Value::String(tgt_org_url.to_string()),
                );
            }
        }
    }
}

/// rdc-managed top-level directories under an env root — the same per-kind
/// dirs `paths.rs` exposes and `push::scan` reads. Migrate copies only files
/// WITHIN these. Everything else under `envs/<env>/` — user pytest `tests/`,
/// helper `scripts/`, `README`s, `__pycache__`, and the per-env singletons
/// (`_index.md`, `overlay.toml`, `organization.json`) — is NOT rdc-managed and
/// must be left untouched: a snapshot→snapshot transform has no business
/// copying files rdc neither pulls nor pushes.
const MANAGED_DIRS: &[&str] = &[
    "hooks",
    "workspaces",
    "rules",
    "labels",
    "engines",
    "workflows",
    "mdh",
];

/// Enumerate every rdc-managed snapshot file under `env_root`, returning paths
/// relative to `env_root`. Only descends into [`MANAGED_DIRS`]; any other
/// top-level entry is ignored entirely. Within a managed dir, sync shadow
/// artifacts (`<file>.<env>` / `<file>.<env>-deleted`) are skipped via
/// [`should_skip`].
fn enumerate_files(env_root: &Path, env: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for dir in MANAGED_DIRS {
        let managed = env_root.join(dir);
        if managed.exists() {
            walk_dir(env_root, &managed, env, &mut out)?;
        }
    }
    out.sort();
    Ok(out)
}

/// File extensions rdc actually writes inside a managed dir: `.json` (objects),
/// `.py` (hook / rule / schema-formula code), `.js` (Node.js hook code).
/// Anything else sitting next to them — `.pyc` bytecode, `.DS_Store`, editor
/// temp files, sync shadow artifacts (`<file>.<env>`) — is foreign to rdc and
/// must not be migrated.
fn is_managed_leaf(name: &str) -> bool {
    matches!(
        name.rsplit_once('.').map(|(_, ext)| ext),
        Some("json") | Some("py") | Some("js")
    )
}

fn walk_dir(base: &Path, dir: &Path, env: &str, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            // Never descend Python bytecode caches — they sit beside the `.py`
            // sidecars but are tooling output, not rdc's.
            if name == "__pycache__" {
                continue;
            }
            walk_dir(base, &path, env, out)?;
        } else if !should_skip(&name, env) && is_managed_leaf(&name) {
            let rel = path
                .strip_prefix(base)
                .expect("walked path is under base")
                .to_path_buf();
            out.push(rel);
        }
    }
    Ok(())
}

/// True for files that must not be migrated (per-env config / generated /
/// shadow artifacts).
fn should_skip(name: &str, env: &str) -> bool {
    matches!(name, "_index.md" | "overlay.toml" | "organization.json")
        || crate::paths::is_shadow_artifact(name, env)
}

/// Plan a `--mirror` prune: target-env relative paths that would NOT be
/// produced by migrating the source snapshot. These are tgt-only objects the
/// user must delete to make tgt mirror src exactly. The returned paths are
/// relative to the *target* env root.
fn mirror_prune_paths(
    src_root: &Path,
    src_env: &str,
    tgt_root: &Path,
    tgt_env: &str,
    mapping: &Mapping,
) -> Result<Vec<PathBuf>> {
    use std::collections::BTreeSet;
    let produced: BTreeSet<PathBuf> = enumerate_files(src_root, src_env)?
        .into_iter()
        .map(|rel| remap_relative(&rel, mapping))
        .collect();
    let existing = enumerate_files(tgt_root, tgt_env)?;
    Ok(existing
        .into_iter()
        .filter(|rel| !produced.contains(rel))
        .collect())
}

/// `rdc migrate <src> <tgt>` — pure-local snapshot→snapshot transform.
///
/// Copies `envs/<src>/` into `envs/<tgt>/`, renaming slugs per the auto-matched
/// and hand-curated [`Mapping`], substituting `rdc://<kind>/<src>` refs to their
/// `<tgt>` slug in JSON content, and applying the target overlay. Makes zero
/// remote calls — afterward the user reviews `git diff` and runs `rdc sync <tgt>`.
///
/// `--mirror` additionally deletes target-only objects (files present in tgt
/// but not produced by the migration). `--dry-run` prints the plan and writes
/// nothing. `--only <selector>` narrows the operation to matching
/// `<kind>/<slug>` objects (reusing deploy's selection machinery).
pub fn run(src: &str, tgt: &str, mirror: bool, dry_run: bool, only: Vec<String>) -> Result<()> {
    if src == tgt {
        anyhow::bail!(
            "src and tgt envs are the same ('{src}'). Use two different envs for `rdc migrate`."
        );
    }

    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = crate::paths::Paths::for_env(&cwd, src);
    let tgt_paths = crate::paths::Paths::for_env(&cwd, tgt);
    let src_root = src_paths.env_root();
    let tgt_root = tgt_paths.env_root();

    if !src_root.exists() {
        anyhow::bail!(
            "source snapshot {} does not exist — pull '{src}' first (`rdc sync {src}`)",
            src_root.display()
        );
    }

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));

    // Mapping: load, auto-match same-slug pairs, validate sources, persist.
    let mapping_file = src_paths.mapping_file(src, tgt);
    let mut mapping = Mapping::load(&mapping_file)?;
    let added = crate::cli::deploy::map::auto_match(&mut mapping, &src_paths, &tgt_paths)?;
    crate::cli::deploy::map::validate_mapping_sources(&mapping, &src_paths, &mapping_file)?;
    if !dry_run {
        std::fs::create_dir_all(src_paths.mapping_dir())
            .with_context(|| format!("creating {}", src_paths.mapping_dir().display()))?;
        mapping.save(&mapping_file)?;
    }

    // Optional `--only` selection filter (reuses deploy's pure-fs machinery).
    let selection = crate::cli::deploy::selection::resolve(&only, &src_paths, &tgt_paths)?;

    let subst = build_subst(&mapping);
    let tgt_overlay = Overlay::load(&tgt_paths.overlay_file()).with_context(|| {
        format!(
            "loading tgt overlay from {}",
            tgt_paths.overlay_file().display()
        )
    })?;

    // The target env's organization URL, used to set `organization` on objects
    // that are NEW in tgt (a cross-env create would otherwise carry the source
    // env's org). Matched objects take their org from the existing tgt file.
    let project_cfg = crate::config::ProjectConfig::load(&cwd.join("rdc.toml"))?;
    let tgt_env_cfg = project_cfg
        .envs
        .get(tgt)
        .ok_or_else(|| anyhow::anyhow!("env '{tgt}' is not defined in rdc.toml"))?;
    let tgt_org_url = format!(
        "{}/organizations/{}",
        tgt_env_cfg.api_base.trim_end_matches('/'),
        tgt_env_cfg.org_id
    );

    let files = enumerate_files(&src_root, src)?;
    let mut copied = 0usize;
    let mut renamed = 0usize;

    for rel in &files {
        // `--only`: keep a file only when its classified (kind, slug) is in the
        // selection. Files with no classifiable object (workflows, mdh) are
        // skipped under an active selection — the user narrowed scope.
        if let Some(sel) = &selection {
            match classify(rel) {
                Some((kind, slug)) if sel.contains(kind, &slug) => {}
                _ => continue,
            }
        }

        let dst_rel = remap_relative(rel, &mapping);
        if &dst_rel != rel {
            renamed += 1;
        }
        copied += 1;

        if dry_run {
            log.event(
                crate::log::Action::Plan,
                &format!("{} -> {}", rel.display(), dst_rel.display()),
            );
        } else {
            transform_file(
                rel,
                &src_root,
                &tgt_root,
                &mapping,
                &subst,
                tgt_overlay.as_ref(),
                &tgt_org_url,
            )
            .with_context(|| format!("migrating {}", rel.display()))?;
        }
    }

    // `--mirror`: prune target-only objects.
    let mut pruned = 0usize;
    if mirror {
        let prune = mirror_prune_paths(&src_root, src, &tgt_root, tgt, &mapping)?;
        for rel in &prune {
            pruned += 1;
            if dry_run {
                log.event(
                    crate::log::Action::Delete,
                    &format!("prune {}", rel.display()),
                );
            } else {
                let abs = tgt_root.join(rel);
                std::fs::remove_file(&abs).with_context(|| format!("pruning {}", abs.display()))?;
            }
        }
    }

    let verb = if dry_run { "would migrate" } else { "migrated" };
    log.event(
        crate::log::Action::Done,
        &format!(
            "{verb} {copied} file(s) ({renamed} renamed, {pruned} pruned, {added} auto-matched) \
             envs/{src} -> envs/{tgt}"
        ),
    );
    if !dry_run {
        log.event(
            crate::log::Action::Info,
            &format!("review `git diff`, then `rdc sync {tgt}` to push"),
        );
    }

    Ok(())
}

/// Remap a snapshot-relative path's structured slug components to their
/// target slugs per `mapping`. Files with no mapped slug (or unmapped slugs)
/// pass through with identity. The path is relative to `env_root()`.
///
/// Examples:
/// - `hooks/<slug>.{json,py}` → `hooks/<tgt(hooks,slug)>.…`
/// - `labels/<slug>.json`, `rules/<slug>.{json,py}` likewise.
/// - `workspaces/<ws>/…` → `workspaces/<tgt(workspaces,ws)>/…`; within it
///   `queues/<q>/…` → `queues/<tgt(queues,q)>/…`; the leaf `queue.json` /
///   `schema.json` / `inbox.json` filenames are unchanged; an
///   `email-templates/<t>.json` leaf is remapped via the `email_templates`
///   compound key `<ws>/<q>/<t>`.
/// - `engines/<e>/…` → `engines/<tgt(engines,e)>/…`; a field leaf is remapped
///   via the `engine_fields` compound key `<e>/<field>`.
/// - `workflows/…` and `mdh/…` are identity (pull-only / match-by-slug).
/// - Anything else → identity.
pub fn remap_relative(rel: &Path, mapping: &Mapping) -> PathBuf {
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if comps.is_empty() {
        return rel.to_path_buf();
    }

    match comps[0].as_str() {
        "hooks" if comps.len() == 2 => remap_flat_leaf(&comps, "hooks", mapping),
        "labels" if comps.len() == 2 => remap_flat_leaf(&comps, "labels", mapping),
        "rules" if comps.len() == 2 => remap_flat_leaf(&comps, "rules", mapping),
        "engines" => remap_engine(&comps, mapping),
        "workspaces" => remap_workspace(&comps, mapping),
        _ => rel.to_path_buf(),
    }
}

/// `<dir>/<slug>.<ext>` where `<slug>` is the mapping key for `kind`.
fn remap_flat_leaf(comps: &[String], kind: &str, mapping: &Mapping) -> PathBuf {
    let dir = &comps[0];
    let leaf = &comps[1];
    let new_leaf = match split_ext(leaf) {
        Some((slug, ext)) => format!("{}.{ext}", tgt_slug(mapping, kind, slug)),
        None => leaf.clone(),
    };
    Path::new(dir).join(new_leaf)
}

fn remap_engine(comps: &[String], mapping: &Mapping) -> PathBuf {
    // engines/<engine>/engine.json
    // engines/<engine>/fields/<field>.json
    if comps.len() < 2 {
        return join_comps(comps);
    }
    let src_engine = &comps[1];
    let new_engine = tgt_slug(mapping, "engines", src_engine);
    let mut out = vec!["engines".to_string(), new_engine.clone()];
    if comps.len() == 4 && comps[2] == "fields" {
        out.push("fields".to_string());
        let leaf = &comps[3];
        let new_leaf = match leaf.strip_suffix(".json") {
            Some(field) => {
                let key = format!("{src_engine}/{field}");
                // The mapped field key is `<tgt_engine>/<tgt_field>`; we already
                // placed `<tgt_engine>` in the dir, so take its field segment.
                let mapped = tgt_slug(mapping, "engine_fields", &key);
                let tgt_field = mapped.rsplit_once('/').map(|(_, f)| f).unwrap_or(&mapped);
                format!("{tgt_field}.json")
            }
            None => leaf.clone(),
        };
        out.push(new_leaf);
    } else {
        out.extend_from_slice(&comps[2..]);
    }
    join_comps(&out)
}

fn remap_workspace(comps: &[String], mapping: &Mapping) -> PathBuf {
    if comps.len() < 2 {
        return join_comps(comps);
    }
    let src_ws = &comps[1];
    let new_ws = tgt_slug(mapping, "workspaces", src_ws);
    let mut out = vec!["workspaces".to_string(), new_ws];

    // workspaces/<ws>/queues/<q>/...
    if comps.len() >= 4 && comps[2] == "queues" {
        let src_q = &comps[3];
        let new_q = tgt_slug(mapping, "queues", src_q);
        out.push("queues".to_string());
        out.push(new_q);

        if comps.len() == 6 && comps[4] == "email-templates" {
            let leaf = &comps[5];
            let new_leaf = match leaf.strip_suffix(".json") {
                Some(tmpl) => {
                    let key = format!("{src_ws}/{src_q}/{tmpl}");
                    let mapped = tgt_slug(mapping, "email_templates", &key);
                    // Mapped value is the full compound `<ws>/<q>/<tmpl>`; the
                    // ws/q dirs are already remapped above, so only the last
                    // segment (template slug) feeds the leaf filename.
                    let tgt_tmpl = mapped.rsplit_once('/').map(|(_, t)| t).unwrap_or(&mapped);
                    format!("{tgt_tmpl}.json")
                }
                None => leaf.clone(),
            };
            out.push("email-templates".to_string());
            out.push(new_leaf);
        } else {
            out.extend_from_slice(&comps[4..]);
        }
    } else {
        out.extend_from_slice(&comps[2..]);
    }
    join_comps(&out)
}

fn join_comps(comps: &[String]) -> PathBuf {
    let mut p = PathBuf::new();
    for c in comps {
        p.push(c);
    }
    p
}

/// Split a leaf filename into `(stem, ext)` on the LAST `.`. Returns `None`
/// for a filename with no extension. Used to keep the `.json` / `.py`
/// extension while remapping the stem (slug).
fn split_ext(leaf: &str) -> Option<(&str, &str)> {
    leaf.rsplit_once('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping_with_renames() -> Mapping {
        let mut m = Mapping::default();
        // hook renamed, label identity (not present => identity fallback)
        m.hooks.insert("extractor".into(), "extractor-prod".into());
        // queue renamed under a renamed workspace
        m.workspaces.insert("main".into(), "main-prod".into());
        m.queues.insert("invoices".into(), "invoices-prod".into());
        m
    }

    #[test]
    fn remaps_renamed_hook_leaf() {
        let m = mapping_with_renames();
        assert_eq!(
            remap_relative(Path::new("hooks/extractor.json"), &m),
            PathBuf::from("hooks/extractor-prod.json")
        );
        // The `.py` sidecar follows the same rename.
        assert_eq!(
            remap_relative(Path::new("hooks/extractor.py"), &m),
            PathBuf::from("hooks/extractor-prod.py")
        );
    }

    #[test]
    fn identity_label_passes_through() {
        let m = mapping_with_renames();
        assert_eq!(
            remap_relative(Path::new("labels/priority-high.json"), &m),
            PathBuf::from("labels/priority-high.json")
        );
    }

    #[test]
    fn remaps_renamed_queue_under_renamed_workspace() {
        let m = mapping_with_renames();
        assert_eq!(
            remap_relative(Path::new("workspaces/main/queues/invoices/queue.json"), &m),
            PathBuf::from("workspaces/main-prod/queues/invoices-prod/queue.json")
        );
        // schema + inbox leaves keep their fixed filenames.
        assert_eq!(
            remap_relative(Path::new("workspaces/main/queues/invoices/schema.json"), &m),
            PathBuf::from("workspaces/main-prod/queues/invoices-prod/schema.json")
        );
        assert_eq!(
            remap_relative(Path::new("workspaces/main/queues/invoices/inbox.json"), &m),
            PathBuf::from("workspaces/main-prod/queues/invoices-prod/inbox.json")
        );
    }

    #[test]
    fn enumerate_files_includes_only_rdc_managed_dirs() {
        use std::collections::BTreeSet;
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();

        // rdc-managed files — including non-JSON sidecars that don't classify
        // to a kind but ARE rdc's (hook code, schema formulas).
        let managed = [
            "hooks/extractor.json",
            "hooks/extractor.py",
            "rules/r.json",
            "labels/l.json",
            "engines/e/engine.json",
            "engines/e/fields/f.json",
            "workspaces/main/queues/inv/queue.json",
            "workspaces/main/queues/inv/schema.json",
            "workspaces/main/queues/inv/formulas/123.py",
            "workflows/wf/steps/s.json",
            "mdh/datasets/d.json",
        ];
        // NOT rdc-managed — user content / per-env singletons / generated /
        // tooling detritus. Migrate must leave these entirely alone, including
        // when they appear INSIDE a managed dir (Python bytecode caches next to
        // hook/formula sidecars, editor noise, sync shadow artifacts).
        let non_managed = [
            "tests/test_header_product_match_config.py",
            "tests/__pycache__/test_x.cpython-312.pyc",
            "scripts/deploy.sh",
            "README.md",
            "_index.md",
            "overlay.toml",
            "organization.json",
            // Inside managed dirs but not rdc's:
            "hooks/__pycache__/extractor.cpython-312.pyc",
            "workspaces/main/queues/inv/formulas/__pycache__/f.cpython-312.pyc",
            "hooks/.DS_Store",
            // Sync shadow artifact (must stay skipped):
            "hooks/extractor.json.test-mtr",
        ];
        for rel in managed.iter().chain(non_managed.iter()) {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"x").unwrap();
        }

        let got: BTreeSet<String> = enumerate_files(root, "test-mtr")
            .unwrap()
            .into_iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();

        for rel in managed {
            assert!(got.contains(rel), "managed file {rel} must be enumerated; got {got:?}");
        }
        for rel in non_managed {
            assert!(
                !got.contains(rel),
                "non-rdc-managed entry {rel} must NOT be enumerated; got {got:?}"
            );
        }
    }

    // ---- A2: ref substitution + overlay + write ----

    #[test]
    fn build_subst_emits_only_non_identity_pairs() {
        let mut m = Mapping::default();
        m.workspaces.insert("main".into(), "main".into()); // identity → skipped
        m.queues.insert("invoices".into(), "invoices-prod".into()); // renamed
        m.schemas.insert("invoices".into(), "invoices-prod".into()); // renamed
        let subst = build_subst(&m);
        assert_eq!(
            subst.get("rdc://queues/invoices").map(String::as_str),
            Some("rdc://queues/invoices-prod")
        );
        assert_eq!(
            subst.get("rdc://schemas/invoices").map(String::as_str),
            Some("rdc://schemas/invoices-prod")
        );
        assert!(
            !subst.contains_key("rdc://workspaces/main"),
            "identity pairs must not appear in the subst dict"
        );
    }

    #[test]
    fn transform_rewrites_renamed_refs_and_leaves_identity_refs_intact() {
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();

        // queue.json with a workspace ref (renamed), a schema ref (renamed),
        // and a hook ref (identity — must survive unchanged).
        let mut m = Mapping::default();
        m.workspaces.insert("main".into(), "main-prod".into());
        m.queues.insert("invoices".into(), "invoices-prod".into());
        m.schemas.insert("invoices".into(), "invoices-prod".into());
        m.hooks.insert("extractor".into(), "extractor".into()); // identity

        let rel = Path::new("workspaces/main/queues/invoices/queue.json");
        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        fs::write(
            &src_file,
            serde_json::to_vec(&serde_json::json!({
                "name": "Invoices",
                "workspace": "rdc://workspaces/main",
                "schema": "rdc://schemas/invoices",
                "hooks": ["rdc://hooks/extractor"],
            }))
            .unwrap(),
        )
        .unwrap();

        let subst = build_subst(&m);
        transform_file(rel, src.path(), tgt.path(), &m, &subst, None, "https://tgt.example/api/v1/organizations/2").unwrap();

        // File landed at the remapped path.
        let dst = tgt
            .path()
            .join("workspaces/main-prod/queues/invoices-prod/queue.json");
        assert!(dst.exists(), "transformed file must land at remapped path");

        let v: serde_json::Value = serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap();
        assert_eq!(v["workspace"], "rdc://workspaces/main-prod");
        assert_eq!(v["schema"], "rdc://schemas/invoices-prod");
        // identity hook ref untouched
        assert_eq!(v["hooks"][0], "rdc://hooks/extractor");
        // unrelated field untouched
        assert_eq!(v["name"], "Invoices");
    }

    #[test]
    fn transform_applies_tgt_overlay_under_tgt_slug() {
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();

        let mut m = Mapping::default();
        m.hooks.insert("extractor".into(), "extractor-prod".into());

        // Overlay keyed by the TARGET slug.
        let mut overlay = Overlay::default();
        let mut ov = BTreeMap::new();
        ov.insert(
            "name".to_string(),
            serde_json::Value::String("Extractor (PROD)".into()),
        );
        overlay.hooks.insert("extractor-prod".to_string(), ov);

        let rel = Path::new("hooks/extractor.json");
        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        fs::write(
            &src_file,
            serde_json::to_vec(&serde_json::json!({ "name": "Extractor", "type": "function" }))
                .unwrap(),
        )
        .unwrap();

        let subst = build_subst(&m);
        transform_file(rel, src.path(), tgt.path(), &m, &subst, Some(&overlay), "https://tgt.example/api/v1/organizations/2").unwrap();

        let dst = tgt.path().join("hooks/extractor-prod.json");
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap();
        assert_eq!(v["name"], "Extractor (PROD)", "tgt overlay must be applied");
        assert_eq!(v["type"], "function");
    }

    #[test]
    fn transform_matched_hook_preserves_target_env_metadata_and_sorts_run_after() {
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();

        // Identity mapping (auto-matched same-slug hook).
        let m = Mapping::default();

        let rel = Path::new("hooks/mdh-vendors.json");

        // SOURCE (dev): per-env store-template metadata points at the source
        // env, and run_after is in the source env's arbitrary API order.
        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        fs::write(
            &src_file,
            serde_json::to_vec(&serde_json::json!({
                "id": 111,
                "url": "https://acme-dev.rossum.app/api/v1/hooks/111",
                "name": "MDH: Vendors",
                "type": "webhook",
                "extension_source": "rossum_store",
                "run_after": ["rdc://hooks/valve-template", "rdc://hooks/fitting-template"],
                "token_owner": "https://acme-dev.rossum.app/api/v1/users/1",
                "hook_template": "https://acme-dev.rossum.app/api/v1/hook_templates/39",
                "guide": "<form action=\"https://acme-dev.rossum.app/svc/x/\"></form>",
                "organization": "https://acme-dev.rossum.app/api/v1/organizations/1",
            }))
            .unwrap(),
        )
        .unwrap();

        // TARGET (test): an already-matched hook with the test env's metadata.
        let tgt_file = tgt.path().join(rel);
        fs::create_dir_all(tgt_file.parent().unwrap()).unwrap();
        fs::write(
            &tgt_file,
            serde_json::to_vec(&serde_json::json!({
                "id": 222,
                "url": "https://acme-test.rossum.app/api/v1/hooks/222",
                "name": "MDH: Vendors",
                "type": "webhook",
                "extension_source": "rossum_store",
                "run_after": [],
                "token_owner": "https://acme-test.rossum.app/api/v1/users/2",
                "hook_template": "https://acme-test.rossum.app/api/v1/hook_templates/39",
                "guide": "<form action=\"https://acme-test.rossum.app/svc/x/\"></form>",
                "organization": "https://acme-test.rossum.app/api/v1/organizations/2",
            }))
            .unwrap(),
        )
        .unwrap();

        let subst = build_subst(&m);
        transform_file(
            rel,
            src.path(),
            tgt.path(),
            &m,
            &subst,
            None,
            "https://acme-test.rossum.app/api/v1/organizations/2",
        )
        .unwrap();

        let v: serde_json::Value = serde_json::from_slice(&fs::read(&tgt_file).unwrap()).unwrap();

        // Per-env, read-only store-template metadata must stay the TARGET's —
        // migrate must NOT import the source env's host (the reported bug).
        assert_eq!(
            v["hook_template"], "https://acme-test.rossum.app/api/v1/hook_templates/39",
            "hook_template must be preserved as the target env's value",
        );
        assert_eq!(
            v["guide"], "<form action=\"https://acme-test.rossum.app/svc/x/\"></form>",
            "guide must be preserved as the target env's value",
        );
        assert_eq!(
            v["token_owner"], "https://acme-test.rossum.app/api/v1/users/2",
            "token_owner must be preserved as the target env's value",
        );

        // run_after is a dependency SET — its rdc:// refs must be sorted to a
        // canonical, env-stable order so migrate produces no spurious reorder.
        assert_eq!(
            v["run_after"],
            serde_json::json!([
                "rdc://hooks/fitting-template",
                "rdc://hooks/valve-template"
            ]),
            "run_after rdc:// refs must be lexicographically sorted",
        );
    }

    #[test]
    fn transform_copies_py_sidecar_verbatim_to_remapped_path() {
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();

        let mut m = Mapping::default();
        m.hooks.insert("extractor".into(), "extractor-prod".into());

        let rel = Path::new("hooks/extractor.py");
        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        let code = b"def f(payload):\n    return {}  # rdc://hooks/extractor literal\n";
        fs::write(&src_file, code).unwrap();

        let subst = build_subst(&m);
        transform_file(rel, src.path(), tgt.path(), &m, &subst, None, "https://tgt.example/api/v1/organizations/2").unwrap();

        let dst = tgt.path().join("hooks/extractor-prod.py");
        assert_eq!(
            fs::read(&dst).unwrap(),
            code,
            ".py is copied verbatim — no ref substitution inside code"
        );
    }
}
