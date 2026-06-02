//! [`KindCodec`] implementation for the `email_templates` kind.
//!
//! Archetype: **flat plain** — no redaction, no sidecars.
//!
//! Path nesting + compound slug: email templates are queue-scoped in the live
//! API (each carries a singular `queue` URL), so the snapshot nests them under
//! the owning queue:
//!
//! ```text
//! workspaces/<ws_slug>/queues/<q_slug>/email-templates/<template_slug>.json
//! ```
//!
//! Most queues carry the same five built-in templates, so a bare per-template
//! slug would collide across queues. The pull driver therefore keys each
//! template in the lockfile (and the overlay) by the **three-segment**
//! composite `"<ws_slug>/<q_slug>/<template_slug>"`. `path()` accepts that
//! composite key and splits it back into its three slug components.
//!
//! The overlay is keyed by the full composite key — `Overlay::email_template`
//! is looked up with the full `lockfile_key` at the call site, so `overlay()`
//! passes `slug` through unchanged.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::key_order::strip_hidden_fields_recursive;

pub struct EmailTemplates;

impl KindCodec for EmailTemplates {
    fn kind(&self) -> &'static str {
        "email_templates"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        // Flat plain: no per-kind redaction (email_templates has no entry in
        // `create::redact_on_pull`). Only the universal hidden-field strip
        // (`modified_at`) is applied.
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
        // Strips the universal server fields plus `triggers`: `triggers`
        // references a sub-resource kind that rdc doesn't pull or deploy,
        // and shipping src trigger URLs to a target 400s with "Invalid hyperlink".
        strip_for_create(body, "email_templates");
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "email_templates");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        // The overlay is keyed by the FULL three-segment composite key
        // "<ws_slug>/<q_slug>/<template_slug>" — the same `lockfile_key` the
        // pull driver passes to `o.email_template(&lockfile_key)`.
        overlay.email_template(slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        // `slug` is the three-segment composite lockfile key
        // "<ws_slug>/<q_slug>/<template_slug>". Split off the LAST segment
        // (the template slug); the remaining head is "<ws_slug>/<q_slug>",
        // which we split on its FIRST `/`.
        match slug.rsplit_once('/') {
            Some((parent, template_slug)) => {
                let (ws_slug, q_slug) = parent.split_once('/').unwrap_or(("", parent));
                paths
                    .queue_email_templates_dir(ws_slug, q_slug)
                    .join(format!("{template_slug}.json"))
            }
            None => paths
                .queue_email_templates_dir("", "")
                .join(format!("{slug}.json")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_template() -> Value {
        json!({
            "id": 9001,
            "url": "https://x.rossum.app/api/v1/email_templates/9001",
            "name": "Rejection Notice",
            "subject": "Your invoice was rejected",
            "queue": "https://x.rossum.app/api/v1/queues/2137275",
            "body_template": "Hello,\nYour document was rejected.\n",
            "triggers": ["https://x.rossum.app/api/v1/triggers/55"],
            "modified_at": "2026-04-20T08:00:00Z"
        })
    }

    #[test]
    fn modified_at_stripped_from_disk() {
        let art = EmailTemplates.disk_bytes(&sample_template()).unwrap();
        let disk: Value = serde_json::from_slice(&art.json).unwrap();
        assert!(
            disk.get("modified_at").is_none(),
            "modified_at must be stripped from disk"
        );
    }

    #[test]
    fn no_sidecars() {
        let art = EmailTemplates.disk_bytes(&sample_template()).unwrap();
        assert!(
            art.sidecars.is_empty(),
            "email_templates must produce no sidecars"
        );
    }

    #[test]
    fn create_body_strips_triggers_and_server_fields() {
        let mut v = sample_template();
        EmailTemplates.create_body(&mut v);
        let obj = v.as_object().unwrap();
        for f in ["id", "url", "modified_at", "triggers"] {
            assert!(!obj.contains_key(f), "create_body must strip {f}");
        }
        assert!(obj.contains_key("name"), "name must survive create_body");
        assert!(
            obj.contains_key("subject"),
            "subject must survive create_body"
        );
    }

    #[test]
    fn path_splits_three_segment_composite_key() {
        let paths = Paths::for_env("/proj", "dev");
        assert_eq!(
            EmailTemplates.path(&paths, "invoices-ap/cost-invoices/rejection-notice"),
            PathBuf::from(
                "/proj/envs/dev/workspaces/invoices-ap/queues/cost-invoices/email-templates/rejection-notice.json"
            )
        );
    }

    #[test]
    fn kind_is_email_templates() {
        assert_eq!(EmailTemplates.kind(), "email_templates");
    }
}
