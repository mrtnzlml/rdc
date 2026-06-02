//! [`KindCodec`] implementation for the `workspaces` kind.

use std::path::PathBuf;

use serde_json::Value;

use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::key_order::strip_hidden_fields_recursive;

pub struct Workspaces;

impl KindCodec for Workspaces {
    fn kind(&self) -> &'static str {
        "workspaces"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let mut v = value.clone();
        // No redaction for workspaces — `redact_on_pull("workspaces")` is empty.
        // Strip `modified_at` recursively so that nested timestamps (e.g. inside
        // a sub-object) don't survive to disk and cause spurious sync drift.
        strip_hidden_fields_recursive(&mut v);
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact {
            json,
            sidecars: vec![],
        })
    }

    fn create_body(&self, body: &mut Value) {
        strip_for_create(body, "workspaces");
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "workspaces");
    }

    // No overlay for workspaces — `Overlay` has no `workspace` accessor.
    // The default `overlay()` impl returns `None`.

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.workspace_dir(slug).join("workspace.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use serde_json::json;

    fn sample_workspace() -> Value {
        json!({
            "id": 700852,
            "url": "https://x.rossum.app/api/v1/workspaces/700852",
            "name": "Invoices AP",
            "organization": "https://x.rossum.app/api/v1/organizations/285704",
            "queues": ["https://x.rossum.app/api/v1/queues/2137275"],
            "modified_at": "2026-03-15T11:00:00Z",
            "metadata": {
                "tag": "ap",
                "modified_at": "2026-03-15T11:00:00Z"
            }
        })
    }

    /// The nested `modified_at` inside `metadata` must be stripped from disk.
    /// This is the fix for a known sync bug where only the top-level
    /// `modified_at` was stripped, leaving nested timestamps to cause
    /// spurious remote drift on every re-pull.
    #[test]
    fn disk_bytes_strips_modified_at_recursively() {
        let codec = Workspaces;
        let art = codec.disk_bytes(&sample_workspace()).unwrap();

        assert!(art.sidecars.is_empty(), "workspaces have no sidecars");
        assert_eq!(art.json.last(), Some(&b'\n'), "trailing newline required");

        let out: Value = serde_json::from_slice(&art.json).unwrap();
        assert!(
            out.get("modified_at").is_none(),
            "top-level modified_at must be stripped"
        );
        assert!(
            out["metadata"].get("modified_at").is_none(),
            "nested modified_at inside metadata must also be stripped (sync bug fix)"
        );
        // Meaningful fields preserved — no redaction for workspaces.
        assert_eq!(out["name"], json!("Invoices AP"));
        assert_eq!(out["metadata"]["tag"], json!("ap"));
    }

    #[test]
    fn no_sidecars() {
        let codec = Workspaces;
        let art = codec.disk_bytes(&sample_workspace()).unwrap();
        assert!(art.sidecars.is_empty(), "workspaces produce no sidecars");
    }

    #[test]
    fn path_is_workspace_dir_plus_workspace_json() {
        let paths = Paths::for_env("/proj", "dev");
        let codec = Workspaces;
        assert_eq!(
            codec.path(&paths, "invoices-ap"),
            std::path::PathBuf::from("/proj/envs/dev/workspaces/invoices-ap/workspace.json")
        );
    }
}
