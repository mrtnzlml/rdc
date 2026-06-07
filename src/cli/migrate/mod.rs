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
use std::path::{Path, PathBuf};

/// Look up the tgt slug for `(kind, src_slug)`, falling back to identity
/// (same slug) when the pair isn't mapped — the common auto-matched case.
fn tgt_slug(mapping: &Mapping, kind: &str, src_slug: &str) -> String {
    mapping
        .lookup_tgt_slug(kind, src_slug)
        .map(str::to_string)
        .unwrap_or_else(|| src_slug.to_string())
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
}
