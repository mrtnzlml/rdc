//! [`KindCodec`] implementation for the `workflow_steps` kind.

use std::path::PathBuf;

use serde_json::Value;

use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::key_order::strip_hidden_fields_recursive;

pub struct WorkflowSteps;

impl KindCodec for WorkflowSteps {
    fn kind(&self) -> &'static str {
        "workflow_steps"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let mut v = value.clone();
        // No redaction for workflow_steps — `redact_on_pull("workflow_steps")` is empty.
        strip_hidden_fields_recursive(&mut v);
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact {
            json,
            sidecars: vec![],
        })
    }

    fn create_body(&self, body: &mut Value) {
        strip_for_create(body, "workflow_steps");
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "workflow_steps");
    }

    // No overlay for workflow_steps — the default impl returns `None`.

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        // `slug` is the composite lockfile key "<workflow_slug>/<step_slug>".
        // Split on the first '/': workflow slugs come from `slugify_unique`
        // and never contain a slash.
        if let Some((workflow_slug, step_slug)) = slug.split_once('/') {
            paths
                .workflow_steps_dir(workflow_slug)
                .join(format!("{step_slug}.json"))
        } else {
            // Defensive fallback for legacy flat keys not yet rewritten by the pull driver.
            paths.workflows_dir().join(format!("{slug}.json"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use serde_json::json;

    fn sample_workflow_step() -> Value {
        json!({
            "id": 42,
            "url": "https://x.rossum.app/api/v1/workflow_steps/42",
            "name": "Manager Approval",
            "workflow": "https://x.rossum.app/api/v1/workflows/700",
            "modified_at": "2026-04-20T08:00:00Z",
            "step_type": "approval"
        })
    }

    #[test]
    fn disk_bytes_strips_modified_at_no_sidecars() {
        let codec = WorkflowSteps;
        let art = codec.disk_bytes(&sample_workflow_step()).unwrap();

        assert!(art.sidecars.is_empty(), "workflow_steps have no sidecars");
        assert_eq!(art.json.last(), Some(&b'\n'), "trailing newline required");

        let out: Value = serde_json::from_slice(&art.json).unwrap();
        assert!(
            out.get("modified_at").is_none(),
            "modified_at must be absent on disk"
        );
        assert_eq!(out["name"], json!("Manager Approval"));
        assert_eq!(out["step_type"], json!("approval"));
    }

    #[test]
    fn path_splits_composite_slug() {
        let paths = Paths::for_env("/proj", "dev");
        let codec = WorkflowSteps;
        assert_eq!(
            codec.path(&paths, "ap-flow/manager-approval"),
            std::path::PathBuf::from(
                "/proj/envs/dev/workflows/ap-flow/steps/manager-approval.json"
            )
        );
    }

    #[test]
    fn path_fallback_for_legacy_flat_slug() {
        let paths = Paths::for_env("/proj", "dev");
        let codec = WorkflowSteps;
        // A legacy flat key (no '/') falls under workflows/ directly.
        let p = codec.path(&paths, "manager-approval");
        assert!(
            p.to_string_lossy().contains("manager-approval"),
            "fallback path must include the slug"
        );
    }
}
