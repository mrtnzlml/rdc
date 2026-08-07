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
//! 5. replace a code/formula sidecar's content when the target env carries a
//!    shadow file at `envs/<tgt>/overlay/<same-relpath>` (whole-file override;
//!    a shadow that mirrors no source sidecar is a hard error).
//!
//! Afterwards the user reviews `git diff` and runs `rdc sync <tgt>` to push.
//! `rdc deploy` is untouched; `migrate` is added alongside it.

use crate::mapping::{GenericMapping, Mapping};
use crate::overlay::{Overlay, apply_overrides};
use crate::snapshot::refs::{RDC_SCHEME, walk_strings_mut};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Recover `(env_a, env_b)` from a legacy `<a>-to-<b>` file stem by matching
/// both sides against known envs. Robust to hyphens in env names: it accepts
/// the split where both halves are real envs.
fn parse_legacy_env_pair(stem: &str, known_envs: &BTreeSet<String>) -> Option<(String, String)> {
    for a in known_envs {
        if let Some(rest) = stem.strip_prefix(&format!("{a}-to-"))
            && known_envs.contains(rest)
        {
            return Some((a.clone(), rest.to_string()));
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
        let generic = GenericMapping::load(&generic_path)?;
        // Validate the existing, hand-authored file every time it's loaded —
        // this is the ONLY place the happy path (no legacy conversion) runs
        // validation, so a duplicate-(env,slug) row still hard-errors and an
        // unknown-env row still gets warned about.
        log_mapping_warnings(log, &generic.validate(known_envs)?);
        // A committed `.rdc/mapping.toml` is authoritative and legacy per-pair
        // files are NEVER read once it exists. If any linger — one that
        // reappeared via a branch switch/merge, or one left behind because an
        // earlier conversion skipped it as malformed — warn instead of ignoring
        // it silently: the renames it encodes are being dropped, and the user
        // must fold them into `.rdc/mapping.toml` (then delete the file).
        let leftover = src_paths.legacy_mapping_files();
        if !leftover.is_empty() {
            log.event(
                crate::log::Action::Warn,
                &format!(
                    "{} legacy .rdc/map/*.toml file(s) are present alongside \
                     .rdc/mapping.toml and are IGNORED: {}. Fold any renames they \
                     encode into .rdc/mapping.toml, then delete them.",
                    leftover.len(),
                    display_paths(&leftover),
                ),
            );
        }
        return Ok(generic);
    }
    let legacy = src_paths.legacy_mapping_files();
    if legacy.is_empty() {
        return Ok(GenericMapping::default());
    }

    let mut parsed: Vec<(String, String, Mapping)> = Vec::new();
    // Only the legacy files we actually parsed above — never the ones
    // skipped because their env pair isn't in `rdc.toml`. Deleting a
    // skipped file would silently destroy a mapping this run never
    // converted; the cleanup loop below MUST use this list, not `legacy`.
    let mut converted: Vec<PathBuf> = Vec::new();
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
        // A malformed legacy file must NOT abort the whole conversion — before
        // the generic file existed, `migrate <src> <tgt>` read only its own
        // pair file, so an unrelated bad file (a stray hand edit in some other
        // pair) never blocked it. Warn, skip it, and crucially leave it on disk
        // (not in `converted`, so the cleanup loop never deletes it): the user
        // fixes or removes it, and the leftover-legacy warning above then
        // surfaces it on the next run.
        let m = match Mapping::load(path) {
            Ok(m) => m,
            Err(e) => {
                log.event(
                    crate::log::Action::Warn,
                    &format!(
                        "skipping malformed legacy mapping file {} ({e:#}); it is \
                         left in place — fix or remove it, then re-run migrate to \
                         convert it",
                        path.display()
                    ),
                );
                continue;
            }
        };
        parsed.push((a, b, m));
        converted.push(path.clone());
    }

    // A genuine cross-file slug conflict (one env mapped to two different slugs)
    // is unrepresentable in a single N-way row and MUST NOT be silently
    // resolved, so `from_legacy` hard-errors — but nothing has been written or
    // deleted yet, so no data is lost. Attribute the exact files being unioned
    // so the abort is actionable (which .rdc/map/*.toml to reconcile).
    let generic = GenericMapping::from_legacy(&parsed).with_context(|| {
        format!(
            "converting legacy mapping files ({}) into {}",
            display_paths(&converted),
            generic_path.display(),
        )
    })?;
    // Validate BEFORE persisting/deleting anything: an invalid conversion
    // must neither be saved nor cost us the legacy files it came from. A
    // duplicate-(env,slug) row still hard-errors via `?`; an unknown-env row
    // is only ever a warning, logged below.
    log_mapping_warnings(log, &generic.validate(known_envs)?);

    let verb = if dry_run { "would migrate" } else { "migrated" };
    log.event(
        crate::log::Action::Info,
        &format!(
            "{verb} {} legacy mapping file(s) -> {}",
            converted.len(),
            generic_path.display()
        ),
    );

    if !dry_run {
        if !generic.is_empty() {
            generic.save(&generic_path)?;
        }
        for path in &converted {
            std::fs::remove_file(path)
                .with_context(|| format!("removing legacy mapping {}", path.display()))?;
        }
    }
    Ok(generic)
}

/// Log each `GenericMapping::validate` warning (e.g. a row referencing an env
/// no longer in `rdc.toml`) via `log`, one event per warning.
fn log_mapping_warnings(log: &crate::log::Log, warnings: &[String]) {
    for w in warnings {
        log.event(crate::log::Action::Warn, w);
    }
}

/// Comma-join a list of paths for a human-facing log/error message.
fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

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

/// Selection-aware variant of [`classify`]: additionally maps code sidecars
/// to the primary object they belong to, so `--only hooks/<slug>` carries
/// `hooks/<slug>.py`/`.js` (and rules `.py`, schema `formulas/*.py`) along
/// with the JSON. Only the `--only` filter uses this — remapping and overlay
/// lookups keep operating on primary JSON files via [`classify`].
fn classify_for_selection(rel: &Path) -> Option<(&'static str, String)> {
    if let Some(hit) = classify(rel) {
        return Some(hit);
    }
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let leaf = comps.last()?;
    let strip_code = |l: &str| {
        l.strip_suffix(".py")
            .or_else(|| l.strip_suffix(".js"))
            .map(str::to_string)
    };
    match comps.first().map(String::as_str) {
        Some("hooks") if comps.len() == 2 => strip_code(leaf).map(|s| ("hooks", s)),
        Some("rules") if comps.len() == 2 => leaf
            .strip_suffix(".py")
            .map(|s| ("rules", s.to_string())),
        // workspaces/<ws>/queues/<q>/formulas/<field>.py → the queue's schema.
        Some("workspaces")
            if comps.len() == 6 && comps[2] == "queues" && comps[4] == "formulas" =>
        {
            leaf.strip_suffix(".py")
                .map(|_| ("schemas", comps[3].clone()))
        }
        _ => None,
    }
}

/// True for a non-JSON code/formula sidecar leaf (`.py`/`.js`) that belongs to
/// a hook, rule, or schema — the files `migrate` copies verbatim and that an
/// `overlay/` shadow may replace. JSON objects are excluded (they are
/// overlay-able through `overlay.toml`); non-sidecar code returns false.
fn is_sidecar(rel: &Path) -> bool {
    let is_json = rel
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    !is_json && classify_for_selection(rel).is_some()
}

/// Recursively list files under `overlay_dir`, returning paths RELATIVE to it,
/// sorted. Skips `__pycache__` directories (tooling output), mirroring
/// [`walk_dir`]. Returns an empty vec if `overlay_dir` does not exist.
fn list_overlay_files(overlay_dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if entry.file_name() == "__pycache__" {
                    continue;
                }
                walk(base, &path, out)?;
            } else {
                out.push(
                    path.strip_prefix(base)
                        .expect("walked path is under base")
                        .to_path_buf(),
                );
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if overlay_dir.exists() {
        walk(overlay_dir, overlay_dir, &mut out)?;
    }
    out.sort();
    Ok(out)
}

/// Validate the target env's `overlay/` shadow directory. Every file under it
/// must mirror a code/formula sidecar that migrating the source produces in the
/// target — i.e. its relpath must be in `produced` (the full source enumeration
/// remapped to target paths, filtered to sidecars, independent of `--only`). A
/// shadow that overwrites nothing — a typo, a stale path, a `.json`, or a
/// sidecar absent from the source — is a hard error naming the offending files.
/// Run BEFORE any target file is written so the migration aborts cleanly.
fn validate_overlay_dir(
    overlay_dir: &Path,
    produced: &std::collections::BTreeSet<PathBuf>,
) -> Result<()> {
    let offenders: Vec<PathBuf> = list_overlay_files(overlay_dir)?
        .into_iter()
        .filter(|rel| !produced.contains(rel))
        .collect();
    if !offenders.is_empty() {
        let list = offenders
            .iter()
            .map(|p| format!("  - overlay/{}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "overlay/ contains shadow file(s) that overwrite no source code/formula sidecar:\n\
             {list}\n\
             Each file under envs/<env>/overlay/ must mirror a sidecar produced by migrating the \
             source (a hook/rule .py/.js, or a queue's formulas/<field>.py). Fix the path or remove it."
        );
    }
    Ok(())
}

/// The reserved kind-wide-default overlay key. Not a valid Rossum slug, so it
/// never targets a single object — it is exempt from the "every key must target
/// a produced object" validation.
const OVERLAY_WILDCARD: &str = "*";

/// An `overlay.toml` key that targets no object the migration produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DanglingOverlayKey {
    pub kind: &'static str,
    pub key: String,
    /// The existing slugs of that kind the migration DOES produce — offered as
    /// suggestions (a dangling key is usually a since-renamed or mistyped slug).
    pub existing: Vec<String>,
}

/// Return every non-`"*"` overlay key that targets no object in `produced`.
///
/// `produced` maps each overlay kind to the set of TARGET-env slugs the
/// migration writes (overlays are keyed by target slug). A key absent from that
/// set would be silently ignored during migrate, letting the source env's value
/// promote unchanged — e.g. a hook renamed after the overlay was written leaves
/// `[hooks.<old-slug>]` dangling and its per-env override (SFTP credentials, an
/// `active=false`, a prod URL) never applies. Callers turn these into a hard
/// error via [`format_dangling_overlay_error`].
pub(crate) fn dangling_overlay_keys(
    overlay: &Overlay,
    produced: &BTreeMap<&'static str, std::collections::BTreeSet<String>>,
) -> Vec<DanglingOverlayKey> {
    let mut out = Vec::new();
    for (kind, entries) in overlay.kind_maps() {
        let have = produced.get(kind);
        for key in entries.keys() {
            if key == OVERLAY_WILDCARD {
                continue;
            }
            let present = have.is_some_and(|s| s.contains(key));
            if !present {
                let existing = have.map(|s| s.iter().cloned().collect()).unwrap_or_default();
                out.push(DanglingOverlayKey {
                    kind,
                    key: key.clone(),
                    existing,
                });
            }
        }
    }
    out
}

/// Build the hard-error message for [`dangling_overlay_keys`] offenders. Names
/// each dangling `[kind.key]` and lists the existing slugs of that kind so the
/// user can fix the slug or drop the entry. The reserved `"*"` default is noted
/// as exempt.
pub(crate) fn format_dangling_overlay_error(
    offenders: &[DanglingOverlayKey],
    _src: &str,
    tgt: &str,
) -> String {
    let mut body = String::new();
    for o in offenders {
        body.push_str(&format!("  - [{}.{}]", o.kind, o.key));
        if o.existing.is_empty() {
            body.push_str(&format!("  (no {} are migrated)\n", o.kind));
        } else {
            body.push_str(&format!(
                "  (existing {}: {})\n",
                o.kind,
                o.existing.join(", ")
            ));
        }
    }
    format!(
        "envs/{tgt}/overlay.toml has override(s) targeting no object this migration produces:\n\
         {body}\
         Each non-\"*\" key must be the TARGET-env slug of a migrated object — a slug left stale \
         after the object was renamed (e.g. by `rdc doctor`) or a typo. Rename the key to one of \
         the existing slugs listed above, or remove the entry. (The kind-wide `\"*\"` default is \
         exempt.)"
    )
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

/// The PRIMARY object leaf for the managed dirs that [`classify`] deliberately
/// ignores because they are never `rdc://` reference targets, overlay units, or
/// substitution keys: `workflows/<slug>/workflow.json` and
/// `mdh/<slug>/indexes.json`. Returns `None` for their sidecars (workflow
/// `steps/*.json`, extra index files) so each object counts exactly once.
///
/// Used ONLY by the migrate summary count, which must include the workflow and
/// MDH objects the write loop copies — keeping it out of `classify` proper
/// preserves that function's None-for-workflows/mdh contract that overlay-key
/// validation, `--only` selection, and the substitution map all rely on.
fn classify_managed_primary(rel: &Path) -> Option<(&'static str, String)> {
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match comps.first().map(String::as_str) {
        Some("workflows") if comps.len() == 3 && comps[2] == "workflow.json" => {
            Some(("workflows", comps[1].clone()))
        }
        Some("mdh") if comps.len() == 3 && comps[2] == "indexes.json" => {
            Some(("mdh", comps[1].clone()))
        }
        _ => None,
    }
}

/// Dispatch an overlay lookup for a `(kind, slug)` pair to the matching
/// accessor. `slug` is the literal overlay key — either a remapped target slug
/// (a per-object override) or the reserved `"*"` wildcard (a kind-wide default).
/// `"*"` is not a valid Rossum slug, so it never collides with a real object.
fn overlay_slug<'a>(
    overlay: &'a Overlay,
    kind: &str,
    slug: &str,
) -> Option<&'a BTreeMap<String, serde_json::Value>> {
    match kind {
        "hooks" => overlay.hook(slug),
        "rules" => overlay.rule(slug),
        "labels" => overlay.label(slug),
        "schemas" => overlay.schema(slug),
        "queues" => overlay.queue(slug),
        "inboxes" => overlay.inbox(slug),
        "email_templates" => overlay.email_template(slug),
        "engines" => overlay.engine(slug),
        "engine_fields" => overlay.engine_field(slug),
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
    overlay_slug(overlay, kind, &tgt_slug(mapping, kind, src_slug))
}

/// Transform one source file into its target location.
///
/// - `.json`: parse → substitute whole-string `rdc://` refs via `subst` →
///   apply the tgt overlay for the file's `(kind, tgt_slug)` → `write_atomic`.
/// - anything else (`.py`, …): copied verbatim.
///
/// `rel` is relative to the source env root; the destination is `remap_relative`
/// joined onto `tgt_root`.
#[allow(clippy::too_many_arguments)]
fn transform_file(
    rel: &Path,
    src_root: &Path,
    tgt_root: &Path,
    mapping: &Mapping,
    subst: &BTreeMap<String, String>,
    overlay: Option<&Overlay>,
    tgt_org_url: &str,
    migrate_score_thresholds: bool,
    src_lockfile: &crate::state::Lockfile,
    dry_run: bool,
) -> Result<FileOutcome> {
    let src_path = src_root.join(rel);
    let dst_rel = remap_relative(rel, mapping);
    let dst_path = tgt_root.join(&dst_rel);

    let is_json = rel
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if !is_json {
        // Shadow override: a file at <env>/overlay/<dst_rel> replaces the source
        // sidecar's content for this target env. `run` validates the overlay dir
        // up-front, so any shadow present here mirrors a real source sidecar.
        let shadow = tgt_root.join(crate::paths::OVERLAY_DIR).join(&dst_rel);
        let bytes = if shadow.is_file() {
            std::fs::read(&shadow)
                .with_context(|| format!("reading overlay shadow {}", shadow.display()))?
        } else {
            std::fs::read(&src_path).with_context(|| format!("reading {}", src_path.display()))?
        };
        return settle(&dst_path, &bytes, dry_run);
    }

    let raw =
        std::fs::read(&src_path).with_context(|| format!("reading {}", src_path.display()))?;
    let mut value: serde_json::Value = serde_json::from_slice(&raw)
        .with_context(|| format!("parsing JSON {}", src_path.display()))?;

    // Re-portabilize the source body against the SOURCE lockfile before slug
    // substitution. A source snapshot pulled before the portabilization fixes
    // (e.g. the webhooks→hooks endpoint mapping) can still carry raw
    // `https://<src-host>/…/<id>` URLs for tracked objects — most visibly a
    // queue's `webhooks` back-reference array, but also any nested ref that
    // escaped an older pull. Left as-is they pass through `subst` unchanged
    // (it only matches whole `rdc://` strings) and leak the source host into
    // the target, conflicting forever. Converting them here to
    // `rdc://<kind>/<src-slug>` lets the `subst` pass below remap them to the
    // target slug, so migrate output stays byte-identical to a fresh target
    // pull regardless of how stale the source snapshot is. Untracked kinds
    // (users, `generic_engines`, organization) are left as raw URLs — they are
    // not deployable refs and are reconciled/stripped separately. An empty
    // lockfile (tests) makes this a no-op.
    crate::snapshot::refs::portabilize_value(&mut value, src_lockfile);

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

    // `reconcile_target_identity` restores env-field values verbatim from the
    // TARGET snapshot, which may itself carry stale raw source-host URLs — most
    // visibly a queue's `webhooks` back-reference array (a reverse-ref field
    // pulled before the webhooks→hooks portabilization fix, or carried in by an
    // earlier leaky migrate). Those restored refs skipped the portabilize/subst
    // above (they were injected after it), so re-run both: portabilize converts
    // any source-host ref the source lockfile can resolve to `rdc://<src-slug>`,
    // and subst remaps that to `rdc://<tgt-slug>`. Idempotent for values already
    // in portable form, and a no-op for genuine target-host env refs (the source
    // lockfile can't resolve a different host), so legit env-specific fields
    // (e.g. a matched target's `modified_by`) are left intact.
    crate::snapshot::refs::portabilize_value(&mut value, src_lockfile);
    walk_strings_mut(&mut value, &mut |s| {
        if let Some(replacement) = subst.get(s.as_str()) {
            *s = replacement.clone();
        }
    });

    // Drop any residual SOURCE-HOST URL from the target-restored reverse-ref env
    // fields (e.g. a queue's `users` access list a contaminated target carried,
    // or `workflows`). These fields are reverse-membership lists stripped on
    // push, and the portabilize pass above already converted every ref the
    // source lockfile can resolve — so a leftover source-host entry is an
    // unresolvable, non-deployable ref that is meaningless in the target
    // (verified: the target env's queues carry no such users). Removing it keeps
    // the migrated snapshot free of the source host. Scoped to env fields via
    // `cross_env_body`, so deployable content (a hook's lookup `settings`, …) is
    // never touched here — an unresolvable source-host ref there is a source
    // data bug the user must fix, not something migrate may silently rewrite.
    if let Some((kind, _)) = classify(rel)
        && let Some(codec) = crate::snapshot::codec::codec(kind)
        && let Some(src_host) = url_host(&src_lockfile.api_base)
    {
        strip_source_host_env_refs(&mut value, codec, &src_host);
    }

    // Apply the tgt overlay for this object. A kind-wide default lives under the
    // reserved `"*"` slug (e.g. `[hooks."*"]`) and is applied FIRST; the
    // per-object entry (`[hooks.<slug>]`) is applied SECOND so it wins on any
    // shared key. Both run AFTER `reconcile_target_identity`, so an overlay value
    // overrides the object's reconciled/source content. Precedence:
    // per-object override > kind-wide `"*"` default > reconciled value.
    if let Some((kind, src_slug)) = classify(rel)
        && let Some(ov) = overlay
    {
        if let Some(defaults) = overlay_slug(ov, kind, "*") {
            apply_overrides(&mut value, defaults);
        }
        if let Some(overrides) = overlay_for(ov, mapping, kind, &src_slug) {
            apply_overrides(&mut value, overrides);
        }
    }

    // Reconcile per-org confidence thresholds unless the user opted to carry
    // them. `score_threshold` (per schema datapoint) and `default_score_threshold`
    // (per queue) are tuned per queue/organization and expected to differ across
    // envs, so by default a matched target keeps its own values and a brand-new
    // object drops them (falling back to the queue/server default). Runs after
    // overlay so an explicit overlay override still wins, and before the sort +
    // serialize below.
    if !migrate_score_thresholds
        && let Some((kind, _)) = classify(rel)
    {
        reconcile_score_thresholds(&mut value, kind, &dst_path);
    }

    // Reconcile the per-queue `training_enabled` flag. Engine auto-training is a
    // per-env policy (train in dev, not in a test clone) and Rossum resets the
    // flag to `false` on queue creation, so carrying the source's value would
    // make every migrate+sync conflict (source `true` vs deployed `false`). Like
    // the score thresholds, a matched target keeps its own value and a brand-new
    // queue drops the field. Unconditional (no flag): there is no case for
    // blindly propagating a training toggle across orgs.
    if let Some((kind, _)) = classify(rel) {
        reconcile_training_enabled(&mut value, kind, &dst_path);
    }

    // Trailing-whitespace normalization: Rossum strips trailing whitespace from
    // stored text (an email_template `message` posted as `…</p>\n` is returned
    // `…</p>`), so a freshly pulled snapshot never carries it. Trim here too, so
    // a migrated snapshot is byte-identical to a pulled one (`git diff` stays
    // clean) rather than reintroducing the source's trailing newline every run.
    // Mirrors `canonicalize_for_hash`, keeping the on-disk form and the hash in
    // agreement.
    crate::snapshot::noise::trim_trailing_whitespace(&mut value);

    // Canonicalize the order of every set-like reference array to stable,
    // env-independent order. The refs are now in portable `rdc://<slug>` form
    // (post-subst), and these arrays (`hook.run_after`/`queues`,
    // `workspace.queues`, `schema.queues`, `rule.queues`, `engine.training_queues`,
    // …) are unordered sets, so sorting the slugs makes migrate emit no spurious
    // reorder regardless of the source env's API ordering. This is the SAME
    // normalization the pull post-pass (`portabilize_refs`) applies, so a
    // migrated snapshot is byte-identical to one freshly pulled from the target.
    crate::snapshot::noise::sort_url_arrays(&mut value);

    let mut json = serde_json::to_vec_pretty(&value)?;
    json.push(b'\n');
    settle(&dst_path, &json, dry_run)
}

/// What migrating one file did (or, under `--dry-run`, would do) to the target
/// tree. The distinction is what makes the migrate summary describe the RUN
/// rather than the size of the snapshot: a target already holding these exact
/// bytes is [`FileOutcome::Unchanged`] and nothing is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FileOutcome {
    /// No file at the target path.
    Created,
    /// A file at the target path held different bytes.
    Updated,
    /// The target path already held exactly these bytes.
    Unchanged,
}

/// Classify `bytes` against whatever is already at `dst_path`, then write when
/// they differ (never under `dry_run`). A target that cannot be read for any
/// reason other than "missing" counts as [`FileOutcome::Updated`]: we cannot
/// prove it matches, so we write it and report a write — matching
/// [`crate::snapshot::writer::write_atomic`], which also treats an unreadable
/// target as "not known equal".
fn settle(dst_path: &Path, bytes: &[u8], dry_run: bool) -> Result<FileOutcome> {
    let outcome = match std::fs::read(dst_path) {
        Ok(existing) if existing == bytes => FileOutcome::Unchanged,
        Ok(_) => FileOutcome::Updated,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileOutcome::Created,
        Err(_) => FileOutcome::Updated,
    };
    if !dry_run && outcome != FileOutcome::Unchanged {
        crate::snapshot::writer::write_atomic(dst_path, bytes)?;
    }
    Ok(outcome)
}

/// What migrating an object's files did to it as a whole. Aggregated from the
/// per-file [`FileOutcome`]s by [`record_object_status`] so the summary counts
/// OBJECTS (what the follow-up `rdc sync <tgt>` will POST / PATCH / leave
/// alone), not files.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ObjStatus {
    /// The object's primary leaf was absent — `sync` will POST it.
    Created,
    /// The object existed and at least one of its files changed — `sync` will
    /// PATCH it.
    Updated,
    /// Every one of the object's files was already byte-identical — `sync` has
    /// nothing to do for it.
    Unchanged,
}

/// The object a snapshot file belongs to. Extends [`classify_for_selection`]
/// (which already maps a `.py`/`.js` sidecar and a queue formula to its owning
/// object) with the multi-file managed kinds that have no `classify` identity of
/// their own: every leaf under `mdh/<slug>/` or `workflows/<slug>/` belongs to
/// that slug's object. Used only to aggregate per-file outcomes — an object must
/// be reachable from ANY of its files, which is why this is deliberately wider
/// than [`classify_managed_primary`].
fn owning_object(rel: &Path) -> Option<(&'static str, String)> {
    if let Some(hit) = classify_for_selection(rel) {
        return Some(hit);
    }
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if comps.len() < 3 {
        return None;
    }
    match comps[0].as_str() {
        "mdh" => Some(("mdh", comps[1].clone())),
        "workflows" => Some(("workflows", comps[1].clone())),
        _ => None,
    }
}

/// Fold one file's outcome into its object's status. `dst_rel` is the file's
/// path in the TARGET tree (post-remap), so the tallies describe the target env.
///
/// `Created` is reserved for a missing PRIMARY leaf, because that is what
/// decides POST-vs-PATCH on the next sync: a fresh sidecar beside an existing
/// primary is an update to an object the env already has. `Created` is never
/// downgraded once set.
fn record_object_status(
    acc: &mut BTreeMap<(&'static str, String), ObjStatus>,
    dst_rel: &Path,
    outcome: FileOutcome,
) {
    let Some(key) = owning_object(dst_rel) else {
        return;
    };
    let is_primary = classify(dst_rel).is_some() || classify_managed_primary(dst_rel).is_some();
    let slot = acc.entry(key).or_insert(ObjStatus::Unchanged);
    if *slot == ObjStatus::Created {
        return;
    }
    *slot = match (is_primary, outcome) {
        (true, FileOutcome::Created) => ObjStatus::Created,
        (_, FileOutcome::Unchanged) => *slot,
        _ => ObjStatus::Updated,
    };
}

/// Directories inside the target's [`MANAGED_DIRS`] subtrees that hold no file
/// at any depth once `pruned` is gone. Returned deepest-first (post-order), the
/// order `remove_dir` needs.
///
/// A directory survives if ANY file beneath it is not being pruned — including
/// files rdc does not manage (an old sync shadow, an editor leftover, a
/// `.DS_Store`). Those are the user's, so their directory stays. Symlinks count
/// as files here (`DirEntry::file_type` does not follow them), which keeps a
/// symlinked directory from being followed or removed. An unreadable directory
/// is assumed to be in use and left alone.
fn emptied_dirs(tgt_root: &Path, pruned: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    /// Returns whether `dir` still holds a surviving file, pushing every
    /// fully-emptied directory (children before parents) onto `out`.
    fn walk(base: &Path, dir: &Path, pruned: &BTreeSet<PathBuf>, out: &mut Vec<PathBuf>) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return true;
        };
        let mut survives = false;
        let mut children = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => children.push(path),
                Ok(_) => {
                    let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
                    if !pruned.contains(&rel) {
                        survives = true;
                    }
                }
                // Unknown kind — assume it is a file that must be kept.
                Err(_) => survives = true,
            }
        }
        children.sort();
        for child in children {
            if walk(base, &child, pruned, out) {
                survives = true;
            }
        }
        if !survives {
            out.push(dir.strip_prefix(base).unwrap_or(dir).to_path_buf());
        }
        survives
    }

    let mut out = Vec::new();
    for dir in MANAGED_DIRS {
        let managed = tgt_root.join(dir);
        if managed.exists() {
            walk(tgt_root, &managed, pruned, &mut out);
        }
    }
    out
}

/// The bare `host[:port]` of an API base URL, e.g.
/// `https://org-dev.rossum.app/api/v1` -> `org-dev.rossum.app`. Returns `None`
/// for a malformed/empty base (an empty lockfile in tests), so callers skip
/// source-host handling rather than match everything.
fn url_host(api_base: &str) -> Option<String> {
    let (_scheme, rest) = api_base.split_once("://")?;
    rest.split('/')
        .next()
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
}

/// Strip SOURCE-HOST references from a value's reverse-ref / server-assigned
/// ENV fields — the top-level keys `cross_env_body` removes (`users`,
/// `workflows`, `webhooks`, … and an inbox's `email` for the relevant kinds).
/// Called after portabilization, so any value still carrying the source host is
/// an unresolvable, non-deployable leftover a contaminated target snapshot
/// restored; the source host is meaningless in the target, so we clean it:
///   - array field: drop the entries that reference the source host;
///   - string field (e.g. `email`): drop the field entirely, so `rdc sync`
///     re-reads the target's server-assigned value.
///
/// Matches the bare host as a substring, covering both URLs
/// (`https://<host>/…`) and emails (`<local>@<host>`). Scoped to env fields via
/// `cross_env_body`, so deployable content (a hook's lookup `settings`, …) is
/// never touched here — an unresolvable source-host ref there is a source data
/// bug the user must fix, not something migrate may silently rewrite.
fn strip_source_host_env_refs(
    value: &mut serde_json::Value,
    codec: &'static dyn crate::snapshot::codec::KindCodec,
    src_host: &str,
) {
    if !value.is_object() {
        return;
    }
    // Env fields = top-level keys present in the full body but removed by
    // `cross_env_body` (probe on a clone so `value` is untouched). Same
    // derivation `reconcile_target_identity` uses.
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
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for field in env_fields {
        let drop_field = match obj.get_mut(&field) {
            Some(serde_json::Value::Array(arr)) => {
                arr.retain(
                    |v| !matches!(v, serde_json::Value::String(s) if s.contains(src_host)),
                );
                false
            }
            Some(serde_json::Value::String(s)) => s.contains(src_host),
            _ => false,
        };
        if drop_field {
            // shift_remove (not swap_remove) so removing a field preserves the
            // order of the surviving keys — otherwise the on-disk key order
            // depends on which fields were dropped, and migrate's output flips
            // between the create and update paths (non-deterministic bytes).
            obj.shift_remove(&field);
        }
    }
}

/// Reconcile per-org confidence thresholds so migrate does not carry them from
/// the source env (see the module-level flag `--migrate-score-thresholds`).
///
/// The affected keys are `score_threshold` (on each schema datapoint, inside
/// `content`) and `default_score_threshold` (on a queue). Both are tuned per
/// queue/organization and expected to differ across envs. The rule mirrors
/// [`reconcile_target_identity`] but reaches the two threshold fields wherever
/// they sit:
///
/// - **Matched target** (`tgt_path` exists): the migrated object adopts the
///   TARGET's threshold. For schemas, datapoints are matched by their stable
///   `id`; for queues the single default is matched by key name (so its exact
///   nesting — top-level or under `settings` — does not matter). Where the
///   target has no threshold for a given field, the migrated field is dropped.
/// - **New target** (no `tgt_path`): every threshold is dropped so the field
///   falls back to the queue/server default.
///
/// A no-op for any kind other than `schemas` / `queues`.
fn reconcile_score_thresholds(value: &mut serde_json::Value, kind: &str, tgt_path: &Path) {
    // The target snapshot (if it exists) is the source of truth for thresholds.
    // Absent/unparseable => brand-new object => `None` => every threshold drops.
    let target: Option<serde_json::Value> = std::fs::read(tgt_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());

    match kind {
        "schemas" => {
            // Target's `datapoint id -> score_threshold` (only ids that carry
            // one). Empty for a new target, so every source threshold drops.
            let target_thresholds = target
                .as_ref()
                .map(collect_datapoint_thresholds)
                .unwrap_or_default();
            apply_datapoint_thresholds(value, &target_thresholds);
        }
        "queues" => {
            const KEY: &str = "default_score_threshold";
            match target.as_ref().and_then(|t| find_key_value(t, KEY)) {
                // Matched target carries a default: adopt it. Update in place if
                // the migrated body already has the key; otherwise mirror the
                // target's placement (under `settings` or top-level) so the
                // target org keeps its value.
                Some(v) => {
                    if !set_existing_key(value, KEY, &v) {
                        let under_settings = target
                            .as_ref()
                            .and_then(|t| t.get("settings"))
                            .and_then(|s| s.get(KEY))
                            .is_some();
                        if let Some(obj) = value.as_object_mut() {
                            if under_settings {
                                obj.entry("settings")
                                    .or_insert_with(|| serde_json::json!({}))
                                    .as_object_mut()
                                    .map(|s| s.insert(KEY.to_string(), v));
                            } else {
                                obj.insert(KEY.to_string(), v);
                            }
                        }
                    }
                }
                // New target, or target has no default: drop it everywhere so it
                // falls back to the queue/server default.
                None => remove_key_everywhere(value, KEY),
            }
        }
        _ => {}
    }
}

/// Reconcile the per-queue `training_enabled` flag so migrate+sync is stable.
///
/// Engine auto-training is a per-env policy (you train the model in dev, not in
/// a throwaway test clone), and Rossum resets `training_enabled` to `false` when
/// a queue is created. Carrying the source's value therefore makes every
/// migrate+sync conflict — the migrated queue says `true`, the deployed queue is
/// `false`, and neither side ever converges. Following the same rule as
/// [`reconcile_score_thresholds`]:
///
/// - **Matched target** (`tgt_path` exists + carries the flag): adopt the
///   TARGET's value, so each env keeps its own training policy.
/// - **New target** (or the target lacks the flag): drop it, so Rossum's
///   create-time default (`false`) applies and the round-trip is stable.
///
/// A no-op for any kind other than `queues`.
fn reconcile_training_enabled(value: &mut serde_json::Value, kind: &str, tgt_path: &Path) {
    const KEY: &str = "training_enabled";
    if kind != "queues" {
        return;
    }
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if !obj.contains_key(KEY) {
        return;
    }
    let target: Option<serde_json::Value> = std::fs::read(tgt_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    match target.as_ref().and_then(|t| t.get(KEY)).cloned() {
        Some(v) => {
            obj.insert(KEY.to_string(), v);
        }
        None => {
            obj.shift_remove(KEY);
        }
    }
}

/// Collect `id -> score_threshold` for every object that has BOTH a string `id`
/// and a `score_threshold`, anywhere in the tree. Schema datapoint ids are
/// unique within a schema, so this is an unambiguous per-field map.
fn collect_datapoint_thresholds(
    value: &serde_json::Value,
) -> BTreeMap<String, serde_json::Value> {
    let mut out = BTreeMap::new();
    fn walk(value: &serde_json::Value, out: &mut BTreeMap<String, serde_json::Value>) {
        match value {
            serde_json::Value::Object(map) => {
                if let (Some(serde_json::Value::String(id)), Some(th)) =
                    (map.get("id"), map.get("score_threshold"))
                {
                    out.insert(id.clone(), th.clone());
                }
                for v in map.values() {
                    walk(v, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    walk(v, out);
                }
            }
            _ => {}
        }
    }
    walk(value, &mut out);
    out
}

/// For every object carrying a string `id`, adopt the target's threshold for
/// that id (adding or overwriting `score_threshold`), or remove any existing
/// `score_threshold` when the target has none for that id. Objects without an
/// `id` are left alone but still recursed into.
fn apply_datapoint_thresholds(
    value: &mut serde_json::Value,
    target: &BTreeMap<String, serde_json::Value>,
) {
    match value {
        serde_json::Value::Object(map) => {
            let id = match map.get("id") {
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            if let Some(id) = id {
                match target.get(&id) {
                    Some(th) => {
                        map.insert("score_threshold".to_string(), th.clone());
                    }
                    None => {
                        map.shift_remove("score_threshold");
                    }
                }
            }
            for v in map.values_mut() {
                apply_datapoint_thresholds(v, target);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                apply_datapoint_thresholds(v, target);
            }
        }
        _ => {}
    }
}

/// Recursively return a clone of the first value stored under `key` anywhere in
/// `value` (depth-first). Used to read a scalar like `default_score_threshold`
/// regardless of whether it sits top-level or nested (e.g. under `settings`).
fn find_key_value(value: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map
            .get(key)
            .cloned()
            .or_else(|| map.values().find_map(|v| find_key_value(v, key))),
        serde_json::Value::Array(arr) => arr.iter().find_map(|v| find_key_value(v, key)),
        _ => None,
    }
}

/// Set `key` to `new` in the first object that ALREADY contains it (depth-first).
/// Returns `true` if an existing key was updated, `false` if `key` was not found.
fn set_existing_key(value: &mut serde_json::Value, key: &str, new: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key(key) {
                map.insert(key.to_string(), new.clone());
                return true;
            }
            map.values_mut().any(|v| set_existing_key(v, key, new))
        }
        serde_json::Value::Array(arr) => arr.iter_mut().any(|v| set_existing_key(v, key, new)),
        _ => false,
    }
}

/// Remove `key` from every object in the tree (depth-first).
fn remove_key_everywhere(value: &mut serde_json::Value, key: &str) {
    match value {
        serde_json::Value::Object(map) => {
            map.shift_remove(key);
            for v in map.values_mut() {
                remove_key_everywhere(v, key);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                remove_key_everywhere(v, key);
            }
        }
        _ => {}
    }
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
                        obj.shift_remove(&field);
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
                obj.shift_remove(*field);
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

/// Whether `env_root` holds ANY managed leaf, short-circuiting on the first one
/// found. The migrate summary only needs the boolean, so this avoids
/// `enumerate_files`'s full walk + collect + sort. Uses the same managed-leaf
/// definition (`should_skip` + `is_managed_leaf`, skipping `__pycache__`) so the
/// two agree on what "empty" means. A read error (e.g. a vanished dir) is
/// treated as "no file here" — this is an advisory hint, never a gate.
fn env_tree_has_managed_file(env_root: &Path, env: &str) -> bool {
    fn any_managed(dir: &Path, env: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if name == "__pycache__" {
                    continue;
                }
                if any_managed(&entry.path(), env) {
                    return true;
                }
            } else if !should_skip(&name, env) && is_managed_leaf(&name) {
                return true;
            }
        }
        false
    }
    MANAGED_DIRS.iter().any(|dir| {
        let managed = env_root.join(dir);
        managed.exists() && any_managed(&managed, env)
    })
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
/// relative to the *target* env root. `skip` holds source-relative paths the
/// migration refuses to produce (un-creatable unique-typed email-template
/// duplicates) — their target counterparts count as NOT produced, so a stale
/// copy left by an earlier migrate gets pruned.
fn mirror_prune_paths(
    src_root: &Path,
    src_env: &str,
    tgt_root: &Path,
    tgt_env: &str,
    mapping: &Mapping,
    skip: &std::collections::BTreeSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    use std::collections::BTreeSet;
    let produced: BTreeSet<PathBuf> = enumerate_files(src_root, src_env)?
        .into_iter()
        .filter(|rel| !skip.contains(rel))
        .map(|rel| remap_relative(&rel, mapping))
        .collect();
    let existing = enumerate_files(tgt_root, tgt_env)?;
    Ok(existing
        .into_iter()
        .filter(|rel| !produced.contains(rel))
        .collect())
}

/// Email-template `type`s the Rossum API enforces as ONE-per-queue. Creating
/// a second template of such a type on a queue fails with `400 Cannot create
/// template with unique type: <type>`. (Historical duplicates can still exist
/// server-side — created before the constraint — which is exactly what makes
/// a source snapshot carry them.)
const UNIQUE_EMAIL_TEMPLATE_TYPES: [&str; 2] =
    ["rejection_default", "email_with_no_processable_attachments"];

/// Source-relative email-template paths that `migrate` must NOT produce:
/// members of a (target queue, unique type) group of size > 1 that the target
/// cannot hold. Per group: keep every member whose migrated slug has a REAL
/// remote identity in the target lockfile (it exists on the target env —
/// patchable); when none has one, keep the single lowest-source-id member
/// (exactly one fresh create is admissible; the choice is deterministic).
/// Everything else in the group is skipped. Groups of size 1 and non-unique
/// types (e.g. `custom`) are never touched.
fn unique_template_skips(
    files: &[PathBuf],
    src_root: &Path,
    mapping: &Mapping,
    tgt_lockfile: &crate::state::Lockfile,
) -> std::collections::BTreeSet<PathBuf> {
    use std::collections::{BTreeMap, BTreeSet};

    /// One template in a (target queue, unique type) group:
    /// (source rel path, target lockfile key, source id).
    type Member = (PathBuf, String, u64);
    let mut groups: BTreeMap<(PathBuf, String), Vec<Member>> = BTreeMap::new();
    for rel in files {
        let comps: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        // workspaces/<ws>/queues/<q>/email-templates/<t>.json
        let is_template = comps.len() == 6
            && comps[0] == "workspaces"
            && comps[2] == "queues"
            && comps[4] == "email-templates"
            && comps[5].ends_with(".json");
        if !is_template {
            continue;
        }
        let Ok(raw) = std::fs::read(src_root.join(rel)) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(ty) = v.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        if !UNIQUE_EMAIL_TEMPLATE_TYPES.contains(&ty) {
            continue;
        }
        let dst = remap_relative(rel, mapping);
        let dst_comps: Vec<String> = dst
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if dst_comps.len() != 6 {
            continue;
        }
        let stem = dst_comps[5].trim_end_matches(".json");
        let lockfile_key = format!("{}/{}/{stem}", dst_comps[1], dst_comps[3]);
        let id = v.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
        groups
            .entry((dst.parent().expect("template path has a parent").to_path_buf(), ty.to_string()))
            .or_default()
            .push((rel.clone(), lockfile_key, id));
    }

    let mut skips: BTreeSet<PathBuf> = BTreeSet::new();
    for ((_queue_dir, _ty), mut members) in groups {
        if members.len() < 2 {
            continue;
        }
        let backed: Vec<bool> = members
            .iter()
            .map(|(_, key, _)| {
                tgt_lockfile
                    .objects
                    .get("email_templates")
                    .and_then(|m| m.get(key))
                    .map(|e| e.id != 0)
                    .unwrap_or(false)
            })
            .collect();
        if backed.iter().any(|b| *b) {
            // The target already holds these — keep exactly those, skip the
            // rest (no fresh create can succeed while the type is occupied).
            for (i, (rel, _, _)) in members.iter().enumerate() {
                if !backed[i] {
                    skips.insert(rel.clone());
                }
            }
        } else {
            // Fresh target: exactly one create is admissible — lowest id wins.
            members.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
            for (rel, _, _) in members.into_iter().skip(1) {
                skips.insert(rel);
            }
        }
    }
    skips
}

/// `rdc migrate <src> <tgt>` — pure-local snapshot→snapshot transform.
///
/// Copies `envs/<src>/` into `envs/<tgt>/`, renaming slugs per the hand-authored,
/// oriented [`Mapping`] (projected from `.rdc/mapping.toml`), substituting
/// `rdc://<kind>/<src>` refs to their `<tgt>` slug in JSON content, and applying
/// the target overlay. Makes zero remote calls — afterward the user reviews
/// `git diff` and runs `rdc sync <tgt>`.
///
/// `--mirror` additionally deletes target-only objects (files present in tgt
/// but not produced by the migration). `--dry-run` prints the plan and writes
/// nothing. `--only <selector>` narrows the operation to matching
/// `<kind>/<slug>` objects (reusing deploy's selection machinery).
pub fn run(
    src: &str,
    tgt: &str,
    mirror: bool,
    dry_run: bool,
    only: Vec<String>,
    migrate_score_thresholds: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    run_at(&cwd, src, tgt, mirror, dry_run, only, migrate_score_thresholds)
}

/// Like [`run`], but takes the project root explicitly instead of reading
/// the process's current directory — the embedding seam non-CLI consumers
/// (e.g. the desktop app's promote flow) use to drive migrate without a
/// `std::env::set_current_dir` dance.
pub fn run_at(
    cwd: &Path,
    src: &str,
    tgt: &str,
    mirror: bool,
    dry_run: bool,
    only: Vec<String>,
    migrate_score_thresholds: bool,
) -> Result<()> {
    if src == tgt {
        anyhow::bail!(
            "src and tgt envs are the same ('{src}'). Use two different envs for `rdc migrate`."
        );
    }

    let src_paths = crate::paths::Paths::for_env(cwd, src);
    let tgt_paths = crate::paths::Paths::for_env(cwd, tgt);
    let src_root = src_paths.env_root();
    let tgt_root = tgt_paths.env_root();

    if !src_root.exists() {
        anyhow::bail!(
            "source snapshot {} does not exist — pull '{src}' first (`rdc sync {src}`)",
            src_root.display()
        );
    }

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());

    let project_cfg = crate::config::ProjectConfig::load(&cwd.join("rdc.toml"))?;
    let known_envs: std::collections::BTreeSet<String> =
        project_cfg.envs.keys().cloned().collect();

    // Slug map: load the generic .rdc/mapping.toml (converting legacy per-pair
    // files once if needed), validating it — logging warnings for unknown-env
    // rows and hard-erroring on ambiguous (env,slug) duplicates — and orient
    // onto this direction. `load_or_migrate_mapping` validates on both the
    // existing-file path and the just-converted path, so no second validate
    // call is needed here.
    let generic = load_or_migrate_mapping(&src_paths, &known_envs, dry_run, &log)?;
    let mapping = generic.orient(src, tgt);

    // Optional `--only` selection filter (reuses deploy's pure-fs machinery).
    let selection = crate::cli::deploy::selection::resolve(&only, &src_paths, &tgt_paths)?;

    let subst = build_subst(&mapping);
    // Source lockfile: used to re-portabilize any stale raw URLs in the source
    // snapshot (see `transform_file`). Missing/unparseable → empty (no-op), so
    // migrate still runs; it just can't recover raw URLs the source carries.
    let src_lockfile = crate::state::Lockfile::load(&src_paths.lockfile()).unwrap_or_default();
    let tgt_overlay = Overlay::load(&tgt_paths.overlay_file()).with_context(|| {
        format!(
            "loading tgt overlay from {}",
            tgt_paths.overlay_file().display()
        )
    })?;

    // The target env's organization URL, used to set `organization` on objects
    // that are NEW in tgt (a cross-env create would otherwise carry the source
    // env's org). Matched objects take their org from the existing tgt file.
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

    // Guardrail: warn (don't abort) when an oriented mapping row's SOURCE slug
    // has no matching object on disk in '<src>'. This restores the check the
    // old (now-removed) `stale_mapping_sources` provided: a hand-edited
    // mapping.toml row can go stale after the object it names is renamed or
    // deleted, and without a warning the rename simply and silently never
    // applies — indistinguishable from a typo.
    let src_object_keys: BTreeSet<(&'static str, String)> =
        files.iter().filter_map(|rel| classify(rel)).collect();
    for kind in GenericMapping::KINDS {
        let Some(map) = mapping.kind_map(kind) else {
            continue;
        };
        for src_slug in map.keys() {
            if !src_object_keys.contains(&(kind, src_slug.clone())) {
                log.event(
                    crate::log::Action::Warn,
                    &format!(
                        "mapping entry {kind}/{src_slug} has no matching object in \
                         '{src}' (likely a typo or a stale row) — this rename won't apply"
                    ),
                );
            }
        }
    }

    // Only the boolean is needed, so probe with an early-exit scan instead of
    // `enumerate_files(..).is_empty()`, which walks + collects + sorts the whole
    // target tree just to discard it.
    let tgt_was_empty = !tgt_root.exists() || !env_tree_has_managed_file(&tgt_root, tgt);

    // Un-creatable duplicate unique-typed email templates: when the SOURCE
    // queue holds more than one template of a per-queue-unique type, the
    // target can only hold the copies it already has (plus at most one fresh
    // create) — the Rossum API refuses the rest with `400 Cannot create
    // template with unique type`. Producing them would make every subsequent
    // `rdc sync <tgt>` retry the doomed POST forever. Skip them here instead
    // (and let `--mirror` prune stale copies of them from the target tree).
    let tgt_lockfile = crate::state::Lockfile::load(&tgt_paths.lockfile()).unwrap_or_default();
    let unique_tpl_skips = unique_template_skips(&files, &src_root, &mapping, &tgt_lockfile);
    if !unique_tpl_skips.is_empty() {
        let listing: Vec<String> = unique_tpl_skips
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect();
        log.event(
            crate::log::Action::Info,
            &format!(
                "skipping {} email template(s) un-creatable on '{tgt}' (another template of \
                 the same unique type already occupies the target queue):\n{}",
                unique_tpl_skips.len(),
                listing.join("\n"),
            ),
        );
    }

    // Validate the target env's `overlay/` shadow dir before writing anything:
    // every shadow must mirror a sidecar this migration produces. Built from the
    // FULL source enumeration (not the `--only` subset), so scoping with `--only`
    // never falsely flags a valid shadow it simply did not apply this run.
    let produced_sidecars: std::collections::BTreeSet<PathBuf> = files
        .iter()
        .filter(|rel| is_sidecar(rel))
        .map(|rel| remap_relative(rel, &mapping))
        .collect();
    validate_overlay_dir(&tgt_paths.overlay_dir(), &produced_sidecars)?;

    // Validate `overlay.toml` KEYS the same way the shadow dir is validated: every
    // non-`"*"` key must target an object this migration produces, else its per-env
    // override would be silently ignored — letting the source value promote
    // unchanged (dev SFTP creds into test, a missing `active=false`, a stale URL).
    // Built from the FULL enumeration (independent of `--only`, like the shadow
    // check) so scoping never false-flags a valid key. Runs before any write.
    if let Some(ov) = tgt_overlay.as_ref() {
        let mut produced_slugs: BTreeMap<&'static str, std::collections::BTreeSet<String>> =
            BTreeMap::new();
        for rel in &files {
            if let Some((kind, src_slug)) = classify(rel) {
                produced_slugs
                    .entry(kind)
                    .or_default()
                    .insert(tgt_slug(&mapping, kind, &src_slug));
            }
        }
        let dangling = dangling_overlay_keys(ov, &produced_slugs);
        if !dangling.is_empty() {
            anyhow::bail!(format_dangling_overlay_error(&dangling, src, tgt));
        }
    }

    // `considered` is every file the migration looked at; `written` is the
    // subset whose bytes actually changed on disk. `obj_status` aggregates the
    // per-file outcomes into per-object create / update / unchanged tallies.
    let mut considered = 0usize;
    let mut written = 0usize;
    let mut renamed = 0usize;
    let mut obj_status: BTreeMap<(&'static str, String), ObjStatus> = BTreeMap::new();

    for rel in &files {
        // Un-creatable duplicate unique-typed email templates (see above).
        if unique_tpl_skips.contains(rel) {
            continue;
        }
        // `--only`: keep a file only when its classified (kind, slug) is in the
        // selection. Files with no classifiable object (workflows, mdh) are
        // skipped under an active selection — the user narrowed scope.
        if let Some(sel) = &selection {
            match classify_for_selection(rel) {
                Some((kind, slug)) if sel.contains(kind, &slug) => {}
                _ => continue,
            }
        }

        let dst_rel = remap_relative(rel, &mapping);
        if &dst_rel != rel {
            renamed += 1;
        }
        considered += 1;

        if dry_run {
            log.event(
                crate::log::Action::Plan,
                &format!("{} -> {}", rel.display(), dst_rel.display()),
            );
        }
        // Runs in BOTH modes: the transform is what reveals whether the target
        // would actually change, so `--dry-run` can only forecast the same
        // counts the real run reports by performing it (minus the write).
        let outcome = transform_file(
            rel,
            &src_root,
            &tgt_root,
            &mapping,
            &subst,
            tgt_overlay.as_ref(),
            &tgt_org_url,
            migrate_score_thresholds,
            &src_lockfile,
            dry_run,
        )
        .with_context(|| format!("migrating {}", rel.display()))?;
        if outcome != FileOutcome::Unchanged {
            written += 1;
        }
        record_object_status(&mut obj_status, &dst_rel, outcome);
    }
    let creates = obj_status
        .values()
        .filter(|s| **s == ObjStatus::Created)
        .count();
    let updates = obj_status
        .values()
        .filter(|s| **s == ObjStatus::Updated)
        .count();
    let unchanged = obj_status
        .values()
        .filter(|s| **s == ObjStatus::Unchanged)
        .count();

    // `--mirror`: prune target-only objects.
    let mut pruned = 0usize;
    if mirror {
        let prune =
            mirror_prune_paths(&src_root, src, &tgt_root, tgt, &mapping, &unique_tpl_skips)?;
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
        // A pruned object's directory must go too, or the target keeps a phantom
        // queue / dataset / engine folder forever. git cannot represent an empty
        // directory, so this is the one part of the migration the `git diff`
        // review below can never show — hence a log line per directory, in both
        // modes. A failure here is reported but never aborts: the migration's
        // real work is already on disk and correct.
        let prune_set: BTreeSet<PathBuf> = prune.iter().cloned().collect();
        for rel in emptied_dirs(&tgt_root, &prune_set) {
            let abs = tgt_root.join(&rel);
            if !dry_run
                && let Err(e) = std::fs::remove_dir(&abs)
            {
                log.event(
                    crate::log::Action::Warn,
                    &format!("could not remove empty dir {}: {e}", rel.display()),
                );
                continue;
            }
            log.event(
                crate::log::Action::Delete,
                &format!("prune dir {}", rel.display()),
            );
        }
    }

    let verb = if dry_run { "would migrate" } else { "migrated" };
    log.event(
        crate::log::Action::Done,
        &format!(
            "{verb} envs/{src} -> envs/{tgt}: {written} of {considered} file(s) changed \
             ({renamed} renamed, {pruned} pruned)"
        ),
    );
    log.event(
        crate::log::Action::Info,
        &format!("-> {tgt}: {creates} create, {updates} update, {unchanged} unchanged"),
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
    use std::collections::BTreeSet;

    /// Proves `run_at` takes its project root from the `cwd` PARAMETER, not
    /// `std::env::current_dir()` — the embedding seam the desktop app relies
    /// on to drive migrate without a `std::env::set_current_dir` dance (which
    /// would be unsound to do concurrently from a GUI app). Builds a tiny
    /// two-env project in a tempdir unrelated to the process cwd, and asserts
    /// both that the dry-run succeeds and that the process cwd never moved.
    #[test]
    fn run_at_uses_explicit_cwd_not_process_current_dir() {
        let before = std::env::current_dir().unwrap();

        let project = tempfile::TempDir::new().unwrap();
        let root = project.path();
        let mut envs = BTreeMap::new();
        envs.insert(
            "dev".to_string(),
            crate::config::EnvConfig {
                api_base: "https://dev.example/api/v1".to_string(),
                org_id: 1,
            },
        );
        envs.insert(
            "prod".to_string(),
            crate::config::EnvConfig {
                api_base: "https://prod.example/api/v1".to_string(),
                org_id: 2,
            },
        );
        crate::config::ProjectConfig { envs }
            .save(&root.join("rdc.toml"))
            .unwrap();
        // Minimal pulled source snapshot: an empty managed dir is enough for
        // `run_at` to consider 'dev' present (no writes happen under dry_run
        // regardless).
        std::fs::create_dir_all(root.join("envs/dev/workspaces")).unwrap();

        let result = run_at(root, "dev", "prod", false, true /* dry_run */, vec![], false);

        assert!(result.is_ok(), "run_at should succeed: {result:?}");
        assert_eq!(
            std::env::current_dir().unwrap(),
            before,
            "run_at must not change the process cwd"
        );
    }

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
    fn load_or_migrate_mapping_deletes_only_converted_legacy_files() {
        use crate::mapping::Mapping;
        let dir = tempfile::TempDir::new().unwrap();
        let paths = crate::paths::Paths::for_env(dir.path(), "dev");
        std::fs::create_dir_all(paths.mapping_dir()).unwrap();

        // Known pair (dev/test both in rdc.toml) -> parsed, converted, and
        // its legacy file deleted.
        let known_path = paths.mapping_dir().join("dev-to-test.toml");
        let mut known_mapping = Mapping::default();
        known_mapping.hooks.insert("a".to_string(), "b".to_string());
        known_mapping.save(&known_path).unwrap();

        // Unknown pair ("prod" is not in rdc.toml) -> skipped, and its
        // legacy file MUST survive on disk (never parsed as TOML either).
        let unknown_path = paths.mapping_dir().join("dev-to-prod.toml");
        std::fs::write(&unknown_path, b"not valid mapping toml, never parsed").unwrap();

        let known_envs: BTreeSet<String> = BTreeSet::from(["dev".to_string(), "test".to_string()]);
        let log = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);

        let generic = load_or_migrate_mapping(&paths, &known_envs, false, &log).unwrap();
        assert!(!generic.is_empty(), "the known-pair edge should have been converted");

        assert!(
            !known_path.exists(),
            "the converted legacy file must be deleted after a non-dry-run migration"
        );
        assert!(
            unknown_path.exists(),
            "a legacy file skipped for an unknown env pair must NOT be deleted (data loss)"
        );
    }

    #[test]
    fn owning_object_maps_sidecars_and_multifile_kinds_to_their_object() {
        // Primary leaves resolve to themselves.
        assert_eq!(
            owning_object(Path::new("hooks/extractor.json")),
            Some(("hooks", "extractor".to_string()))
        );
        // A code sidecar resolves to the hook that owns it, not to nothing —
        // this is what lets a sidecar-only edit count as one object update.
        assert_eq!(
            owning_object(Path::new("hooks/extractor.py")),
            Some(("hooks", "extractor".to_string()))
        );
        // Both leaves of a multi-file MDH dataset resolve to the same object,
        // so a `collection.json`-only change is still attributed.
        assert_eq!(
            owning_object(Path::new("mdh/vendors/indexes.json")),
            Some(("mdh", "vendors".to_string()))
        );
        assert_eq!(
            owning_object(Path::new("mdh/vendors/collection.json")),
            Some(("mdh", "vendors".to_string()))
        );
        // Env-level files belong to no object.
        assert_eq!(owning_object(Path::new("organization.json")), None);
    }

    #[test]
    fn object_status_is_created_only_when_the_primary_leaf_is_new() {
        let mut acc = BTreeMap::new();
        // A brand-new hook: primary missing => the object is a create, and the
        // sidecar that follows must not downgrade it to an update.
        record_object_status(&mut acc, Path::new("hooks/new.json"), FileOutcome::Created);
        record_object_status(&mut acc, Path::new("hooks/new.py"), FileOutcome::Created);
        assert_eq!(acc[&("hooks", "new".to_string())], ObjStatus::Created);

        // An existing hook whose JSON is untouched but whose sidecar changed is
        // an UPDATE — sync will PATCH it, not POST it.
        let mut acc = BTreeMap::new();
        record_object_status(
            &mut acc,
            Path::new("hooks/old.json"),
            FileOutcome::Unchanged,
        );
        record_object_status(&mut acc, Path::new("hooks/old.py"), FileOutcome::Updated);
        assert_eq!(acc[&("hooks", "old".to_string())], ObjStatus::Updated);

        // A brand-new sidecar beside an existing primary is still an update:
        // the env already has the object.
        let mut acc = BTreeMap::new();
        record_object_status(
            &mut acc,
            Path::new("hooks/old.json"),
            FileOutcome::Unchanged,
        );
        record_object_status(&mut acc, Path::new("hooks/old.py"), FileOutcome::Created);
        assert_eq!(acc[&("hooks", "old".to_string())], ObjStatus::Updated);

        // Every file byte-identical => unchanged.
        let mut acc = BTreeMap::new();
        record_object_status(
            &mut acc,
            Path::new("hooks/same.json"),
            FileOutcome::Unchanged,
        );
        record_object_status(&mut acc, Path::new("hooks/same.py"), FileOutcome::Unchanged);
        assert_eq!(acc[&("hooks", "same".to_string())], ObjStatus::Unchanged);
    }

    #[test]
    fn emptied_dirs_reports_deepest_first_and_spares_surviving_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let tgt = dir.path();
        // `mdh/gone/` loses both its files => it and nothing above it is empty.
        std::fs::create_dir_all(tgt.join("mdh/gone")).unwrap();
        std::fs::write(tgt.join("mdh/gone/indexes.json"), b"[]").unwrap();
        std::fs::write(tgt.join("mdh/gone/collection.json"), b"{}").unwrap();
        // `mdh/stays/` keeps a file, so `mdh/` itself must survive.
        std::fs::create_dir_all(tgt.join("mdh/stays")).unwrap();
        std::fs::write(tgt.join("mdh/stays/indexes.json"), b"[]").unwrap();

        let pruned: BTreeSet<PathBuf> = [
            PathBuf::from("mdh/gone/indexes.json"),
            PathBuf::from("mdh/gone/collection.json"),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            emptied_dirs(tgt, &pruned),
            vec![PathBuf::from("mdh/gone")],
            "only the fully-emptied dataset dir is reported"
        );
    }

    #[test]
    fn emptied_dirs_walks_up_to_the_managed_root_when_all_of_it_goes() {
        let dir = tempfile::TempDir::new().unwrap();
        let tgt = dir.path();
        std::fs::create_dir_all(tgt.join("workspaces/ws/queues/q")).unwrap();
        std::fs::write(tgt.join("workspaces/ws/queues/q/queue.json"), b"{}").unwrap();
        // An env-level file outside MANAGED_DIRS must not keep the tree alive.
        std::fs::write(tgt.join("organization.json"), b"{}").unwrap();

        let pruned: BTreeSet<PathBuf> =
            [PathBuf::from("workspaces/ws/queues/q/queue.json")].into_iter().collect();

        assert_eq!(
            emptied_dirs(tgt, &pruned),
            vec![
                PathBuf::from("workspaces/ws/queues/q"),
                PathBuf::from("workspaces/ws/queues"),
                PathBuf::from("workspaces/ws"),
                PathBuf::from("workspaces"),
            ],
            "deepest-first, so remove_dir always sees an empty dir"
        );
    }

    #[test]
    fn emptied_dirs_keeps_a_dir_holding_an_unmanaged_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let tgt = dir.path();
        std::fs::create_dir_all(tgt.join("mdh/gone")).unwrap();
        std::fs::write(tgt.join("mdh/gone/indexes.json"), b"[]").unwrap();
        // A foreign leaf rdc never manages — the dir is not ours to remove.
        std::fs::write(tgt.join("mdh/gone/notes.txt"), b"keep").unwrap();

        let pruned: BTreeSet<PathBuf> =
            [PathBuf::from("mdh/gone/indexes.json")].into_iter().collect();

        assert!(
            emptied_dirs(tgt, &pruned).is_empty(),
            "a directory still holding any file must never be reported"
        );
    }

    #[test]
    fn url_host_extracts_bare_host() {
        assert_eq!(
            url_host("https://org-dev.rossum.app/api/v1").as_deref(),
            Some("org-dev.rossum.app")
        );
        // Empty / malformed base → None (tests run with an empty lockfile, so
        // source-host handling must no-op rather than match everything).
        assert_eq!(url_host(""), None);
        assert_eq!(url_host("no-scheme"), None);
        assert_eq!(url_host("https://"), None);
    }

    #[test]
    fn strip_source_host_env_refs_cleans_env_fields_only() {
        let codec = crate::snapshot::codec::codec("queues").unwrap();
        let host = "org-dev.rossum.app";
        let mut v = serde_json::json!({
            // deployable content (a field cross_env_body KEEPS) — must be left
            // untouched even if it carries a source-host ref.
            "name": "q",
            "settings": { "lookup": [format!("https://{host}/api/v1/queues/999")] },
            // reverse-ref ENV array field: drop only the source-host entries,
            // keep rdc:// and other hosts.
            "users": [
                format!("https://{host}/api/v1/users/1"),
                "rdc://hooks/keep-me",
                "https://org-test.rossum.app/api/v1/users/2"
            ],
            "webhooks": [format!("https://{host}/api/v1/webhooks/5")]
        });
        strip_source_host_env_refs(&mut v, codec, host);
        assert_eq!(
            v["users"],
            serde_json::json!(["rdc://hooks/keep-me", "https://org-test.rossum.app/api/v1/users/2"]),
            "only the source-host user entry must be dropped"
        );
        assert_eq!(v["webhooks"], serde_json::json!([]), "source-host webhook must be dropped");
        assert_eq!(
            v["settings"],
            serde_json::json!({ "lookup": [format!("https://{host}/api/v1/queues/999")] }),
            "deployable content (a kept field) must NOT be touched"
        );
    }

    #[test]
    fn strip_source_host_env_refs_drops_string_email_env_field() {
        // An inbox's `email` is a server-assigned string env field. A
        // contaminated target restoring a source-host address (`…@<src-host>`)
        // must be dropped entirely so `rdc sync` re-reads the target's own.
        let codec = crate::snapshot::codec::codec("inboxes").unwrap();
        let host = "org-dev.rossum.app";
        let mut v = serde_json::json!({
            "name": "in",
            "email": format!("inbox-abc@{host}")
        });
        strip_source_host_env_refs(&mut v, codec, host);
        assert!(v.get("email").is_none(), "source-host email must be dropped: {v}");
        assert_eq!(v["name"], "in", "deployable content untouched");

        // A target-host email is legit env identity — keep it.
        let mut v2 = serde_json::json!({ "name": "in", "email": "inbox-abc@org-test.rossum.app" });
        strip_source_host_env_refs(&mut v2, codec, host);
        assert_eq!(v2["email"], "inbox-abc@org-test.rossum.app", "target-host email must be kept");
    }

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
            "hooks/extractor.json.test",
        ];
        for rel in managed.iter().chain(non_managed.iter()) {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"x").unwrap();
        }

        let got: BTreeSet<String> = enumerate_files(root, "test")
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
        transform_file(rel, src.path(), tgt.path(), &m, &subst, None, "https://tgt.example/api/v1/organizations/2", true, &crate::state::Lockfile::default(), false).unwrap();

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
    fn transform_reportabilizes_stale_source_urls_then_remaps() {
        use crate::state::{Lockfile, ObjectEntry};
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();

        let mut m = Mapping::default();
        m.workspaces.insert("main".into(), "main".into()); // identity (path)
        m.queues.insert("invoices".into(), "invoices".into()); // identity (path)
        m.hooks.insert("my-hook".into(), "my-hook-prod".into());

        let rel = Path::new("workspaces/main/queues/invoices/queue.json");
        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        fs::write(
            &src_file,
            serde_json::to_vec(&serde_json::json!({
                "name": "Q",
                // Stale raw URL on the LEGACY /webhooks/ endpoint — the form a
                // source pulled before the portabilization fixes carries.
                "webhooks": ["https://src.example/api/v1/webhooks/99"],
                // Already-portable ref on the new endpoint (must still remap).
                "hooks": ["rdc://hooks/my-hook"],
            }))
            .unwrap(),
        )
        .unwrap();

        // Source lockfile tracks the hook (id 99) under slug `my-hook`, so
        // `/webhooks/99` resolves (webhooks→hooks by id).
        let mut src_lock = Lockfile {
            api_base: "https://src.example/api/v1".into(),
            ..Lockfile::default()
        };
        src_lock.upsert(
            "hooks",
            "my-hook",
            ObjectEntry { id: 99, modified_at: None, content_hash: None, secrets_hash: None },
        );

        let subst = build_subst(&m);
        transform_file(
            rel,
            src.path(),
            tgt.path(),
            &m,
            &subst,
            None,
            "https://tgt.example/api/v1/organizations/2",
            true,
            &src_lock,
            false,
        )
        .unwrap();

        let dst = tgt.path().join("workspaces/main/queues/invoices/queue.json");
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap();
        // The stale /webhooks/99 URL was re-portabilized to rdc://hooks/my-hook,
        // then remapped to the target slug — the source host never leaks.
        assert_eq!(
            v["webhooks"][0], "rdc://hooks/my-hook-prod",
            "stale source webhooks URL must portabilize + remap, not leak the source host"
        );
        assert_eq!(v["hooks"][0], "rdc://hooks/my-hook-prod");
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
        transform_file(rel, src.path(), tgt.path(), &m, &subst, Some(&overlay), "https://tgt.example/api/v1/organizations/2", true, &crate::state::Lockfile::default(), false).unwrap();

        let dst = tgt.path().join("hooks/extractor-prod.json");
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap();
        assert_eq!(v["name"], "Extractor (PROD)", "tgt overlay must be applied");
        assert_eq!(v["type"], "function");
    }

    #[test]
    fn transform_applies_wildcard_default_to_objects_without_per_object_override() {
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();

        // Identity mapping; NO per-hook overlay entry for this slug — only the
        // reserved `"*"` kind-wide default.
        let m = Mapping::default();
        let mut overlay = Overlay::default();
        let mut star = BTreeMap::new();
        star.insert(
            "token_owner".to_string(),
            serde_json::Value::String("https://tgt.example/api/v1/users/2".into()),
        );
        overlay.hooks.insert("*".to_string(), star);

        let rel = Path::new("hooks/any-hook.json");
        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        // New hook (no tgt file): reconcile keeps the SOURCE-env token_owner, so
        // the wildcard default must override it to the target user.
        fs::write(
            &src_file,
            serde_json::to_vec(&serde_json::json!({
                "name": "Any Hook",
                "type": "function",
                "token_owner": "https://src.example/api/v1/users/1",
            }))
            .unwrap(),
        )
        .unwrap();

        let subst = build_subst(&m);
        transform_file(rel, src.path(), tgt.path(), &m, &subst, Some(&overlay), "https://tgt.example/api/v1/organizations/2", true, &crate::state::Lockfile::default(), false).unwrap();

        let dst = tgt.path().join("hooks/any-hook.json");
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap();
        assert_eq!(
            v["token_owner"], "https://tgt.example/api/v1/users/2",
            "[hooks.\"*\"] wildcard default must apply to a hook with no per-hook override",
        );
    }

    #[test]
    fn transform_per_object_override_wins_over_wildcard_default() {
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();

        let m = Mapping::default();
        let mut overlay = Overlay::default();
        // Wildcard sets BOTH a key the per-hook entry overrides (token_owner)
        // and a key only it sets (name).
        let mut star = BTreeMap::new();
        star.insert(
            "token_owner".to_string(),
            serde_json::Value::String("https://tgt.example/api/v1/users/2".into()),
        );
        star.insert(
            "name".to_string(),
            serde_json::Value::String("Wildcard Name".into()),
        );
        overlay.hooks.insert("*".to_string(), star);
        let mut per_hook = BTreeMap::new();
        per_hook.insert(
            "token_owner".to_string(),
            serde_json::Value::String("https://tgt.example/api/v1/users/99".into()),
        );
        overlay.hooks.insert("special-hook".to_string(), per_hook);

        let rel = Path::new("hooks/special-hook.json");
        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        fs::write(
            &src_file,
            serde_json::to_vec(&serde_json::json!({ "name": "Special", "type": "function" }))
                .unwrap(),
        )
        .unwrap();

        let subst = build_subst(&m);
        transform_file(rel, src.path(), tgt.path(), &m, &subst, Some(&overlay), "https://tgt.example/api/v1/organizations/2", true, &crate::state::Lockfile::default(), false).unwrap();

        let dst = tgt.path().join("hooks/special-hook.json");
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap();
        assert_eq!(
            v["token_owner"], "https://tgt.example/api/v1/users/99",
            "per-hook [hooks.<slug>] override must win over the wildcard on a shared key",
        );
        assert_eq!(
            v["name"], "Wildcard Name",
            "wildcard-only key must still be applied alongside a per-hook override",
        );
        assert_eq!(v["type"], "function", "unrelated field untouched");
    }

    #[test]
    fn transform_wildcard_default_applies_to_non_hook_kinds() {
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();

        // The wildcard is generic across kinds — prove it for rules.
        let m = Mapping::default();
        let mut overlay = Overlay::default();
        let mut star = BTreeMap::new();
        star.insert(
            "name".to_string(),
            serde_json::Value::String("Default Rule Name".into()),
        );
        overlay.rules.insert("*".to_string(), star);

        let rel = Path::new("rules/some-rule.json");
        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        fs::write(
            &src_file,
            serde_json::to_vec(&serde_json::json!({ "name": "Original Rule" })).unwrap(),
        )
        .unwrap();

        let subst = build_subst(&m);
        transform_file(rel, src.path(), tgt.path(), &m, &subst, Some(&overlay), "https://tgt.example/api/v1/organizations/2", true, &crate::state::Lockfile::default(), false).unwrap();

        let dst = tgt.path().join("rules/some-rule.json");
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap();
        assert_eq!(
            v["name"], "Default Rule Name",
            "[rules.\"*\"] wildcard default must apply to rules too",
        );
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
            true,
            &crate::state::Lockfile::default(),
            false,
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
        transform_file(rel, src.path(), tgt.path(), &m, &subst, None, "https://tgt.example/api/v1/organizations/2", true, &crate::state::Lockfile::default(), false).unwrap();

        let dst = tgt.path().join("hooks/extractor-prod.py");
        assert_eq!(
            fs::read(&dst).unwrap(),
            code,
            ".py is copied verbatim — no ref substitution inside code"
        );
    }

    #[test]
    fn validate_overlay_dir_ok_when_all_files_match_produced() {
        use std::fs;
        let dir = tempfile::TempDir::new().unwrap();
        let ov = dir.path();
        fs::create_dir_all(ov.join("hooks")).unwrap();
        fs::write(ov.join("hooks/extractor.py"), b"x").unwrap();

        let produced: std::collections::BTreeSet<PathBuf> =
            [PathBuf::from("hooks/extractor.py")].into_iter().collect();
        assert!(validate_overlay_dir(ov, &produced).is_ok());
    }

    #[test]
    fn validate_overlay_dir_errors_on_dangling_shadow() {
        use std::fs;
        let dir = tempfile::TempDir::new().unwrap();
        let ov = dir.path();
        fs::create_dir_all(ov.join("hooks")).unwrap();
        fs::write(ov.join("hooks/ghost.py"), b"x").unwrap();

        let produced: std::collections::BTreeSet<PathBuf> =
            [PathBuf::from("hooks/extractor.py")].into_iter().collect();
        let err = validate_overlay_dir(ov, &produced).unwrap_err().to_string();
        assert!(err.contains("hooks/ghost.py"), "names the offending file: {err}");
    }

    #[test]
    fn validate_overlay_dir_errors_on_json_shadow() {
        use std::fs;
        let dir = tempfile::TempDir::new().unwrap();
        let ov = dir.path();
        fs::create_dir_all(ov.join("hooks")).unwrap();
        fs::write(ov.join("hooks/extractor.json"), b"{}").unwrap();

        // `produced` is sidecars only — JSON is never in it.
        let produced: std::collections::BTreeSet<PathBuf> =
            [PathBuf::from("hooks/extractor.py")].into_iter().collect();
        let err = validate_overlay_dir(ov, &produced).unwrap_err().to_string();
        assert!(err.contains("hooks/extractor.json"), "names the json file: {err}");
    }

    #[test]
    fn validate_overlay_dir_ok_when_dir_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let produced = std::collections::BTreeSet::new();
        assert!(validate_overlay_dir(&dir.path().join("overlay"), &produced).is_ok());
    }

    fn produced_map(
        pairs: &[(&'static str, &[&str])],
    ) -> std::collections::BTreeMap<&'static str, std::collections::BTreeSet<String>> {
        pairs
            .iter()
            .map(|(kind, slugs)| {
                (
                    *kind,
                    slugs.iter().map(|s| s.to_string()).collect::<std::collections::BTreeSet<_>>(),
                )
            })
            .collect()
    }

    fn overlay_with_hook(slug: &str) -> Overlay {
        let mut ov = Overlay::default();
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("active".to_string(), serde_json::json!(false));
        ov.hooks.insert(slug.to_string(), fields);
        ov
    }

    #[test]
    fn dangling_overlay_keys_flags_key_with_no_produced_object() {
        // The overlay targets `sftp-import-initial-load` but the migration only
        // produces `sftp-import-master-data` (the hook was renamed) — the key is
        // dangling and its override would silently never apply.
        let ov = overlay_with_hook("sftp-import-initial-load");
        let produced = produced_map(&[("hooks", &["sftp-import-master-data", "validator"])]);
        let offenders = dangling_overlay_keys(&ov, &produced);
        assert_eq!(offenders.len(), 1);
        assert_eq!(offenders[0].kind, "hooks");
        assert_eq!(offenders[0].key, "sftp-import-initial-load");
        // Suggestions are the existing slugs of that kind, sorted.
        assert_eq!(
            offenders[0].existing,
            vec!["sftp-import-master-data".to_string(), "validator".to_string()]
        );
    }

    #[test]
    fn dangling_overlay_keys_accepts_a_produced_key() {
        let ov = overlay_with_hook("validator");
        let produced = produced_map(&[("hooks", &["validator"])]);
        assert!(dangling_overlay_keys(&ov, &produced).is_empty());
    }

    #[test]
    fn dangling_overlay_keys_exempts_the_wildcard_default() {
        // `[hooks."*"]` is the kind-wide default, not a real slug — never flagged,
        // even when nothing of that kind is produced.
        let ov = overlay_with_hook("*");
        let produced = produced_map(&[]);
        assert!(dangling_overlay_keys(&ov, &produced).is_empty());
    }

    #[test]
    fn dangling_overlay_keys_flags_kind_with_nothing_produced() {
        // A real (non-wildcard) key of a kind the migration produces none of is
        // still dangling; the suggestion list is simply empty.
        let ov = overlay_with_hook("some-hook");
        let produced = produced_map(&[]);
        let offenders = dangling_overlay_keys(&ov, &produced);
        assert_eq!(offenders.len(), 1);
        assert!(offenders[0].existing.is_empty());
    }

    #[test]
    fn format_dangling_overlay_error_lists_key_and_existing_slugs() {
        let offenders = vec![DanglingOverlayKey {
            kind: "hooks",
            key: "sftp-import-initial-load".to_string(),
            existing: vec![
                "sftp-import-master-data".to_string(),
                "validator".to_string(),
            ],
        }];
        let msg = format_dangling_overlay_error(&offenders, "dev-ap", "test-ap");
        assert!(msg.contains("sftp-import-initial-load"), "names the dangling key: {msg}");
        assert!(msg.contains("hooks"), "names the kind: {msg}");
        assert!(msg.contains("test-ap"), "names the tgt overlay env: {msg}");
        assert!(
            msg.contains("sftp-import-master-data"),
            "suggests the existing slugs: {msg}"
        );
    }

    #[test]
    fn list_overlay_files_returns_relpaths_skipping_pycache() {
        use std::fs;
        let dir = tempfile::TempDir::new().unwrap();
        let ov = dir.path();
        fs::create_dir_all(ov.join("hooks")).unwrap();
        fs::write(ov.join("hooks/extractor.py"), b"x").unwrap();
        fs::create_dir_all(ov.join("workspaces/main/queues/invoices/formulas")).unwrap();
        fs::write(
            ov.join("workspaces/main/queues/invoices/formulas/sftp_path.py"),
            b"y",
        )
        .unwrap();
        // __pycache__ must be ignored.
        fs::create_dir_all(ov.join("hooks/__pycache__")).unwrap();
        fs::write(ov.join("hooks/__pycache__/extractor.cpython-312.pyc"), b"z").unwrap();

        let got: Vec<std::path::PathBuf> = list_overlay_files(ov).unwrap();
        assert_eq!(
            got,
            vec![
                std::path::PathBuf::from("hooks/extractor.py"),
                std::path::PathBuf::from(
                    "workspaces/main/queues/invoices/formulas/sftp_path.py"
                ),
            ]
        );
    }

    #[test]
    fn is_sidecar_matches_only_code_files() {
        // Code/formula sidecars → true.
        assert!(is_sidecar(Path::new("hooks/extractor.py")));
        assert!(is_sidecar(Path::new("hooks/extractor.js")));
        assert!(is_sidecar(Path::new("rules/r1.py")));
        assert!(is_sidecar(Path::new(
            "workspaces/main/queues/invoices/formulas/sftp_path.py"
        )));
        // JSON objects → false (overlay.toml handles those).
        assert!(!is_sidecar(Path::new("hooks/extractor.json")));
        assert!(!is_sidecar(Path::new(
            "workspaces/main/queues/invoices/schema.json"
        )));
        // Non-sidecar code → false.
        assert!(!is_sidecar(Path::new("workspaces/main/workspace.py")));
    }

    // ---- score_threshold reconciliation (migrate default: ignore) ----

    /// Write `value` as JSON to a fresh temp file and return (dir, path). The
    /// TempDir is returned so the caller keeps it alive for the file's lifetime.
    fn tgt_file(value: &serde_json::Value) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("obj.json");
        std::fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
        (dir, path)
    }

    #[test]
    fn reconcile_schema_matched_adopts_target_per_datapoint_thresholds() {
        // Source datapoints (nested in a section) carry the source org's
        // thresholds; the target has its own tuning (amount tuned, vendor has
        // none) and lacks `newfield` entirely.
        let mut source = serde_json::json!({
            "content": [{
                "category": "section", "id": "header",
                "children": [
                    { "category": "datapoint", "id": "amount", "score_threshold": 0.5 },
                    { "category": "datapoint", "id": "vendor", "score_threshold": 0.5 },
                    { "category": "datapoint", "id": "newfield", "score_threshold": 0.5 }
                ]
            }]
        });
        let (_d, tgt) = tgt_file(&serde_json::json!({
            "content": [{
                "category": "section", "id": "header",
                "children": [
                    { "category": "datapoint", "id": "amount", "score_threshold": 0.9 },
                    { "category": "datapoint", "id": "vendor" }
                ]
            }]
        }));

        reconcile_score_thresholds(&mut source, "schemas", &tgt);

        let kids = &source["content"][0]["children"];
        // amount: adopts the TARGET's tuned value.
        assert_eq!(kids[0]["score_threshold"], serde_json::json!(0.9));
        // vendor: target has the datapoint but no threshold → dropped.
        assert!(kids[1].get("score_threshold").is_none(), "vendor threshold must be dropped");
        // newfield: absent from target (a field being introduced) → dropped.
        assert!(kids[2].get("score_threshold").is_none(), "new-field threshold must be dropped");
    }

    #[test]
    fn reconcile_schema_new_target_drops_all_thresholds() {
        let mut source = serde_json::json!({
            "content": [{
                "category": "section", "id": "header",
                "children": [
                    { "category": "datapoint", "id": "amount", "score_threshold": 0.5 },
                    { "category": "datapoint", "id": "vendor", "score_threshold": 0.7 }
                ]
            }]
        });
        // Point at a path that does not exist (brand-new target object).
        let missing = std::path::Path::new("/nonexistent/does-not-exist/schema.json");
        reconcile_score_thresholds(&mut source, "schemas", missing);
        let kids = &source["content"][0]["children"];
        assert!(kids[0].get("score_threshold").is_none());
        assert!(kids[1].get("score_threshold").is_none());
    }

    #[test]
    fn reconcile_queue_matched_adopts_target_default_top_level() {
        let mut source = serde_json::json!({ "name": "Q", "default_score_threshold": 0.5 });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "Q", "default_score_threshold": 0.85 }));
        reconcile_score_thresholds(&mut source, "queues", &tgt);
        assert_eq!(source["default_score_threshold"], serde_json::json!(0.85));
    }

    #[test]
    fn reconcile_queue_matched_adopts_target_default_under_settings() {
        // Position-agnostic: the key nested under `settings` must still be
        // matched by name and take the target's value.
        let mut source = serde_json::json!({
            "name": "Q", "settings": { "default_score_threshold": 0.5 }
        });
        let (_d, tgt) = tgt_file(&serde_json::json!({
            "name": "Q", "settings": { "default_score_threshold": 0.85 }
        }));
        reconcile_score_thresholds(&mut source, "queues", &tgt);
        assert_eq!(source["settings"]["default_score_threshold"], serde_json::json!(0.85));
    }

    #[test]
    fn reconcile_queue_new_target_drops_default() {
        let mut source = serde_json::json!({ "name": "Q", "default_score_threshold": 0.5 });
        let missing = std::path::Path::new("/nonexistent/does-not-exist/queue.json");
        reconcile_score_thresholds(&mut source, "queues", missing);
        assert!(source.get("default_score_threshold").is_none());
    }

    #[test]
    fn reconcile_training_matched_adopts_target_value() {
        // Engine auto-training is a per-env policy; a matched target keeps its
        // own `training_enabled` (source `true` must not overwrite target
        // `false`, or migrate+sync perpetually conflicts).
        let mut source = serde_json::json!({ "name": "Q", "training_enabled": true });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "Q", "training_enabled": false }));
        reconcile_training_enabled(&mut source, "queues", &tgt);
        assert_eq!(source["training_enabled"], serde_json::json!(false));
    }

    #[test]
    fn reconcile_training_new_target_drops_flag() {
        // No target => brand-new queue => drop the flag; Rossum's create default
        // (false) applies and the round-trip is stable.
        let mut source = serde_json::json!({ "name": "Q", "training_enabled": true });
        let missing = std::path::Path::new("/nonexistent/does-not-exist/queue.json");
        reconcile_training_enabled(&mut source, "queues", missing);
        assert!(source.get("training_enabled").is_none());
    }

    #[test]
    fn reconcile_training_no_op_for_non_queue() {
        // Only queues carry `training_enabled`; other kinds are untouched.
        let mut source = serde_json::json!({ "name": "S", "training_enabled": true });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "S" }));
        reconcile_training_enabled(&mut source, "schemas", &tgt);
        assert_eq!(source["training_enabled"], serde_json::json!(true));
    }

    #[test]
    fn transform_file_carries_thresholds_when_opted_in() {
        // With --migrate-score-thresholds (the `true` arg), the source's
        // per-datapoint threshold survives verbatim even against a matched
        // target that tuned it differently.
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();
        let rel = Path::new("workspaces/main/queues/invoices/schema.json");

        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        fs::write(
            &src_file,
            serde_json::to_vec(&serde_json::json!({
                "name": "S",
                "content": [{ "category": "datapoint", "id": "amount", "score_threshold": 0.5 }]
            }))
            .unwrap(),
        )
        .unwrap();

        // Matched target with a different tuned threshold.
        let dst = tgt.path().join(rel);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(
            &dst,
            serde_json::to_vec(&serde_json::json!({
                "name": "S",
                "content": [{ "category": "datapoint", "id": "amount", "score_threshold": 0.9 }]
            }))
            .unwrap(),
        )
        .unwrap();

        let m = Mapping::default();
        let subst = build_subst(&m);
        transform_file(
            rel, src.path(), tgt.path(), &m, &subst, None,
            "https://tgt.example/api/v1/organizations/2",
            /* migrate_score_thresholds = */ true,
            &crate::state::Lockfile::default(),
            false,
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap();
        assert_eq!(
            out["content"][0]["score_threshold"],
            serde_json::json!(0.5),
            "opting in must carry the SOURCE threshold verbatim"
        );
    }

    #[test]
    fn transform_file_ignores_thresholds_by_default() {
        // Without the flag (default), a matched target keeps its own tuning.
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();
        let rel = Path::new("workspaces/main/queues/invoices/schema.json");

        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        fs::write(
            &src_file,
            serde_json::to_vec(&serde_json::json!({
                "name": "S",
                "content": [{ "category": "datapoint", "id": "amount", "score_threshold": 0.5 }]
            }))
            .unwrap(),
        )
        .unwrap();

        let dst = tgt.path().join(rel);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(
            &dst,
            serde_json::to_vec(&serde_json::json!({
                "name": "S",
                "content": [{ "category": "datapoint", "id": "amount", "score_threshold": 0.9 }]
            }))
            .unwrap(),
        )
        .unwrap();

        let m = Mapping::default();
        let subst = build_subst(&m);
        transform_file(
            rel, src.path(), tgt.path(), &m, &subst, None,
            "https://tgt.example/api/v1/organizations/2",
            /* migrate_score_thresholds = */ false,
            &crate::state::Lockfile::default(),
            false,
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap();
        assert_eq!(
            out["content"][0]["score_threshold"],
            serde_json::json!(0.9),
            "default must preserve the TARGET's tuned threshold"
        );
    }
}
