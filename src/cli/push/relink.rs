//! Two-phase push relink: PATCH cross-references that were deferred during the
//! skeleton create/patch because their target object did not yet exist.

use crate::snapshot::refs::{residual_rdc_refs, resolve_value};
use crate::state::Lockfile;
use serde_json::{Map, Value};

/// One object that had ≥1 cross-reference field deferred during push. The
/// object itself (`kind`/`slug`) already exists + is lockfile-pinned; `fields`
/// are the `(name, original_rdc_value)` pairs to re-resolve and PATCH.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredRelink {
    pub kind: String,
    pub slug: String,
    pub path: std::path::PathBuf,
    pub fields: Vec<(String, Value)>,
}

/// Re-resolve every deferred field against the now-complete lockfile.
/// Returns `Ok(patch_body)` when all fields fully resolve, or `Err(unresolved)`
/// listing every `rdc://` ref that still has no target (the referenced object
/// was never created — e.g. an engine whose create was 403-skipped).
pub fn resolve_relink_body(
    fields: &[(String, Value)],
    lockfile: &Lockfile,
) -> Result<Map<String, Value>, Vec<String>> {
    let mut body = Map::new();
    let mut unresolved = Vec::new();
    for (name, orig) in fields {
        let mut v = orig.clone();
        resolve_value(&mut v, lockfile);
        let residual = residual_rdc_refs(&v);
        if residual.is_empty() {
            body.insert(name.clone(), v);
        } else {
            unresolved.extend(residual);
        }
    }
    if unresolved.is_empty() {
        Ok(body)
    } else {
        unresolved.sort();
        unresolved.dedup();
        Err(unresolved)
    }
}

use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::snapshot::codec::{codec, combined_hash};
use anyhow::Result;
use std::sync::Arc;

/// PATCH every deferred cross-reference now that all objects exist. Applies all
/// resolvable relinks; collects failures (unresolved refs OR API rejections)
/// and returns them so the caller can fail loud after the whole pass.
pub async fn run_relink(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut crate::state::Lockfile,
    items: &[DeferredRelink],
    progress: &Arc<Log>,
) -> Result<Vec<String>> {
    let mut failures = Vec::new();
    for it in items {
        let field_names: Vec<&String> = it.fields.iter().map(|(k, _)| k).collect();
        let Some(entry) = lockfile.objects.get(&it.kind).and_then(|m| m.get(&it.slug)) else {
            failures.push(format!(
                "{}/{}: object missing in lockfile (its create was skipped/failed); cannot relink {:?}",
                it.kind, it.slug, field_names
            ));
            continue;
        };
        let id = entry.id;
        let body = match resolve_relink_body(&it.fields, lockfile) {
            Ok(b) => b,
            Err(unresolved) => {
                failures.push(format!(
                    "{}/{}: unresolved reference(s) {:?} — target object not present in this environment",
                    it.kind, it.slug, unresolved
                ));
                continue;
            }
        };
        let api_path = format!("/{}/{}", it.kind, id); // endpoint == kind for queues/engines
        match client
            .patch_value(&api_path, &Value::Object(body), Some(progress.clone()))
            .await
        {
            Ok(updated) => {
                // Post-write bookkeeping mirrors a normal push: rewrite the disk
                // file + re-record the lockfile hash from the relinked object so
                // the subsequent portabilize_refs post-pass (URL -> rdc://) lands
                // on Clean.
                if let Some(c) = codec(&it.kind) {
                    let art = c.disk_bytes(&updated)?;
                    let hash = combined_hash(&art.json, &art.sidecars, lockfile);
                    crate::state::base_cache::write_disk_and_cache(paths, &it.path, &art.json)?;
                    let modified_at = updated
                        .get("modified_at")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    crate::cli::pull::common::record_object(
                        lockfile, &it.kind, &it.slug, id, modified_at, Some(hash),
                    );
                }
                progress.event(Action::Patch, &format!("relink {}/{} {:?}", it.kind, it.slug, field_names));
            }
            Err(e) => failures.push(format!("{}/{}: PATCH {} rejected: {e:#}", it.kind, it.slug, api_path)),
        }
    }
    Ok(failures)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Lockfile, ObjectEntry};

    fn lf(api_base: &str, kind: &str, slug: &str, id: u64) -> Lockfile {
        let mut lf = Lockfile { api_base: api_base.to_string(), ..Lockfile::default() };
        lf.upsert(kind, slug, ObjectEntry { id, modified_at: None, content_hash: None, secrets_hash: None });
        lf
    }

    #[test]
    fn resolve_relink_body_resolves_when_target_now_exists() {
        let api_base = "https://x.rossum.app/api/v1";
        let lockfile = lf(api_base, "engines", "1-intake-triage-ops", 392);
        let fields = vec![("engine".to_string(),
            serde_json::json!("rdc://engines/1-intake-triage-ops"))];
        let body = resolve_relink_body(&fields, &lockfile).expect("should resolve");
        assert_eq!(body["engine"], format!("{api_base}/engines/392"));
    }

    #[test]
    fn resolve_relink_body_errors_listing_unresolved() {
        let lockfile = Lockfile { api_base: "https://x.rossum.app/api/v1".into(), ..Lockfile::default() };
        let fields = vec![("engine".to_string(),
            serde_json::json!("rdc://engines/never-created"))];
        let err = resolve_relink_body(&fields, &lockfile).unwrap_err();
        assert_eq!(err, vec!["rdc://engines/never-created".to_string()]);
    }
}
