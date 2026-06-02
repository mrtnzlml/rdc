//! [`KindCodec`] implementation for the `inboxes` kind.
//!
//! Inboxes are flat plain objects nested under their owning queue (one inbox
//! per queue). The on-disk path mirrors the queue pull driver:
//! `workspaces/<ws_slug>/queues/<q_slug>/inbox.json`. There is no redaction
//! for inboxes, but `strip_for_create` removes the server-assigned `email`
//! field. The composite slug `"<ws_slug>/<q_slug>"` is split on the last `/`
//! to recover both components, matching the pattern used by queues and schemas.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::key_order::strip_hidden_fields_recursive;

pub struct Inboxes;

impl KindCodec for Inboxes {
    fn kind(&self) -> &'static str {
        "inboxes"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let mut v = value.clone();
        strip_hidden_fields_recursive(&mut v);
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact {
            json,
            sidecars: vec![],
        })
    }

    fn create_body(&self, body: &mut Value) {
        // strip_for_create for "inboxes" removes the server-assigned `email`
        // address along with the universal server fields (id, url, modified_at …).
        strip_for_create(body, "inboxes");
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "inboxes");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        // Overlay is keyed by the queue slug (the trailing segment of the
        // composite key), since each queue has exactly one inbox.
        let queue_slug = slug.rsplit('/').next().unwrap_or(slug);
        overlay.inbox(queue_slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        // `slug` is the composite lockfile-style key `"<ws_slug>/<q_slug>"`.
        // Split on the last `/` to recover both components. If not composite
        // (no `/`), defensive fallback.
        match slug.rsplit_once('/') {
            Some((ws_slug, q_slug)) => paths.queue_dir(ws_slug, q_slug).join("inbox.json"),
            None => paths.queue_dir("", slug).join("inbox.json"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::codec::KindCodec;
    use serde_json::json;

    fn sample_inbox_value() -> Value {
        json!({
            "id": 5,
            "url": "https://example/api/v1/inboxes/5",
            "name": "AP Invoices Inbox",
            "queues": ["https://example/api/v1/queues/10"],
            "email": "ap-invoices@example.rossum.app",
            "email_prefix": "ap-invoices",
            "bounce_email_to": null,
            "filters": [],
            "modified_at": "2026-05-01T12:00:00Z"
        })
    }

    #[test]
    fn modified_at_stripped_from_disk() {
        let codec = Inboxes;
        let v = sample_inbox_value();
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            !disk_str.contains("modified_at"),
            "modified_at must be stripped from disk; got:\n{disk_str}"
        );
    }

    #[test]
    fn no_sidecars() {
        let codec = Inboxes;
        let v = sample_inbox_value();
        let art = codec.disk_bytes(&v).unwrap();
        assert!(art.sidecars.is_empty(), "inboxes must produce no sidecars");
    }

    #[test]
    fn email_stripped_by_create_body() {
        let codec = Inboxes;
        let mut v = sample_inbox_value();
        codec.create_body(&mut v);
        let obj = v.as_object().unwrap();
        assert!(
            !obj.contains_key("email"),
            "create_body must strip the server-assigned email; got: {obj:?}"
        );
        // Other user-editable fields survive.
        assert!(
            obj.contains_key("email_prefix"),
            "email_prefix must survive create_body"
        );
    }

    #[test]
    fn path_splits_composite_slug() {
        let paths = crate::paths::Paths::for_env("/proj", "dev");
        let codec = Inboxes;
        let p = codec.path(&paths, "my-workspace/my-queue");
        assert_eq!(
            p,
            std::path::PathBuf::from(
                "/proj/envs/dev/workspaces/my-workspace/queues/my-queue/inbox.json"
            )
        );
    }

    #[test]
    fn path_fallback_for_non_composite_slug() {
        let paths = crate::paths::Paths::for_env("/proj", "dev");
        let codec = Inboxes;
        let p = codec.path(&paths, "bare-slug");
        assert!(
            p.to_string_lossy().contains("bare-slug"),
            "fallback path must include the slug"
        );
    }
}
