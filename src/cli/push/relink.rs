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
    kind: &str,
    fields: &[(String, Value)],
    lockfile: &Lockfile,
) -> Result<Map<String, Value>, Vec<String>> {
    let mut body = Map::new();
    let mut unresolved = Vec::new();
    for (name, orig) in fields {
        // Never relink a field `strip_for_create` removes. It only reached the
        // deferred set because it held `rdc://` refs at create time, but it's a
        // server-derived back-reference (e.g. a queue's `webhooks`/`hooks`/
        // `rules`, populated from each child's `queues`) or self-identity
        // (`url`). PATCHing it is a no-op at best and 400s at worst (`webhooks`
        // only accepts `/webhooks/<id>` URLs the resolver never emits).
        if crate::snapshot::create::is_server_stripped(kind, name) {
            continue;
        }
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
        let body = match resolve_relink_body(&it.kind, &it.fields, lockfile) {
            Ok(b) => b,
            Err(unresolved) => {
                failures.push(format!(
                    "{}/{}: unresolved reference(s) {:?} — target object not present in this environment",
                    it.kind, it.slug, unresolved
                ));
                continue;
            }
        };
        // Every deferred field was a server-derived back-reference or
        // self-identity (dropped by `resolve_relink_body`); there is nothing to
        // PATCH. The link already exists (the server populated it from the child
        // side), so skip the request rather than send an empty body.
        if body.is_empty() {
            continue;
        }
        // Log the fields we actually relink (post-filter), not the raw deferred
        // set — server-derived back-refs were dropped above.
        let relinked: Vec<String> = body.keys().cloned().collect();
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
                progress.event(Action::Patch, &format!("relink {}/{} {:?}", it.kind, it.slug, relinked));
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
        let lockfile = lf(api_base, "engines", "1-inbox-sorting", 392);
        let fields = vec![("engine".to_string(),
            serde_json::json!("rdc://engines/1-inbox-sorting"))];
        let body = resolve_relink_body("queues", &fields, &lockfile).expect("should resolve");
        assert_eq!(body["engine"], format!("{api_base}/engines/392"));
    }

    #[test]
    fn resolve_relink_body_errors_listing_unresolved() {
        let lockfile = Lockfile { api_base: "https://x.rossum.app/api/v1".into(), ..Lockfile::default() };
        let fields = vec![("engine".to_string(),
            serde_json::json!("rdc://engines/never-created"))];
        let err = resolve_relink_body("queues", &fields, &lockfile).unwrap_err();
        assert_eq!(err, vec!["rdc://engines/never-created".to_string()]);
    }

    /// Regression: a queue's `webhooks` (and `hooks`/`rules`) is a server-derived
    /// back-reference `strip_for_create` removes; it lands in the deferred set
    /// only because it held `rdc://` refs at create time. It must NOT be
    /// relinked — the `webhooks` field only accepts `/webhooks/<id>` URLs the
    /// resolver never emits (it produces `/hooks/<id>`), so PATCHing it 400s
    /// with "Invalid hyperlink - Incorrect URL match". The link already exists
    /// (Rossum populates it from `hook.queues`), so the correct body omits it.
    #[test]
    fn resolve_relink_body_skips_server_derived_backrefs() {
        let api_base = "https://x.rossum.app/api/v1";
        let mut lockfile = lf(api_base, "engines", "e1", 392);
        lockfile.upsert("hooks", "validator",
            ObjectEntry { id: 55, modified_at: None, content_hash: None, secrets_hash: None });
        // `webhooks` resolves fine (to /hooks/55) but must still be dropped;
        // `engine` is a real cross-ref and must survive.
        let fields = vec![
            ("webhooks".to_string(), serde_json::json!(["rdc://hooks/validator"])),
            ("engine".to_string(), serde_json::json!("rdc://engines/e1")),
        ];
        let body = resolve_relink_body("queues", &fields, &lockfile).expect("should resolve");
        assert!(!body.contains_key("webhooks"), "server back-ref must be dropped");
        assert_eq!(body["engine"], format!("{api_base}/engines/392"));
    }

    /// When every deferred field is a server-derived back-ref, the relink body
    /// is empty — the caller must skip the PATCH entirely rather than send `{}`.
    #[test]
    fn resolve_relink_body_empty_when_only_backrefs() {
        let api_base = "https://x.rossum.app/api/v1";
        let mut lockfile = Lockfile { api_base: api_base.into(), ..Lockfile::default() };
        lockfile.upsert("hooks", "validator",
            ObjectEntry { id: 55, modified_at: None, content_hash: None, secrets_hash: None });
        let fields = vec![
            ("webhooks".to_string(), serde_json::json!(["rdc://hooks/validator"])),
            ("url".to_string(), serde_json::json!("rdc://queues/q1")),
        ];
        let body = resolve_relink_body("queues", &fields, &lockfile).expect("should resolve");
        assert!(body.is_empty(), "only back-refs → empty body");
    }
}
