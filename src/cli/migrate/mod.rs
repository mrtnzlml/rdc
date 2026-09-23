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
//! This is the whole local half of a promotion: the command it replaced,
//! `deploy`, is gone. The hidden shim that answered it with a guiding error
//! went too, so an old invocation now gets clap's unrecognized-subcommand
//! error like any other typo.

use crate::mapping::{GenericMapping, Mapping};
use crate::overlay::{Overlay, apply_overrides};
use crate::snapshot::refs::{RDC_SCHEME, walk_strings_mut};
use anyhow::{Context, Result};
use serde_json::Value;
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
        // The directory is hyphenated (`saved-views/`) while the kind string
        // is underscored (`saved_views`), matching the lockfile/mapping/overlay
        // key convention.
        Some("saved-views") if comps.len() == 2 => leaf
            .strip_suffix(".json")
            .map(|s| ("saved_views", s.to_string())),
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
        // The organization singleton, at the env root. Slug-independent (one
        // per env), so the reserved constant `"self"` stands in for a slug —
        // matching the codec's and the lockfile's convention.
        Some("organization.json") if comps.len() == 1 => {
            Some(("organization", "self".to_string()))
        }
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
        // Any leaf of a dataset dir selects with its dataset, so `--only
        // mdh/<slug>` carries collection.json + indexes.json + data.jsonl
        // together. `classify` deliberately keeps returning None for mdh.
        Some("mdh") if comps.len() >= 3 => Some(("mdh", comps[1].clone())),
        _ => None,
    }
}

/// True for a non-JSON code/formula sidecar leaf (`.py`/`.js`) that belongs to
/// a hook, rule, or schema, or for an MDH dataset's row data (`data.jsonl`) —
/// the files `migrate` copies verbatim and that an `overlay/` shadow may replace.
/// JSON objects are excluded (they are overlay-able through `overlay.toml`);
/// non-sidecar code and non-data files return false.
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
/// must mirror a code/formula sidecar or MDH row data that migrating the source
/// produces in the target — i.e. its relpath must be in `produced` (the full
/// source enumeration remapped to target paths, filtered to sidecars and data,
/// independent of `--only`). A shadow that overwrites nothing — a typo, a stale
/// path, a `.json`, or a sidecar/data absent from the source — is a hard error
/// naming the offending files. Run BEFORE any target file is written so the
/// migration aborts cleanly.
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
             source (a hook/rule .py/.js, a queue's formulas/<field>.py, or an MDH dataset's \
             data.jsonl). Fix the path or remove it."
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

/// Why a saved view's reference cannot cross into the target env.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SavedViewRefReason {
    /// A raw API URL that survived portabilization — it names a kind rdc does
    /// not snapshot (users are the common case), so there is nothing to remap
    /// it to. The server validates refs inside `query` as hyperlinks, so
    /// promoting it would 400.
    NonPortable,
    /// A well-formed `rdc://<kind>/<slug>` naming an object the target snapshot
    /// does not contain.
    Unresolvable,
}

/// One reference in a saved view that blocks promotion.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SavedViewRefProblem {
    pub slug: String,
    /// A JSON path into the view body, e.g. `queues_filter[0]` or
    /// `query.$and[1].modifier.$in[0]`.
    pub location: String,
    pub reference: String,
    pub reason: SavedViewRefReason,
}

/// Walk every string leaf of `value`, tracking a JSON path.
///
/// `walk_strings_mut` in `snapshot::refs` deliberately carries no path (it is a
/// blind rewriter), and the error messages here are only useful if they can say
/// WHERE the bad ref is — so this is a separate, path-aware walk. Object keys
/// are not visited, which is exactly why `field.<schema_id>` keys are out of
/// scope.
fn walk_strings_with_path(value: &Value, path: &str, f: &mut dyn FnMut(&str, &str)) {
    match value {
        Value::String(s) => f(path, s),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk_strings_with_path(item, &format!("{path}[{i}]"), f);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                let child = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                walk_strings_with_path(v, &child, f);
            }
        }
        _ => {}
    }
}

/// Order two JSON-path locations the way a reader expects, with runs of digits
/// compared by VALUE rather than byte by byte.
///
/// The listing these locations end up in ([`format_saved_view_ref_error`]) is a
/// report a human walks top to bottom against the file, so plain `str::cmp` —
/// which puts `queues_filter[10]` before `queues_filter[2]` — reads as a bug in
/// the report. Locations are built by [`walk_strings_with_path`] out of JSON
/// object keys and `[<index>]` segments, so the non-digit half is compared
/// bytewise (keys can hold any UTF-8; byte order on UTF-8 matches code-point
/// order, and only *stability* matters there, not collation).
///
/// Digit runs compare by trimmed length, then bytes, then raw length, so two
/// runs of equal value but different spelling (`[07]` vs `[7]`) still order
/// deterministically rather than comparing `Equal` — belt-and-braces, since an
/// index `walk_strings_with_path` produced never carries a leading zero.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn without_leading_zeros(d: &[u8]) -> &[u8] {
        let first = d.iter().position(|&c| c != b'0').unwrap_or(d.len());
        &d[first..]
    }
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    loop {
        let (Some(&ca), Some(&cb)) = (x.first(), y.first()) else {
            // One side ran out: the shorter string is the prefix, so it sorts
            // first (and equal lengths mean equal strings).
            return x.len().cmp(&y.len());
        };
        let ord = if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let na = x.iter().take_while(|c| c.is_ascii_digit()).count();
            let nb = y.iter().take_while(|c| c.is_ascii_digit()).count();
            let (da, db) = (&x[..na], &y[..nb]);
            let (ta, tb) = (without_leading_zeros(da), without_leading_zeros(db));
            x = &x[na..];
            y = &y[nb..];
            ta.len()
                .cmp(&tb.len())
                .then_with(|| ta.cmp(tb))
                .then_with(|| da.len().cmp(&db.len()))
        } else {
            x = &x[1..];
            y = &y[1..];
            ca.cmp(&cb)
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
}

/// The `(kind, slug)` set the target tree WILL contain once this run finishes:
/// what is there now, plus what the run writes, minus what `--mirror` prunes.
///
/// Used by both modes. For a real run this is equivalent to enumerating the
/// target tree after the writes — `existing` already covers the target-only
/// objects an overlay may legitimately point at, and `would_write` covers what
/// this run creates. For `--dry-run` it is the only correct answer: nothing has
/// been written, so a disk-only read would miss every object the run would
/// create and call each of its refs unresolvable.
///
/// The ORDER of operations is load-bearing, not incidental: `pruned` is
/// subtracted from `existing` FIRST, and `would_write` is unioned in AFTER —
/// never the other way around. A slug is known if it will exist at ANY path
/// once the run finishes, and a write always beats a prune of the same slug.
/// This matters because `classify` derives a queue/schema/inbox's `(kind,
/// slug)` from the queue path component alone (see `classify_workspace`), not
/// the workspace it lives under — so an object that moves workspace between
/// envs (a renamed workspace, a reparented queue) classifies identically at
/// its OLD and NEW path. `--mirror` then prunes the old path in the very same
/// run that writes the new one: subtracting `pruned` after the union would
/// drop that object from `known` even though it demonstrably exists on disk,
/// and diverge from what a post-write enumeration of the real run would show.
fn projected_known(paths: ProjectedPaths<'_>) -> BTreeSet<(String, String)> {
    let ProjectedPaths { existing, would_write, pruned } = paths;
    let mut out: BTreeSet<(String, String)> = existing
        .iter()
        .filter_map(|rel| classify(rel).map(|(k, s)| (k.to_string(), s)))
        .collect();
    for rel in pruned {
        if let Some((k, s)) = classify(rel) {
            out.remove(&(k.to_string(), s));
        }
    }
    out.extend(
        would_write
            .iter()
            .filter_map(|rel| classify(rel).map(|(k, s)| (k.to_string(), s))),
    );
    out
}

/// The three path lists [`projected_known`] combines, named rather than
/// positional. All three are `&[PathBuf]` of env-relative paths, so as bare
/// parameters they were freely interchangeable at every call site: swapping
/// `pruned` with either of the others compiled and silently produced a wrong
/// `known` set — which for saved views means a false refusal (or a missed one).
/// Named fields make the mix-up unrepresentable.
struct ProjectedPaths<'a> {
    /// Env-relative paths the target tree holds right now.
    existing: &'a [PathBuf],
    /// Env-relative paths this run writes.
    would_write: &'a [PathBuf],
    /// Env-relative paths `--mirror` prunes.
    pruned: &'a [PathBuf],
}

/// Validate a migrated saved view's references against what the target snapshot
/// actually contains.
///
/// `known` is the set of `(kind, slug)` pairs the migration produced, derived by
/// running [`classify`] over the enumerated target files.
///
/// Saved views are checked strictly rather than deferred. `resolve_value_deferring`
/// would drop a top-level field still holding `rdc://` refs and PATCH it later,
/// which for this kind is unsafe twice over: an empty `queues_filter` makes a
/// shared view visible to the WHOLE organization, and `query` is required on
/// POST so deferring it fails the create.
///
/// Only **deployable content** is checked — the same scope
/// [`strip_source_host_env_refs`] uses, and derived the same way: the top-level
/// keys `cross_env_body` keeps. The env fields it strips carry raw API URLs by
/// design, and walking them flagged every promoted view: `organization` is a
/// plain `…/api/v1/organizations/<id>` URL that [`reconcile_target_identity`]
/// sets from the TARGET's `rdc.toml` (`is_portable_kind` deliberately never
/// portabilizes it), and a matched target's `url` comes back from the target
/// file. Both are correct values, not refs that "cannot cross". Deriving the
/// set from the codec also means a ref-bearing field Rossum adds later is
/// covered automatically, while a new env field is excluded automatically.
///
/// `src_host` is the bare host of the SOURCE env's `api_base` (via [`url_host`]),
/// and it is what makes the non-portable half work on BOTH api_base shapes rdc
/// supports. `https://<org>.rossum.app/api/v1` yields refs carrying `/api/v1/`,
/// but `https://api.elis.rossum.ai/v1` does not — on that family a
/// `…/v1/users/7` ref matched neither branch and promoted verbatim, so this half
/// of the validation silently did nothing. Nothing downstream catches it either:
/// push's `residual_rdc_refs` only looks for surviving `rdc://`. `None` (no
/// lockfile, no `rdc.toml` entry) falls back to the `/api/v1/` shape alone.
pub(crate) fn check_saved_view_refs(
    slug: &str,
    value: &Value,
    known: &BTreeSet<(String, String)>,
    src_host: Option<&str>,
) -> Vec<SavedViewRefProblem> {
    let mut out = Vec::new();
    let Some(obj) = value.as_object() else {
        return out;
    };
    let deployable: BTreeSet<String> = match crate::snapshot::codec::codec("saved_views") {
        Some(codec) => {
            // Probe on a clone so `value` is untouched.
            let mut probe = value.clone();
            codec.cross_env_body(&mut probe);
            probe
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        }
        // The kind is registered at compile time, so this is unreachable in
        // practice. Fall back to the two fields that actually carry refs rather
        // than silently checking nothing.
        None => ["queues_filter", "query"]
            .into_iter()
            .map(String::from)
            .collect(),
    };
    for (key, sub) in obj {
        if !deployable.contains(key) {
            continue;
        }
        walk_strings_with_path(sub, key, &mut |location, s| {
            if let Some((kind, target)) = crate::snapshot::refs::parse_rdc_ref(s) {
                if !known.contains(&(kind.to_string(), target.to_string())) {
                    out.push(SavedViewRefProblem {
                        slug: slug.to_string(),
                        location: location.to_string(),
                        reference: s.to_string(),
                        reason: SavedViewRefReason::Unresolvable,
                    });
                }
            } else if s.starts_with("http")
                && (s.contains("/api/v1/") || src_host.is_some_and(|h| s.contains(h)))
            {
                out.push(SavedViewRefProblem {
                    slug: slug.to_string(),
                    location: location.to_string(),
                    reference: s.to_string(),
                    reason: SavedViewRefReason::NonPortable,
                });
            }
        });
    }
    out.sort_by(|a, b| natural_cmp(&a.location, &b.location));
    out
}

/// Build the hard-error message for [`check_saved_view_refs`] offenders. Names
/// each view, the JSON path inside it and the offending ref, then says what the
/// two ways out are.
///
/// The whole listing lives in the returned error rather than in `eprintln!`s
/// beside it: an error that carries its own listing can be shown wherever
/// the caller wants, rather than depending on stderr already having been
/// seen.
pub(crate) fn format_saved_view_ref_error(problems: &[SavedViewRefProblem], tgt: &str) -> String {
    let mut body = String::new();
    for p in problems {
        let why = match p.reason {
            SavedViewRefReason::NonPortable => {
                "not a portable reference — a raw API URL rdc cannot remap \
                 (a user ref, or a foreign host)"
            }
            SavedViewRefReason::Unresolvable => "no such object in the target snapshot",
        };
        body.push_str(&format!(
            "  - saved-views/{}: {} → {} ({why})\n",
            p.slug, p.location, p.reference
        ));
    }
    format!(
        "{} saved-view reference(s) cannot cross into '{tgt}':\n\
         {body}\
         Fix the source view, or override `query` / `queues_filter` for this env in \
         envs/{tgt}/overlay.toml. rdc refuses rather than dropping the clause, because an empty \
         queues_filter would make a shared view visible to the whole organization and a \
         dropped query filter would silently change what the view shows.",
        problems.len()
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
        "saved_views" => overlay.saved_view(slug),
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
fn transform_file(
    rel: &Path,
    src_root: &Path,
    tgt_root: &Path,
    mapping: &Mapping,
    subst: &BTreeMap<String, String>,
    overlay: Option<&Overlay>,
    tgt_org_url: &str,
    carry: Carry,
    src_lockfile: &crate::state::Lockfile,
    tgt_lockfile: &crate::state::Lockfile,
    dry_run: bool,
    id_remap: &IdRemap,
    id_hits: &mut Vec<(String, u64, u64)>,
    carried_prefixes: &mut Vec<(String, String)>,
    tgt_env: &str,
    missing_schema_ids: &mut Vec<String>,
    promoted: &mut Vec<(&'static str, String, serde_json::Value)>,
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
        // A sidecar that differs from the target's only in its EOF newline is
        // the same code as far as rdc is concerned: `sidecar_bytes_for_hash`
        // ignores trailing newlines on BOTH sides, because editors add them and
        // the API returns bodies without them. So rewriting the target file to
        // the source's (or an overlay shadow's) convention changes bytes that
        // no `content_hash` can see — `rdc sync` correctly finds nothing to
        // push, the file stays modified in `git diff`, and the next migrate does
        // it again: a working tree that never comes clean. Keep what the target
        // already has; a real content change still lands.
        let bytes = match std::fs::read(&dst_path) {
            Ok(existing)
                if crate::snapshot::codec::sidecar_bytes_for_hash(&existing)
                    == crate::snapshot::codec::sidecar_bytes_for_hash(&bytes) =>
            {
                existing
            }
            _ => bytes,
        };
        return settle(&dst_path, &bytes, dry_run);
    }

    let raw =
        std::fs::read(&src_path).with_context(|| format!("reading {}", src_path.display()))?;
    let mut value: serde_json::Value = serde_json::from_slice(&raw)
        .with_context(|| format!("parsing JSON {}", src_path.display()))?;

    // The SOURCE's `hook_template`, read before the env-field passes below drop
    // it (its host is the source org's, so `strip_source_host_env_refs` removes
    // it outright). Unlike every other env field it is MANDATORY to create the
    // hook — see `reconcile_hook_template`, which puts it back, retargeted.
    let src_hook_template = value
        .get("hook_template")
        .and_then(|v| v.as_str())
        .map(str::to_string);

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
    let mut matched_in_target = false;
    if let Some((kind, _)) = classify(rel)
        && let Some(codec) = crate::snapshot::codec::codec(kind)
    {
        matched_in_target = reconcile_target_identity(&mut value, &dst_path, codec, tgt_org_url);
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
    //
    // The heuristic needs one host per env. For a project whose envs are two
    // ORGANIZATIONS inside a single Rossum instance — one `api_base`, two
    // `org_id`s — "carries the source host" says nothing about whose ref it is:
    // the target's own `created_by`, `modified_by`, `token_owner`,
    // `hook_template`, `guide`, an inbox's `email`, a queue's `generic_engine`
    // and back-refs all carry it too. Applied to a MATCHED object it deleted
    // exactly the values `reconcile_target_identity` had just restored FROM the
    // target file, so migrate's output no longer matched the target's own pull:
    // `rdc sync` classified every object as a local edit, PATCHed it, wrote the
    // server's response back — and the next migrate stripped the fields again.
    // A migrate&&sync chain that never converges, re-pushing the whole env on
    // every run. So on a shared host the cleanup is skipped for a matched
    // object: there is nothing source-derived left in its env fields to clean
    // (contamination in one is then undetectable — same host, same shape — and
    // keeping the target's pulled value is the conservative half of that
    // trade). `organization` is unaffected either way: it stays owned by
    // `reconcile_target_identity`, which sets it from `rdc.toml`.
    //
    // A NEW object is still cleaned on any host — nothing restored it, so its
    // env fields ARE the source's — and a host-per-env promotion is unchanged.
    let shared_host = url_host(tgt_org_url) == url_host(&src_lockfile.api_base);
    if let Some((kind, _)) = classify(rel)
        && let Some(codec) = crate::snapshot::codec::codec(kind)
        && let Some(src_host) = url_host(&src_lockfile.api_base)
        && !(matched_in_target && shared_host)
    {
        strip_source_host_env_refs(&mut value, codec, &src_host);
    }

    // ---- env-tuned field reconciles ----
    //
    // These restore the TARGET's own value for fields that are per-env by
    // nature, so promoting a source env does not overwrite them. They all run
    // BEFORE the overlay is applied: `overlay.toml` is the user declaring the
    // target's value on purpose and must win, per the precedence the overlay
    // block below documents. (Running them after silently made an overlay entry
    // for any of these keys permanently inert — the reconcile just wrote the
    // target's pulled value back on every run.)

    // Per-org confidence thresholds, unless the user opted to carry them.
    // `score_threshold` (per schema datapoint) and `default_score_threshold`
    // (per queue) are tuned per queue/organization and expected to differ across
    // envs, so by default a matched target keeps its own values and a brand-new
    // object drops them (falling back to the queue/server default).
    if !carry.score_thresholds
        && let Some((kind, _)) = classify(rel)
    {
        reconcile_score_thresholds(&mut value, kind, &dst_path);
    }

    // The per-queue `training_enabled` flag — unconditional, no carry group.
    if let Some((kind, _)) = classify(rel) {
        reconcile_target_owned_keys(&mut value, kind, &dst_path, &["training_enabled"]);
    }

    // The queue's automation configuration, unless the user opted to carry it.
    // Unlike `training_enabled` these are NOT in `snapshot::noise::NOISE_FIELDS`
    // — rdc hashes and pushes them — so promoting the source's values is a real
    // change that the following `rdc sync` really applies to the target org.
    // That is the whole bug: a promotion silently switched a target queue's
    // automation to whatever the source env happened to have.
    if !carry.automation
        && let Some((kind, _)) = classify(rel)
    {
        reconcile_target_owned_keys(&mut value, kind, &dst_path, AUTOMATION_KEYS);
    }

    // The queue's engine binding. `engine`/`dedicated_engine` are promoted
    // from the source; `generic_engine` is restored from the target — and the
    // three are mutually exclusive, so a source bound to a custom engine plus
    // a target on the built-in generic engine produced a queue with two
    // bindings and every later push 400'd. Runs after the restore above (the
    // value it has to drop is the one the restore put back) and, like the
    // other reconciles, before the overlay.
    if let Some((kind, _)) = classify(rel) {
        reconcile_engine_slot(&mut value, kind);
    }

    // An inbox's `email_prefix`, unless the user opted to carry it. The prefix
    // is the left-hand side of the inbox's PUBLIC address, so promoting the
    // source env's value re-addresses the target's mailbox — but unlike the
    // score thresholds it is MANDATORY on create, so a brand-new inbox keeps
    // the source's (and is warned about) instead of dropping it into a body the
    // API rejects. "Brand-new" is the target LOCKFILE's verdict, not the file's:
    // an object the target has never deployed is the one the push will POST.
    //
    // The reported carry is only PROVISIONAL here — the overlay runs after this
    // and may replace the value, which is the documented way to choose a new
    // env's address. It is confirmed further down, once the final value is
    // known, so the warning never names a prefix the user has already overridden.
    let mut provisional_carry: Option<(String, String)> = None;
    if !carry.email_prefixes
        && let Some((kind, src_slug)) = classify(rel)
        && kind == "inboxes"
    {
        let slug = tgt_slug(mapping, kind, &src_slug);
        let will_create = tgt_lockfile
            .objects
            .get(kind)
            .and_then(|m| m.get(&slug))
            .is_none();
        if let Some(prefix) = reconcile_email_prefix(&mut value, kind, &dst_path, will_create) {
            provisional_carry = Some((slug, prefix));
        }
    }

    // A store extension's `hook_template`. Same shape as the inbox prefix above:
    // a per-env value that is nonetheless MANDATORY on create, so the target
    // lockfile — not the file on disk — decides whether to restore it.
    if let Some((kind, src_slug)) = classify(rel)
        && kind == "hooks"
    {
        let slug = tgt_slug(mapping, kind, &src_slug);
        let will_create = tgt_lockfile
            .objects
            .get(kind)
            .and_then(|m| m.get(&slug))
            .is_none();
        if will_create {
            reconcile_hook_template(
                &mut value,
                src_hook_template.as_deref(),
                api_base_of(tgt_org_url),
            );
        }
    }

    // Remap raw numeric object ids embedded in deployable content (the stock
    // Duplicate Handling extension's `scope.ids` / `excluded_queues` /
    // `target_queue`, a file-storage-import `queue_id`, …). These are plain
    // integers, so the portable-ref machinery above never sees them — it walks
    // string leaves and resolves to URLs. Without this they promote verbatim and
    // point at the SOURCE org's objects forever, invisibly: once the wrong id is
    // in the target snapshot, both sides agree and every later migrate shows no
    // diff. Kind-safe by construction (see `IdRemap`), and runs BEFORE the
    // overlay so an explicit overlay pin still wins — which is also the escape
    // hatch for a large integer that is NOT a reference.
    if let Some((kind, _)) = classify(rel)
        && let Some(codec) = crate::snapshot::codec::codec(kind)
    {
        remap_object_ids(&mut value, codec, id_remap, id_hits);
    }

    // Apply the tgt overlay for this object. A kind-wide default lives under the
    // reserved `"*"` slug (e.g. `[hooks."*"]`) and is applied FIRST; the
    // per-object entry (`[hooks.<slug>]`) is applied SECOND so it wins on any
    // shared key. Both run AFTER `reconcile_target_identity` and after the
    // env-tuned reconciles above, so an overlay value overrides the object's
    // reconciled/source content. Precedence:
    // per-object override > kind-wide `"*"` default > reconciled value.
    //
    // `organization` is a per-env SINGLETON — no slug, so no `"*"` wildcard and
    // no `overlay_for`/`tgt_slug` lookup — so it is dispatched separately, via
    // the codec's own `overlay()` hook (`Organization::overlay` returns
    // `Overlay::organization()` regardless of the slug it's passed).
    if let Some((kind, src_slug)) = classify(rel)
        && let Some(ov) = overlay
    {
        if kind == "organization" {
            if let Some(codec) = crate::snapshot::codec::codec(kind)
                && let Some(overrides) = codec.overlay(ov, &src_slug)
            {
                apply_overrides(&mut value, overrides);
            }
        } else {
            if let Some(defaults) = overlay_slug(ov, kind, "*") {
                apply_overrides(&mut value, defaults);
            }
            if let Some(overrides) = overlay_for(ov, mapping, kind, &src_slug) {
                apply_overrides(&mut value, overrides);
            }
        }
    }

    // Warn (never drop) when a promoted `column_type: "schema"` column names a
    // `schema_id` no schema under the target env defines. The API accepts an
    // unknown id with a 200 (verified against the live sandbox), so the server
    // never catches this — offline is the only place it can surface, and
    // silently editing deployable content is worse than a dead column the
    // warning names.
    if let Some((kind, _)) = classify(rel)
        && kind == "organization"
    {
        missing_schema_ids.extend(org_columns_missing_in_target(&value, tgt_root, tgt_env));
    }

    // Confirm the provisional inbox-prefix carry recorded above, now that the
    // overlay has had its say. If an overlay entry replaced the value, the user
    // has already chosen this env's address deliberately — reporting the source's
    // would name a prefix that is not used and nag about a decision already made.
    if let Some((slug, prefix)) = provisional_carry
        && value.get("email_prefix").and_then(|v| v.as_str()) == Some(prefix.as_str())
    {
        carried_prefixes.push((slug, prefix));
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

    // Collect the POST-OVERLAY body so both modes validate the same value the
    // real run would write. Collecting here rather than reading the file back
    // afterwards is what lets `--dry-run` validate at all — and collecting
    // after the overlay is what keeps the documented `overlay.toml` escape
    // hatch working.
    // Saved views are collected for their strict ref check; queues for the
    // `schema` link `POST /queues` demands. Both are validated after the whole
    // run, against the set of objects the target tree WILL hold.
    if let Some((kind @ ("saved_views" | "queues"), slug)) = classify(&dst_rel) {
        promoted.push((kind, slug, value.clone()));
    }

    let mut json = serde_json::to_vec_pretty(&value)?;
    json.push(b'\n');
    settle(&dst_path, &json, dry_run)
}

/// Every promoted `column_type: "schema"` column (in either
/// `annotation_list_table` or `request_dashboard_table`) whose `schema_id`
/// appears in no schema under the target env, named for a warning. `value` is
/// the organization body post-overlay — the final promoted content.
///
/// A read error while scanning `tgt_root` (a vanished dir, an unreadable file)
/// is treated as "no schemas found" rather than aborting the migration: this
/// check is advisory, never a gate.
fn org_columns_missing_in_target(
    value: &serde_json::Value,
    tgt_root: &Path,
    tgt_env: &str,
) -> Vec<String> {
    let mut known = std::collections::BTreeSet::new();
    if let Ok(files) = enumerate_files(tgt_root, tgt_env) {
        for rel in files {
            if rel.file_name().and_then(|n| n.to_str()) != Some("schema.json") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(tgt_root.join(&rel))
                && let Ok(schema) = serde_json::from_slice::<serde_json::Value>(&bytes)
            {
                collect_schema_ids(&schema, &mut known);
            }
        }
    }
    let mut missing = Vec::new();
    for table in ["annotation_list_table", "request_dashboard_table"] {
        let Some(cols) = value
            .get("settings")
            .and_then(|s| s.get(table))
            .and_then(|t| t.get("columns"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for col in cols {
            if col.get("column_type").and_then(|v| v.as_str()) == Some("schema")
                && let Some(id) = col.get("schema_id").and_then(|v| v.as_str())
                && !known.contains(id)
            {
                missing.push(id.to_string());
            }
        }
    }
    missing.sort();
    missing.dedup();
    missing
}

/// Every `schema_id` in a schema's `content` tree, at any depth.
fn collect_schema_ids(value: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(id)) = map.get("id") {
                out.insert(id.clone());
            }
            for v in map.values() {
                collect_schema_ids(v, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_schema_ids(v, out)),
        _ => {}
    }
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
        // `organization` is exempt: `reconcile_target_identity` just set it to
        // the TARGET org, authoritatively, from `rdc.toml`. The source-host
        // heuristic below cannot tell a source ref from a target one when both
        // envs live on the SAME host — two orgs in one Rossum instance, e.g.
        // `https://acme.rossum.app/api/v1` with `org_id` 1 and 2 — so it would
        // delete the correct value it had just been given, leaving a body the
        // API rejects with `organization: This field is required.` on create.
        if field == "organization" {
            continue;
        }
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

/// Cross-env object-id remap: source id -> target id, for RAW NUMERIC ids
/// embedded in deployable content.
///
/// Portable `rdc://<kind>/<slug>` refs cover every *link* field, because those
/// hold a URL. But some extensions take a bare object **id** instead — e.g. the
/// Duplicate Handling extension's `settings…scope.ids` / `excluded_queues` /
/// `target_queue`, or a file-storage-import `queue_id`. Those are plain
/// integers, so [`crate::snapshot::refs`] never sees them (it walks string
/// leaves only) and they promote verbatim into the target env, where they point
/// at objects in the wrong organization. Worse, once a wrong id is in the target
/// snapshot both sides agree and every later migrate reports NO diff, so a diff
/// review cannot catch it.
///
/// # Kind safety
///
/// A Rossum id is unique only **within a kind** — `/queues/1010` and
/// `/labels/1010` are different objects — and a bare integer inside a `settings`
/// blob carries no kind. So the map is built from ids that exactly ONE kind
/// claims in the source env. An id claimed by two kinds is AMBIGUOUS and is
/// deliberately excluded rather than guessed at, because remapping it could
/// silently turn a label id into a queue id. Ambiguous ids are reported so the
/// user can pin the field explicitly in `overlay.toml` instead.
///
/// One kind claiming an id under several slugs is NOT ambiguous by itself: a
/// schema or inbox shared by many queues is snapshotted once per consuming queue
/// and every entry carries the same remote id. That only becomes ambiguous if
/// those slugs disagree about the target id.
///
/// Non-portable kinds (`organization`, `mdh_indexes`, `mdh_data`) are skipped,
/// mirroring [`crate::snapshot::refs::is_portable_kind`]: the organization is a
/// per-env singleton reconciled separately, and the two MDH kinds carry the
/// sentinel `id: 0`.
#[derive(Debug, Default)]
struct IdRemap {
    /// Source id -> target id. Kind-safe: only ids uniquely owned by one kind.
    map: BTreeMap<u64, u64>,
    /// Source ids claimed by more than one kind (or whose slugs disagree about
    /// the target), with the claimants, for a warning. Never remapped.
    ambiguous: Vec<(u64, Vec<String>)>,
}

impl IdRemap {
    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Build the [`IdRemap`] by pairing the two lockfiles through the slug
/// `subst` map, so an id follows exactly the same slug remapping its
/// `rdc://` counterpart would.
fn build_id_remap(
    src_lockfile: &crate::state::Lockfile,
    tgt_lockfile: &crate::state::Lockfile,
    subst: &BTreeMap<String, String>,
) -> IdRemap {
    use crate::snapshot::refs::{RDC_SCHEME, is_portable_kind, parse_rdc_ref};

    // Which (kind, slug) pairs claim each source id.
    let mut claims: BTreeMap<u64, Vec<(&str, &str)>> = BTreeMap::new();
    for (kind, entries) in &src_lockfile.objects {
        if !is_portable_kind(kind) {
            continue;
        }
        for (slug, entry) in entries {
            if entry.id != 0 {
                claims.entry(entry.id).or_default().push((kind, slug));
            }
        }
    }

    let mut out = IdRemap::default();
    for (src_id, owners) in claims {
        let kinds: std::collections::BTreeSet<&str> = owners.iter().map(|(k, _)| *k).collect();
        if kinds.len() > 1 {
            out.ambiguous.push((
                src_id,
                owners.iter().map(|(k, s)| format!("{k}/{s}")).collect(),
            ));
            continue;
        }
        // Resolve every claiming slug to a target id; they must agree.
        let targets: std::collections::BTreeSet<u64> = owners
            .iter()
            .filter_map(|(kind, slug)| {
                let tgt_slug = subst
                    .get(&format!("{RDC_SCHEME}{kind}/{slug}"))
                    .and_then(|r| parse_rdc_ref(r))
                    .map(|(_, s)| s)
                    .unwrap_or(slug);
                tgt_lockfile
                    .objects
                    .get(*kind)
                    .and_then(|m| m.get(tgt_slug))
                    .map(|e| e.id)
                    .filter(|id| *id != 0)
            })
            .collect();
        match targets.len() {
            // Not in the target env yet (a brand-new object): leave the id
            // alone. `rdc sync` creates the object, and the next migrate maps it.
            0 => {}
            1 => {
                let tgt_id = *targets.iter().next().expect("len checked");
                if tgt_id != src_id {
                    out.map.insert(src_id, tgt_id);
                }
            }
            _ => out.ambiguous.push((
                src_id,
                owners.iter().map(|(k, s)| format!("{k}/{s}")).collect(),
            )),
        }
    }
    out
}

/// Rewrite every raw source id in `value`'s DEPLOYABLE content to its target
/// counterpart, appending `(path, from, to)` for each rewrite.
///
/// Scoped to the fields `cross_env_body` KEEPS — the inverse of
/// [`strip_source_host_env_refs`]. Env/identity fields (`id`, `url`,
/// `organization`, reverse-ref arrays, …) are owned by
/// [`reconcile_target_identity`] and must not be touched here: after that
/// reconcile they already hold the TARGET's values, and a target id can
/// coincide with some unrelated source id.
///
/// Both JSON numbers and digit-strings are rewritten, preserving the original
/// type — `file-storage-import` stores `queue_id` as a string (`"1001"`) while
/// duplicate detection stores ints. A digit-string is only considered when it
/// round-trips exactly, so `"007"` is never treated as id `7`.
fn remap_object_ids(
    value: &mut serde_json::Value,
    codec: &'static dyn crate::snapshot::codec::KindCodec,
    remap: &IdRemap,
    hits: &mut Vec<(String, u64, u64)>,
) {
    if remap.is_empty() || !value.is_object() {
        return;
    }
    // Env fields = top-level keys present in the full body but removed by
    // `cross_env_body` (probe on a clone so `value` is untouched). Same
    // derivation `reconcile_target_identity` / `strip_source_host_env_refs` use.
    let mut probe = value.clone();
    codec.cross_env_body(&mut probe);
    let deployable: std::collections::BTreeSet<String> = probe
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for (key, field) in obj.iter_mut() {
        if deployable.contains(key.as_str()) {
            walk_remap_ids(field, key, &remap.map, hits);
        }
    }
}

/// Recursive worker for [`remap_object_ids`]. `path` is a dotted/indexed trail
/// used only for the log line.
fn walk_remap_ids(
    value: &mut serde_json::Value,
    path: &str,
    map: &BTreeMap<u64, u64>,
    hits: &mut Vec<(String, u64, u64)>,
) {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(src) = n.as_u64()
                && let Some(&tgt) = map.get(&src)
            {
                *value = serde_json::Value::from(tgt);
                hits.push((path.to_string(), src, tgt));
            }
        }
        serde_json::Value::String(s) => {
            if let Ok(src) = s.parse::<u64>()
                // Only an exact round-trip is an id ("007" is not id 7).
                && src.to_string() == *s
                && let Some(&tgt) = map.get(&src)
            {
                *s = tgt.to_string();
                hits.push((path.to_string(), src, tgt));
            }
        }
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                walk_remap_ids(item, &format!("{path}[{i}]"), map, hits);
            }
        }
        serde_json::Value::Object(m) => {
            for (k, v) in m.iter_mut() {
                walk_remap_ids(v, &format!("{path}.{k}"), map, hits);
            }
        }
        _ => {}
    }
}

/// Reconcile per-org confidence thresholds so migrate does not carry them from
/// the source env (see `--carry score-thresholds`).
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

/// The queue fields that make up the `automation` carry group.
///
/// All three are top-level on a queue — verified against 240 pulled
/// `queue.json` files, where every one carries them at the top level — so this
/// needs no position-agnostic search of the kind `default_score_threshold` has.
const AUTOMATION_KEYS: &[&str] = &[
    "automation_enabled",
    "automation_level",
    "quality_spot_check_percentage",
];

/// Reconcile top-level queue fields that the TARGET env owns, so migrate does
/// not promote them from the source.
///
/// Two callers, one rule:
///
/// * `training_enabled` — unconditional. Engine auto-training is a per-env
///   policy (you train the model in dev, not in a throwaway test clone), and
///   Rossum resets the flag to `false` when a queue is created. Carrying the
///   source's value makes every migrate+sync conflict — the migrated queue says
///   `true`, the deployed queue is `false`, and neither side ever converges.
/// * [`AUTOMATION_KEYS`] — unless `--carry automation`. Whether a queue
///   auto-exports without human review is the target organization's operational
///   decision, taken after watching its own accuracy; it is not solution
///   configuration that travels with a schema change.
///
/// The rule, following [`reconcile_score_thresholds`]:
///
/// - **Matched target** (`tgt_path` exists and carries the key): adopt the
///   TARGET's value, so each env keeps its own policy.
/// - **New target, or the target lacks that key**: drop it, so the server's
///   create-time default applies and the round-trip is stable. Per key, not
///   all-or-nothing.
///
/// A key the source body does not have is never introduced from the target:
/// this adopts, it does not populate.
///
/// A no-op for any kind other than `queues`.
fn reconcile_target_owned_keys(
    value: &mut serde_json::Value,
    kind: &str,
    tgt_path: &Path,
    keys: &[&str],
) {
    if kind != "queues" {
        return;
    }
    // Check before reading: a queue body carrying none of the keys needs no
    // target file at all, and `transform_file` runs this per file. `Value::get`
    // answers `None` for a non-object value too, so this one check covers both
    // "not an object" and "object without any of the keys".
    if !keys.iter().any(|k| value.get(*k).is_some()) {
        return;
    }
    // Absent/unparseable target => brand-new queue => every key drops.
    let target: Option<serde_json::Value> = std::fs::read(tgt_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for key in keys {
        if !obj.contains_key(*key) {
            continue;
        }
        match target.as_ref().and_then(|t| t.get(*key)).cloned() {
            Some(v) => {
                obj.insert((*key).to_string(), v);
            }
            None => {
                obj.shift_remove(*key);
            }
        }
    }
}

/// Leave a migrated queue with exactly ONE engine binding.
///
/// A queue names its extraction engine through one of three mutually
/// exclusive fields (`crate::snapshot::limits::QUEUE_ENGINE_FIELDS`), and
/// `PATCH /queues/<id>` with two of them non-null answers `400
/// non_field_errors: Only one of dedicated_engine, generic_engine or engine
/// can be set.`
///
/// Migrate treats the three asymmetrically, and that is what produced an
/// impossible body. `engine` and `dedicated_engine` are deployable content
/// promoted from the source; `generic_engine` is a per-env URL that
/// `strip_for_cross_env_patch` drops so `reconcile_target_identity` restores
/// the TARGET's (its host is the target org's — see the strip's own comment).
/// Promote a source queue bound to a custom `engine` onto a target queue
/// sitting on the built-in generic engine and both halves fire: the source's
/// `engine` lands, the target's `generic_engine` comes back, and the queue now
/// names two engines. Nothing downstream noticed — the snapshot committed
/// clean, and every subsequent `rdc sync` died on the same PATCH, wedging the
/// env (the push phase precedes the pull, so its error aborts the whole cycle).
///
/// The fix is to treat the three as one SLOT rather than three fields: keep the
/// highest-precedence value the reconciled body carries and null the rest.
/// Precedence (`QUEUE_ENGINE_FIELDS`) puts the promoted `engine` ahead of the
/// restored `generic_engine`, so the source's binding wins — the same rule
/// every other deployable field follows. Concretely:
///
/// - source binds a custom `engine` -> `engine` survives, the target's
///   `generic_engine` is cleared. This is exactly the payload the platform's
///   own generic->custom conversion sends, and the server drops the generic
///   binding itself.
/// - source is on the generic engine -> its `engine`/`dedicated_engine` are
///   already null, so the target's restored `generic_engine` is untouched and
///   the queue keeps the built-in engine its own env resolved.
/// - target has no `generic_engine` to restore (a new queue) -> one key, no
///   work.
///
/// Losers are set to `null` rather than removed, because a pulled queue body
/// always carries all three keys and the API counts VALUES, not keys — so the
/// migrated file stays byte-identical in shape to a fresh pull and `git diff`
/// stays honest about what changed.
///
/// An `overlay.toml` can still reintroduce a second binding: overlays run
/// after every reconcile and are documented to win. That is caught offline by
/// `cli::push::scan::ChangeList::queue_engine_conflicts` before the first
/// remote write.
fn reconcile_engine_slot(value: &mut serde_json::Value, kind: &str) {
    if kind != "queues" {
        return;
    }
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let mut kept = false;
    for field in crate::snapshot::limits::QUEUE_ENGINE_FIELDS {
        let Some(current) = obj.get(field) else {
            continue;
        };
        if current.is_null() {
            continue;
        }
        if kept {
            obj.insert(field.to_string(), serde_json::Value::Null);
        } else {
            kept = true;
        }
    }
}

/// Reconcile an inbox's `email_prefix` so migrate never re-addresses the
/// target's mailbox (see `--carry email-prefixes`).
///
/// `email_prefix` is the left-hand side of an inbox's PUBLIC address: Rossum
/// derives `email` as `<email_prefix>-<hash>@<host>`. rdc already treats the
/// derived `email` as env-specific — [`crate::snapshot::create::strip_for_create`]
/// drops it for inboxes, and [`strip_source_host_env_refs`] removes a
/// source-host one — but the field that DETERMINES it was carried verbatim, so
/// promoting a source env whose prefix differs silently rewrote the target's
/// address and broke mail to the old one. That is the outward-facing half of
/// the same problem, so it follows the same rule as
/// [`reconcile_score_thresholds`]:
///
/// - **Target file carries a prefix**: adopt the TARGET's, so each env keeps
///   the address its senders already use (and a deliberate local value is
///   never overwritten).
/// - **Target has no prefix and the object is NEW** (`will_create`, i.e. absent
///   from the target lockfile): keep the SOURCE's. The field is *mandatory* on
///   create and rdc has no other value to offer — see below.
/// - **Target has no prefix and the object is deployed**: drop the field. The
///   push PATCHes, and a PATCH that omits the key leaves the remote's own
///   address untouched.
///
/// The create case is not symmetric with [`reconcile_score_thresholds`] /
/// [`reconcile_target_owned_keys`], which this originally copied: those fields
/// are OPTIONAL on create, so dropping them lets the server apply its default.
/// `email_prefix` is not — `POST /inboxes` answers
/// `400 non_field_errors: One of fields 'email_prefix' or 'email' needs to be
/// provided`, and [`crate::snapshot::create::strip_for_create`] removes `email`
/// for inboxes, so a dropped prefix left a body the API can never accept and
/// every brand-new inbox was unpushable (verified live: a first `migrate` into
/// an empty env aborted the sync mid-push, after the queues were created).
/// Carrying the source's value is safe where overwriting a live one is not: a
/// created mailbox has no senders yet, the address is host-scoped to the target
/// org, and `email_prefix` is NOT unique per org (verified: 7 inboxes in one
/// org share a prefix; the server appends its own `-<6hex>` discriminator).
/// Every carry is reported by [`format_carried_email_prefix_warning`], because
/// a source prefix that names its env (`acme-sandbox`) would otherwise reach a
/// production address unannounced.
///
/// Returns `Some(prefix)` when an inbox the push will CREATE ends up carrying
/// the SOURCE env's prefix — whether this run wrote it or an earlier one did —
/// so the caller can name it in that warning until it is deployed or changed.
///
/// A source inbox that carries no prefix is left alone — this only ever
/// protects a value the target already owns; the push pre-flight
/// (`ChangeList::missing_create_fields`) refuses a create with no prefix at
/// all. Set one deliberately per env with an `[inboxes.<queue-slug>]` entry in
/// the target's `overlay.toml`, which is applied after this runs and therefore
/// always wins.
///
/// A no-op for any kind other than `inboxes`.
fn reconcile_email_prefix(
    value: &mut serde_json::Value,
    kind: &str,
    tgt_path: &Path,
    will_create: bool,
) -> Option<String> {
    const KEY: &str = "email_prefix";
    if kind != "inboxes" {
        return None;
    }
    let obj = value.as_object_mut()?;
    if !obj.contains_key(KEY) {
        return None;
    }
    let source_prefix = obj.get(KEY).and_then(|v| v.as_str()).map(str::to_string);
    let target: Option<serde_json::Value> = std::fs::read(tgt_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    // An explicit `null` is not a prefix: adopting it would send
    // `email_prefix: null` and hit the same 400 as omitting the key.
    let target_prefix = target
        .as_ref()
        .and_then(|t| t.get(KEY))
        .filter(|v| !v.is_null())
        .cloned();
    match target_prefix {
        Some(v) => {
            obj.insert(KEY.to_string(), v);
        }
        None if will_create => {} // keep the source's — mandatory on create
        None => {
            obj.shift_remove(KEY);
        }
    }
    // Report whenever an inbox the push will CREATE ends up addressed with the
    // source env's prefix — not only on the run that first wrote it. The value
    // survives in the target snapshot, so a first-run-only notice would go
    // silent exactly when someone re-runs `migrate && sync` and is the last
    // chance to catch a dev-flavoured prefix before it becomes a live address.
    let final_prefix = obj.get(KEY).and_then(|v| v.as_str()).map(str::to_string);
    if will_create && final_prefix.is_some() && final_prefix == source_prefix {
        return final_prefix;
    }
    None
}

/// The target env's API base, recovered from the organization URL `run` already
/// builds for [`reconcile_target_identity`] (`{api_base}/organizations/{id}`).
/// `None` for any other shape.
fn api_base_of(tgt_org_url: &str) -> Option<&str> {
    tgt_org_url
        .rsplit_once("/organizations/")
        .map(|(base, _)| base)
}

/// Restore a store extension's `hook_template` for a hook the target env has
/// not created yet, retargeted to the target org.
///
/// `hook_template` is a per-env URL — the host is the org's — so the env-field
/// passes drop the source's before it can leak into the target snapshot, and a
/// matched object inherits the target's own. But it is also **mandatory to
/// create the hook**: `POST /hooks/create` refuses a body without it, and
/// [`crate::cli::deploy::store_extensions::check_store_extension_anomaly`]
/// refuses even earlier, so every store extension promoted into a fresh env was
/// unpushable (observed live: a first sync into an empty env died on the first
/// of 16 store hooks, after the queues and custom hooks were already written).
///
/// Restoring the source's value is sound because the template id is the stable
/// cross-environment identity — store templates are Rossum-global, only the
/// host differs per org, which is exactly what
/// [`crate::cli::deploy::store_extensions::retarget_hook_template`] rewrites
/// (verified against a live org: every template id referenced by a source env's
/// snapshot resolved in the target org to a template of the same name). Writing
/// the retargeted URL rather than the source's also keeps the source host out
/// of the target snapshot, so the value matches what a pull of the created hook
/// writes back.
///
/// Only ever fills a GAP: a value already present (a matched target's own, or
/// an overlay's — the overlay runs after this) is left alone, and a hook that
/// is not a store extension never gains the field.
fn reconcile_hook_template(
    value: &mut serde_json::Value,
    src_hook_template: Option<&str>,
    tgt_api_base: Option<&str>,
) {
    const KEY: &str = "hook_template";
    let (Some(src), Some(api_base)) = (src_hook_template, tgt_api_base) else {
        return;
    };
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if obj.get(KEY).and_then(|v| v.as_str()).is_some() {
        return;
    }
    // `POST /hooks/create` is only used for store extensions; a custom hook
    // must not acquire a template link it never had.
    if obj.get("extension_source").and_then(|v| v.as_str()) != Some("rossum_store") {
        return;
    }
    let Some(retargeted) =
        crate::cli::deploy::store_extensions::retarget_hook_template(src, api_base)
    else {
        return;
    };
    obj.insert(KEY.to_string(), serde_json::Value::String(retargeted));
}

/// The `warn` migrate emits for every brand-new inbox that kept the SOURCE
/// env's `email_prefix` (see [`reconcile_email_prefix`]).
///
/// Split out as a pure function so the wording is unit-testable: this is the
/// only notice a user gets that a production mailbox is about to be addressed
/// with a prefix chosen for another env, so it has to name the inbox, the
/// value, and the exact `overlay.toml` key that overrides it.
fn format_carried_email_prefix_warning(
    src: &str,
    tgt: &str,
    carried: &[(String, String)],
) -> String {
    use std::fmt::Write as _;
    let mut msg = format!(
        "{} new inbox(es) in '{tgt}' keep the source env's email_prefix — each public \
         address becomes <prefix>-<hash>@<{tgt} host>. POST /inboxes requires one, so \
         migrate carries '{src}'s rather than write an object the API rejects. Change any \
         of them before syncing, in envs/{tgt}/overlay.toml — a new overlay file also \
         needs `version = 1` (values below are the ones that will be used):",
        carried.len(),
    );
    for (slug, prefix) in carried {
        let _ = write!(msg, "\n  [inboxes.{slug}]\n  email_prefix = \"{prefix}\"");
    }
    msg
}

/// Format ONE aggregate warning naming every promoted organization column
/// whose `schema_id` matches no schema in the target env — mirrors
/// `format_carried_email_prefix_warning`'s shape (a single combined message,
/// emitted once after the file loop) rather than one `log.event` per id.
fn format_org_columns_missing_warning(tgt: &str, missing: &[String]) -> String {
    use std::fmt::Write as _;
    let mut msg = format!(
        "{} organization column(s) name a schema_id absent from every schema in \
         '{tgt}' — the API accepts an unknown id with 200, so nothing else catches \
         this; each will render empty:",
        missing.len(),
    );
    for id in missing {
        let _ = write!(msg, "\n  schema_id `{id}`");
    }
    msg
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
/// - **New** (no target file): strip the server-assigned fields
///   (`UNIVERSAL_SERVER_FIELDS`) to a clean create payload so the subsequent
///   `rdc sync` POSTs.
///
/// Returns whether the object was MATCHED — the target already held it, so
/// every env field in `value` is now the target's own. `transform_file` needs
/// that verdict to decide whether the source-host cleanup still has anything
/// legitimate to do (see `strip_source_host_env_refs`).
fn reconcile_target_identity(
    value: &mut serde_json::Value,
    tgt_path: &Path,
    codec: &'static dyn crate::snapshot::codec::KindCodec,
    tgt_org_url: &str,
) -> bool {
    if !value.is_object() {
        return false;
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

    // Does the source object carry an `organization`? Decides whether the
    // field is set at all — never invent it for a kind whose API body has no
    // such field (a queue, a schema, a hook).
    let had_org = value.get("organization").is_some();

    let tgt_obj = tgt.as_ref().and_then(|t| t.as_object());
    let matched = tgt_obj.is_some();
    let obj = value.as_object_mut().expect("checked is_object above");

    match tgt_obj {
        Some(tobj) => {
            // Matched: take the TARGET's value for every env field (or drop it
            // if the target lacks it). Deployable content stays the source's.
            // `organization` is excluded — it is set from `tgt_org_url` below on
            // BOTH paths, never inherited and never dropped.
            for field in env_fields {
                if field == "organization" {
                    continue;
                }
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
            // them. Leave the rest as the source's transformed content,
            // including portable `rdc://` ref lists — the subst already remapped
            // them to tgt slugs, and the server reconciles reverse-ref lists on
            // create.
            for field in crate::snapshot::create::UNIVERSAL_SERVER_FIELDS {
                obj.shift_remove(*field);
            }
        }
    }

    // `organization` is the one env field whose target value rdc KNOWS offline:
    // `tgt_org_url` is built from the target env's own `api_base` + `org_id` in
    // `rdc.toml`, and the object is being written into that org. So it is set
    // here on both paths rather than inherited from the target file:
    //
    // * a matched target holds that same URL anyway (it was pulled from that
    //   org), so this is a no-op for a healthy snapshot;
    // * a matched target MISSING the field — one an older migrate wrote before
    //   this was fixed — gets it back, instead of inheriting the absence for
    //   ever and 400ing every create with `organization: This field is
    //   required.`;
    // * a new object gets the target org, never the source's (which would be
    //   rejected).
    //
    // Setting an existing key keeps its position in the body, so migrate's bytes
    // stay identical to a fresh target pull's (re-inserting after a removal
    // would append the key at the end and churn the diff for ever).
    if had_org {
        obj.insert(
            "organization".to_string(),
            serde_json::Value::String(tgt_org_url.to_string()),
        );
    }

    matched
}

/// rdc-managed top-level directories under an env root — the same per-kind
/// dirs `paths.rs` exposes and `push::scan` reads. Migrate copies only files
/// WITHIN these. Everything else under `envs/<env>/` — user pytest `tests/`,
/// helper `scripts/`, `README`s, `__pycache__`, and the per-env singletons
/// `_index.md` / `overlay.toml` — is NOT rdc-managed and must be left
/// untouched: a snapshot→snapshot transform has no business copying files rdc
/// neither pulls nor pushes. `organization.json` is also a per-env singleton
/// living outside these dirs, but [`enumerate_files`] adds it back in
/// explicitly: unlike the others it IS promoted (its `settings` subtree only —
/// see [`classify`], `reconcile_target_identity`).
const MANAGED_DIRS: &[&str] = &[
    "hooks",
    "workspaces",
    "rules",
    "labels",
    "saved-views",
    "engines",
    "workflows",
    "mdh",
];

/// Enumerate every rdc-managed snapshot file under `env_root`, returning paths
/// relative to `env_root`. Only descends into [`MANAGED_DIRS`]; any other
/// top-level entry is ignored entirely, except `organization.json` — the
/// env-root singleton — which is appended when present. Within a managed dir,
/// sync shadow artifacts (`<file>.<env>` / `<file>.<env>-deleted`) are skipped
/// via [`should_skip`].
fn enumerate_files(env_root: &Path, env: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for dir in MANAGED_DIRS {
        let managed = env_root.join(dir);
        if managed.exists() {
            walk_dir(env_root, &managed, env, &mut out)?;
        }
    }
    // The organization singleton lives at the env root, outside MANAGED_DIRS, so
    // it is added explicitly rather than by loosening `should_skip` (which also
    // guards `_index.md` / `overlay.toml`, both of which stay excluded).
    if env_root.join("organization.json").exists() {
        out.push(PathBuf::from("organization.json"));
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
/// `.py` (hook / rule / schema-formula code), `.js` (Node.js hook code),
/// `.jsonl` (MDH row data for a dataset flagged `"data": "manual"`, copied
/// verbatim — it is not `.json`, so it does not go through the
/// parse/substitute path). Anything else sitting next to them — `.pyc`
/// bytecode, `.DS_Store`, editor temp files, sync shadow artifacts
/// (`<file>.<env>`) — is foreign to rdc and must not be migrated.
fn is_managed_leaf(name: &str) -> bool {
    matches!(
        name.rsplit_once('.').map(|(_, ext)| ext),
        Some("json") | Some("py") | Some("js") | Some("jsonl")
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

/// One kind in which a `--mirror` run would DELETE a live target object while
/// creating another of the same kind — the shape a slug rename takes when
/// nothing recorded it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RecreateGroup {
    pub kind: &'static str,
    /// `(slug, remote id)` per target object the prune would destroy.
    pub deleted: Vec<(String, u64)>,
    /// Target slugs this run would create.
    pub created: Vec<String>,
}

/// Target-env relative paths the migration WOULD produce: every source file,
/// minus the un-creatable unique-typed email templates, remapped through the
/// oriented mapping. Shared by the `--mirror` prune (everything else in the
/// target is target-only) and by the recreate guard (everything not already on
/// disk is a create).
fn produced_paths(
    src_root: &Path,
    src_env: &str,
    mapping: &Mapping,
    skip: &std::collections::BTreeSet<PathBuf>,
) -> Result<std::collections::BTreeSet<PathBuf>> {
    Ok(enumerate_files(src_root, src_env)?
        .into_iter()
        .filter(|rel| !skip.contains(rel))
        .map(|rel| remap_relative(&rel, mapping))
        .collect())
}

/// Detect the delete-and-recreate shape, per kind.
///
/// A pruned object counts only when the target LOCKFILE still holds a remote id
/// for it: a local-only file the target never pushed has nothing to lose, and
/// `--mirror` removing it destroys no remote state. A pruned `(kind, slug)` the
/// run also produces is not a deletion at all — that is an object moving path
/// within the target (a queue changing workspace), written at its new path in
/// the same run.
///
/// The pairing is deliberately coarse: one live prune plus one create in the
/// same kind. It is not trying to identify WHICH object was renamed — after a
/// create has had its `id` stripped there is nothing left to match on — only to
/// stop a shape that is almost never deliberate, and to stay rare enough that
/// `--allow-recreate` never becomes a permanent fixture of a pipeline.
fn recreate_groups(
    prune: &[PathBuf],
    produced: &std::collections::BTreeSet<PathBuf>,
    tgt_root: &Path,
    tgt_env: &str,
    tgt_lockfile: &crate::state::Lockfile,
) -> Result<Vec<RecreateGroup>> {
    use std::collections::{BTreeMap, BTreeSet};

    let produced_objs: BTreeSet<(&'static str, String)> =
        produced.iter().filter_map(|rel| classify(rel)).collect();
    let existing_objs: BTreeSet<(&'static str, String)> = enumerate_files(tgt_root, tgt_env)?
        .iter()
        .filter_map(|rel| classify(rel))
        .collect();

    let mut created: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    for (kind, slug) in produced_objs.difference(&existing_objs) {
        created.entry(kind).or_default().insert(slug.clone());
    }

    let mut deleted: BTreeMap<&'static str, BTreeMap<String, u64>> = BTreeMap::new();
    for rel in prune {
        let Some((kind, slug)) = classify(rel) else {
            continue;
        };
        if produced_objs.contains(&(kind, slug.clone())) {
            continue; // moved, not deleted
        }
        let Some(id) = tgt_lockfile
            .objects
            .get(kind)
            .and_then(|by_slug| by_slug.get(&slug))
            .map(|e| e.id)
            .filter(|id| *id != 0)
        else {
            continue; // never pushed: no remote object to lose
        };
        deleted.entry(kind).or_default().insert(slug, id);
    }

    Ok(deleted
        .into_iter()
        .filter_map(|(kind, dels)| {
            let created = created.get(kind)?;
            Some(RecreateGroup {
                kind,
                deleted: dels.into_iter().collect(),
                created: created.iter().cloned().collect(),
            })
        })
        .collect())
}

/// The refusal. Names every object at risk, and the exact row that turns the
/// pair back into a rename.
fn format_recreate_error(groups: &[RecreateGroup], src: &str, tgt: &str) -> String {
    let mut body = String::new();
    for g in groups {
        let dels: Vec<String> = g
            .deleted
            .iter()
            .map(|(slug, id)| format!("{slug} (id {id})"))
            .collect();
        body.push_str(&format!(
            "  {}: delete {} | create {}\n",
            g.kind,
            dels.join(", "),
            g.created.join(", "),
        ));
    }
    // One worked example, from the first pair — enough to copy, short enough
    // to read.
    let sample = groups
        .first()
        .and_then(|g| Some((g.kind, g.deleted.first()?.0.clone(), g.created.first()?.clone())))
        .map(|(kind, old, new)| {
            format!("\n  [[{kind}]]\n  {src} = \"{new}\"\n  {tgt} = \"{old}\"\n")
        })
        .unwrap_or_default();
    format!(
        "--mirror would DELETE live '{tgt}' objects while creating others of the same kind:\n\
         {body}\n\
         That is what a slug rename looks like when nothing recorded it: migrate strips \
         `id`/`url` from an object it creates, so no file under envs/{tgt}/ ties the new slug \
         to the object '{tgt}' already has. Deleting a queue takes its documents with it.\n\
         \n\
         If these are the SAME objects under a new name, record each one in \
         .rdc/mapping.toml:\n\
         {sample}\n\
         `rdc doctor {src}` writes those rows itself when it renames a slug. If the objects \
         really are unrelated, re-run with --allow-recreate."
    )
}

/// Plan a `--mirror` prune: target-env relative paths that would NOT be
/// produced by migrating the source snapshot. These are tgt-only objects the
/// user must delete to make tgt mirror src exactly. The returned paths are
/// relative to the *target* env root. `skip` holds source-relative paths the
/// migration refuses to produce (un-creatable unique-typed email-template
/// duplicates) — their target counterparts count as NOT produced, so a stale
/// copy left by an earlier migrate gets pruned. `organization.json` is always
/// exempt — a per-env singleton is never a "target-only object", even when the
/// source env has no org file of its own to have produced it from.
fn mirror_prune_paths(
    src_root: &Path,
    src_env: &str,
    tgt_root: &Path,
    tgt_env: &str,
    mapping: &Mapping,
    skip: &std::collections::BTreeSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let produced = produced_paths(src_root, src_env, mapping, skip)?;
    let existing = enumerate_files(tgt_root, tgt_env)?;
    Ok(existing
        .into_iter()
        // A per-env singleton is never a "target-only object": the target's org
        // file must survive even when the source env has never been pulled.
        .filter(|rel| rel != Path::new("organization.json"))
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

/// One group of fields the TARGET env owns on migrate, as named on the CLI by
/// `--carry <GROUP>`.
///
/// Every value is a field — or a small set of fields — that migrate leaves to
/// the target env by default, because it is tuned per organization rather than
/// promoted with the solution. Naming the group carries the SOURCE env's
/// values instead, which is the pre-reconcile behavior.
///
/// `training_enabled` is deliberately absent. It is always the target's, with
/// no opt-in: Rossum resets it to `false` on queue creation, so carrying it
/// would make every migrate+sync conflict, and there is no case for blindly
/// propagating a training toggle across orgs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CarryGroup {
    /// A schema datapoint's `score_threshold` and a queue's
    /// `default_score_threshold`.
    ScoreThresholds,
    /// An inbox's `email_prefix`.
    EmailPrefixes,
    /// A queue's `automation_enabled`, `automation_level` and
    /// `quality_spot_check_percentage`.
    Automation,
    /// Every group above. A group added later widens it, which is the intended
    /// reading of "carry everything the target normally owns".
    All,
}

/// The resolved set of [`CarryGroup`]s for one migrate run.
///
/// [`Carry::NONE`] — every group left to the target — is the default, and what
/// an unflagged `rdc migrate` uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Carry {
    pub score_thresholds: bool,
    pub email_prefixes: bool,
    pub automation: bool,
}

impl Carry {
    /// Carry nothing: the target env owns every group.
    pub const NONE: Carry = Carry {
        score_thresholds: false,
        email_prefixes: false,
        automation: false,
    };

    /// Carry only the score thresholds — a schema datapoint's
    /// `score_threshold` and a queue's `default_score_threshold` — leaving the
    /// other groups target-owned. This is the behavior that predates the
    /// threshold reconcile, which is why most migrate tests use it: a test
    /// asserting on some unrelated field is not perturbed by threshold
    /// handling.
    pub const SCORE_THRESHOLDS: Carry = Carry {
        score_thresholds: true,
        email_prefixes: false,
        automation: false,
    };

    /// Turn on the single group `group` names.
    fn set(&mut self, group: CarryGroup) {
        match group {
            CarryGroup::ScoreThresholds => self.score_thresholds = true,
            CarryGroup::EmailPrefixes => self.email_prefixes = true,
            CarryGroup::Automation => self.automation = true,
            // `All` is literally every other variant, read off the enum rather
            // than hand-listed — so a group added later widens it for free,
            // which is what its doc comment promises. Recurses exactly one
            // level: every variant it forwards to is a non-`All` leaf.
            CarryGroup::All => {
                for variant in <CarryGroup as clap::ValueEnum>::value_variants() {
                    if !matches!(variant, CarryGroup::All) {
                        self.set(*variant);
                    }
                }
            }
        }
    }

    /// Resolve the repeated / comma-separated `--carry` values. Unknown values
    /// never reach here — clap rejects them against the [`CarryGroup`] enum.
    pub fn from_groups(groups: &[CarryGroup]) -> Self {
        let mut carry = Carry::NONE;
        for group in groups {
            carry.set(*group);
        }
        carry
    }
}

/// `rdc migrate <src> <tgt>` — pure-local snapshot→snapshot transform.
///
/// Copies `envs/<src>/` into `envs/<tgt>/`, renaming slugs per the hand-authored,
/// oriented [`Mapping`] (projected from `.rdc/mapping.toml`), substituting
/// `rdc://<kind>/<src>` refs to their `<tgt>` slug in JSON content, and applying
/// the target overlay. Makes zero remote calls — afterward the user reviews
/// `git diff` and runs `rdc sync <tgt>`.
///
/// What a run does with target objects the source no longer produces.
///
/// Replaces what used to be a bare `mirror: bool` sitting next to `dry_run`:
/// with a third flag in play, three adjacent booleans at every call site are
/// freely interchangeable, and the compiler cannot tell them apart (the same
/// reasoning that produced `ProjectedPaths`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorMode {
    /// The default: extras in the target are left intact.
    Additive,
    /// `--mirror`: prune target-only objects. `allow_recreate` waives the
    /// guard that refuses to prune a LIVE target object while creating one of
    /// the same kind — see `recreate_groups`.
    Mirror { allow_recreate: bool },
}

impl MirrorMode {
    pub fn is_mirror(self) -> bool {
        matches!(self, MirrorMode::Mirror { .. })
    }

    fn allows_recreate(self) -> bool {
        matches!(self, MirrorMode::Mirror { allow_recreate: true })
    }
}

/// `--mirror` additionally deletes target-only objects (files present in tgt
/// but not produced by the migration). `--dry-run` prints the plan and writes
/// nothing. `--only <selector>` narrows the operation to matching
/// `<kind>/<slug>` objects (reusing deploy's selection machinery).
pub fn run(
    src: &str,
    tgt: &str,
    mirror: MirrorMode,
    dry_run: bool,
    only: Vec<String>,
    carry: Carry,
) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    run_at(&cwd, src, tgt, mirror, dry_run, only, carry)
}

/// Like [`run`], but takes the project root explicitly instead of reading
/// the process's current directory — this is the cwd-parameterised form
/// that [`run`] itself calls, so nothing here has to shell out to a
/// `std::env::set_current_dir` dance.
pub fn run_at(
    cwd: &Path,
    src: &str,
    tgt: &str,
    mirror_mode: MirrorMode,
    dry_run: bool,
    only: Vec<String>,
    carry: Carry,
) -> Result<()> {
    let mirror = mirror_mode.is_mirror();
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
    let tgt_env_cfg = project_cfg.env_or_err(tgt)?;
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

    // Raw numeric object ids embedded in deployable content follow the same slug
    // mapping their `rdc://` counterparts do. Built once for the whole run; an
    // id no single kind owns is left out and warned about rather than guessed.
    let id_remap = build_id_remap(&src_lockfile, &tgt_lockfile, &subst);
    if !id_remap.ambiguous.is_empty() {
        let listing: Vec<String> = id_remap
            .ambiguous
            .iter()
            .map(|(id, owners)| format!("  {id}: {}", owners.join(", ")))
            .collect();
        log.event(
            crate::log::Action::Warn,
            &format!(
                "{} object id(s) are claimed by more than one kind in '{src}' and are NOT \
                 remapped — a bare id in a settings blob carries no kind, so remapping \
                 could point a field at the wrong object. Pin these explicitly in \
                 envs/{tgt}/overlay.toml if any object references them:\n{}",
                id_remap.ambiguous.len(),
                listing.join("\n"),
            ),
        );
    }

    // `--mirror` plans its prune BEFORE the first write, so the recreate guard
    // can refuse without leaving a half-written target tree behind. Planning
    // early is equivalent to planning late: the plan is the source enumeration
    // remapped, subtracted from the target tree, and every path the write loop
    // adds to that tree is by construction one the migration produces — which
    // is exactly what `mirror_prune_paths` filters out.
    let prune_plan: Vec<PathBuf> = if mirror {
        mirror_prune_paths(&src_root, src, &tgt_root, tgt, &mapping, &unique_tpl_skips)?
    } else {
        Vec::new()
    };
    if mirror && !mirror_mode.allows_recreate() {
        let produced = produced_paths(&src_root, src, &mapping, &unique_tpl_skips)?;
        let groups = recreate_groups(&prune_plan, &produced, &tgt_root, tgt, &tgt_lockfile)?;
        if !groups.is_empty() {
            anyhow::bail!(format_recreate_error(&groups, src, tgt));
        }
    }

    // `considered` is every file the migration looked at; `written` is the
    // subset whose bytes actually changed on disk. `obj_status` aggregates the
    // per-file outcomes into per-object create / update / unchanged tallies.
    let mut considered = 0usize;
    let mut written = 0usize;
    let mut renamed = 0usize;
    let mut obj_status: BTreeMap<(&'static str, String), ObjStatus> = BTreeMap::new();
    let mut id_hits: Vec<(String, u64, u64)> = Vec::new();
    let mut carried_prefixes: Vec<(String, String)> = Vec::new();
    let mut missing_schema_ids: Vec<String> = Vec::new();
    // `(target slug, post-overlay body)` for every saved view this run
    // actually promoted — collected here, post-`--only`, post-skip, so the
    // ref check below covers exactly what was written and nothing else.
    // `transform_file` pushes the value itself (post-overlay), so both
    // `--dry-run` and a real run validate the same body without reading
    // anything back off disk.
    let mut promoted: Vec<(&'static str, String, serde_json::Value)> = Vec::new();
    // Every path this run writes, needed by `projected_known`. Collected for
    // EVERY file, not only changed ones: an unchanged target file is still part
    // of what the target tree contains.
    let mut would_write: Vec<PathBuf> = Vec::new();

    for rel in &files {
        // Un-creatable duplicate unique-typed email templates (see above).
        if unique_tpl_skips.contains(rel) {
            continue;
        }
        // `--only`: keep a file only when its classified (kind, slug) is in the
        // selection. Files with no classifiable object (workflows) are
        // skipped under an active selection — the user narrowed scope. `mdh`
        // IS classifiable (see `classify_for_selection`'s `Some("mdh") if
        // comps.len() >= 3` arm) — a dataset became selectable once MDH row
        // data shipped, so `--only mdh/<slug>` carries collection.json /
        // indexes.json / data.jsonl together.
        if let Some(sel) = &selection {
            match classify_for_selection(rel) {
                Some((kind, slug)) if sel.contains(kind, &slug) => {}
                _ => continue,
            }
        }

        // Organization promotion writes the source's `settings` into the
        // TARGET's own org object: `reconcile_target_identity` restores
        // id/url/name/ui_settings/metadata from the target's file. With no
        // target file there is nothing to restore, so skip rather than emit a
        // settings-only `organization.json` no pull would ever produce.
        if rel.as_path() == Path::new("organization.json") {
            if !tgt_root.join(rel).exists() {
                log.event(
                    crate::log::Action::Warn,
                    &format!(
                        "envs/{tgt}/organization.json does not exist yet — organization \
                         settings not promoted; run `rdc sync {tgt}` to pull it first"
                    ),
                );
                continue;
            }

            // Symmetric guard on the SOURCE side. `reconcile_target_identity`
            // derives its "env field" set from `cross_env_body`'s retain-list,
            // which only keeps a top-level `settings` key when the SOURCE body
            // has one. With no source `settings` (missing or `null` — treated
            // the same, see the push driver), that set is empty: every key the
            // source DOES have gets restored from the target, but `settings`
            // itself was never a source key, so it is never re-inserted — the
            // promoted file loses the target's own `settings` outright, a
            // state no pull would ever produce. Skip instead: leave the
            // target's `organization.json` byte-untouched, exactly as if this
            // file were absent from the migration.
            let src_has_settings = std::fs::read(src_root.join(rel))
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .is_some_and(|v| v.get("settings").is_some_and(|s| !s.is_null()));
            if !src_has_settings {
                log.event(
                    crate::log::Action::Warn,
                    &format!(
                        "envs/{src}/organization.json has no `settings` key — organization \
                         settings not promoted; the target's own `settings` is left untouched"
                    ),
                );
                continue;
            }
        }

        let dst_rel = remap_relative(rel, &mapping);
        would_write.push(dst_rel.clone());
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
        let hits_before = id_hits.len();
        let outcome = transform_file(
            rel,
            &src_root,
            &tgt_root,
            &mapping,
            &subst,
            tgt_overlay.as_ref(),
            &tgt_org_url,
            carry,
            &src_lockfile,
            &tgt_lockfile,
            dry_run,
            &id_remap,
            &mut id_hits,
            &mut carried_prefixes,
            tgt,
            &mut missing_schema_ids,
            &mut promoted,
        )
        .with_context(|| format!("migrating {}", rel.display()))?;
        if outcome != FileOutcome::Unchanged {
            written += 1;
        }
        // Report the id rewrites this file needed. Silent remapping would be the
        // very thing that made the original bug invisible, so each one is named.
        if id_hits.len() > hits_before {
            let listing: Vec<String> = id_hits[hits_before..]
                .iter()
                .map(|(path, from, to)| format!("  {path}: {from} -> {to}"))
                .collect();
            log.event(
                crate::log::Action::Plan,
                &format!(
                    "{} remapped {} object id(s):\n{}",
                    dst_rel.display(),
                    listing.len(),
                    listing.join("\n"),
                ),
            );
        }
        record_object_status(&mut obj_status, &dst_rel, outcome);
    }

    // Every brand-new inbox that inherited the source env's public address
    // prefix, named once for the whole run. Emitted in `--dry-run` too: the
    // transform runs in both modes, and forecasting the address is exactly what
    // the dry run is for.
    if !carried_prefixes.is_empty() {
        log.event(
            crate::log::Action::Warn,
            &format_carried_email_prefix_warning(src, tgt, &carried_prefixes),
        );
    }

    // Every promoted organization column whose `schema_id` names a field no
    // target schema defines, folded into ONE aggregate warning (never one
    // `log.event` per id — see `format_org_columns_missing_warning`). Emitted
    // in `--dry-run` too, like the carried-prefix warning above.
    if !missing_schema_ids.is_empty() {
        missing_schema_ids.sort();
        missing_schema_ids.dedup();
        log.event(
            crate::log::Action::Warn,
            &format_org_columns_missing_warning(tgt, &missing_schema_ids),
        );
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
    // Hoisted out of the `if mirror` block below so the saved-view validation
    // can subtract it from `known` regardless of whether `--mirror` ran.
    let mut pruned_rels: Vec<PathBuf> = Vec::new();
    if mirror {
        let prune = prune_plan;
        pruned_rels = prune.clone();
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

    // Saved views resolve strictly — see `check_saved_view_refs`.
    //
    // Scoped to the views this run PROMOTED, never to every saved view the
    // target tree holds. A target-native view is allowed to carry a raw user
    // URL in its `query` (it is valid in its own env, and push says so), so
    // checking the whole tree turned one such view into a hard error on every
    // migrate — including runs whose source has no saved views at all.
    //
    // ONE path for both modes, over a PROJECTED target set: what the target
    // tree already has, plus what this run writes, minus what `--mirror`
    // prunes. For a real run this is equivalent to enumerating the target tree
    // after the writes; for `--dry-run` (which writes nothing) it is the only
    // correct answer — a disk-only `known` would miss every object this run
    // would create and call each of its refs unresolvable. `transform_file`
    // pushed the POST-OVERLAY body above, so there is nothing to read back
    // either: the overlay escape hatch is covered for free.
    //
    // The SOURCE env's host, so a ref carrying it is recognized on an
    // api_base with no `/api/v1/` segment (see `check_saved_view_refs`). The
    // lockfile is authoritative when present; `rdc.toml` covers a source
    // snapshot that has never been synced (no lockfile on disk).
    let src_host = url_host(&src_lockfile.api_base)
        .or_else(|| project_cfg.envs.get(src).and_then(|c| url_host(&c.api_base)));
    if !promoted.is_empty() {
        let existing = enumerate_files(&tgt_root, tgt)?;
        let known = projected_known(ProjectedPaths {
            existing: &existing,
            would_write: &would_write,
            pruned: &pruned_rels,
        });
        let mut problems: Vec<SavedViewRefProblem> = Vec::new();
        for (kind, slug, value) in &promoted {
            if *kind == "saved_views" {
                problems.extend(check_saved_view_refs(
                    slug,
                    value,
                    &known,
                    src_host.as_deref(),
                ));
            }
        }
        if !problems.is_empty() {
            anyhow::bail!(format_saved_view_ref_error(&problems, tgt));
        }

        // A queue this run will CREATE in the target must bring its schema.
        // `POST /queues` answers `400 schema: This field is required.`, so a
        // queue whose `schema` names something the target will not hold is a
        // snapshot that can never be pushed — and the push only finds out
        // after it has already written the run's workspaces and schemas.
        //
        // In practice this is `--only`: the selectors are per-object by
        // design, so `--only queues/<slug>` promotes the queue and nothing
        // else. That is the right behavior for a queue the target already has
        // (its PATCH simply leaves the remote's schema alone) and a dead end
        // for one it does not, which is why the check keys off the target
        // LOCKFILE rather than the selection: an object the target has never
        // deployed is the one the push will POST.
        //
        // `known` is the same projected set the saved-view check uses, so a
        // schema this very run writes counts — the whole-snapshot migrate that
        // carries both queue and schema is unaffected.
        let mut orphans: Vec<(String, String)> = Vec::new();
        for (kind, slug, value) in &promoted {
            if *kind != "queues" {
                continue;
            }
            if tgt_lockfile.objects.get("queues").and_then(|m| m.get(slug)).is_some() {
                continue; // deployed already: a PATCH, and `schema` may be omitted
            }
            let Some(schema_ref) = value.get("schema").and_then(|v| v.as_str()) else {
                continue; // absent/null is `push`'s missing-create-field check
            };
            let Some((rk, rs)) = crate::snapshot::refs::parse_rdc_ref(schema_ref) else {
                continue; // a concrete URL is the user's own doing
            };
            if !known.contains(&(rk.to_string(), rs.to_string())) {
                orphans.push((slug.clone(), schema_ref.to_string()));
            }
        }
        if !orphans.is_empty() {
            use std::fmt::Write as _;
            let mut msg = format!(
                "{} queue(s) would be created in envs/{tgt} without a schema; \
                 `POST /queues` requires one:",
                orphans.len()
            );
            for (slug, schema_ref) in &orphans {
                let _ = write!(msg, "\n  - queues/{slug} -> {schema_ref} (not in envs/{tgt})");
            }
            let _ = write!(
                msg,
                "\n  `--only` is per-object and does not carry a queue's schema along. \
                 Add the schema to the selection (e.g. `--only schemas/<slug>`, or \
                 `--only '*/<slug>'` for both) or migrate without `--only`."
            );
            anyhow::bail!(msg);
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
        "saved-views" if comps.len() == 2 => remap_flat_leaf(&comps, "saved_views", mapping),
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
    /// `std::env::current_dir()` — the whole reason `run` (the CLI entry
    /// point) can call it without a `std::env::set_current_dir` dance. Builds
    /// a tiny two-env project in a tempdir unrelated to the process cwd, and
    /// asserts both that the dry-run succeeds and that the process cwd never
    /// moved.
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

        let result = run_at(root, "dev", "prod", MirrorMode::Additive, true /* dry_run */, vec![], Carry::NONE);

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
        // The organization singleton resolves to its own object too — migrate
        // promotes its `settings` subtree (via `classify`), so a file outcome
        // for it must be attributable to something, like every other kind.
        assert_eq!(
            owning_object(Path::new("organization.json")),
            Some(("organization", "self".to_string()))
        );
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

    // ---- object-id remap ------------------------------------------------

    fn entry(id: u64) -> crate::state::ObjectEntry {
        crate::state::ObjectEntry { id, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None }
    }

    #[test]
    fn id_remap_follows_the_slug_rename() {
        use crate::state::Lockfile;
        let mut src = Lockfile::default();
        src.upsert("queues", "invoices", entry(100));
        let mut tgt = Lockfile::default();
        tgt.upsert("queues", "invoices-prod", entry(200));

        let mut m = Mapping::default();
        m.queues.insert("invoices".into(), "invoices-prod".into());

        let remap = build_id_remap(&src, &tgt, &build_subst(&m));
        assert_eq!(remap.map.get(&100), Some(&200), "id must follow its slug rename");
        assert!(remap.ambiguous.is_empty());
    }

    #[test]
    fn id_remap_refuses_cross_kind_ambiguous_id() {
        use crate::state::Lockfile;
        // Rossum ids are unique only WITHIN a kind, and a bare integer in a
        // settings blob carries no kind. Remapping 1010 here could turn a label
        // reference into a rule reference, so it must be refused, not guessed.
        let mut src = Lockfile::default();
        src.upsert("rules", "needs-review", entry(1010));
        src.upsert("labels", "urgent", entry(1010));
        src.upsert("queues", "invoices", entry(100));
        let mut tgt = Lockfile::default();
        tgt.upsert("rules", "needs-review", entry(2010));
        tgt.upsert("labels", "urgent", entry(3010));
        tgt.upsert("queues", "invoices", entry(200));

        let remap = build_id_remap(&src, &tgt, &build_subst(&Mapping::default()));
        assert!(!remap.map.contains_key(&1010), "kind-ambiguous id must NOT be remapped");
        assert_eq!(remap.map.get(&100), Some(&200), "unambiguous ids still map");
        let owners = &remap.ambiguous.iter().find(|(id, _)| *id == 1010).expect("reported").1;
        assert_eq!(owners, &vec!["labels/urgent".to_string(), "rules/needs-review".to_string()]);
    }

    #[test]
    fn id_remap_allows_one_id_shared_by_several_slugs_of_one_kind() {
        use crate::state::Lockfile;
        // A schema shared by several queues is snapshotted once per consuming
        // queue, so one remote id legitimately appears under several slugs.
        // That is not ambiguous while the slugs agree on the target.
        let mut src = Lockfile::default();
        src.upsert("schemas", "invoices", entry(500));
        src.upsert("schemas", "credit-notes", entry(500));
        let mut tgt = Lockfile::default();
        tgt.upsert("schemas", "invoices", entry(600));
        tgt.upsert("schemas", "credit-notes", entry(600));

        let remap = build_id_remap(&src, &tgt, &build_subst(&Mapping::default()));
        assert_eq!(remap.map.get(&500), Some(&600));
        assert!(remap.ambiguous.is_empty());
    }

    #[test]
    fn id_remap_refuses_when_shared_slugs_disagree_on_target() {
        use crate::state::Lockfile;
        let mut src = Lockfile::default();
        src.upsert("schemas", "invoices", entry(500));
        src.upsert("schemas", "credit-notes", entry(500));
        let mut tgt = Lockfile::default();
        tgt.upsert("schemas", "invoices", entry(600));
        tgt.upsert("schemas", "credit-notes", entry(601)); // diverged in target

        let remap = build_id_remap(&src, &tgt, &build_subst(&Mapping::default()));
        assert!(!remap.map.contains_key(&500), "conflicting targets must not be guessed");
        assert_eq!(remap.ambiguous.len(), 1);
    }

    #[test]
    fn id_remap_skips_objects_absent_from_the_target() {
        use crate::state::Lockfile;
        // Two-phase: the object does not exist in the target env yet. Leave the
        // id alone; `rdc sync` creates the object and the next migrate maps it.
        let mut src = Lockfile::default();
        src.upsert("email_templates", "q/notify", entry(700));
        let remap = build_id_remap(&src, &Lockfile::default(), &build_subst(&Mapping::default()));
        assert!(remap.map.is_empty());
        assert!(remap.ambiguous.is_empty(), "absent is not ambiguous");
    }

    #[test]
    fn id_remap_skips_non_portable_kinds() {
        use crate::state::Lockfile;
        // `organization` is a per-env singleton reconciled separately and
        // `mdh_indexes` carry the sentinel id 0.
        let mut src = Lockfile::default();
        src.upsert("organization", "organization", entry(11));
        src.upsert("mdh_indexes", "vendors", entry(0));
        let mut tgt = Lockfile::default();
        tgt.upsert("organization", "organization", entry(22));
        tgt.upsert("mdh_indexes", "vendors", entry(0));

        let remap = build_id_remap(&src, &tgt, &build_subst(&Mapping::default()));
        assert!(remap.map.is_empty(), "non-portable kinds must not take part");
    }

    #[test]
    fn remap_rewrites_numbers_and_digit_strings_preserving_type() {
        let codec = crate::snapshot::codec::codec("hooks").unwrap();
        let remap = IdRemap { map: BTreeMap::from([(100, 200)]), ambiguous: vec![] };
        let mut v = serde_json::json!({
            "name": "Duplicates",
            "settings": {
                // Duplicate Handling stores ints; file-storage-import stores strings.
                "scope": { "ids": [100] },
                "target_queue": 100,
                "notifications": [{ "queue_id": "100" }],
                "grace_hours": 2
            }
        });
        let mut hits = Vec::new();
        remap_object_ids(&mut v, codec, &remap, &mut hits);

        assert_eq!(v["settings"]["scope"]["ids"][0], serde_json::json!(200));
        assert_eq!(v["settings"]["target_queue"], serde_json::json!(200));
        assert_eq!(
            v["settings"]["notifications"][0]["queue_id"],
            serde_json::json!("200"),
            "a digit-string id must stay a string"
        );
        assert_eq!(v["settings"]["grace_hours"], serde_json::json!(2), "non-ids untouched");
        assert_eq!(hits.len(), 3, "every rewrite is reported: {hits:?}");
    }

    #[test]
    fn remap_leaves_env_identity_fields_alone() {
        let codec = crate::snapshot::codec::codec("hooks").unwrap();
        // After `reconcile_target_identity` these already hold the TARGET's
        // values, and a target id can coincide with an unrelated source id.
        let remap = IdRemap { map: BTreeMap::from([(100, 200)]), ambiguous: vec![] };
        let mut v = serde_json::json!({
            "id": 100,
            "url": "https://acme-test.rossum.app/api/v1/hooks/100",
            "organization": 100,
            "token_owner": "https://acme-test.rossum.app/api/v1/users/100",
            "settings": { "queue_id": 100 }
        });
        let mut hits = Vec::new();
        remap_object_ids(&mut v, codec, &remap, &mut hits);

        assert_eq!(v["id"], serde_json::json!(100), "identity id must not be remapped");
        assert_eq!(v["organization"], serde_json::json!(100), "env field untouched");
        assert_eq!(v["settings"]["queue_id"], serde_json::json!(200), "deployable content mapped");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn remap_ignores_digit_strings_that_do_not_round_trip() {
        let codec = crate::snapshot::codec::codec("hooks").unwrap();
        let remap = IdRemap { map: BTreeMap::from([(7, 9)]), ambiguous: vec![] };
        let mut v = serde_json::json!({ "settings": { "code": "007", "real": "7" } });
        let mut hits = Vec::new();
        remap_object_ids(&mut v, codec, &remap, &mut hits);
        assert_eq!(v["settings"]["code"], serde_json::json!("007"), "\"007\" is not id 7");
        assert_eq!(v["settings"]["real"], serde_json::json!("9"));
    }

    #[test]
    fn transform_remaps_settings_ids_but_an_overlay_pin_still_wins() {
        use crate::state::Lockfile;
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();

        let rel = Path::new("hooks/duplicates.json");
        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        fs::write(
            &src_file,
            serde_json::to_vec(&serde_json::json!({
                "name": "Duplicates",
                "settings": { "scope": { "ids": [100] }, "pinned_queue": 100 }
            }))
            .unwrap(),
        )
        .unwrap();

        let mut src_lock = Lockfile::default();
        src_lock.upsert("queues", "invoices", entry(100));
        let mut tgt_lock = Lockfile::default();
        tgt_lock.upsert("queues", "invoices", entry(200));

        let m = Mapping::default();
        let subst = build_subst(&m);
        let remap = build_id_remap(&src_lock, &tgt_lock, &subst);

        // The overlay pins one field to a deliberate value; it must beat the
        // automatic remap, which is the documented escape hatch for an integer
        // that only looks like a reference.
        let overlay: Overlay = toml::from_str(
            "version = 1\n[hooks.duplicates]\nsettings.pinned_queue = 999\n",
        )
        .unwrap();

        let mut hits = Vec::new();
        transform_file(
            rel,
            src.path(),
            tgt.path(),
            &m,
            &subst,
            Some(&overlay),
            "https://tgt.example/api/v1/organizations/2",
            Carry::SCORE_THRESHOLDS,
            &src_lock,
            &crate::state::Lockfile::default(),
            false,
            &remap,
            &mut hits,
            &mut Vec::new(),
            "tgt",
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_slice(&fs::read(tgt.path().join(rel)).unwrap()).unwrap();
        assert_eq!(
            out["settings"]["scope"]["ids"][0],
            serde_json::json!(200),
            "un-pinned id must be remapped to the target env"
        );
        assert_eq!(
            out["settings"]["pinned_queue"],
            serde_json::json!(999),
            "an explicit overlay pin must override the remap"
        );
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
            // Inside managed dirs but not rdc's:
            "hooks/__pycache__/extractor.cpython-312.pyc",
            "workspaces/main/queues/inv/formulas/__pycache__/f.cpython-312.pyc",
            "hooks/.DS_Store",
            // Sync shadow artifact (must stay skipped):
            "hooks/extractor.json.test",
        ];
        // The one deliberate exception: `organization.json` sits outside
        // `MANAGED_DIRS` at the env root like the other per-env singletons
        // above, but migrate now promotes it (its `settings` subtree — see
        // `classify`), so `enumerate_files` adds it back in explicitly.
        let org_file = "organization.json";
        for rel in managed.iter().chain(non_managed.iter()).chain([&org_file]) {
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
        assert!(
            got.contains(org_file),
            "organization.json is the one non-MANAGED_DIRS entry that IS \
             enumerated — migrate promotes its settings subtree; got {got:?}"
        );
        for rel in non_managed {
            assert!(
                !got.contains(rel),
                "non-rdc-managed entry {rel} must NOT be enumerated; got {got:?}"
            );
        }
    }

    /// `data.jsonl` is how manual MDH rows travel between envs. Without `jsonl`
    /// as a managed leaf, migrate silently drops it and replication is a no-op.
    #[test]
    fn is_managed_leaf_accepts_jsonl_row_data() {
        assert!(is_managed_leaf("data.jsonl"));
        assert!(is_managed_leaf("collection.json"));
        assert!(is_managed_leaf("extractor.py"));
        assert!(!is_managed_leaf("notes.txt"));
        assert!(!is_managed_leaf("data.jsonl.bak"));
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

    /// Two envs sharing ONE API host — two organizations inside a single Rossum
    /// instance (`https://acme.rossum.app/api/v1` with `org_id` 1 and 2), which
    /// is how a customer-hosted org pair is addressed. The source-host cleanup
    /// cannot tell a source ref from a target one there, and used to delete the
    /// `organization` `reconcile_target_identity` had just set to the TARGET org
    /// — so migrate wrote a body the API rejects with
    /// `organization: This field is required.` on `POST /workspaces`.
    #[test]
    fn shared_host_migrate_keeps_the_target_organization() {
        use std::fs;
        const HOST: &str = "https://acme.rossum.app/api/v1";
        let tgt_org = format!("{HOST}/organizations/2");

        let mut m = Mapping::default();
        m.workspaces.insert("main".into(), "main".into());
        let subst = build_subst(&m);
        let rel = Path::new("workspaces/main/workspace.json");

        let src_lf =
            crate::state::Lockfile { api_base: HOST.into(), ..Default::default() };

        // `tgt_seed`: what already sits at the target path — None for a brand-new
        // object, Some(bytes) for a matched one.
        let run = |tgt_seed: Option<Vec<u8>>| -> serde_json::Value {
            let src = tempfile::TempDir::new().unwrap();
            let tgt = tempfile::TempDir::new().unwrap();
            let src_file = src.path().join(rel);
            fs::create_dir_all(src_file.parent().unwrap()).unwrap();
            fs::write(
                &src_file,
                serde_json::to_vec(&serde_json::json!({
                    "id": 111,
                    "url": "rdc://workspaces/main",
                    "name": "Main",
                    "organization": format!("{HOST}/organizations/1"),
                    "queues": [],
                    "metadata": {},
                }))
                .unwrap(),
            )
            .unwrap();
            let dst = tgt.path().join(rel);
            if let Some(bytes) = tgt_seed {
                fs::create_dir_all(dst.parent().unwrap()).unwrap();
                fs::write(&dst, bytes).unwrap();
            }
            transform_file(
                rel,
                src.path(),
                tgt.path(),
                &m,
                &subst,
                None,
                &tgt_org,
                Carry::SCORE_THRESHOLDS,
                &src_lf,
                &crate::state::Lockfile::default(),
                false,
                &IdRemap::default(),
                &mut Vec::new(),
                &mut Vec::new(),
                "tgt",
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .unwrap();
            serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap()
        };

        // New in tgt: carries the TARGET org, never the source's.
        let created = run(None);
        assert_eq!(
            created.get("organization").and_then(|o| o.as_str()),
            Some(tgt_org.as_str()),
            "a new object must carry the target org: {created}"
        );
        // Key order matches a fresh pull's (org right after `name`), so migrate
        // and pull agree byte-for-byte.
        assert_eq!(
            created.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["name", "organization", "queues", "metadata"],
            "organization must keep its position, not be appended: {created}"
        );

        // Matched, and the target file is MISSING `organization` — exactly what
        // an older migrate left behind. The field is restored, not inherited as
        // absent, so the project heals on the next migrate instead of failing
        // every create for ever.
        let healed = run(Some(
            serde_json::to_vec(&serde_json::json!({ "name": "Main", "queues": [], "metadata": {} }))
                .unwrap(),
        ));
        assert_eq!(
            healed.get("organization").and_then(|o| o.as_str()),
            Some(tgt_org.as_str()),
            "a matched target missing the field must get the target org back: {healed}"
        );

        // Matched with a CONTAMINATED target org (an earlier leaky migrate wrote
        // the source org's URL): the target env's own org wins.
        let cleaned = run(Some(
            serde_json::to_vec(&serde_json::json!({
                "name": "Main",
                "organization": format!("{HOST}/organizations/1"),
                "queues": [],
                "metadata": {},
            }))
            .unwrap(),
        ));
        assert_eq!(
            cleaned.get("organization").and_then(|o| o.as_str()),
            Some(tgt_org.as_str()),
            "a contaminated target org must be replaced by the target env's: {cleaned}"
        );
    }

    /// The source-host cleanup must leave `organization` alone — it is owned by
    /// `reconcile_target_identity`, which runs immediately before it.
    #[test]
    fn strip_source_host_env_refs_leaves_organization_alone() {
        let codec = crate::snapshot::codec::codec("workspaces").unwrap();
        let host = "acme.rossum.app";
        let mut v = serde_json::json!({
            "name": "w",
            "organization": format!("https://{host}/api/v1/organizations/2"),
            "queues": [format!("https://{host}/api/v1/queues/9")],
        });
        strip_source_host_env_refs(&mut v, codec, host);
        assert_eq!(
            v["organization"], format!("https://{host}/api/v1/organizations/2"),
            "organization must survive even when it carries the source host: {v}"
        );
        assert_eq!(v["queues"], serde_json::json!([]), "other env refs still cleaned");
    }

    /// Two envs in ONE Rossum instance (one `api_base`, two `org_id`s). The
    /// source-host cleanup's premise — "an env field carrying the SOURCE host is
    /// a leaked source ref" — is false there: the TARGET's own refs carry that
    /// same host. On a MATCHED object it therefore deleted the very values
    /// `reconcile_target_identity` had just restored from the target file
    /// (`created_by`, `modified_by`, a hook's `token_owner` / `hook_template` /
    /// `guide`, an inbox's `email`, a queue's `generic_engine` and back-refs).
    /// `rdc sync` then saw ~every object as locally edited, pushed it, and wrote
    /// the server's response back — restoring the fields for the next migrate to
    /// strip again: a migrate↔sync ping-pong that never converges.
    #[test]
    fn shared_host_migrate_keeps_a_matched_targets_env_fields() {
        use std::fs;
        const HOST: &str = "https://acme.rossum.app/api/v1";

        let mut m = Mapping::default();
        m.workspaces.insert("main".into(), "main".into());
        let subst = build_subst(&m);
        let rel = Path::new("workspaces/main/workspace.json");

        // `src_api_base` decides whether the promotion is same-host (two orgs in
        // one instance) or the classic host-per-env pair; `tgt_seed` whether the
        // object already exists in the target.
        let run = |src_api_base: &str, tgt_seed: Option<serde_json::Value>| -> serde_json::Value {
            let src = tempfile::TempDir::new().unwrap();
            let tgt = tempfile::TempDir::new().unwrap();
            let src_lf =
                crate::state::Lockfile { api_base: src_api_base.into(), ..Default::default() };
            let src_file = src.path().join(rel);
            fs::create_dir_all(src_file.parent().unwrap()).unwrap();
            fs::write(
                &src_file,
                serde_json::to_vec(&serde_json::json!({
                    "id": 111,
                    "url": "rdc://workspaces/main",
                    "name": "Main",
                    "organization": format!("{src_api_base}/organizations/1"),
                    "queues": [format!("{src_api_base}/queues/9")],
                    "metadata": {},
                    "created_by": format!("{src_api_base}/users/1"),
                    "modified_by": format!("{src_api_base}/users/1"),
                }))
                .unwrap(),
            )
            .unwrap();
            let dst = tgt.path().join(rel);
            if let Some(seed) = tgt_seed {
                fs::create_dir_all(dst.parent().unwrap()).unwrap();
                fs::write(&dst, serde_json::to_vec(&seed).unwrap()).unwrap();
            }
            transform_file(
                rel,
                src.path(),
                tgt.path(),
                &m,
                &subst,
                None,
                &format!("{HOST}/organizations/2"),
                Carry::SCORE_THRESHOLDS,
                &src_lf,
                &crate::state::Lockfile::default(),
                false,
                &IdRemap::default(),
                &mut Vec::new(),
                &mut Vec::new(),
                "tgt",
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .unwrap();
            serde_json::from_slice(&fs::read(&dst).unwrap()).unwrap()
        };

        // The target's own snapshot, exactly as a pull from the target org wrote
        // it: same host as the source, different org and users.
        let pulled = serde_json::json!({
            "name": "Main",
            "organization": format!("{HOST}/organizations/2"),
            "queues": [format!("{HOST}/queues/77")],
            "metadata": {},
            "created_by": format!("{HOST}/users/8"),
            "modified_by": format!("{HOST}/users/8"),
        });

        // ---- matched, shared host: the target's env fields must survive ----
        let matched = run(HOST, Some(pulled.clone()));
        for field in ["created_by", "modified_by"] {
            assert_eq!(
                matched.get(field).and_then(|v| v.as_str()),
                Some(format!("{HOST}/users/8").as_str()),
                "the matched target's own {field} must survive a same-host promotion: {matched}"
            );
        }
        assert_eq!(
            matched["queues"],
            serde_json::json!([format!("{HOST}/queues/77")]),
            "the matched target's own back-refs must survive too: {matched}"
        );
        // Byte-for-byte agreement with the target's pulled snapshot is what makes
        // the chain converge: `rdc sync` sees no local edit, so it pushes nothing.
        assert_eq!(
            serde_json::to_vec(&matched).unwrap(),
            serde_json::to_vec(&pulled).unwrap(),
            "migrate output must equal a fresh target pull: {matched}"
        );

        // ---- new object, shared host: source refs must still be cleaned ----
        // `queues` is a reverse-ref env field the server recomputes, so a brand
        // new workspace must not ship the SOURCE's queue list. Same host or not,
        // an unmatched object's env fields are the source's by construction.
        let created = run(HOST, None);
        assert_eq!(
            created["queues"],
            serde_json::json!([]),
            "a new object's source-derived back-refs must still be stripped: {created}"
        );
        assert!(
            !created.as_object().unwrap().contains_key("created_by"),
            "a new object must post a clean create body: {created}"
        );

        // ---- matched, host per env: contamination cleanup unchanged ----
        // The target file carries a SOURCE-host `created_by` an older leaky
        // migrate wrote. Hosts differ, so the heuristic is sound and must still
        // drop it.
        const SRC_HOST: &str = "https://org-dev.rossum.app/api/v1";
        let contaminated = run(
            SRC_HOST,
            Some(serde_json::json!({
                "name": "Main",
                "organization": format!("{HOST}/organizations/2"),
                "queues": [],
                "metadata": {},
                "created_by": format!("{SRC_HOST}/users/1"),
            })),
        );
        assert!(
            !contaminated.as_object().unwrap().contains_key("created_by"),
            "a cross-host promotion must still drop a source-host ref: {contaminated}"
        );
    }

    /// A sidecar (or its overlay shadow) whose only difference from the target's
    /// file is the EOF newline must NOT be rewritten. `sidecar_bytes_for_hash`
    /// ignores trailing newlines, so `rdc sync` sees nothing to push — leaving
    /// migrate to flip those bytes on every run and a working tree that never
    /// comes clean.
    #[test]
    fn transform_keeps_a_sidecars_eof_newline_convention() {
        use std::fs;
        let m = Mapping::default();
        let subst = build_subst(&m);
        let rel = Path::new("workspaces/main/queues/q/formulas/f.py");

        let run = |src_body: &[u8], tgt_body: Option<&[u8]>| -> (FileOutcome, Vec<u8>) {
            let src = tempfile::TempDir::new().unwrap();
            let tgt = tempfile::TempDir::new().unwrap();
            let src_file = src.path().join(rel);
            fs::create_dir_all(src_file.parent().unwrap()).unwrap();
            fs::write(&src_file, src_body).unwrap();
            let dst = tgt.path().join(rel);
            if let Some(body) = tgt_body {
                fs::create_dir_all(dst.parent().unwrap()).unwrap();
                fs::write(&dst, body).unwrap();
            }
            let outcome = transform_file(
                rel,
                src.path(),
                tgt.path(),
                &m,
                &subst,
                None,
                "https://acme.rossum.app/api/v1/organizations/2",
                Carry::SCORE_THRESHOLDS,
                &crate::state::Lockfile::default(),
                &crate::state::Lockfile::default(),
                false,
                &IdRemap::default(),
                &mut Vec::new(),
                &mut Vec::new(),
                "tgt",
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .unwrap();
            (outcome, fs::read(&dst).unwrap())
        };

        // Source ends with a newline (an editor's "insert final newline"), the
        // target's pulled copy does not: keep the target's bytes.
        let (outcome, bytes) = run(b"x = 1\n", Some(b"x = 1"));
        assert_eq!(outcome, FileOutcome::Unchanged, "EOF-newline-only diff must be a no-op");
        assert_eq!(bytes, b"x = 1", "the target's own EOF convention must survive");

        // And the other way round, so migrate never churns in either direction.
        let (outcome, bytes) = run(b"x = 1", Some(b"x = 1\n"));
        assert_eq!(outcome, FileOutcome::Unchanged);
        assert_eq!(bytes, b"x = 1\n");

        // A REAL content change still lands, EOF newline and all.
        let (outcome, bytes) = run(b"x = 2\n", Some(b"x = 1"));
        assert_eq!(outcome, FileOutcome::Updated);
        assert_eq!(bytes, b"x = 2\n");

        // No target file yet: the source's bytes are written verbatim.
        let (outcome, bytes) = run(b"x = 1\n", None);
        assert_eq!(outcome, FileOutcome::Created);
        assert_eq!(bytes, b"x = 1\n");
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
        transform_file(rel, src.path(), tgt.path(), &m, &subst, None, "https://tgt.example/api/v1/organizations/2", Carry::SCORE_THRESHOLDS, &crate::state::Lockfile::default(), &crate::state::Lockfile::default(), false, &IdRemap::default(), &mut Vec::new(), &mut Vec::new(), "tgt", &mut Vec::new(), &mut Vec::new()).unwrap();

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
            ObjectEntry { id: 99, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None },
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
            Carry::SCORE_THRESHOLDS,
            &src_lock,
            &crate::state::Lockfile::default(),
            false,
            &IdRemap::default(),
            &mut Vec::new(),
            &mut Vec::new(),
            "tgt",
            &mut Vec::new(),
            &mut Vec::new(),
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
        transform_file(rel, src.path(), tgt.path(), &m, &subst, Some(&overlay), "https://tgt.example/api/v1/organizations/2", Carry::SCORE_THRESHOLDS, &crate::state::Lockfile::default(), &crate::state::Lockfile::default(), false, &IdRemap::default(), &mut Vec::new(), &mut Vec::new(), "tgt", &mut Vec::new(), &mut Vec::new()).unwrap();

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
        transform_file(rel, src.path(), tgt.path(), &m, &subst, Some(&overlay), "https://tgt.example/api/v1/organizations/2", Carry::SCORE_THRESHOLDS, &crate::state::Lockfile::default(), &crate::state::Lockfile::default(), false, &IdRemap::default(), &mut Vec::new(), &mut Vec::new(), "tgt", &mut Vec::new(), &mut Vec::new()).unwrap();

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
        transform_file(rel, src.path(), tgt.path(), &m, &subst, Some(&overlay), "https://tgt.example/api/v1/organizations/2", Carry::SCORE_THRESHOLDS, &crate::state::Lockfile::default(), &crate::state::Lockfile::default(), false, &IdRemap::default(), &mut Vec::new(), &mut Vec::new(), "tgt", &mut Vec::new(), &mut Vec::new()).unwrap();

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
        transform_file(rel, src.path(), tgt.path(), &m, &subst, Some(&overlay), "https://tgt.example/api/v1/organizations/2", Carry::SCORE_THRESHOLDS, &crate::state::Lockfile::default(), &crate::state::Lockfile::default(), false, &IdRemap::default(), &mut Vec::new(), &mut Vec::new(), "tgt", &mut Vec::new(), &mut Vec::new()).unwrap();

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
                "run_after": ["rdc://hooks/stapler-template", "rdc://hooks/paper-template"],
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
            Carry::SCORE_THRESHOLDS,
            &crate::state::Lockfile::default(),
            &crate::state::Lockfile::default(),
            false,
            &IdRemap::default(),
            &mut Vec::new(),
            &mut Vec::new(),
            "tgt",
            &mut Vec::new(),
            &mut Vec::new(),
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
                "rdc://hooks/paper-template",
                "rdc://hooks/stapler-template"
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
        transform_file(rel, src.path(), tgt.path(), &m, &subst, None, "https://tgt.example/api/v1/organizations/2", Carry::SCORE_THRESHOLDS, &crate::state::Lockfile::default(), &crate::state::Lockfile::default(), false, &IdRemap::default(), &mut Vec::new(), &mut Vec::new(), "tgt", &mut Vec::new(), &mut Vec::new()).unwrap();

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

    /// Every leaf of a dataset dir must select as one `("mdh", slug)` object, so
    /// `--only mdh/<slug>` carries the manifest, indexes, and rows together.
    #[test]
    fn classify_for_selection_maps_every_mdh_leaf_to_its_dataset() {
        for leaf in ["collection.json", "indexes.json", "data.jsonl"] {
            assert_eq!(
                classify_for_selection(Path::new(&format!("mdh/gl-codes/{leaf}"))),
                Some(("mdh", "gl-codes".to_string())),
                "{leaf} must select with its dataset"
            );
        }
        // `classify` itself must KEEP returning None for mdh — overlay-key
        // validation and the substitution map depend on that contract.
        assert_eq!(classify(Path::new("mdh/gl-codes/indexes.json")), None);
    }

    /// A per-env row-data shadow must validate as a sidecar, or
    /// `validate_overlay_dir` aborts the whole migration.
    #[test]
    fn is_sidecar_accepts_mdh_row_data() {
        assert!(is_sidecar(Path::new("mdh/gl-codes/data.jsonl")));
        // JSON leaves are still not sidecars (they are overlay.toml territory).
        assert!(!is_sidecar(Path::new("mdh/gl-codes/indexes.json")));
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
        reconcile_target_owned_keys(&mut source, "queues", &tgt, &["training_enabled"]);
        assert_eq!(source["training_enabled"], serde_json::json!(false));
    }

    #[test]
    fn reconcile_training_new_target_drops_flag() {
        // No target => brand-new queue => drop the flag; Rossum's create default
        // (false) applies and the round-trip is stable.
        let mut source = serde_json::json!({ "name": "Q", "training_enabled": true });
        let missing = std::path::Path::new("/nonexistent/does-not-exist/queue.json");
        reconcile_target_owned_keys(&mut source, "queues", missing, &["training_enabled"]);
        assert!(source.get("training_enabled").is_none());
    }

    #[test]
    fn reconcile_training_no_op_for_non_queue() {
        // Only queues carry `training_enabled`; other kinds are untouched.
        let mut source = serde_json::json!({ "name": "S", "training_enabled": true });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "S" }));
        reconcile_target_owned_keys(&mut source, "schemas", &tgt, &["training_enabled"]);
        assert_eq!(source["training_enabled"], serde_json::json!(true));
    }

    #[test]
    fn reconcile_automation_matched_adopts_every_target_value() {
        // A prod org that has earned automation must not be reset by a dev env
        // that has not — nor the reverse.
        let mut source = serde_json::json!({
            "name": "Q",
            "automation_enabled": true,
            "automation_level": "always",
            "quality_spot_check_percentage": 0.02,
        });
        let (_d, tgt) = tgt_file(&serde_json::json!({
            "name": "Q",
            "automation_enabled": false,
            "automation_level": "never",
            "quality_spot_check_percentage": 0.0,
        }));
        reconcile_target_owned_keys(&mut source, "queues", &tgt, AUTOMATION_KEYS);
        assert_eq!(source["automation_enabled"], serde_json::json!(false));
        assert_eq!(source["automation_level"], serde_json::json!("never"));
        assert_eq!(source["quality_spot_check_percentage"], serde_json::json!(0.0));
    }

    #[test]
    fn reconcile_automation_new_target_drops_every_key() {
        // A brand-new queue POSTs without them, so the server's own defaults
        // apply — `automation_enabled: false` and `automation_level: "never"`,
        // i.e. automation OFF. Verified against the published OpenAPI spec:
        // `POST /v1/queues` requires only `name` and `schema`.
        let mut source = serde_json::json!({
            "name": "Q",
            "automation_enabled": true,
            "automation_level": "confident",
            "quality_spot_check_percentage": 0.02,
        });
        let missing = std::path::Path::new("/nonexistent/does-not-exist/queue.json");
        reconcile_target_owned_keys(&mut source, "queues", missing, AUTOMATION_KEYS);
        assert!(source.get("automation_enabled").is_none());
        assert!(source.get("automation_level").is_none());
        assert!(source.get("quality_spot_check_percentage").is_none());
        assert_eq!(source["name"], serde_json::json!("Q"), "unrelated keys survive");
    }

    #[test]
    fn reconcile_automation_drops_only_the_keys_the_target_lacks() {
        // Per-key, not all-or-nothing: a target pulled before a field existed
        // keeps the reconcile honest for the fields it does carry.
        let mut source = serde_json::json!({
            "name": "Q",
            "automation_enabled": true,
            "automation_level": "always",
            "quality_spot_check_percentage": 0.02,
        });
        let (_d, tgt) = tgt_file(&serde_json::json!({
            "name": "Q",
            "automation_level": "confident",
        }));
        reconcile_target_owned_keys(&mut source, "queues", &tgt, AUTOMATION_KEYS);
        assert_eq!(source["automation_level"], serde_json::json!("confident"));
        assert!(source.get("automation_enabled").is_none());
        assert!(source.get("quality_spot_check_percentage").is_none());
    }

    #[test]
    fn reconcile_automation_no_op_for_non_queue() {
        // Only queues carry these; a schema of the same shape is untouched.
        let mut source = serde_json::json!({ "name": "S", "automation_level": "always" });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "S", "automation_level": "never" }));
        reconcile_target_owned_keys(&mut source, "schemas", &tgt, AUTOMATION_KEYS);
        assert_eq!(source["automation_level"], serde_json::json!("always"));
    }

    #[test]
    fn reconcile_leaves_a_key_the_source_does_not_carry_absent() {
        // The reconcile adopts, it does not introduce: a source body with no
        // automation keys must not grow them from the target, or migrate would
        // write fields into objects that never had them.
        let mut source = serde_json::json!({ "name": "Q" });
        let (_d, tgt) = tgt_file(&serde_json::json!({
            "name": "Q",
            "automation_level": "confident",
        }));
        reconcile_target_owned_keys(&mut source, "queues", &tgt, AUTOMATION_KEYS);
        assert!(source.get("automation_level").is_none());
    }

    // ---- engine-slot reconciliation ----

    /// The live failure, reduced: the source queue is bound to a custom
    /// engine, the target queue is on the built-in generic engine and
    /// `reconcile_target_identity` has just restored that value. Both bindings
    /// present is the body `PATCH /queues/<id>` refuses with "Only one of
    /// dedicated_engine, generic_engine or engine can be set."
    #[test]
    fn reconcile_engine_slot_drops_the_restored_generic_engine() {
        let mut value = serde_json::json!({
            "name": "Q",
            "engine": "rdc://engines/sorting",
            "dedicated_engine": null,
            "generic_engine": "https://tgt.example/api/v1/generic_engines/5",
        });
        reconcile_engine_slot(&mut value, "queues");
        assert_eq!(value["engine"], serde_json::json!("rdc://engines/sorting"));
        assert_eq!(
            value["generic_engine"],
            serde_json::Value::Null,
            "the promoted custom engine wins the slot"
        );
        // The key stays, as `null` — a pulled queue always carries all three,
        // and the API counts values rather than keys.
        assert!(value.as_object().unwrap().contains_key("generic_engine"));
    }

    /// A source on the generic engine promotes nothing into the slot, so the
    /// target keeps the `generic_engine` its own env resolved (whose host is
    /// the target's).
    #[test]
    fn reconcile_engine_slot_keeps_the_generic_engine_when_nothing_outranks_it() {
        let mut value = serde_json::json!({
            "name": "Q",
            "engine": null,
            "dedicated_engine": null,
            "generic_engine": "https://tgt.example/api/v1/generic_engines/5",
        });
        reconcile_engine_slot(&mut value, "queues");
        assert_eq!(
            value["generic_engine"],
            serde_json::json!("https://tgt.example/api/v1/generic_engines/5")
        );
        assert_eq!(value["engine"], serde_json::Value::Null);
    }

    /// A queue with a single binding, or none, is already valid and must come
    /// out byte-identical — otherwise every migrate would rewrite every queue.
    #[test]
    fn reconcile_engine_slot_leaves_a_valid_body_alone() {
        for body in [
            serde_json::json!({ "name": "Q", "engine": "rdc://engines/e", "generic_engine": null }),
            serde_json::json!({ "name": "Q", "engine": null, "generic_engine": null }),
            // A new queue: `strip_for_cross_env_patch` removed `generic_engine`
            // outright, so only the promoted binding is present.
            serde_json::json!({ "name": "Q", "engine": "rdc://engines/e" }),
            serde_json::json!({ "name": "Q" }),
        ] {
            let mut value = body.clone();
            reconcile_engine_slot(&mut value, "queues");
            assert_eq!(value, body, "a valid engine slot must not be rewritten");
        }
    }

    /// A source snapshot that is ITSELF invalid (two bindings — e.g. an env
    /// migrated by an rdc that predates this reconcile, then used as a source)
    /// is resolved by precedence rather than promoted as-is.
    #[test]
    fn reconcile_engine_slot_resolves_three_bindings_by_precedence() {
        let mut value = serde_json::json!({
            "name": "Q",
            "generic_engine": "https://tgt.example/api/v1/generic_engines/5",
            "dedicated_engine": "https://tgt.example/api/v1/dedicated_engines/2",
            "engine": "rdc://engines/sorting",
        });
        reconcile_engine_slot(&mut value, "queues");
        assert_eq!(value["engine"], serde_json::json!("rdc://engines/sorting"));
        assert_eq!(value["dedicated_engine"], serde_json::Value::Null);
        assert_eq!(value["generic_engine"], serde_json::Value::Null);
    }

    /// Precedence keeps `dedicated_engine` when there is no `engine`, so a
    /// legacy dedicated binding is not silently swapped for the generic one.
    #[test]
    fn reconcile_engine_slot_prefers_dedicated_over_generic() {
        let mut value = serde_json::json!({
            "name": "Q",
            "engine": null,
            "dedicated_engine": "https://tgt.example/api/v1/dedicated_engines/2",
            "generic_engine": "https://tgt.example/api/v1/generic_engines/5",
        });
        reconcile_engine_slot(&mut value, "queues");
        assert_eq!(
            value["dedicated_engine"],
            serde_json::json!("https://tgt.example/api/v1/dedicated_engines/2")
        );
        assert_eq!(value["generic_engine"], serde_json::Value::Null);
    }

    #[test]
    fn reconcile_engine_slot_no_op_for_non_queue() {
        // Only queues carry an engine binding; a hook whose settings happen to
        // name these keys is deployable content and must not be touched.
        let body = serde_json::json!({
            "name": "H",
            "engine": "a",
            "generic_engine": "b",
        });
        let mut value = body.clone();
        reconcile_engine_slot(&mut value, "hooks");
        assert_eq!(value, body);
    }

    // ---- email_prefix reconciliation (migrate default: ignore) ----

    #[test]
    fn reconcile_email_prefix_matched_adopts_target_value() {
        // An inbox's `email_prefix` is the left-hand side of its PUBLIC address
        // (`<email_prefix>-<hash>@<host>`), so carrying the source env's value
        // would re-address the target's inbox and break mail to the old
        // address. A matched target keeps its own.
        let mut source = serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "In", "email_prefix": "acme" }));
        let carried = reconcile_email_prefix(&mut source, "inboxes", &tgt, false);
        assert_eq!(source["email_prefix"], serde_json::json!("acme"));
        assert_eq!(carried, None, "adopting the target's is not a carry");
    }

    #[test]
    fn reconcile_email_prefix_new_object_keeps_source_value() {
        // No target file => the next sync POSTs this inbox => the source's
        // prefix must survive: `POST /inboxes` rejects a body with neither
        // `email_prefix` nor `email` (400 non_field_errors), and
        // `strip_for_create` removes `email`. Dropping it here made every
        // brand-new inbox unpushable.
        let mut source = serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" });
        let missing = std::path::Path::new("/nonexistent/does-not-exist/inbox.json");
        let carried = reconcile_email_prefix(&mut source, "inboxes", missing, true);
        assert_eq!(source["email_prefix"], serde_json::json!("acme-dev--ops"));
        assert_eq!(carried.as_deref(), Some("acme-dev--ops"));
    }

    #[test]
    fn reconcile_email_prefix_new_object_still_prefers_the_target_file() {
        // A target inbox that is not deployed yet but already carries a prefix
        // (hand-authored, or written by an earlier migrate) keeps it: the
        // create has a prefix either way, so there is nothing to repair and a
        // deliberate local value must not be overwritten every run.
        let mut source = serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "In", "email_prefix": "acme" }));
        let carried = reconcile_email_prefix(&mut source, "inboxes", &tgt, true);
        assert_eq!(source["email_prefix"], serde_json::json!("acme"));
        assert_eq!(carried, None);
    }

    #[test]
    fn reconcile_email_prefix_reports_a_new_inbox_that_already_holds_the_source_value() {
        // The target file was written by an earlier migrate (or names the same
        // prefix by hand) and the inbox is still not deployed: the warning must
        // fire again, because this run is still the last chance to change the
        // address before `sync` creates it.
        let mut source = serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" });
        let (_d, tgt) =
            tgt_file(&serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" }));
        let carried = reconcile_email_prefix(&mut source, "inboxes", &tgt, true);
        assert_eq!(carried.as_deref(), Some("acme-dev--ops"));
    }

    #[test]
    fn reconcile_email_prefix_is_quiet_once_the_inbox_is_deployed() {
        // Same values, but the object is in the target lockfile: it is a PATCH
        // of a live mailbox that already uses this address — nothing to warn
        // about, and the notice must not become permanent noise.
        let mut source = serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" });
        let (_d, tgt) =
            tgt_file(&serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" }));
        let carried = reconcile_email_prefix(&mut source, "inboxes", &tgt, false);
        assert_eq!(carried, None);
    }

    #[test]
    fn reconcile_email_prefix_deployed_target_without_prefix_drops_it() {
        // The inbox is already deployed (in the target lockfile) and the target
        // file carries no prefix: drop the field. The push PATCHes, and a PATCH
        // that omits the key leaves the remote's own address untouched —
        // filling the gap from the source would re-address a live mailbox.
        let mut source = serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "In" }));
        let carried = reconcile_email_prefix(&mut source, "inboxes", &tgt, false);
        assert!(source.get("email_prefix").is_none());
        assert_eq!(carried, None);
    }

    #[test]
    fn reconcile_email_prefix_null_target_value_is_not_adopted() {
        // An explicit `null` is not a prefix: adopting it would POST
        // `email_prefix: null` and hit the same 400 as omitting the key.
        let mut source = serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" });
        let (_d, tgt) =
            tgt_file(&serde_json::json!({ "name": "In", "email_prefix": serde_json::Value::Null }));
        let carried = reconcile_email_prefix(&mut source, "inboxes", &tgt, true);
        assert_eq!(source["email_prefix"], serde_json::json!("acme-dev--ops"));
        assert_eq!(carried.as_deref(), Some("acme-dev--ops"));
    }

    #[test]
    fn reconcile_email_prefix_no_op_for_non_inbox() {
        // Only inboxes carry `email_prefix`; a same-named key on another kind
        // is not ours to touch.
        let mut source = serde_json::json!({ "name": "H", "email_prefix": "acme-dev--ops" });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "H", "email_prefix": "acme" }));
        let carried = reconcile_email_prefix(&mut source, "hooks", &tgt, false);
        assert_eq!(source["email_prefix"], serde_json::json!("acme-dev--ops"));
        assert_eq!(carried, None);
    }

    #[test]
    fn reconcile_email_prefix_absent_in_source_stays_absent() {
        // A source inbox with no prefix must not gain the target's — the
        // reconcile only ever protects a value the target already owns. The
        // push pre-flight (`missing_create_fields`) is what catches a create
        // that ends up with no prefix at all.
        let mut source = serde_json::json!({ "name": "In" });
        let (_d, tgt) = tgt_file(&serde_json::json!({ "name": "In", "email_prefix": "acme" }));
        let carried = reconcile_email_prefix(&mut source, "inboxes", &tgt, true);
        assert!(source.get("email_prefix").is_none());
        assert_eq!(carried, None);
    }

    /// Helper: run `transform_file` on one source `inbox.json` and return the
    /// carries it reported. `overlay` is applied as the target env's.
    fn carries_for_inbox(
        source: &serde_json::Value,
        overlay: Option<&Overlay>,
    ) -> Vec<(String, String)> {
        use std::fs;
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();
        let mut m = Mapping::default();
        m.workspaces.insert("main".into(), "main".into());
        m.queues.insert("invoices".into(), "invoices".into());

        let rel = Path::new("workspaces/main/queues/invoices/inbox.json");
        let src_file = src.path().join(rel);
        fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        fs::write(&src_file, serde_json::to_vec(source).unwrap()).unwrap();

        let subst = build_subst(&m);
        let mut carries = Vec::new();
        transform_file(
            rel,
            src.path(),
            tgt.path(),
            &m,
            &subst,
            overlay,
            "https://tgt.example/api/v1/organizations/2",
            Carry::NONE,
            &crate::state::Lockfile::default(),
            &crate::state::Lockfile::default(), // empty tgt lockfile => a create
            false,
            &IdRemap::default(),
            &mut Vec::new(),
            &mut carries,
            "tgt",
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        carries
    }

    #[test]
    fn transform_reports_a_carried_inbox_prefix() {
        let carries = carries_for_inbox(
            &serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" }),
            None,
        );
        assert_eq!(
            carries,
            vec![("invoices".to_string(), "acme-dev--ops".to_string())]
        );
    }

    #[test]
    fn transform_does_not_report_a_prefix_the_overlay_replaced() {
        // The overlay is applied AFTER the reconcile, so the provisional carry
        // must be re-checked against the final value. Reporting it here would
        // name a prefix that is not used and nag about a decision the user has
        // already made — the documented way to choose a new env's address.
        let mut overlay = Overlay::default();
        let mut ov = BTreeMap::new();
        ov.insert(
            "email_prefix".to_string(),
            serde_json::Value::String("acme-prod".into()),
        );
        overlay.inboxes.insert("invoices".to_string(), ov);

        let carries = carries_for_inbox(
            &serde_json::json!({ "name": "In", "email_prefix": "acme-dev--ops" }),
            Some(&overlay),
        );
        assert!(
            carries.is_empty(),
            "an overlay-chosen prefix is not a carry: {carries:?}"
        );
    }

    // ---- store-extension hook_template ---------------------------------

    #[test]
    fn api_base_of_recovers_the_target_api_base() {
        assert_eq!(
            api_base_of("https://acme.rossum.app/api/v1/organizations/555972"),
            Some("https://acme.rossum.app/api/v1")
        );
        assert_eq!(api_base_of("not-an-org-url"), None);
    }

    #[test]
    fn reconcile_hook_template_restores_it_retargeted_to_the_target_org() {
        // The field is mandatory on create and there is no target value to
        // inherit; the template id is cross-env stable, so only the host moves.
        let mut v = serde_json::json!({
            "name": "Duplicate Handling",
            "extension_source": "rossum_store",
        });
        reconcile_hook_template(
            &mut v,
            Some("https://acme-test.rossum.app/api/v1/hook_templates/28"),
            Some("https://acme.rossum.app/api/v1"),
        );
        assert_eq!(
            v["hook_template"],
            serde_json::json!("https://acme.rossum.app/api/v1/hook_templates/28"),
            "the id survives, the host becomes the target's: {v}"
        );
    }

    #[test]
    fn reconcile_hook_template_never_overwrites_an_existing_value() {
        // A matched target's own template link (or one an overlay pinned) wins.
        let mut v = serde_json::json!({
            "name": "Duplicate Handling",
            "extension_source": "rossum_store",
            "hook_template": "https://acme.rossum.app/api/v1/hook_templates/99",
        });
        reconcile_hook_template(
            &mut v,
            Some("https://acme-test.rossum.app/api/v1/hook_templates/28"),
            Some("https://acme.rossum.app/api/v1"),
        );
        assert_eq!(
            v["hook_template"],
            serde_json::json!("https://acme.rossum.app/api/v1/hook_templates/99")
        );
    }

    #[test]
    fn reconcile_hook_template_leaves_a_custom_hook_alone() {
        // Only store extensions install through `POST /hooks/create`; a custom
        // hook must not acquire a template link it never had.
        let mut v = serde_json::json!({ "name": "Validator", "extension_source": "custom" });
        reconcile_hook_template(
            &mut v,
            Some("https://acme-test.rossum.app/api/v1/hook_templates/28"),
            Some("https://acme.rossum.app/api/v1"),
        );
        assert!(v.get("hook_template").is_none(), "{v}");
    }

    #[test]
    fn reconcile_hook_template_is_a_no_op_without_a_source_value() {
        let mut v = serde_json::json!({ "name": "H", "extension_source": "rossum_store" });
        reconcile_hook_template(&mut v, None, Some("https://acme.rossum.app/api/v1"));
        assert!(v.get("hook_template").is_none(), "{v}");
    }

    #[test]
    fn carried_email_prefix_warning_names_the_overlay_key() {
        // The warning is the only place a user learns that a brand-new
        // production mailbox is about to be addressed with the source env's
        // prefix, so it must name the inbox, the value, and the exact overlay
        // key that overrides it.
        let msg = format_carried_email_prefix_warning(
            "test",
            "prod",
            &[
                ("invoices".to_string(), "acme-sandbox".to_string()),
                ("receipts".to_string(), "acme".to_string()),
            ],
        );
        assert!(msg.contains("2 new inbox"), "{msg}");
        assert!(msg.contains("envs/prod/overlay.toml"), "{msg}");
        assert!(msg.contains("[inboxes.invoices]"), "{msg}");
        assert!(msg.contains("acme-sandbox"), "{msg}");
        assert!(msg.contains("[inboxes.receipts]"), "{msg}");
    }

    #[test]
    fn org_columns_missing_warning_names_the_target_and_every_id_once() {
        // One combined message for the whole run, not one `log.event` per id —
        // a project with many stale columns must not flood the log.
        let msg = format_org_columns_missing_warning(
            "prod",
            &["field_a".to_string(), "not_in_prod".to_string()],
        );
        assert!(msg.contains("2 organization column"), "{msg}");
        assert!(msg.contains("'prod'"), "{msg}");
        assert!(msg.contains("schema_id `field_a`"), "{msg}");
        assert!(msg.contains("schema_id `not_in_prod`"), "{msg}");
    }

    #[test]
    fn transform_file_carries_thresholds_when_opted_in() {
        // With `Carry::SCORE_THRESHOLDS`, the source's per-datapoint threshold
        // survives verbatim even against a matched target that tuned it
        // differently.
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
            /* carry = */ Carry::SCORE_THRESHOLDS,
            &crate::state::Lockfile::default(),
            &crate::state::Lockfile::default(),
            false,
            &IdRemap::default(),
            &mut Vec::new(),
            &mut Vec::new(),
            "tgt",
            &mut Vec::new(),
            &mut Vec::new(),
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
            /* carry = */ Carry::NONE,
            &crate::state::Lockfile::default(),
            &crate::state::Lockfile::default(),
            false,
            &IdRemap::default(),
            &mut Vec::new(),
            &mut Vec::new(),
            "tgt",
            &mut Vec::new(),
            &mut Vec::new(),
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

    #[test]
    fn carry_from_groups_resolves_each_group() {
        assert_eq!(Carry::from_groups(&[]), Carry::NONE);
        assert_eq!(
            Carry::from_groups(&[CarryGroup::ScoreThresholds]),
            Carry::SCORE_THRESHOLDS
        );
        assert_eq!(
            Carry::from_groups(&[CarryGroup::Automation]),
            Carry { score_thresholds: false, email_prefixes: false, automation: true }
        );
    }

    #[test]
    fn carry_from_groups_unions_repeats_and_expands_all() {
        // Repeating a group is idempotent, and `all` means every group —
        // including any group added after this test was written.
        assert_eq!(
            Carry::from_groups(&[CarryGroup::Automation, CarryGroup::Automation]),
            Carry { score_thresholds: false, email_prefixes: false, automation: true }
        );
        // Derive the expectation from the enum itself rather than hand-listing
        // it a second time: `all` must equal "every non-`All` variant",
        // structurally, so a variant added after this test was written is
        // covered without editing this test.
        let every_other_group: Vec<CarryGroup> =
            <CarryGroup as clap::ValueEnum>::value_variants()
                .iter()
                .copied()
                .filter(|g| !matches!(g, CarryGroup::All))
                .collect();
        assert_eq!(
            Carry::from_groups(&[CarryGroup::All]),
            Carry::from_groups(&every_other_group)
        );
    }

    fn known_pairs(pairs: &[(&str, &str)]) -> BTreeSet<(String, String)> {
        pairs.iter().map(|(k, s)| (k.to_string(), s.to_string())).collect()
    }

    #[test]
    fn saved_view_with_only_resolvable_refs_is_clean() {
        let v = serde_json::json!({
            "name": "Awaiting approval",
            "shared": true,
            "queues_filter": ["rdc://queues/invoices"],
            "query": { "$and": [ { "queue": { "$in": ["rdc://queues/invoices"] } } ] }
        });
        let known = known_pairs(&[("queues", "invoices")]);
        assert!(check_saved_view_refs("awaiting-approval", &v, &known, None).is_empty());
    }

    /// A user ref survives portabilization as a raw URL because users are not a
    /// snapshotted kind. Promoting it would 400 -- the server validates refs
    /// inside `query` as hyperlinks -- so migrate refuses instead.
    #[test]
    fn saved_view_with_a_non_portable_user_ref_is_refused() {
        let v = serde_json::json!({
            "name": "Mine",
            "shared": true,
            "queues_filter": [],
            "query": { "$and": [
                { "modifier": { "$in": ["https://acme.rossum.app/api/v1/users/7"] } }
            ] }
        });
        let problems = check_saved_view_refs("mine", &v, &BTreeSet::new(), None);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].reason, SavedViewRefReason::NonPortable);
        assert!(problems[0].reference.contains("/users/7"));
        assert!(
            problems[0].location.contains("query"),
            "the location must point into query, got {}",
            problems[0].location
        );
    }

    /// An unresolvable queues_filter ref must NOT be deferred: an empty
    /// queues_filter makes a shared view visible to the entire organization.
    #[test]
    fn saved_view_with_an_unresolvable_queue_ref_is_refused() {
        let v = serde_json::json!({
            "name": "Scoped",
            "shared": true,
            "queues_filter": ["rdc://queues/not-in-target"],
            "query": { "$and": [] }
        });
        let problems =
            check_saved_view_refs("scoped", &v, &known_pairs(&[("queues", "invoices")]), None);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].reason, SavedViewRefReason::Unresolvable);
        assert_eq!(problems[0].location, "queues_filter[0]");
        assert!(problems[0].reference.contains("not-in-target"));
    }

    /// `field.<schema_id>` keys are object KEYS, not string leaves. They are
    /// deliberately NOT checked -- documented in the design doc, not guarded.
    #[test]
    fn schema_field_keys_are_not_checked() {
        let v = serde_json::json!({
            "name": "By field",
            "shared": true,
            "queues_filter": [],
            "query": { "$and": [ { "field.document_id.string": { "$eq": "x" } } ] }
        });
        assert!(check_saved_view_refs("by-field", &v, &BTreeSet::new(), None).is_empty());
    }

    /// rdc supports two api_base shapes and treats both as first-class:
    /// `https://<org>.rossum.app/api/v1` and `https://api.elis.rossum.ai/v1`
    /// (`config::EnvConfig` documents both; `rdc init` offers the second as its
    /// prompt example). On the second there is no `/api/v1/` segment, so a user
    /// ref matched NEITHER branch and promoted verbatim — on a shared cluster
    /// that hands the target org a hyperlink to a SOURCE-org user. Knowing the
    /// source host is what closes it.
    #[test]
    fn saved_view_user_ref_on_a_v1_api_base_is_refused() {
        let v = serde_json::json!({
            "name": "Mine",
            "shared": true,
            "queues_filter": [],
            "query": { "$and": [
                { "modifier": { "$in": ["https://api.elis.rossum.ai/v1/users/7"] } }
            ] }
        });
        let problems = check_saved_view_refs(
            "mine",
            &v,
            &BTreeSet::new(),
            Some("api.elis.rossum.ai"),
        );
        assert_eq!(problems.len(), 1, "got {problems:?}");
        assert_eq!(problems[0].reason, SavedViewRefReason::NonPortable);
        assert!(problems[0].reference.contains("/v1/users/7"));
        assert_eq!(problems[0].location, "query.$and[0].modifier.$in[0]");

        // Pinning the residual blind spot rather than papering over it: with no
        // host to compare against (no lockfile AND no `rdc.toml` entry for the
        // source env) this shape carries nothing the predicate can recognize.
        // `run_at` always has one of the two, which is why it is threaded in.
        assert!(check_saved_view_refs("mine", &v, &BTreeSet::new(), None).is_empty());
    }

    /// The source host must not swallow a ref that DOES resolve: a portable
    /// `rdc://` ref never reaches the non-portable branch.
    #[test]
    fn source_host_does_not_flag_resolvable_portable_refs() {
        let v = serde_json::json!({
            "name": "Awaiting approval",
            "shared": true,
            "queues_filter": ["rdc://queues/invoices"],
            "query": { "$and": [ { "queue": { "$in": ["rdc://queues/invoices"] } } ] }
        });
        assert!(
            check_saved_view_refs(
                "awaiting-approval",
                &v,
                &known_pairs(&[("queues", "invoices")]),
                Some("api.elis.rossum.ai"),
            )
            .is_empty()
        );
    }

    /// The ENV fields of a promoted view carry raw API URLs by design and must
    /// never be mistaken for refs that cannot cross: `organization` is set from
    /// the TARGET's `rdc.toml` by `reconcile_target_identity` and is never
    /// portabilized (`is_portable_kind` excludes it), and a matched target's
    /// `url` comes straight back from the target file. Walking the whole body
    /// flagged both, which made migrate refuse EVERY promoted saved view.
    #[test]
    fn saved_view_env_fields_are_not_checked() {
        let v = serde_json::json!({
            "id": 42,
            "url": "https://acme.rossum.app/api/v1/saved_views/42",
            "name": "Awaiting approval",
            "shared": true,
            "queues_filter": ["rdc://queues/invoices"],
            "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
            "organization": "https://acme.rossum.app/api/v1/organizations/2"
        });
        let problems = check_saved_view_refs(
            "awaiting-approval",
            &v,
            &known_pairs(&[("queues", "invoices")]),
            None,
        );
        assert!(problems.is_empty(), "env fields must be out of scope: {problems:?}");
    }

    /// The offender listing is read against the file top to bottom, so array
    /// indices must be ordered by value: a lexicographic sort put
    /// `queues_filter[10]` ahead of `queues_filter[2]`.
    #[test]
    fn saved_view_ref_problems_are_ordered_naturally() {
        let refs: Vec<serde_json::Value> = (0..12)
            .map(|i| serde_json::json!(format!("rdc://queues/missing-{i}")))
            .collect();
        let v = serde_json::json!({
            "name": "Wide filter",
            "shared": true,
            "queues_filter": refs,
        });
        let problems = check_saved_view_refs("wide-filter", &v, &BTreeSet::new(), None);
        let locations: Vec<&str> = problems.iter().map(|p| p.location.as_str()).collect();
        let expected: Vec<String> = (0..12).map(|i| format!("queues_filter[{i}]")).collect();
        assert_eq!(locations, expected.iter().map(String::as_str).collect::<Vec<_>>());
    }

    /// `natural_cmp` on its own, including the shapes the JSON walker cannot
    /// produce but a total order still has to handle.
    #[test]
    fn natural_cmp_orders_digit_runs_by_value() {
        use std::cmp::Ordering;
        assert_eq!(natural_cmp("a[2]", "a[10]"), Ordering::Less);
        assert_eq!(natural_cmp("a[10]", "a[2]"), Ordering::Greater);
        assert_eq!(natural_cmp("a[2]", "a[2]"), Ordering::Equal);
        // Deeper path, same prefix: the second run decides.
        assert_eq!(natural_cmp("q.$and[2].x[9]", "q.$and[2].x[11]"), Ordering::Less);
        // The first run decides even when the second says otherwise.
        assert_eq!(natural_cmp("q.$and[9].x[11]", "q.$and[11].x[2]"), Ordering::Less);
        // Non-digit runs stay bytewise, and a prefix sorts first.
        assert_eq!(natural_cmp("query", "queues_filter"), Ordering::Less);
        assert_eq!(natural_cmp("a[2]", "a[2].b"), Ordering::Less);
        // Leading zeros: same value, so never Equal but stably ordered.
        assert_eq!(natural_cmp("a[07]", "a[7]"), Ordering::Greater);
        assert_eq!(natural_cmp("a[7]", "a[07]"), Ordering::Less);
    }

    /// Two-env project (`dev` -> `prod`) in a tempdir, ready for `run_at`.
    fn saved_view_project() -> tempfile::TempDir {
        saved_view_project_with_bases("https://dev.example/api/v1", "https://prod.example/api/v1")
    }

    /// Same, with the `api_base` of each env chosen by the caller — rdc supports
    /// both `https://<org>.rossum.app/api/v1` and `https://api.elis.rossum.ai/v1`
    /// and the ref check has to work on both.
    fn saved_view_project_with_bases(dev_base: &str, prod_base: &str) -> tempfile::TempDir {
        let project = tempfile::TempDir::new().unwrap();
        let mut envs = BTreeMap::new();
        for (env, base, org_id) in [("dev", dev_base, 1u64), ("prod", prod_base, 2u64)] {
            envs.insert(
                env.to_string(),
                crate::config::EnvConfig {
                    api_base: base.to_string(),
                    org_id,
                },
            );
        }
        crate::config::ProjectConfig { envs }
            .save(&project.path().join("rdc.toml"))
            .unwrap();
        project
    }

    fn write_snapshot_json(path: &Path, v: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    }

    /// One queue for a saved view's refs to point at.
    fn write_source_queue(root: &Path) {
        write_snapshot_json(
            &root.join("envs/dev/workspaces/main/workspace.json"),
            &serde_json::json!({ "name": "Main" }),
        );
        write_snapshot_json(
            &root.join("envs/dev/workspaces/main/queues/invoices/queue.json"),
            &serde_json::json!({ "name": "Invoices", "workspace": "rdc://workspaces/main" }),
        );
    }

    /// End-to-end: the kind is wired into `classify` / `MANAGED_DIRS` /
    /// `remap_relative`, lands in `saved-views/` (hyphenated) in the target, and
    /// the ref check is reached and PASSES on a view whose refs all resolve.
    #[test]
    fn run_at_promotes_a_saved_view_whose_refs_resolve() {
        let project = saved_view_project();
        let root = project.path();
        write_source_queue(root);
        write_snapshot_json(
            &root.join("envs/dev/saved-views/awaiting-approval.json"),
            &serde_json::json!({
                "id": 42,
                "url": "rdc://saved_views/awaiting-approval",
                "name": "Awaiting approval",
                "shared": true,
                "queues_filter": ["rdc://queues/invoices"],
                "query": { "$and": [ { "queue": { "$in": ["rdc://queues/invoices"] } } ] },
                "organization": "https://dev.example/api/v1/organizations/1"
            }),
        );

        run_at(root, "dev", "prod", MirrorMode::Additive, false, vec![], Carry::NONE)
            .expect("a saved view whose refs all resolve must promote");

        let promoted: Value = serde_json::from_slice(
            &std::fs::read(root.join("envs/prod/saved-views/awaiting-approval.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(promoted["queues_filter"][0], "rdc://queues/invoices");
        assert_eq!(
            promoted["query"]["$and"][0]["queue"]["$in"][0],
            "rdc://queues/invoices"
        );
        // The env field the whole-body walk used to flag: retargeted at the
        // TARGET org, and not treated as a ref that cannot cross.
        assert_eq!(
            promoted["organization"],
            "https://prod.example/api/v1/organizations/2"
        );
    }

    /// The refusal is reached on a real run, and the error names the view, the
    /// JSON path and the target env.
    #[test]
    fn run_at_refuses_a_saved_view_ref_missing_from_the_target() {
        let project = saved_view_project();
        let root = project.path();
        write_source_queue(root);
        write_snapshot_json(
            &root.join("envs/dev/saved-views/scoped.json"),
            &serde_json::json!({
                "name": "Scoped",
                "shared": true,
                "queues_filter": ["rdc://queues/not-in-target"],
                "query": { "$and": [] },
                "organization": "https://dev.example/api/v1/organizations/1"
            }),
        );

        let err = run_at(root, "dev", "prod", MirrorMode::Additive, false, vec![], Carry::NONE)
            .expect_err("an unresolvable queues_filter ref must refuse, never defer");
        let msg = format!("{err:#}");
        for want in [
            "saved-views/scoped",
            "queues_filter[0]",
            "not-in-target",
            "cannot cross into 'prod'",
        ] {
            assert!(msg.contains(want), "error must mention {want}, got: {msg}");
        }
    }

    /// End-to-end on the `…/v1` api_base shape (no `/api/v1/` segment): the run
    /// must still refuse. Proves the source host actually reaches the checker —
    /// here from `rdc.toml`, since a hand-authored snapshot has no lockfile.
    #[test]
    fn run_at_refuses_a_user_ref_on_a_v1_api_base() {
        let project = saved_view_project_with_bases(
            "https://api.dev.example/v1",
            "https://api.prod.example/v1",
        );
        let root = project.path();
        write_source_queue(root);
        write_snapshot_json(
            &root.join("envs/dev/saved-views/mine.json"),
            &serde_json::json!({
                "name": "Mine",
                "shared": true,
                "queues_filter": ["rdc://queues/invoices"],
                "query": { "$and": [
                    { "modifier": { "$in": ["https://api.dev.example/v1/users/7"] } }
                ] },
                "organization": "https://api.dev.example/v1/organizations/1"
            }),
        );

        let err = run_at(root, "dev", "prod", MirrorMode::Additive, false, vec![], Carry::NONE)
            .expect_err("a user ref must refuse on an api_base with no /api/v1/ segment");
        let msg = format!("{err:#}");
        for want in [
            "saved-views/mine",
            "query.$and[0].modifier.$in[0]",
            "https://api.dev.example/v1/users/7",
            "cannot cross into 'prod'",
        ] {
            assert!(msg.contains(want), "error must mention {want}, got: {msg}");
        }
    }

    /// A view the target env owns and this run never promoted must not gate the
    /// migration: a raw user URL in its `query` is VALID in its own env (push
    /// accepts it), so the check is scoped to what was promoted.
    #[test]
    fn run_at_ignores_a_target_only_saved_view_with_a_user_ref() {
        let project = saved_view_project();
        let root = project.path();
        write_source_queue(root);
        let target_only = root.join("envs/prod/saved-views/theirs.json");
        write_snapshot_json(
            &target_only,
            &serde_json::json!({
                "name": "Mine",
                "shared": true,
                "queues_filter": [],
                "query": { "$and": [
                    { "modifier": { "$in": ["https://prod.example/api/v1/users/7"] } }
                ] }
            }),
        );
        let before = std::fs::read(&target_only).unwrap();

        run_at(root, "dev", "prod", MirrorMode::Additive, false, vec![], Carry::NONE)
            .expect("a target-native saved view must not block a migration that skips it");

        assert_eq!(
            std::fs::read(&target_only).unwrap(),
            before,
            "the target-only view must be left byte-untouched"
        );
    }

    /// Unit test of the projection itself: writes are added, prunes subtracted.
    #[test]
    fn projected_known_adds_writes_and_subtracts_prunes() {
        let existing = vec![PathBuf::from("labels/kept.json")];
        let would_write = vec![PathBuf::from("workspaces/main/queues/invoices/queue.json")];
        let pruned = vec![PathBuf::from("labels/kept.json")];

        let got = projected_known(ProjectedPaths {
            existing: &existing,
            would_write: &would_write,
            pruned: &pruned,
        });
        assert!(
            got.contains(&("queues".to_string(), "invoices".to_string())),
            "a queue this run would write must be known: {got:?}",
        );
        assert!(
            !got.contains(&("labels".to_string(), "kept".to_string())),
            "a pruned object must not be known: {got:?}",
        );
    }

    /// A write always beats a prune of the SAME `(kind, slug)`. `classify_workspace`
    /// derives a queue/schema/inbox's slug from the queue path component, not the
    /// workspace one, so a queue that moves workspace between envs (a renamed
    /// workspace, a reparented queue) classifies identically at its old and new
    /// path: the run writes it at the new path and `--mirror` prunes the old one
    /// in the SAME run. Ordering the subtraction before the union — never the
    /// reverse — is what keeps that object known.
    #[test]
    fn projected_known_a_write_beats_a_prune_of_the_same_slug() {
        let existing = vec![PathBuf::from("workspaces/old/queues/invoices/queue.json")];
        let would_write = vec![PathBuf::from("workspaces/new/queues/invoices/queue.json")];
        let pruned = vec![PathBuf::from("workspaces/old/queues/invoices/queue.json")];

        let got = projected_known(ProjectedPaths {
            existing: &existing,
            would_write: &would_write,
            pruned: &pruned,
        });
        assert!(
            got.contains(&("queues".to_string(), "invoices".to_string())),
            "a queue this run writes at a new path must stay known even though the \
             same-slug object at its old path is pruned in the same run: {got:?}",
        );
    }

    /// `--dry-run` must forecast the refusal rather than printing an info line.
    /// Fails before this change: dry-run skipped validation and returned Ok.
    #[test]
    fn dry_run_refuses_a_saved_view_ref_missing_from_the_target() {
        let project = saved_view_project();
        let root = project.path();
        write_source_queue(root);
        write_snapshot_json(
            &root.join("envs/dev/saved-views/scoped.json"),
            &serde_json::json!({
                "name": "Scoped",
                "shared": true,
                "queues_filter": ["rdc://queues/not-in-target"],
                "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
                "organization": "https://dev.example/api/v1/organizations/1"
            }),
        );

        // 5th positional arg is `dry_run`.
        let err = run_at(root, "dev", "prod", MirrorMode::Additive, true, vec![], Carry::NONE)
            .expect_err("--dry-run must forecast the refusal");
        let msg = format!("{err:#}");
        for want in ["saved-views/scoped", "queues_filter[0]", "not-in-target"] {
            assert!(msg.contains(want), "error must mention {want}, got: {msg}");
        }
        assert!(
            !root.join("envs/prod/saved-views/scoped.json").exists(),
            "--dry-run must still write nothing",
        );
    }

    /// The other direction, and the reason a disk-only `known` is wrong: the
    /// target starts EMPTY, so the queue this view points at exists only in the
    /// source. A dry run must NOT call that unresolvable.
    #[test]
    fn dry_run_accepts_a_ref_to_a_queue_this_run_would_create() {
        let project = saved_view_project();
        let root = project.path();
        write_source_queue(root); // creates queue `invoices` in the SOURCE only
        write_snapshot_json(
            &root.join("envs/dev/saved-views/scoped.json"),
            &serde_json::json!({
                "name": "Scoped",
                "shared": true,
                "queues_filter": ["rdc://queues/invoices"],
                "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
                "organization": "https://dev.example/api/v1/organizations/1"
            }),
        );

        run_at(root, "dev", "prod", MirrorMode::Additive, true, vec![], Carry::NONE)
            .expect("a ref to a queue this run would create must not be refused");
    }

    /// The equivalence pin. Collapsing the two modes onto one path is only safe
    /// if they agree, so assert they reach the SAME verdict over the same
    /// fixture — both Ok when refs resolve, both Err naming the same ref when
    /// they do not.
    #[test]
    fn dry_run_and_real_run_reach_the_same_saved_view_verdict() {
        // Resolvable: both modes accept.
        for dry in [true, false] {
            let project = saved_view_project();
            let root = project.path();
            write_source_queue(root);
            write_snapshot_json(
                &root.join("envs/dev/saved-views/ok.json"),
                &serde_json::json!({
                    "name": "Ok",
                    "shared": true,
                    "queues_filter": ["rdc://queues/invoices"],
                    "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
                    "organization": "https://dev.example/api/v1/organizations/1"
                }),
            );
            run_at(root, "dev", "prod", MirrorMode::Additive, dry, vec![], Carry::NONE)
                .unwrap_or_else(|e| panic!("dry_run={dry} must accept, got: {e:#}"));
        }

        // Unresolvable: both modes refuse, naming the same ref.
        for dry in [true, false] {
            let project = saved_view_project();
            let root = project.path();
            write_source_queue(root);
            write_snapshot_json(
                &root.join("envs/dev/saved-views/bad.json"),
                &serde_json::json!({
                    "name": "Bad",
                    "shared": true,
                    "queues_filter": ["rdc://queues/not-in-target"],
                    "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
                    "organization": "https://dev.example/api/v1/organizations/1"
                }),
            );
            let err = run_at(root, "dev", "prod", MirrorMode::Additive, dry, vec![], Carry::NONE)
                .unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("not-in-target"),
                "dry_run={dry} must name the offending ref, got: {msg}",
            );
        }
    }

    /// Every object at risk is named, across kinds and however many there are
    /// per kind — a refusal that lists one of three deletions would send the
    /// reader to look for the other two themselves.
    #[test]
    fn format_recreate_error_names_every_object_and_one_copyable_row() {
        let groups = vec![
            RecreateGroup {
                kind: "queues",
                deleted: vec![("invoices".into(), 502), ("orders".into(), 504)],
                created: vec!["vendor-invoices".into(), "sales-orders".into()],
            },
            RecreateGroup {
                kind: "schemas",
                deleted: vec![("invoices".into(), 503)],
                created: vec!["vendor-invoices".into()],
            },
        ];
        let msg = format_recreate_error(&groups, "dev", "prod");
        for want in [
            "invoices (id 502)",
            "orders (id 504)",
            "vendor-invoices",
            "sales-orders",
            "schemas",
            "invoices (id 503)",
            "--allow-recreate",
            "rdc doctor dev",
        ] {
            assert!(msg.contains(want), "must mention {want}:\n{msg}");
        }
        // Exactly one worked example, from the first pair.
        assert_eq!(msg.matches("[[").count(), 1, "one sample row only:\n{msg}");
        assert!(msg.contains("[[queues]]\n  dev = \"vendor-invoices\"\n  prod = \"invoices\""), "{msg}");
    }

    /// The guard reads the target's lockfile, so an object the target tracks
    /// with no remote id (or none at all) is not something a prune destroys.
    #[test]
    fn recreate_groups_ignores_a_prune_with_no_remote_identity() {
        use std::collections::BTreeSet;
        let tmp = tempfile::TempDir::new().unwrap();
        let tgt_root = tmp.path().join("envs/prod");
        let old = PathBuf::from("workspaces/main/queues/invoices/queue.json");
        let new = PathBuf::from("workspaces/main/queues/vendor-invoices/queue.json");
        std::fs::create_dir_all(tgt_root.join(old.parent().unwrap())).unwrap();
        std::fs::write(tgt_root.join(&old), b"{}").unwrap();

        let produced: BTreeSet<PathBuf> = BTreeSet::from([new]);
        // Empty lockfile: prod has never pushed this queue.
        let groups =
            recreate_groups(
                std::slice::from_ref(&old),
                &produced,
                &tgt_root,
                "prod",
                &Default::default(),
            )
            .unwrap();
        assert!(groups.is_empty(), "nothing live to lose: {groups:?}");

        // Same run, now with an id recorded for it.
        let mut lf = crate::state::Lockfile::default();
        lf.upsert(
            "queues",
            "invoices",
            crate::state::ObjectEntry {
                id: 502,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let groups =
            recreate_groups(std::slice::from_ref(&old), &produced, &tgt_root, "prod", &lf).unwrap();
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].kind, "queues");
        assert_eq!(groups[0].deleted, vec![("invoices".to_string(), 502)]);
        assert_eq!(groups[0].created, vec!["vendor-invoices".to_string()]);
    }
}
