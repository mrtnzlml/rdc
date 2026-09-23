//! [`KindCodec`] implementation for the `saved_views` kind.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::strip_for_cross_env_patch;
use crate::snapshot::key_order::strip_hidden_fields;

pub struct SavedViews;

impl KindCodec for SavedViews {
    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let mut v = value.clone();
        strip_hidden_fields(&mut v);
        // `created_by` is an env-specific USER url, and users are not a
        // snapshotted kind, so it never portabilizes — the same leak class as a
        // queue's `rir_url` or a hook's `token_owner`. `created_at` is pure
        // churn. Both are stripped HERE rather than added to the global
        // HIDDEN_FIELDS, which would change the on-disk bytes of every other
        // kind that carries them and force a rewrite of every existing project.
        if let Some(obj) = v.as_object_mut() {
            obj.shift_remove("created_by");
            obj.shift_remove("created_at");
        }
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact { json, sidecars: vec![] })
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "saved_views");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        overlay.saved_view(slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.saved_views_dir().join(format!("{slug}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::codec::KindCodec;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "id": 42,
            "url": "https://acme.rossum.app/api/v1/saved_views/42",
            "name": "Awaiting approval",
            "shared": true,
            "queues_filter": ["rdc://queues/invoices"],
            "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
            "organization": "https://acme.rossum.app/api/v1/organizations/1",
            "created_by": "https://acme.rossum.app/api/v1/users/7",
            "created_at": "2026-08-01T08:00:00Z",
            "modified_at": "2026-08-02T09:00:00Z",
            "modified_by": "https://acme.rossum.app/api/v1/users/8"
        })
    }

    #[test]
    fn strips_stamps_and_creator_from_disk() {
        let art = SavedViews.disk_bytes(&sample()).unwrap();
        let s = std::str::from_utf8(&art.json).unwrap();
        for gone in ["modified_at", "modified_by", "created_by", "created_at"] {
            assert!(!s.contains(gone), "{gone} must be stripped from disk; got:\n{s}");
        }
    }

    #[test]
    fn keeps_managed_fields_on_disk() {
        let art = SavedViews.disk_bytes(&sample()).unwrap();
        let s = std::str::from_utf8(&art.json).unwrap();
        for kept in ["name", "shared", "queues_filter", "query", "organization"] {
            assert!(s.contains(kept), "{kept} must survive on disk; got:\n{s}");
        }
    }

    #[test]
    fn no_sidecars() {
        assert!(SavedViews.disk_bytes(&sample()).unwrap().sidecars.is_empty());
    }

    #[test]
    fn path_is_under_saved_views_dir() {
        use crate::paths::Paths;
        let paths = Paths::for_env("/proj", "dev");
        assert_eq!(
            SavedViews.path(&paths, "awaiting-approval"),
            std::path::PathBuf::from("/proj/envs/dev/saved-views/awaiting-approval.json")
        );
    }

    /// The `created_by` strip is scoped to THIS codec. Widening the global
    /// HIDDEN_FIELDS would rewrite every other kind's on-disk bytes and churn
    /// every existing project on its next sync.
    #[test]
    fn created_by_strip_is_not_global() {
        // Compare as a slice: HIDDEN_FIELDS is `&[&str]`, so a bare array
        // literal on the right-hand side would not unify.
        assert_eq!(
            crate::snapshot::key_order::HIDDEN_FIELDS,
            &["modified_at", "modified_by"][..],
            "HIDDEN_FIELDS must stay exactly these two"
        );
        let label = json!({
            "id": 1, "url": "u", "name": "n",
            "created_by": "https://acme.rossum.app/api/v1/users/7"
        });
        let art = crate::snapshot::codec::codec("labels").unwrap().disk_bytes(&label).unwrap();
        let s = std::str::from_utf8(&art.json).unwrap();
        assert!(s.contains("created_by"), "labels must still keep created_by on disk");
    }

    /// Deferred relink is unsafe for this kind, so the on-disk shape must keep
    /// `queues_filter` as a real array of refs the resolver can see.
    #[test]
    fn queues_filter_refs_survive_as_strings() {
        let art = SavedViews.disk_bytes(&sample()).unwrap();
        let v: Value = serde_json::from_slice(&art.json).unwrap();
        assert_eq!(v["queues_filter"][0], json!("rdc://queues/invoices"));
    }
}
