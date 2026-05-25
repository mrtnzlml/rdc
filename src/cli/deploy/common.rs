//! Shared helpers for `rdc deploy`'s update phase: URL rewriting from
//! src to tgt URLs across cross-references, plus drift + idempotency
//! checks.

use crate::cli::pull::common::maybe_strip_overlay;
use crate::mapping::Mapping;
use crate::snapshot::create::strip_for_cross_env_patch;
use crate::snapshot::noise::{sort_keys_recursive, strip_noise_fields};
use crate::state::{content_hash, Lockfile};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

/// Normalise JSON bytes for a cross-env idempotency check: strip the fields
/// that are guaranteed to differ between two envs (`id`, `url`, the resource's
/// own `organization` URL), strip the noise fields (`modified_at`, `modifier`)
/// that change on every server-side touch, strip the kind-specific
/// server-computed sub-collections (`queue.hooks`, `queue.webhooks`, etc.)
/// that mirror back-references, sort URL arrays, then sort every object's
/// keys alphabetically before re-serialising. Two normalised payloads
/// compare equal iff they represent the same canonical resource state.
///
/// Without this normalisation, byte-level equality between the src snapshot
/// and the tgt remote would never hold and `rdc deploy` would re-PATCH on
/// every run (`README` "Idempotency" claim). The recursive key sort also
/// keeps `rdc diff <src> <tgt>` quiet when only the API's key emission
/// order differs — the Rossum API doesn't guarantee stable key order
/// across endpoints, and on-disk files written by different code paths
/// (`pull` vs the queue-auto-create email-template capture, say) end up
/// with their `extra` IndexMaps populated in different orders.
pub fn normalize_for_cross_env_compare(bytes: &[u8], kind: &str) -> Result<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(bytes)
        .context("parsing JSON for cross-env normalisation")?;
    strip_for_cross_env_patch(&mut value, kind);
    strip_noise_fields(&mut value);
    sort_string_arrays(&mut value);
    sort_keys_recursive(&mut value);
    let mut out = serde_json::to_vec_pretty(&value)
        .context("re-serialising normalised JSON")?;
    out.push(b'\n');
    Ok(out)
}

/// Recursively sort every all-string array in the tree alphabetically.
///
/// Rossum returns set-like URL arrays (a hook's `queues`, its `run_after`,
/// `events`) sorted by the server's internal numeric id. The same set of
/// queues attached to a sandbox hook and a prod hook will therefore appear
/// in *different* orders because the ids are assigned per-env; after URL
/// rewriting the contents match but the ordering doesn't. Sorting both sides
/// alphabetically before comparing makes the idempotency check
/// order-insensitive for these set-like fields, which is the README's
/// "0 PATCHes on re-apply" contract.
///
/// Mixed-type arrays (containing objects, numbers, etc.) are left alone —
/// stable order matters for `content[]` schema definitions where the array
/// order *is* the field order users see in the UI.
fn sort_string_arrays(value: &mut Value) {
    match value {
        Value::Array(arr) => {
            let all_strings = arr.iter().all(|v| matches!(v, Value::String(_)));
            if all_strings {
                arr.sort_by(|a, b| match (a, b) {
                    (Value::String(s1), Value::String(s2)) => s1.cmp(s2),
                    _ => std::cmp::Ordering::Equal,
                });
            } else {
                for v in arr {
                    sort_string_arrays(v);
                }
            }
        }
        Value::Object(obj) => {
            for v in obj.values_mut() {
                sort_string_arrays(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

    #[test]
    fn sort_string_arrays_sorts_top_level_url_array() {
        let mut v = serde_json::json!({"queues": ["https://x/queues/3", "https://x/queues/1", "https://x/queues/2"]});
        sort_string_arrays(&mut v);
        assert_eq!(
            v,
            serde_json::json!({"queues": ["https://x/queues/1", "https://x/queues/2", "https://x/queues/3"]})
        );
    }

    #[test]
    fn sort_string_arrays_leaves_mixed_arrays_alone() {
        // schema content[] mixes datapoint objects + section objects;
        // their order is the UI field order and must not be sorted.
        let mut v = serde_json::json!({"content": [{"id": "b"}, {"id": "a"}]});
        sort_string_arrays(&mut v);
        assert_eq!(v, serde_json::json!({"content": [{"id": "b"}, {"id": "a"}]}));
    }

    #[test]
    fn sort_string_arrays_recurses_into_objects() {
        let mut v = serde_json::json!({"config": {"sideload": ["b", "a"]}});
        sort_string_arrays(&mut v);
        assert_eq!(v, serde_json::json!({"config": {"sideload": ["a", "b"]}}));
    }

    #[test]
    fn normalize_collapses_real_world_email_template_noise() {
        // Regression: the deploy update-diff used to render raw
        // `payload_bytes` vs `remote_bytes`, so dry-run previews padded
        // every email_template PATCH with ~14 lines of server-only
        // fields (`id`/`url`/`organization`/`modified_at`/`modified_by`/
        // `triggers`) plus key-reordering jitter — the comparison
        // already ignored all of it. This test pins the contract that
        // a src snapshot lacking those fields and a tgt remote carrying
        // them produce byte-identical normalised forms, so the diff
        // (which now goes through this same normaliser) collapses to
        // empty when the content really hasn't changed.
        let src = br#"{
          "name": "Annotation status change - received",
          "subject": "Documents received: {{ parent_email_subject }}",
          "queue": "https://ferguson-test.rossum.app/api/v1/queues/2860442",
          "automate": false,
          "bcc": [],
          "cc": [],
          "enabled": false,
          "message": "<p>Hi</p>",
          "to": [{"email": "{{sender_email}}"}],
          "type": "custom"
        }"#;
        let tgt = br#"{
          "id": 14081418,
          "url": "https://ferguson-test.rossum.app/api/v1/email_templates/14081418",
          "name": "Annotation status change - received",
          "subject": "Documents received: {{ parent_email_subject }}",
          "queue": "https://ferguson-test.rossum.app/api/v1/queues/2860442",
          "organization": "https://ferguson-test.rossum.app/api/v1/organizations/418976",
          "message": "<p>Hi</p>",
          "type": "custom",
          "enabled": false,
          "automate": false,
          "triggers": ["https://ferguson-test.rossum.app/api/v1/triggers/11157520"],
          "to": [{"email": "{{sender_email}}"}],
          "cc": [],
          "bcc": [],
          "modified_by": null,
          "modified_at": null
        }"#;
        let ns = normalize_for_cross_env_compare(src, "email_templates").unwrap();
        let nt = normalize_for_cross_env_compare(tgt, "email_templates").unwrap();
        assert_eq!(
            std::str::from_utf8(&ns).unwrap(),
            std::str::from_utf8(&nt).unwrap(),
            "src vs tgt with server-only fields must normalise to the same bytes",
        );
    }

    #[test]
    fn normalize_is_key_order_insensitive() {
        // Two JSON bodies with the same content but different key order
        // (the Rossum API doesn't guarantee stable key order, and on-disk
        // files written by different code paths end up with different
        // `extra` IndexMap orders). After normalisation they must compare
        // byte-equal so (a) `rdc diff <src> <tgt>` doesn't show spurious
        // key-reordering churn, and (b) `bytes_equal_after_strip` doesn't
        // PATCH on every re-deploy.
        // The `id`/`url`/`organization` differences are stripped by
        // `strip_for_cross_env_patch`; what's left has the same content
        // in different key orders.
        let a = br#"{
          "id": 1,
          "url": "https://src/api/v1/email_templates/1",
          "organization": "https://src/api/v1/organizations/1",
          "name": "T",
          "subject": "S",
          "queue": "https://shared/api/v1/queues/100",
          "automate": false,
          "bcc": [],
          "cc": [],
          "enabled": false,
          "message": "Hi",
          "to": [{"email": "{{sender_email}}"}],
          "type": "custom"
        }"#;
        let b = br#"{
          "id": 999,
          "url": "https://tgt/api/v1/email_templates/999",
          "organization": "https://tgt/api/v1/organizations/2",
          "queue": "https://shared/api/v1/queues/100",
          "cc": [],
          "bcc": [],
          "name": "T",
          "subject": "S",
          "message": "Hi",
          "type": "custom",
          "enabled": false,
          "automate": false,
          "to": [{"email": "{{sender_email}}"}]
        }"#;
        let na = normalize_for_cross_env_compare(a, "email_templates").unwrap();
        let nb = normalize_for_cross_env_compare(b, "email_templates").unwrap();
        assert_eq!(
            std::str::from_utf8(&na).unwrap(),
            std::str::from_utf8(&nb).unwrap(),
            "different key order must normalise to the same bytes",
        );
    }
}

/// Convenience: are two serialised payloads equivalent under
/// `normalize_for_cross_env_compare`? Used by `rdc deploy` to decide whether
/// the src snapshot already matches the tgt remote and the PATCH can be
/// skipped — i.e. the README's "0 PATCHes on re-deploy" idempotency claim.
pub fn bytes_equal_after_strip(a: &[u8], b: &[u8], kind: &str) -> Result<bool> {
    Ok(normalize_for_cross_env_compare(a, kind)? == normalize_for_cross_env_compare(b, kind)?)
}

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
///
/// Special case: `kind == "organization"` is a per-env singleton with slug
/// `"self"`, not a deployable kind, and so never appears in `mapping`. But
/// `organization` is a REQUIRED field on workspace POST and the Rossum API
/// rejects a cross-env body that carries the src org URL with
/// `400 {"organization":["Invalid hyperlink - Object does not exist."]}`.
/// Bypass the mapping lookup for this kind and substitute the tgt org URL
/// directly from `tgt_lockfile` (the pull pipeline always captures it).
pub fn rewrite_urls(
    value: &mut Value,
    src_lockfile: &Lockfile,
    tgt_lockfile: &Lockfile,
    mapping: &Mapping,
    explicit_subs: &std::collections::BTreeMap<String, String>,
) {
    walk_strings_mut(value, &mut |s| {
        if let Some(tgt) = explicit_subs.get(s.as_str()) {
            *s = tgt.clone();
            return;
        }
        let Some((kind, src_slug)) = src_lockfile.lookup_url(s) else { return };
        let tgt_slug = if kind == "organization" {
            src_slug
        } else {
            let Some(s2) = mapping.lookup_tgt_slug(kind, src_slug) else { return };
            s2
        };
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
/// the tgt lockfile's recorded `content_hash`. Returns `(in_sync,
/// current_remote_hash)`. Use the hash to refresh the lockfile entry
/// when adopting out-of-band changes.
///
/// Lockfile entries with no `content_hash` (older snapshots) yield
/// `in_sync = true` so deploys don't spuriously block on legacy state.
pub fn tgt_drift_status(
    remote_bytes: Vec<u8>,
    overlay_paths: Option<&BTreeMap<String, Value>>,
    tgt_lockfile: &Lockfile,
    kind: &str,
    tgt_slug: &str,
) -> Result<(bool, String)> {
    let stripped = maybe_strip_overlay(remote_bytes, overlay_paths)?;
    let remote_hash = content_hash(&stripped);
    let base = tgt_lockfile
        .objects
        .get(kind)
        .and_then(|m| m.get(tgt_slug))
        .and_then(|e| e.content_hash.as_deref());
    let in_sync = base.map(|b| b == remote_hash).unwrap_or(true);
    Ok((in_sync, remote_hash))
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
                    secrets_hash: None,
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
        rewrite_urls(&mut payload, &src, &tgt, &mapping, &BTreeMap::new());
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
        rewrite_urls(&mut payload, &src, &tgt, &mapping, &BTreeMap::new());
        assert_eq!(payload["ref"].as_str().unwrap(), "https://prod/api/v1/hooks/99");
        assert_eq!(payload["label"].as_str().unwrap(), "stays unchanged");
    }

    #[test]
    fn rewrite_urls_leaves_unknown_urls_alone() {
        let src = lf_with(&[]);
        let tgt = lf_with(&[]);
        let mapping = Mapping::default();
        let mut payload = serde_json::json!({"description": "see https://docs.rossum.ai"});
        rewrite_urls(&mut payload, &src, &tgt, &mapping, &BTreeMap::new());
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
        rewrite_urls(&mut payload, &src, &tgt, &mapping, &BTreeMap::new());
        assert_eq!(
            payload["outer"]["inner"][0].as_str().unwrap(),
            "https://prod/api/v1/queues/2"
        );
    }

    #[test]
    fn rewrite_urls_rewrites_organization_without_mapping_entry() {
        // Regression: cross-env workspace deploy used to POST the src org
        // URL because `mapping.lookup_tgt_slug("organization", "self")`
        // returns None (organization isn't deployable, so no mapping
        // entry). API responded with 400 "Invalid hyperlink - Object does
        // not exist." Fix: bypass mapping for the organization kind and
        // look up the tgt URL directly from tgt_lockfile.
        let src = lf_with(&[("organization", "self", 1, "https://test/api/v1/organizations/1")]);
        let tgt = lf_with(&[("organization", "self", 214757, "https://prod/api/v1/organizations/214757")]);
        let mapping = Mapping::default();
        let mut payload = serde_json::json!({
            "name": "AP",
            "organization": "https://test/api/v1/organizations/1"
        });
        rewrite_urls(&mut payload, &src, &tgt, &mapping, &BTreeMap::new());
        assert_eq!(
            payload["organization"].as_str().unwrap(),
            "https://prod/api/v1/organizations/214757"
        );
    }

    #[test]
    fn rewrite_urls_explicit_subs_take_precedence() {
        let src = Lockfile::default();
        let tgt = Lockfile::default();
        let mapping = Mapping::default();
        let mut subs = BTreeMap::new();
        subs.insert(
            "https://test/api/v1/hook_templates/39".to_string(),
            "https://prod/api/v1/hook_templates/41".to_string(),
        );

        let mut payload = serde_json::json!({
            "hook_template": "https://test/api/v1/hook_templates/39",
            "unrelated": "https://docs.rossum.ai"
        });
        rewrite_urls(&mut payload, &src, &tgt, &mapping, &subs);
        assert_eq!(payload["hook_template"].as_str().unwrap(), "https://prod/api/v1/hook_templates/41");
        assert_eq!(payload["unrelated"].as_str().unwrap(), "https://docs.rossum.ai");
    }
}
