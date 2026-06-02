//! [`KindCodec`] implementation for the `labels` kind.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::key_order::strip_hidden_fields_recursive;

pub struct Labels;

impl KindCodec for Labels {
    fn kind(&self) -> &'static str {
        "labels"
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
        strip_for_create(body, "labels");
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "labels");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        overlay.label(slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.labels_dir().join(format!("{slug}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::codec::KindCodec;
    use serde_json::json;

    fn sample_label_value() -> Value {
        json!({
            "id": 1,
            "url": "https://example/api/v1/labels/1",
            "name": "Approved",
            "modified_at": "2026-05-01T12:00:00Z"
        })
    }

    #[test]
    fn modified_at_stripped_from_disk() {
        let codec = Labels;
        let v = sample_label_value();
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            !disk_str.contains("modified_at"),
            "modified_at must be stripped from disk; got:\n{disk_str}"
        );
    }

    #[test]
    fn no_sidecars() {
        let codec = Labels;
        let v = sample_label_value();
        let art = codec.disk_bytes(&v).unwrap();
        assert!(art.sidecars.is_empty(), "labels must produce no sidecars");
    }

    #[test]
    fn no_redaction_sentinel() {
        use crate::snapshot::create::REDACTED_VALUE_SENTINEL;
        let codec = Labels;
        let v = sample_label_value();
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            !disk_str.contains(REDACTED_VALUE_SENTINEL),
            "labels must not have any redaction; got:\n{disk_str}"
        );
    }

    #[test]
    fn path_is_under_labels_dir() {
        use crate::paths::Paths;
        let paths = Paths::for_env("/proj", "dev");
        let codec = Labels;
        let p = codec.path(&paths, "approved");
        assert_eq!(
            p,
            std::path::PathBuf::from("/proj/envs/dev/labels/approved.json")
        );
    }
}
