//! [`KindCodec`] implementation for the `organization` kind.
//!
//! Archetype: **flat plain, pull-only**. There is exactly one organization per
//! env, so rdc never *creates* or cross-env-PATCHes it — `create_body` /
//! `cross_env_body` are no-ops.
//!
//! Path / slug: the on-disk location is fixed (`organization.json` directly
//! under the env root, via `Paths::organization_file()`) and does not depend
//! on the slug. The lockfile keys the org under the constant slug `"self"`;
//! `path()` ignores its `slug` argument accordingly.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::key_order::strip_hidden_fields_recursive;

pub struct Organization;

impl KindCodec for Organization {
    fn kind(&self) -> &'static str {
        "organization"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        // Flat plain: no per-kind redaction (organization has no entry in
        // `create::redact_on_pull`). Only the universal hidden-field strip
        // (`modified_at`) is applied.
        let mut v = value.clone();
        strip_hidden_fields_recursive(&mut v);
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact {
            json,
            sidecars: vec![],
        })
    }

    fn create_body(&self, _body: &mut Value) {
        // Pull-only kind: rdc never POSTs an organization. No-op.
    }

    fn cross_env_body(&self, _body: &mut Value) {
        // Pull-only kind: never part of a cross-env create/PATCH body. No-op.
    }

    fn overlay<'a>(
        &self,
        _overlay: &'a Overlay,
        _slug: &str,
    ) -> Option<&'a BTreeMap<String, Value>> {
        // The organization has no per-object overlay surface.
        None
    }

    fn path(&self, paths: &Paths, _slug: &str) -> PathBuf {
        // Fixed location, slug-independent: `<env>/organization.json`.
        // Replicates the pull driver's `ctx.paths.organization_file()`.
        paths.organization_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn org_value() -> Value {
        json!({
            "id": 285704,
            "url": "https://x.rossum.app/api/v1/organizations/285704",
            "name": "Acme Corp",
            "modified_at": "2026-03-01T08:00:00Z",
            "settings": { "ui_settings": { "language": "en" } },
            "users": ["https://x.rossum.app/api/v1/users/1"],
            "metadata": { "tag": "primary", "modified_at": "2026-03-01T08:00:00Z" }
        })
    }

    #[test]
    fn modified_at_stripped_from_disk() {
        let art = Organization.disk_bytes(&org_value()).unwrap();
        let disk: Value = serde_json::from_slice(&art.json).unwrap();
        assert!(
            disk.get("modified_at").is_none(),
            "top-level modified_at must be stripped from disk"
        );
        assert!(
            disk["metadata"].get("modified_at").is_none(),
            "nested modified_at must be stripped too (recursive)"
        );
    }

    #[test]
    fn no_sidecars() {
        let art = Organization.disk_bytes(&org_value()).unwrap();
        assert!(art.sidecars.is_empty(), "organization is sidecar-free");
    }

    #[test]
    fn create_and_cross_env_bodies_are_noops() {
        let before = org_value();
        let mut v = before.clone();
        Organization.create_body(&mut v);
        assert_eq!(v, before, "create_body must be a no-op");
        Organization.cross_env_body(&mut v);
        assert_eq!(v, before, "cross_env_body must be a no-op");
    }

    #[test]
    fn path_is_fixed_organization_file_ignoring_slug() {
        let paths = Paths::for_env("/proj", "dev");
        let expected = std::path::Path::new("/proj/envs/dev/organization.json");
        assert_eq!(Organization.path(&paths, "self"), expected);
        assert_eq!(Organization.path(&paths, "ignored"), expected);
    }

    #[test]
    fn kind_is_organization() {
        assert_eq!(Organization.kind(), "organization");
    }
}
