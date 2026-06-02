//! [`KindCodec`] implementation for the `workflows` kind.

use std::path::PathBuf;

use serde_json::Value;

use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::key_order::strip_hidden_fields_recursive;

pub struct Workflows;

impl KindCodec for Workflows {
    fn kind(&self) -> &'static str {
        "workflows"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let mut v = value.clone();
        // No redaction for workflows — `redact_on_pull("workflows")` is empty.
        strip_hidden_fields_recursive(&mut v);
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact {
            json,
            sidecars: vec![],
        })
    }

    fn create_body(&self, body: &mut Value) {
        strip_for_create(body, "workflows");
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "workflows");
    }

    // No overlay for workflows — the default impl returns `None`.

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.workflow_dir(slug).join("workflow.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use serde_json::json;

    fn sample_workflow() -> Value {
        json!({
            "id": 700,
            "url": "https://x.rossum.app/api/v1/workflows/700",
            "name": "AP Invoice Flow",
            "organization": "https://x.rossum.app/api/v1/organizations/285704",
            "modified_at": "2026-04-01T10:00:00Z"
        })
    }

    #[test]
    fn disk_bytes_strips_modified_at_no_sidecars() {
        let codec = Workflows;
        let art = codec.disk_bytes(&sample_workflow()).unwrap();

        assert!(art.sidecars.is_empty(), "workflows have no sidecars");
        assert_eq!(art.json.last(), Some(&b'\n'), "trailing newline required");

        let out: Value = serde_json::from_slice(&art.json).unwrap();
        assert!(
            out.get("modified_at").is_none(),
            "modified_at must be absent on disk"
        );
        assert_eq!(out["name"], json!("AP Invoice Flow"));
    }

    #[test]
    fn path_is_workflow_dir_plus_workflow_json() {
        let paths = Paths::for_env("/proj", "dev");
        let codec = Workflows;
        assert_eq!(
            codec.path(&paths, "ap-invoice-flow"),
            std::path::PathBuf::from("/proj/envs/dev/workflows/ap-invoice-flow/workflow.json")
        );
    }
}
