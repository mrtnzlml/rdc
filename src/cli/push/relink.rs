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

/// Keys that must NOT be deferred, per kind, because that driver builds its
/// PATCH body from a **typed** model: an absent key is re-materialized by the
/// round-trip as `null` (`Option<String>`) rather than omitted, and the API
/// reads `null` as "clear this field", not "leave it alone". Deferring one
/// would silently unlink a live object instead of postponing the link.
///
/// Only fields that are (a) modeled explicitly and (b) lack
/// `skip_serializing_if` need listing. A queue's `inbox` is modeled but skips
/// serializing when `None` (the API rejects `inbox: null` — see `model::Queue`),
/// so it omits cleanly and stays deferrable. Everything in a model's
/// `#[serde(flatten)] extra` map — a hook's `run_after`, an engine's
/// `training_queues` — disappears from the body when absent and is deferrable
/// by construction.
///
/// `hooks` is deliberately absent: its driver serializes to a `Value` and
/// removes the deferred keys from the body itself, so every field defers
/// cleanly there, including the modeled `queues`.
fn undeferrable(kind: &str) -> &'static [&'static str] {
    match kind {
        // `workspace`/`schema` are mandatory links; `url` is the queue's own
        // identity. None can be sent as `null`.
        "queues" => &["url", "workspace", "schema"],
        // `Engine` models only id/url/name; the rest lives in `extra`.
        "engines" => &["url"],
        _ => &[],
    }
}

/// Put back any deferred field this `kind` cannot safely omit, restoring its
/// ORIGINAL (still-`rdc://`) value into the payload.
///
/// The effect is that such a field keeps the pre-deferral behavior: the
/// unresolved reference stays in the body and the pre-send guard refuses the
/// request, naming it. That is the honest outcome — a queue whose `workspace`
/// cannot resolve is broken now, not later, and workspaces/schemas are pushed
/// before queues so it only happens when their own create failed.
pub fn restore_undeferrable(
    kind: &str,
    payload: &mut Value,
    deferred: &mut Vec<(String, Value)>,
) {
    let never = undeferrable(kind);
    if never.is_empty() {
        return;
    }
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    deferred.retain(|(name, orig)| {
        if never.contains(&name.as_str()) {
            obj.insert(name.clone(), orig.clone());
            false
        } else {
            true
        }
    });
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
                    // Portabilize so concrete env URLs never land on disk. Relink
                    // runs after every object exists + is lockfile-pinned, so self
                    // and all relinked refs resolve back to rdc://.
                    let json =
                        crate::cli::pull::common::portabilize_proposed(&art.json, lockfile);
                    let hash = combined_hash(&json, &art.sidecars, lockfile);
                    crate::state::base_cache::write_disk_and_cache(paths, &it.path, &json)?;
                    let modified_at = updated
                        .get("modified_at")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let modified_by = updated
                        .get("modified_by")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    crate::cli::pull::common::record_object(
                        lockfile, &it.kind, &it.slug, id, modified_at, modified_by, Some(hash),
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
        lf.upsert(kind, slug, ObjectEntry { id, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None });
        lf
    }

    #[test]
    fn restore_undeferrable_puts_back_a_queue_workspace_and_keeps_the_rest() {
        // `workspace` cannot be deferred: `update_queue` sends a typed `Queue`,
        // so an absent key goes out as `workspace: null` — the API reads that
        // as "clear it". Restoring the original leaves the unresolved ref in
        // the body, where the pre-send guard refuses it by name.
        let mut payload = serde_json::json!({ "name": "Q" });
        let mut deferred = vec![
            ("workspace".to_string(), serde_json::json!("rdc://workspaces/main")),
            ("engine".to_string(), serde_json::json!("rdc://engines/e1")),
        ];
        restore_undeferrable("queues", &mut payload, &mut deferred);
        assert_eq!(payload["workspace"], serde_json::json!("rdc://workspaces/main"));
        assert_eq!(
            deferred,
            vec![("engine".to_string(), serde_json::json!("rdc://engines/e1"))],
            "only the undeferrable field is taken back"
        );
    }

    #[test]
    fn restore_undeferrable_leaves_hooks_alone() {
        // The hooks driver scrubs deferred keys from its Value body itself, so
        // every field defers cleanly there — including the modeled `queues`.
        let mut payload = serde_json::json!({ "name": "H" });
        let mut deferred = vec![
            ("run_after".to_string(), serde_json::json!(["rdc://hooks/x"])),
            ("queues".to_string(), serde_json::json!(["rdc://queues/q1"])),
        ];
        let before = deferred.clone();
        restore_undeferrable("hooks", &mut payload, &mut deferred);
        assert_eq!(deferred, before);
        assert!(payload.get("queues").is_none());
    }

    #[test]
    fn resolve_relink_body_resolves_when_target_now_exists() {
        let api_base = "https://x.rossum.app/api/v1";
        let lockfile = lf(api_base, "engines", "1-intake-triage", 392);
        let fields = vec![("engine".to_string(),
            serde_json::json!("rdc://engines/1-intake-triage"))];
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
            ObjectEntry { id: 55, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None });
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
            ObjectEntry { id: 55, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None });
        let fields = vec![
            ("webhooks".to_string(), serde_json::json!(["rdc://hooks/validator"])),
            ("url".to_string(), serde_json::json!("rdc://queues/q1")),
        ];
        let body = resolve_relink_body("queues", &fields, &lockfile).expect("should resolve");
        assert!(body.is_empty(), "only back-refs → empty body");
    }
}
