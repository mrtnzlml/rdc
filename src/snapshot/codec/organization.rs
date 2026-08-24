//! [`KindCodec`] implementation for the `organization` kind.
//!
//! Archetype: **flat plain, pull-only**. There is exactly one organization per
//! env, so rdc never *creates* it (`create_body` is a no-op). Migrate DOES
//! promote it cross-env, but only its `settings` subtree — `cross_env_body`
//! strips everything else so the target's own identity/branding survive.
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
use crate::snapshot::key_order::strip_hidden_fields;

pub struct Organization;

impl KindCodec for Organization {
    fn kind(&self) -> &'static str {
        "organization"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        // Flat plain: no per-kind redaction (organization has no entry in
        // `create::redact_on_pull`). Only the universal hidden-field strip
        // (`modified_at` / `modified_by`, top level) is applied.
        let mut v = value.clone();
        strip_hidden_fields(&mut v);
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

    fn cross_env_body(&self, body: &mut Value) {
        // Cross-env promotion carries exactly ONE subtree: `settings`.
        //
        // Everything else on an organization is either read-only at the API
        // (`id`, `url`, `name`, `sandbox`, the stamps, …) or per-env state that
        // must not move between orgs: `ui_settings` holds branding and the
        // org's applied feature flags, `metadata` is free-form. Stripping the
        // rest here is precisely what makes `migrate`'s
        // `reconcile_target_identity` restore those fields from the TARGET's own
        // `organization.json`.
        //
        // A retain-list rather than a strip-list, so a field a future API
        // revision adds defaults to env-local — the conservative direction.
        if let Some(obj) = body.as_object_mut() {
            obj.retain(|k, _| k == "settings");
        }
    }

    fn overlay<'a>(
        &self,
        overlay: &'a Overlay,
        _slug: &str,
    ) -> Option<&'a BTreeMap<String, Value>> {
        // One org per env, so the section is slug-independent: `[organization]`,
        // not `[organization.<slug>]`.
        overlay.organization()
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
            "modified_by": "https://x.rossum.app/api/v1/users/1",
            "metadata": { "tag": "primary", "modified_at": "2026-03-01T08:00:00Z" }
        })
    }

    #[test]
    fn top_level_stamps_stripped_from_disk() {
        let art = Organization.disk_bytes(&org_value()).unwrap();
        let disk: Value = serde_json::from_slice(&art.json).unwrap();
        assert!(
            disk.get("modified_at").is_none(),
            "top-level modified_at must be stripped from disk"
        );
        assert!(
            disk.get("modified_by").is_none(),
            "top-level modified_by must be stripped from disk"
        );
    }

    #[test]
    fn nested_stamp_inside_user_metadata_is_preserved() {
        // `metadata` is user-writable and push sends it wholesale — stripping a
        // stamp-named key inside it would delete the user's data server-side.
        let art = Organization.disk_bytes(&org_value()).unwrap();
        let disk: Value = serde_json::from_slice(&art.json).unwrap();
        assert_eq!(
            disk["metadata"].get("modified_at").and_then(|v| v.as_str()),
            Some("2026-03-01T08:00:00Z"),
            "a nested stamp is user data and must survive: {disk}"
        );
    }

    #[test]
    fn no_sidecars() {
        let art = Organization.disk_bytes(&org_value()).unwrap();
        assert!(art.sidecars.is_empty(), "organization is sidecar-free");
    }

    #[test]
    fn create_body_is_a_noop() {
        let before = org_value();
        let mut v = before.clone();
        Organization.create_body(&mut v);
        assert_eq!(v, before, "create_body must be a no-op: rdc never POSTs an organization");
    }

    #[test]
    fn cross_env_body_retains_only_settings() {
        let mut v = org_value();
        v["ui_settings"] = json!({ "theme": "white" });
        v["metadata"] = json!({ "source": "registration" });
        Organization.cross_env_body(&mut v);
        assert_eq!(
            v,
            json!({ "settings": { "ui_settings": { "language": "en" } } }),
            "cross-env promotion carries `settings` and nothing else: {v}"
        );
    }

    #[test]
    fn cross_env_body_on_a_body_without_settings_yields_an_empty_object() {
        let mut v = json!({ "id": 1, "name": "Acme", "ui_settings": { "theme": "white" } });
        Organization.cross_env_body(&mut v);
        assert_eq!(v, json!({}));
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

    #[test]
    fn overlay_hook_returns_the_organization_section_for_any_slug() {
        let ov: crate::overlay::Overlay = toml::from_str(
            "version = 1\n\n[organization]\nsettings = { annotation_list_table = { columns = [] } }\n",
        )
        .unwrap();
        assert!(Organization.overlay(&ov, "self").is_some());
        assert!(Organization.overlay(&ov, "ignored").is_some(), "slug-independent");
    }
}
