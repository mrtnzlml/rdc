//! [`KindCodec`] implementation for the `mdh` kind (MDH index sets).
//!
//! Archetype: **flat plain, pull-only, sidecar-free**, with a kind-specific
//! server-managed-field strip moved here from the pull driver
//! (`src/cli/pull/mdh.rs`). Creating / cross-env-deploying index sets is not
//! an rdc push kind, so `cross_env_body` is a no-op.
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
use crate::snapshot::key_order::strip_hidden_fields;

pub struct Mdh;

impl KindCodec for Mdh {
    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        // Deserialize into the typed set, apply the same server-managed strip
        // the pull driver applied before writing `indexes.json`, then
        // re-serialize. `strip_hidden_fields` runs defensively after
        // re-encoding, in case a future API revision stamps the set itself. It
        // is TOP-LEVEL only, which matters here: an index spec keys each index
        // by COLUMN name (`{"key": {"col": 1}}`), so a recursive strip would
        // delete the index of a dataset column named `modified_at` /
        // `modified_by` — see `key_order::HIDDEN_FIELDS`.
        let set: IndexSet = serde_json::from_value(value.clone())
            .map_err(|e| anyhow::anyhow!("deserializing MDH index set: {e}"))?;
        let trimmed = strip_server_managed(&set);
        let mut v = serde_json::to_value(&trimmed)
            .map_err(|e| anyhow::anyhow!("re-encoding trimmed MDH index set: {e}"))?;
        strip_hidden_fields(&mut v);
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact {
            json,
            sidecars: vec![],
        })
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
///   fields. Normalise to the shape the create body expects — see
///   [`normalize_search_index`].
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

/// The user-authored search-index definition keys, as `(on-disk / create-body
/// name, list-response name)`. The list endpoint answers in snake_case inside
/// `latest_definition`; the create body is camelCase. Each key is written only
/// when set: the list reports an unset key as `null` (or `[]` for the arrays),
/// and the create body accepts its absence. `stored_source` / `num_partitions`
/// are listed too, but `search_indexes/create` silently drops them
/// (live-verified), so they are not part of what rdc can manage.
const SEARCH_DEF_KEYS: [(&str, &str); 4] = [
    ("analyzer", "analyzer"),
    ("analyzers", "analyzers"),
    ("searchAnalyzer", "search_analyzer"),
    ("synonyms", "synonyms"),
];

/// Reshape a search-index list response to the create-body shape
/// `{name, mappings, analyzer?, analyzers?, searchAnalyzer?, synonyms?}`.
/// Idempotent: an already-normalised definition maps to itself. Returns `None`
/// for entries that can't supply the minimum fields (`name` and `mappings`) —
/// defensive against future API drift.
pub(crate) fn normalize_search_index(remote: &Value) -> Option<Value> {
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
    for (key, listed) in SEARCH_DEF_KEYS {
        let value = match definition {
            Some(d) => d.get(listed),
            None => obj.get(key),
        };
        let unset = match value {
            None | Some(Value::Null) => true,
            Some(Value::Array(a)) => a.is_empty(),
            Some(_) => false,
        };
        if !unset {
            out.insert(key.to_string(), value.cloned().expect("checked set"));
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
    fn cross_env_body_is_a_noop() {
        let before = raw_index_set();
        let mut v = before.clone();
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

    #[test]
    fn normalize_search_index_reshapes_raw_list_response() {
        // The exact shape `list_search_indexes` returns for a freshly-created
        // search index (live-verified): mappings live under `latest_definition`,
        // plus server-status fields. It must reshape to the canonical
        // `{name, mappings}` form (empty analyzers omitted) so the push diff
        // compares like-with-like and does NOT see a spurious change.
        let raw = json!({
            "name": "sx_a",
            "type": "search",
            "status": "PENDING",
            "queryable": false,
            "latest_definition": {
                "mappings": {"dynamic": true},
                "analyzer": null,
                "analyzers": [],
                "search_analyzer": null,
                "synonyms": null
            }
        });
        let got = normalize_search_index(&raw).expect("normalizes");
        assert_eq!(got, json!({"name": "sx_a", "mappings": {"dynamic": true}}));
    }

    /// The live-verified list shape of a search index that sets every
    /// user-authored key. All four must survive (renamed to the create
    /// body's camelCase), or pull silently loses them and push re-creates the
    /// index without them.
    #[test]
    fn normalize_search_index_keeps_analyzers_and_synonyms() {
        let raw = json!({
            "name": "sx_full",
            "type": "search",
            "status": "READY",
            "queryable": true,
            "latest_definition": {
                "mappings": {"dynamic": false, "fields": {"title": {"type": "string"}}},
                "analyzer": "lucene.english",
                "analyzers": [{"name": "custom", "tokenizer": {"type": "whitespace"}}],
                "search_analyzer": "lucene.english",
                "synonyms": [{
                    "name": "syn",
                    "analyzer": "lucene.english",
                    "source": {"collection": "vendor_synonyms"}
                }],
                "stored_source": null,
                "num_partitions": null
            },
            "latest_definition_version": null
        });
        let want = json!({
            "name": "sx_full",
            "mappings": {"dynamic": false, "fields": {"title": {"type": "string"}}},
            "analyzer": "lucene.english",
            "analyzers": [{"name": "custom", "tokenizer": {"type": "whitespace"}}],
            "searchAnalyzer": "lucene.english",
            "synonyms": [{
                "name": "syn",
                "analyzer": "lucene.english",
                "source": {"collection": "vendor_synonyms"}
            }]
        });
        let got = normalize_search_index(&raw).expect("normalizes");
        assert_eq!(got, want);
        assert_eq!(
            normalize_search_index(&got).expect("normalizes"),
            want,
            "normalizing the on-disk form must be a no-op"
        );
    }

    /// The live API reports an unset key as `null` — `analyzers` too, not
    /// only `[]`. None of them may reach the on-disk form.
    #[test]
    fn normalize_search_index_omits_null_keys() {
        let raw = json!({
            "name": "sx_min",
            "latest_definition": {
                "mappings": {"dynamic": true},
                "analyzer": null,
                "analyzers": null,
                "search_analyzer": null,
                "synonyms": null
            }
        });
        assert_eq!(
            normalize_search_index(&raw).expect("normalizes"),
            json!({"name": "sx_min", "mappings": {"dynamic": true}})
        );
    }

    /// A dataset column can legitimately be named `modified_at`/`modified_by`,
    /// and an index spec keys each index by COLUMN name. The hidden-field strip
    /// must not reach into that map, or the index silently disappears from the
    /// snapshot — and with it from what push reconciles.
    #[test]
    fn index_keyed_by_a_stamp_named_column_survives() {
        let set = json!({
            "regular": [
                { "key": { "modified_by": 1 }, "name": "modified_by_idx" },
                { "key": { "modified_at": -1 }, "name": "modified_at_idx" }
            ],
            "search": []
        });
        let art = Mdh.disk_bytes(&set).unwrap();
        let disk: Value = serde_json::from_slice(&art.json).unwrap();
        assert_eq!(
            disk["regular"][0]["key"],
            json!({ "modified_by": 1 }),
            "an index keyed by a `modified_by` column must survive: {disk}"
        );
        assert_eq!(
            disk["regular"][1]["key"],
            json!({ "modified_at": -1 }),
            "an index keyed by a `modified_at` column must survive: {disk}"
        );
    }
}
