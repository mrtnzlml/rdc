//! Shared helpers for `rdc deploy`'s update phase: URL rewriting from
//! src to tgt URLs across cross-references, plus drift + idempotency
//! checks.

use crate::cli::pull::common::maybe_strip_overlay;
use crate::mapping::Mapping;
use crate::snapshot::codec::combined_hash;
use crate::snapshot::create::strip_for_cross_env_patch;
use crate::snapshot::noise::{sort_keys_recursive, sort_string_arrays, strip_noise_fields};
use crate::state::Lockfile;
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
/// keeps `rdc deploy --dry-run` previews quiet when only the API's key
/// emission order differs — the Rossum API doesn't guarantee stable key order
/// across endpoints, and on-disk files written by different code paths
/// (`pull` vs the queue-auto-create email-template capture, say) end up
/// with their `extra` IndexMaps populated in different orders.
pub fn normalize_for_cross_env_compare(bytes: &[u8], kind: &str) -> Result<Vec<u8>> {
    let mut value: Value =
        serde_json::from_slice(bytes).context("parsing JSON for cross-env normalisation")?;
    strip_for_cross_env_patch(&mut value, kind);
    strip_noise_fields(&mut value);
    sort_string_arrays(&mut value);
    sort_keys_recursive(&mut value);
    let mut out = serde_json::to_vec_pretty(&value).context("re-serialising normalised JSON")?;
    out.push(b'\n');
    Ok(out)
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

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
          "queue": "https://example.rossum.app/api/v1/queues/100",
          "automate": false,
          "bcc": [],
          "cc": [],
          "enabled": false,
          "message": "<p>Hi</p>",
          "to": [{"email": "{{sender_email}}"}],
          "type": "custom"
        }"#;
        let tgt = br#"{
          "id": 200,
          "url": "https://example.rossum.app/api/v1/email_templates/200",
          "name": "Annotation status change - received",
          "subject": "Documents received: {{ parent_email_subject }}",
          "queue": "https://example.rossum.app/api/v1/queues/100",
          "organization": "https://example.rossum.app/api/v1/organizations/1",
          "message": "<p>Hi</p>",
          "type": "custom",
          "enabled": false,
          "automate": false,
          "triggers": ["https://example.rossum.app/api/v1/triggers/300"],
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
        // byte-equal so (a) `rdc deploy --dry-run` previews don't show
        // spurious key-reordering churn, and (b) `bytes_equal_after_strip` doesn't
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

    #[test]
    fn normalize_strips_token_owner_for_hooks() {
        // `token_owner` is a per-env user URL — each env's hooks point at
        // that env's users (not a deployable kind). It always differs across
        // envs and is never meaningful cross-env drift, so cross-env
        // normalization must strip it (like id/url/organization), keeping
        // `rdc deploy --dry-run` previews quiet on it.
        let dev = br#"{
          "id": 1,
          "url": "https://dev.example.app/api/v1/hooks/1",
          "name": "h",
          "type": "function",
          "events": ["annotation_status"],
          "queues": [],
          "token_owner": "https://dev.example.app/api/v1/users/111",
          "config": {"runtime": "python3.12"}
        }"#;
        let test = br#"{
          "id": 2,
          "url": "https://test.example.app/api/v1/hooks/2",
          "name": "h",
          "type": "function",
          "events": ["annotation_status"],
          "queues": [],
          "token_owner": "https://test.example.app/api/v1/users/222",
          "config": {"runtime": "python3.12"}
        }"#;
        let nd = normalize_for_cross_env_compare(dev, "hooks").unwrap();
        let nt = normalize_for_cross_env_compare(test, "hooks").unwrap();
        assert_eq!(
            std::str::from_utf8(&nd).unwrap(),
            std::str::from_utf8(&nt).unwrap(),
            "hooks differing only in token_owner must normalise equal",
        );
    }

    #[test]
    fn normalize_strips_name_for_engine_fields() {
        // The Rossum API treats engine_field `name` as immutable after
        // create. A cross-env mapping may legitimately pair two fields
        // with different names (e.g. `item-qty` -> `item-quantity`); the
        // cross-env compare must treat that as a non-difference, otherwise
        // deploy attempts a PATCH that the API rejects with 400.
        let src = br#"{
          "id": 1,
          "url": "https://dev.example.app/api/v1/engine_fields/1",
          "name": "item_qty",
          "engine": "https://example.app/api/v1/engines/100",
          "label": "Qty",
          "type": "string"
        }"#;
        let tgt = br#"{
          "id": 2,
          "url": "https://test.example.app/api/v1/engine_fields/2",
          "name": "item_quantity",
          "engine": "https://example.app/api/v1/engines/100",
          "label": "Qty",
          "type": "string"
        }"#;
        let ns = normalize_for_cross_env_compare(src, "engine_fields").unwrap();
        let nt = normalize_for_cross_env_compare(tgt, "engine_fields").unwrap();
        assert_eq!(
            std::str::from_utf8(&ns).unwrap(),
            std::str::from_utf8(&nt).unwrap(),
            "engine_fields differing only in `name` must normalise equal",
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
        let Some((kind, src_slug)) = src_lockfile.lookup_url(s) else {
            return;
        };
        let tgt_slug = if kind == "organization" {
            src_slug
        } else {
            let Some(s2) = mapping.lookup_tgt_slug(kind, src_slug) else {
                return;
            };
            s2
        };
        let Some(tgt_url) = tgt_lockfile.url_for_slug(kind, tgt_slug) else {
            return;
        };
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

/// Drift check: hash the remote bytes through the kind's [`KindCodec`]
/// (which applies the same redaction that pull/sync record as the
/// baseline — e.g. `agenda_id` → sentinel for engines, `counts` →
/// sentinel for queues, `status` → sentinel for hooks) and compare to
/// the tgt lockfile's recorded `content_hash`. Returns `(in_sync,
/// current_remote_hash)`. Use the hash to refresh the lockfile entry
/// when adopting out-of-band changes.
///
/// Using the codec guarantees the drift hash always agrees with what
/// pull/sync write as the baseline: a single code path handles the
/// per-kind redaction instead of a hand-maintained lookup table
/// (`pull_redacts_kind`, now deleted). Overlay paths are stripped from
/// the JSON before hashing, mirroring the pull driver's post-overlay
/// hash.
///
/// Lockfile entries with no `content_hash` (older snapshots) yield
/// `in_sync = true` so deploys don't spuriously block on legacy state.
///
/// For kinds with no registered codec (defensive — all deployed kinds
/// should be registered), falls back to `content_hash` over the raw
/// overlay-stripped bytes.
pub fn tgt_drift_status(
    remote_bytes: Vec<u8>,
    overlay_paths: Option<&BTreeMap<String, Value>>,
    tgt_lockfile: &Lockfile,
    kind: &str,
    tgt_slug: &str,
) -> Result<(bool, String)> {
    let remote_value: Value =
        serde_json::from_slice(&remote_bytes).context("parsing remote JSON for drift hash")?;
    let remote_hash = if let Some(c) = crate::snapshot::codec::codec(kind) {
        let art = c
            .disk_bytes(&remote_value)
            .with_context(|| format!("codec disk_bytes for {kind}/{tgt_slug}"))?;
        let json_for_hash = maybe_strip_overlay(art.json, overlay_paths)?;
        combined_hash(&json_for_hash, &art.sidecars)
    } else {
        // No codec registered — fall back to overlay-stripped content_hash.
        let stripped = maybe_strip_overlay(remote_bytes, overlay_paths)?;
        crate::state::content_hash(&stripped)
    };
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
    use crate::state::{ObjectEntry, content_hash};

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
        let src = lf_with(&[(
            "queues",
            "cost-invoices",
            100,
            "https://test/api/v1/queues/100",
        )]);
        let tgt = lf_with(&[(
            "queues",
            "cost-invoices",
            700,
            "https://prod/api/v1/queues/700",
        )]);
        let mut mapping = Mapping::default();
        mapping
            .queues
            .insert("cost-invoices".into(), "cost-invoices".into());
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
        let tgt = lf_with(&[(
            "hooks",
            "validator-prod",
            99,
            "https://prod/api/v1/hooks/99",
        )]);
        let mut mapping = Mapping::default();
        mapping
            .hooks
            .insert("validator".into(), "validator-prod".into());
        let mut payload = serde_json::json!({
            "ref": "https://test/api/v1/hooks/1",
            "label": "stays unchanged",
        });
        rewrite_urls(&mut payload, &src, &tgt, &mapping, &BTreeMap::new());
        assert_eq!(
            payload["ref"].as_str().unwrap(),
            "https://prod/api/v1/hooks/99"
        );
        assert_eq!(payload["label"].as_str().unwrap(), "stays unchanged");
    }

    #[test]
    fn rewrite_urls_leaves_unknown_urls_alone() {
        let src = lf_with(&[]);
        let tgt = lf_with(&[]);
        let mapping = Mapping::default();
        let mut payload = serde_json::json!({"description": "see https://docs.rossum.ai"});
        rewrite_urls(&mut payload, &src, &tgt, &mapping, &BTreeMap::new());
        assert_eq!(
            payload["description"].as_str().unwrap(),
            "see https://docs.rossum.ai"
        );
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
        let src = lf_with(&[(
            "organization",
            "self",
            1,
            "https://test/api/v1/organizations/1",
        )]);
        let tgt = lf_with(&[(
            "organization",
            "self",
            214757,
            "https://prod/api/v1/organizations/214757",
        )]);
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
        assert_eq!(
            payload["hook_template"].as_str().unwrap(),
            "https://prod/api/v1/hook_templates/41"
        );
        assert_eq!(
            payload["unrelated"].as_str().unwrap(),
            "https://docs.rossum.ai"
        );
    }

    fn lf_with_hash(kind: &str, slug: &str, hash: &str) -> Lockfile {
        let mut lf = Lockfile::default();
        lf.upsert(
            kind,
            slug,
            ObjectEntry {
                id: 1,
                url: Some(format!("https://test/api/v1/{kind}/1")),
                modified_at: None,
                content_hash: Some(hash.to_string()),
                secrets_hash: None,
            },
        );
        lf
    }

    #[test]
    fn tgt_drift_status_queue_with_live_counts_matches_redacted_baseline() {
        // Regression: deploy reported phantom drift on every queue because
        // it hashed the raw remote bytes (`counts` = live numeric object)
        // while sync's pull driver hashed the redacted form
        // (`counts` = sentinel string). Drift hash must apply the same
        // `redact_for_disk` to match.
        let value = serde_json::json!({
            "id": 1,
            "name": "Inbox sorting",
            "url": "https://test/api/v1/queues/1",
            "counts": { "document_status": { "to_review": 7, "exported": 12 } },
        });
        let baseline_bytes =
            crate::snapshot::create::redacted_disk_bytes(&value, "queues").unwrap();
        let baseline_hash = content_hash(&baseline_bytes);
        let lf = lf_with_hash("queues", "inbox-sorting", &baseline_hash);
        let remote_bytes = serde_json::to_vec_pretty(&value).unwrap();
        let (in_sync, _) =
            tgt_drift_status(remote_bytes, None, &lf, "queues", "inbox-sorting").unwrap();
        assert!(
            in_sync,
            "queue with live counts should be in_sync against redacted baseline"
        );
    }

    #[test]
    fn tgt_drift_status_engine_agenda_id_churn_does_not_phantom_drift() {
        // Bug-a regression: `pull_redacts_kind` returned false for engines,
        // so `tgt_drift_status` hashed the raw remote bytes (live agenda_id)
        // while pull/sync wrote the baseline from redacted bytes (sentinel).
        // Result: every engine phantom-drifted on every deploy.
        //
        // Fix: route the drift hash through the engine KindCodec, which
        // calls `redact_for_disk("engines")` — the same path that pull and
        // write_back_flat use. The baseline must therefore be built the
        // same way (via the codec), not from raw bytes.
        //
        // Assert 1: a different live agenda_id value does NOT drift.
        let value_at_pull = serde_json::json!({
            "id": 7,
            "name": "training-engine",
            "url": "https://test/api/v1/engines/7",
            "agenda_id": "original-id-at-pull-time",
        });
        // Simulate baseline: what write_back_flat (= codec) would record.
        let codec = crate::snapshot::codec::codec("engines").expect("engines codec must exist");
        let art = codec.disk_bytes(&value_at_pull).unwrap();
        let baseline_hash = crate::snapshot::codec::combined_hash(&art.json, &art.sidecars);
        let lf = lf_with_hash("engines", "training", &baseline_hash);

        // Remote now has a rotated agenda_id (training completed) — only
        // agenda_id changed, everything else is identical.
        let remote_value = serde_json::json!({
            "id": 7,
            "name": "training-engine",
            "url": "https://test/api/v1/engines/7",
            "agenda_id": "new-rotated-id-after-training",
        });
        let remote_bytes = {
            let mut b = serde_json::to_vec_pretty(&remote_value).unwrap();
            b.push(b'\n');
            b
        };
        let (in_sync, _) =
            tgt_drift_status(remote_bytes, None, &lf, "engines", "training").unwrap();
        assert!(
            in_sync,
            "engine agenda_id churn must NOT phantom-drift (codec redacts agenda_id to sentinel)"
        );

        // Assert 2: a real change to a non-redacted field DOES drift.
        let remote_changed = serde_json::json!({
            "id": 7,
            "name": "training-engine-renamed",
            "url": "https://test/api/v1/engines/7",
            "agenda_id": "new-rotated-id-after-training",
        });
        let changed_bytes = {
            let mut b = serde_json::to_vec_pretty(&remote_changed).unwrap();
            b.push(b'\n');
            b
        };
        let (in_sync_changed, _) =
            tgt_drift_status(changed_bytes, None, &lf, "engines", "training").unwrap();
        assert!(
            !in_sync_changed,
            "a real name change on the engine must still register as drift"
        );
    }

    #[test]
    fn tgt_drift_status_still_detects_real_change_after_redaction() {
        // The redaction fix must not mask actual drift: a name change
        // should still flip in_sync to false even when counts churn.
        let value = serde_json::json!({
            "id": 1,
            "name": "Old name",
            "url": "https://test/api/v1/queues/1",
            "counts": { "document_status": { "to_review": 0 } },
        });
        let baseline_bytes =
            crate::snapshot::create::redacted_disk_bytes(&value, "queues").unwrap();
        let baseline_hash = content_hash(&baseline_bytes);
        let lf = lf_with_hash("queues", "q1", &baseline_hash);
        let mut changed = value.clone();
        changed["name"] = serde_json::Value::String("New name".into());
        let remote_bytes = serde_json::to_vec_pretty(&changed).unwrap();
        let (in_sync, _) = tgt_drift_status(remote_bytes, None, &lf, "queues", "q1").unwrap();
        assert!(!in_sync, "name change should still register as drift");
    }
}
