//! Shared helpers for `rdc apply`: URL rewriting from src to tgt URLs
//! across cross-references, plus drift + idempotency checks.

use crate::cli::pull::common::maybe_strip_overlay;
use crate::mapping::Mapping;
use crate::state::{content_hash, Lockfile};
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;

/// Walk a Value and rewrite any string that's a URL of a known src object
/// (per `src_lockfile.lookup_url`) into the equivalent tgt URL (via
/// `mapping.lookup_tgt_slug` + `tgt_lockfile.url_for_slug`). Strings that
/// don't match a known URL are left alone.
///
/// This catches every URL-shaped cross-reference (hooks.queues, rule.queues,
/// queue.schema, queue.hooks, email_template.queue, etc.) without needing
/// a hardcoded per-kind list. URLs in description / metadata fields are
/// also rewritten if they happen to point at known objects, which is
/// almost always what you want.
pub fn rewrite_urls(
    value: &mut Value,
    src_lockfile: &Lockfile,
    tgt_lockfile: &Lockfile,
    mapping: &Mapping,
) {
    walk_strings_mut(value, &mut |s| {
        let Some((kind, src_slug)) = src_lockfile.lookup_url(s) else { return };
        let Some(tgt_slug) = mapping.lookup_tgt_slug(kind, src_slug) else { return };
        let Some(tgt_url) = tgt_lockfile.url_for_slug(kind, tgt_slug) else { return };
        *s = tgt_url.to_string();
    });
}

fn walk_strings_mut(value: &mut Value, f: &mut dyn FnMut(&mut String)) {
    match value {
        Value::String(s) => f(s),
        Value::Array(arr) => {
            for item in arr {
                walk_strings_mut(item, f);
            }
        }
        Value::Object(obj) => {
            for (_k, v) in obj.iter_mut() {
                walk_strings_mut(v, f);
            }
        }
        _ => {}
    }
}

/// Drift check: hash the post-overlay-strip remote bytes and compare to
/// the tgt lockfile's recorded `content_hash`. Returns `Ok(true)` when in
/// sync (safe to apply), `Ok(false)` when drifted (refuse with warning).
/// Lockfile entries with no `content_hash` (older snapshots) yield `true`
/// to avoid spurious blocks.
pub fn tgt_in_sync(
    remote_bytes: Vec<u8>,
    overlay_paths: Option<&BTreeMap<String, Value>>,
    tgt_lockfile: &Lockfile,
    kind: &str,
    tgt_slug: &str,
) -> Result<bool> {
    let stripped = maybe_strip_overlay(remote_bytes, overlay_paths)?;
    let remote_hash = content_hash(&stripped);
    let base = tgt_lockfile
        .objects
        .get(kind)
        .and_then(|m| m.get(tgt_slug))
        .and_then(|e| e.content_hash.as_deref());
    Ok(base.map(|b| b == remote_hash).unwrap_or(true))
}

/// Idempotency: payload bytes vs current remote bytes (full forms,
/// without overlay strip — both have overlay-applied values for an
/// unchanged tgt). Returns `true` when a PATCH would be a no-op.
pub fn payload_matches_remote(payload_bytes: &[u8], remote_bytes: &[u8]) -> bool {
    payload_bytes == remote_bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ObjectEntry;

    fn lf_with(entries: &[(&str, &str, u64, &str)]) -> Lockfile {
        let mut lf = Lockfile::default();
        for (kind, slug, id, url) in entries {
            lf.upsert(
                kind,
                slug,
                ObjectEntry {
                    id: *id,
                    url: Some(url.to_string()),
                    modified_at: None,
                    content_hash: None,
                },
            );
        }
        lf
    }

    #[test]
    fn rewrite_urls_swaps_known_src_url() {
        let src = lf_with(&[("queues", "cost-invoices", 100, "https://test/api/v1/queues/100")]);
        let tgt = lf_with(&[("queues", "cost-invoices", 700, "https://prod/api/v1/queues/700")]);
        let mut mapping = Mapping::default();
        mapping.queues.insert("cost-invoices".into(), "cost-invoices".into());
        let mut payload = serde_json::json!({
            "queues": ["https://test/api/v1/queues/100"]
        });
        rewrite_urls(&mut payload, &src, &tgt, &mapping);
        assert_eq!(
            payload["queues"][0],
            serde_json::Value::String("https://prod/api/v1/queues/700".into()),
        );
    }

    #[test]
    fn rewrite_urls_handles_mapping_with_renamed_slug() {
        let src = lf_with(&[("hooks", "validator", 1, "https://test/api/v1/hooks/1")]);
        let tgt = lf_with(&[("hooks", "validator-prod", 99, "https://prod/api/v1/hooks/99")]);
        let mut mapping = Mapping::default();
        mapping.hooks.insert("validator".into(), "validator-prod".into());
        let mut payload = serde_json::json!({
            "ref": "https://test/api/v1/hooks/1",
            "label": "stays unchanged",
        });
        rewrite_urls(&mut payload, &src, &tgt, &mapping);
        assert_eq!(payload["ref"].as_str().unwrap(), "https://prod/api/v1/hooks/99");
        assert_eq!(payload["label"].as_str().unwrap(), "stays unchanged");
    }

    #[test]
    fn rewrite_urls_leaves_unknown_urls_alone() {
        let src = lf_with(&[]);
        let tgt = lf_with(&[]);
        let mapping = Mapping::default();
        let mut payload = serde_json::json!({"description": "see https://docs.rossum.ai"});
        rewrite_urls(&mut payload, &src, &tgt, &mapping);
        assert_eq!(payload["description"].as_str().unwrap(), "see https://docs.rossum.ai");
    }

    #[test]
    fn rewrite_urls_walks_nested_arrays_and_objects() {
        let src = lf_with(&[("queues", "q", 1, "https://test/api/v1/queues/1")]);
        let tgt = lf_with(&[("queues", "q", 2, "https://prod/api/v1/queues/2")]);
        let mut mapping = Mapping::default();
        mapping.queues.insert("q".into(), "q".into());
        let mut payload = serde_json::json!({
            "outer": {
                "inner": ["https://test/api/v1/queues/1"]
            }
        });
        rewrite_urls(&mut payload, &src, &tgt, &mapping);
        assert_eq!(
            payload["outer"]["inner"][0].as_str().unwrap(),
            "https://prod/api/v1/queues/2"
        );
    }
}
