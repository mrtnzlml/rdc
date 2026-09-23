//! [`KindCodec`] implementation for the `queues` kind.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{redact_for_disk, strip_for_cross_env_patch};
use crate::snapshot::key_order::strip_hidden_fields;

pub struct Queues;

impl KindCodec for Queues {
    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let mut v = value.clone();
        redact_for_disk(&mut v, "queues");
        strip_hidden_fields(&mut v);
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact {
            json,
            sidecars: vec![],
        })
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "queues");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        overlay.queue(slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        // `slug` is the composite lockfile key `"<ws_slug>/<q_slug>"`.
        // Split on the first '/' to obtain the two components.
        if let Some(sep) = slug.find('/') {
            let ws_slug = &slug[..sep];
            let q_slug = &slug[sep + 1..];
            paths.queue_dir(ws_slug, q_slug).join("queue.json")
        } else {
            // Defensive fallback: treat the whole slug as the queue dir.
            paths.queues_dir(slug).join("queue.json")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::snapshot::codec::KindCodec;
    use crate::snapshot::create::REDACTED_VALUE_SENTINEL;
    use serde_json::json;

    fn sample_queue_value() -> Value {
        json!({
            "id": 10,
            "url": "https://example/api/v1/queues/10",
            "name": "Invoices",
            "workspace": "https://example/api/v1/workspaces/1",
            "schema": "https://example/api/v1/schemas/5",
            "counts": {
                "to_review": 3,
                "importing": 0,
                "exported": 100
            },
            "automation_level": "never",
            "modified_at": "2026-05-01T12:00:00Z"
        })
    }

    #[test]
    fn disk_json_redacts_counts() {
        let codec = Queues;
        let v = sample_queue_value();
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            disk_str.contains(REDACTED_VALUE_SENTINEL),
            "disk json must contain the sentinel for counts; got:\n{disk_str}"
        );
        assert!(
            !disk_str.contains("to_review"),
            "raw counts fields must not appear on disk; got:\n{disk_str}"
        );
    }

    #[test]
    fn disk_json_redacts_rir_url() {
        // `rir_url` is a per-cluster, server-managed internal RIR service URL
        // (e.g. `http://…svc.cluster.local`). It is stripped from outbound
        // PATCH/POST bodies (the API 400s on it), so it must likewise be
        // redacted on pull — otherwise the concrete internal hostname lands in
        // tracked files, leaks across envs via `migrate`, and produces a
        // spurious cross-env pull whenever the target remote's value differs.
        let codec = Queues;
        let v = json!({
            "id": 10,
            "url": "https://example/api/v1/queues/10",
            "name": "Invoices",
            "rir_url": "http://acme-api.prod-rir.svc.cluster.local",
            "modified_at": "2026-05-01T12:00:00Z"
        });
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            !disk_str.contains("acme-api.prod-rir.svc.cluster.local"),
            "concrete per-cluster rir_url must not leak onto disk; got:\n{disk_str}"
        );
        let disk: Value = serde_json::from_slice(&art.json).unwrap();
        assert_eq!(
            disk.get("rir_url").and_then(|v| v.as_str()),
            Some(REDACTED_VALUE_SENTINEL),
            "rir_url must be redacted to the sentinel on disk; got:\n{disk_str}"
        );
    }

    #[test]
    fn no_sidecars() {
        let codec = Queues;
        let v = sample_queue_value();
        let art = codec.disk_bytes(&v).unwrap();
        assert!(art.sidecars.is_empty(), "queues must produce no sidecars");
    }

    #[test]
    fn modified_at_stripped_from_disk() {
        let codec = Queues;
        let v = sample_queue_value();
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            !disk_str.contains("modified_at"),
            "modified_at must be stripped from disk; got:\n{disk_str}"
        );
    }

    #[test]
    fn path_splits_composite_slug() {
        let paths = Paths::for_env("/proj", "dev");
        let codec = Queues;
        let p = codec.path(&paths, "my-workspace/my-queue");
        assert_eq!(
            p,
            std::path::PathBuf::from(
                "/proj/envs/dev/workspaces/my-workspace/queues/my-queue/queue.json"
            )
        );
    }

    #[test]
    fn path_fallback_for_non_composite_slug() {
        let paths = Paths::for_env("/proj", "dev");
        let codec = Queues;
        // A bare slug (no '/') should not panic — fallback treats slug as ws_slug.
        let p = codec.path(&paths, "bare-slug");
        assert!(
            p.to_string_lossy().contains("bare-slug"),
            "fallback path must include the slug"
        );
    }
}
