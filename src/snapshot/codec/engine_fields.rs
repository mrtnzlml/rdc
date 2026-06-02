//! [`KindCodec`] implementation for the `engine_fields` kind.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::key_order::strip_hidden_fields_recursive;

pub struct EngineFields;

impl KindCodec for EngineFields {
    fn kind(&self) -> &'static str {
        "engine_fields"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let mut v = value.clone();
        // No redaction for engine_fields — `redact_on_pull("engine_fields")` is empty.
        strip_hidden_fields_recursive(&mut v);
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact {
            json,
            sidecars: vec![],
        })
    }

    fn create_body(&self, body: &mut Value) {
        strip_for_create(body, "engine_fields");
    }

    fn cross_env_body(&self, body: &mut Value) {
        // Also strips `name`: the Rossum API treats an engine field's `name` as
        // immutable after create, so a cross-env PATCH with a changed name 400s.
        strip_for_cross_env_patch(body, "engine_fields");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        // `slug` is the composite lockfile key "<engine_slug>/<field_slug>".
        overlay.engine_field(slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        // `slug` is the composite lockfile key "<engine_slug>/<field_slug>".
        // Split on the first '/': engine slugs never contain a slash.
        let (engine_slug, field_slug) = slug.split_once('/').unwrap_or(("", slug));
        paths
            .engine_fields_dir(engine_slug)
            .join(format!("{field_slug}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use serde_json::json;

    fn sample_engine_field() -> Value {
        json!({
            "id": 501,
            "url": "https://x.rossum.app/api/v1/engine_fields/501",
            "name": "Invoice ID",
            "engine": "https://x.rossum.app/api/v1/engines/401",
            "modified_at": "2026-04-15T08:00:00Z",
            "field_type": "string"
        })
    }

    #[test]
    fn disk_bytes_strips_modified_at_no_sidecars() {
        let codec = EngineFields;
        let art = codec.disk_bytes(&sample_engine_field()).unwrap();

        assert!(
            art.sidecars.is_empty(),
            "engine_fields must not emit sidecars"
        );
        assert_eq!(art.json.last(), Some(&b'\n'), "trailing newline required");

        let out: Value = serde_json::from_slice(&art.json).unwrap();
        assert!(
            out.get("modified_at").is_none(),
            "modified_at must be stripped from on-disk JSON"
        );
        // No redaction sentinel — engine_fields has no redact_on_pull entry.
        assert!(
            !String::from_utf8_lossy(&art.json)
                .contains(crate::snapshot::create::REDACTED_VALUE_SENTINEL),
            "engine_fields has no redaction set"
        );
        assert_eq!(out["name"], json!("Invoice ID"));
        assert_eq!(out["field_type"], json!("string"));
    }

    /// `create_body` must keep `name` (only cross-env strips it).
    #[test]
    fn create_body_keeps_name() {
        let codec = EngineFields;
        let mut v = sample_engine_field();
        codec.create_body(&mut v);
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("id"), "create strips id");
        assert!(!obj.contains_key("url"), "create strips url");
        assert!(
            !obj.contains_key("modified_at"),
            "create strips modified_at"
        );
        assert!(obj.contains_key("name"), "create body must keep name");
        assert!(
            obj.contains_key("engine"),
            "create body must keep engine URL"
        );
    }

    /// `cross_env_body` must strip `name` (immutable after create in the API).
    #[test]
    fn cross_env_body_strips_immutable_name() {
        let codec = EngineFields;
        let mut v = sample_engine_field();
        codec.cross_env_body(&mut v);
        let obj = v.as_object().unwrap();
        assert!(
            !obj.contains_key("name"),
            "cross-env body must strip the immutable engine-field name"
        );
        assert!(
            obj.contains_key("engine"),
            "cross-env body keeps engine URL"
        );
    }

    #[test]
    fn path_splits_composite_slug() {
        let paths = Paths::for_env("/proj", "dev");
        let codec = EngineFields;
        assert_eq!(
            codec.path(&paths, "invoice-extractor/item-qty"),
            std::path::PathBuf::from(
                "/proj/envs/dev/engines/invoice-extractor/fields/item-qty.json"
            )
        );
    }
}
