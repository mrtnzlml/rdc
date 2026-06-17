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
        let lockfile = lf(api_base, "engines", "1-inbox-sorting-mtr", 392);
        let fields = vec![("engine".to_string(),
            serde_json::json!("rdc://engines/1-inbox-sorting-mtr"))];
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
