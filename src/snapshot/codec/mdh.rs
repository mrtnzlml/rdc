//! [`KindCodec`] implementation for the `mdh` kind (MDH index sets).
//!
//! Archetype: **flat plain, pull-only, sidecar-free**, with a kind-specific
//! server-managed-field strip moved here from the pull driver
//! (`src/cli/pull/mdh.rs`). Creating / cross-env-deploying index sets is not
//! an rdc push kind, so `create_body` / `cross_env_body` are no-ops.
//!
//! `disk_bytes` takes the index set as a `serde_json::Value`, deserializes it
//! into the typed `IndexSet`, strips server-managed fields, and re-serializes —
//! producing byte-for-byte the same on-disk JSON the legacy pull driver wrote,
//! so the default `base_hash` matches the `content_hash` the driver recorded.
//!
//! Path: `<env>/mdh/<dataset_slug>/indexes.json`
//! (via `Paths::dataset_dir(slug).join("indexes.json")`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::model::IndexSet;
use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::key_order::strip_hidden_fields_recursive;

pub struct Mdh;

impl KindCodec for Mdh {
    fn kind(&self) -> &'static str {
        "mdh"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        // Deserialize into the typed set, apply the same server-managed strip
        // the pull driver applied before writing `indexes.json`, then
        // re-serialize. Run `strip_hidden_fields_recursive` defensively after
        // re-encoding — a no-op on the `{regular, search}` shape today but
        // hash-neutral if a future API revision adds `modified_at`.
        let set: IndexSet = serde_json::from_value(value.clone())
            .map_err(|e| anyhow::anyhow!("deserializing MDH index set: {e}"))?;
        let trimmed = strip_server_managed(&set);
        let mut v = serde_json::to_value(&trimmed)
            .map_err(|e| anyhow::anyhow!("re-encoding trimmed MDH index set: {e}"))?;
        strip_hidden_fields_recursive(&mut v);
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact {
            json,
            sidecars: vec![],
        })
    }

    fn create_body(&self, _body: &mut Value) {
        // Pull-only kind: index sets are reconciled by the MDH-specific push
        // path, not the generic create-body flow. No-op.
    }

    fn cross_env_body(&self, _body: &mut Value) {
        // Pull-only kind: never part of a generic cross-env create/PATCH body.
        // No-op.
    }

    fn overlay<'a>(
        &self,
        _overlay: &'a Overlay,
        _slug: &str,
    ) -> Option<&'a BTreeMap<String, Value>> {
        // MDH index sets have no per-object overlay surface.
        None
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        // `slug` is the dataset slug (the lockfile key under `mdh_indexes`).
        // Replicates the pull driver: `dataset_dir(slug)/indexes.json`.
        paths.dataset_dir(slug).join("indexes.json")
    }
}

/// Strip server-only fields from an index set so the user only sees /
/// round-trips the fields they can actually edit. Moved from
/// `src/cli/pull/mdh.rs` (same logic, same behaviour).
///
/// - **Regular indexes**: drop the implicit `_id_` (server-managed) and the
///   `v` index-version field (server-assigned).
/// - **Search indexes**: the list response wraps user-authored `mappings` /
///   `analyzers` inside a `latest_definition` envelope and adds server-status
///   fields. Normalise to the shape the create body expects:
///   `{name, mappings, analyzers?}`.
fn strip_server_managed(set: &IndexSet) -> IndexSet {
    let mut regular: Vec<Value> = set
        .regular
        .iter()
        .filter(|ix| ix.get("name").and_then(|n| n.as_str()) != Some("_id_"))
        .cloned()
        .collect();
    for ix in regular.iter_mut() {
        if let Value::Object(obj) = ix {
            obj.shift_remove("v");
        }
    }
    let search: Vec<Value> = set
        .search
        .iter()
        .filter_map(normalize_search_index)
        .collect();
    IndexSet { regular, search }
}

/// Reshape a search-index list response to the create-body shape. Returns
/// `None` for entries that can't supply the minimum fields (`name` and
/// `mappings`) — defensive against future API drift.
fn normalize_search_index(remote: &Value) -> Option<Value> {
    let obj = remote.as_object()?;
    let name = obj.get("name")?.clone();
    let definition = obj.get("latest_definition").and_then(|v| v.as_object());
    let mappings = definition
        .and_then(|d| d.get("mappings"))
        .or_else(|| obj.get("mappings"))?
        .clone();
    let mut out = serde_json::Map::new();
    out.insert("name".to_string(), name);
    out.insert("mappings".to_string(), mappings);
    // Only include `analyzers` when the user actually configured them
    // (non-empty array). The default-empty case matches the create body's
    // optional shape and keeps the on-disk JSON minimal.
    let analyzers = definition
        .and_then(|d| d.get("analyzers"))
        .or_else(|| obj.get("analyzers"));
    if let Some(a) = analyzers {
        let non_empty = a.as_array().map(|arr| !arr.is_empty()).unwrap_or(true);
        if non_empty {
            out.insert("analyzers".to_string(), a.clone());
        }
    }
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw_index_set() -> Value {
        json!({
            "regular": [
                // implicit, server-managed — must be dropped.
                { "name": "_id_", "key": { "_id": 1 }, "v": 2 },
                // user index — kept, but `v` (server-assigned) is stripped.
                { "name": "ix_vendor_id", "key": { "vendor_id": 1 }, "unique": true, "v": 2 }
            ],
            "search": [
                {
                    "name": "vendor_search",
                    "type": "search",
                    "status": "READY",
                    "queryable": true,
                    "latest_definition": {
                        "mappings": { "dynamic": true },
                        "analyzers": []
                    }
                }
            ]
        })
    }

    #[test]
    fn kind_is_mdh() {
        assert_eq!(Mdh.kind(), "mdh");
    }

    #[test]
    fn id_and_v_stripped_from_disk() {
        let art = Mdh.disk_bytes(&raw_index_set()).unwrap();
        assert!(art.sidecars.is_empty(), "mdh index set is sidecar-free");
        assert_eq!(art.json.last(), Some(&b'\n'), "trailing newline required");

        let on_disk: IndexSet = serde_json::from_slice(&art.json).unwrap();

        // `_id_` dropped; only the user index survives.
        assert_eq!(
            on_disk.regular.len(),
            1,
            "implicit `_id_` index must be dropped"
        );
        let user_ix = on_disk.regular[0].as_object().unwrap();
        assert_eq!(user_ix.get("name").unwrap(), &json!("ix_vendor_id"));
        assert!(
            !user_ix.contains_key("v"),
            "server-assigned `v` must be stripped"
        );

        // Search index reshaped; server-status fields removed.
        assert_eq!(on_disk.search.len(), 1);
        let si = on_disk.search[0].as_object().unwrap();
        assert_eq!(si.get("name").unwrap(), &json!("vendor_search"));
        assert!(si.contains_key("mappings"), "mappings must be present");
        for server_field in ["type", "status", "queryable", "latest_definition"] {
            assert!(
                !si.contains_key(server_field),
                "server field {server_field} must be stripped"
            );
        }
    }

    #[test]
    fn create_and_cross_env_bodies_are_noops() {
        let before = raw_index_set();
        let mut v = before.clone();
        Mdh.create_body(&mut v);
        assert_eq!(
            v, before,
            "create_body must be a no-op for the pull-only mdh kind"
        );
        Mdh.cross_env_body(&mut v);
        assert_eq!(v, before, "cross_env_body must be a no-op");
    }

    #[test]
    fn path_is_dataset_dir_plus_indexes_json() {
        let paths = Paths::for_env("/proj", "dev");
        assert_eq!(
            Mdh.path(&paths, "vendors"),
            std::path::Path::new("/proj/envs/dev/mdh/vendors/indexes.json")
        );
    }
}
