//! [`KindCodec`] implementation for the `schemas` kind.
//!
//! Mirrors the queue pull driver's schema handling
//! (`src/cli/pull/queues.rs::write_schema_for_queue`): a schema is written
//! at `<queue_dir>/schema.json` with each formula-field's `formula` string
//! extracted into a `<queue_dir>/formulas/<field_id>.py` sidecar (sorted by
//! field_id). There is NO redaction for schemas; the only on-disk
//! normalization is the `modified_at` strip, which `serialize_schema` already
//! performs via `strip_hidden_fields_recursive`.
//!
//! Slug / path: a schema is keyed by the composite `"<ws_slug>/<q_slug>"`
//! key in the lockfile and overlay (each queue has exactly one schema). The
//! on-disk location nests under the workspace too:
//! `workspaces/<ws_slug>/queues/<q_slug>/schema.json`. We split the composite
//! key on the last `/` to recover both components.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Context as _;
use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};

pub struct Schemas;

impl KindCodec for Schemas {
    fn kind(&self) -> &'static str {
        "schemas"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let schema: crate::model::Schema = serde_json::from_value(value.clone())
            .context("deserializing schema value for disk_bytes")?;
        let (json, formulas) =
            crate::snapshot::schema::serialize_schema(&schema).context("serializing schema")?;

        // Frame each formula sidecar with the SAME label the legacy
        // `schema_combined_hash` used: `formulas/<field_id>.py`. The default
        // `base_hash` (combined_hash over json + sidecars) hashes each sidecar
        // as `0x00 || <label> || 0x00 || bytes`, so this label is load-bearing
        // for hash parity with the lockfile base recorded by the queues pull
        // driver.
        let sidecars = formulas
            .into_iter()
            .map(|(field_id, bytes)| (format!("formulas/{field_id}.py"), bytes))
            .collect();

        Ok(DiskArtifact { json, sidecars })
    }

    fn create_body(&self, body: &mut Value) {
        crate::snapshot::create::strip_for_create(body, "schemas");
    }

    fn cross_env_body(&self, body: &mut Value) {
        crate::snapshot::create::strip_for_cross_env_patch(body, "schemas");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        // Overlay is keyed by the queue slug (the trailing path segment of
        // the composite key), since each queue owns exactly one schema.
        let queue_slug = slug.rsplit('/').next().unwrap_or(slug);
        overlay.schema(queue_slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        // `slug` is the composite lockfile-style key `"<ws_slug>/<q_slug>"`.
        // Split on the last `/` to get both components. If not composite
        // (no `/`), defensive fallback.
        match slug.rsplit_once('/') {
            Some((ws_slug, q_slug)) => paths.queue_dir(ws_slug, q_slug).join("schema.json"),
            None => paths.queue_dir("", slug).join("schema.json"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::codec::KindCodec;
    use serde_json::json;

    fn schema_value_with_formula() -> Value {
        json!({
            "id": 1824379,
            "url": "https://x.rossum.app/api/v1/schemas/1824379",
            "name": "Cost Invoices Schema",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "content": [
                {
                    "category": "section",
                    "id": "header",
                    "label": "Header",
                    "children": [
                        { "category": "datapoint", "id": "invoice_id", "type": "string" },
                        {
                            "category": "datapoint",
                            "id": "amount_total",
                            "type": "number",
                            "formula": "amount_due + amount_tax"
                        }
                    ]
                }
            ],
            "modified_at": "2026-04-10T09:00:00Z"
        })
    }

    #[test]
    fn formula_goes_to_sidecar_framed_like_legacy_combined_hash() {
        let v = schema_value_with_formula();
        let art = Schemas.disk_bytes(&v).unwrap();

        // Exactly one formula sidecar, framed `formulas/<field_id>.py`.
        assert_eq!(art.sidecars.len(), 1);
        assert_eq!(art.sidecars[0].0, "formulas/amount_total.py");
        assert_eq!(art.sidecars[0].1, b"amount_due + amount_tax".to_vec());

        // The formula string itself is NOT in the JSON (it was extracted),
        // and `modified_at` is stripped from disk.
        let json_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            !json_str.contains("amount_due + amount_tax"),
            "formula must live in the .py sidecar, not schema.json: {json_str}"
        );
        assert!(
            !json_str.contains("modified_at"),
            "modified_at must be stripped from on-disk schema JSON: {json_str}"
        );
        assert!(
            json_str.contains("amount_total"),
            "field id is preserved in JSON"
        );
        assert_eq!(art.json.last(), Some(&b'\n'), "trailing newline required");
    }

    #[test]
    fn base_hash_matches_legacy_schema_combined_hash() {
        let v = schema_value_with_formula();
        let schema: crate::model::Schema = serde_json::from_value(v.clone()).unwrap();
        let (json, formulas) = crate::snapshot::schema::serialize_schema(&schema).unwrap();
        let legacy = crate::state::schema_combined_hash(&json, &formulas);

        let codec_hash = Schemas.base_hash(&v).unwrap();
        assert_eq!(
            codec_hash, legacy,
            "codec hash must match legacy schema_combined_hash"
        );
    }

    #[test]
    fn path_splits_composite_key_into_workspace_and_queue() {
        let paths = crate::paths::Paths::for_env("/proj", "dev");
        let p = Schemas.path(&paths, "invoices-ap/cost-invoices");
        assert_eq!(
            p,
            std::path::Path::new(
                "/proj/envs/dev/workspaces/invoices-ap/queues/cost-invoices/schema.json"
            )
        );
    }

    #[test]
    fn no_sidecars_for_schema_without_formulas() {
        let v = json!({
            "id": 2,
            "url": "https://x.rossum.app/api/v1/schemas/2",
            "name": "Plain Schema",
            "queues": [],
            "content": [
                { "category": "datapoint", "id": "invoice_id", "type": "string" }
            ]
        });
        let art = Schemas.disk_bytes(&v).unwrap();
        assert!(
            art.sidecars.is_empty(),
            "schema without formulas must produce no sidecars"
        );
    }
}
