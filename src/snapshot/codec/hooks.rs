//! [`KindCodec`] implementation for the `hooks` kind.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::model::Hook;
use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::hook::serialize_hook;

pub struct Hooks;

impl KindCodec for Hooks {
    fn kind(&self) -> &'static str {
        "hooks"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let hook: Hook = serde_json::from_value(value.clone())
            .with_context(|| "deserializing hook from API body")?;
        let (json, code) = serialize_hook(&hook)?;
        let sidecars = if let Some(code) = code {
            vec![("code".to_string(), code.into_bytes())]
        } else {
            vec![]
        };
        Ok(DiskArtifact { json, sidecars })
    }

    fn create_body(&self, body: &mut Value) {
        strip_for_create(body, "hooks");
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "hooks");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        overlay.hook(slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.hooks_dir().join(format!("{slug}.json"))
    }
}

// Bring `with_context` into scope for the `?` chain above.
use anyhow::Context as _;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::codec::KindCodec;
    use crate::snapshot::create::REDACTED_VALUE_SENTINEL;
    use serde_json::json;

    fn sample_hook_value() -> Value {
        json!({
            "id": 1,
            "url": "https://example/api/v1/hooks/1",
            "name": "My Hook",
            "type": "function",
            "queues": [],
            "events": [],
            "status": "ready",
            "config": {
                "runtime": "python3.12",
                "code": "def x():\n    return 1\n"
            },
            "modified_at": "2026-05-01T12:00:00Z"
        })
    }

    #[test]
    fn disk_json_contains_sentinel_not_ready() {
        let codec = Hooks;
        let v = sample_hook_value();
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            disk_str.contains(REDACTED_VALUE_SENTINEL),
            "disk json must contain the sentinel; got:\n{disk_str}"
        );
        assert!(
            !disk_str.contains("\"ready\""),
            "disk json must not contain raw 'ready' status; got:\n{disk_str}"
        );
    }

    #[test]
    fn disk_json_does_not_contain_code() {
        let codec = Hooks;
        let v = sample_hook_value();
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            !disk_str.contains("def x"),
            "code must be extracted from json into sidecar; got:\n{disk_str}"
        );
    }

    #[test]
    fn exactly_one_code_sidecar() {
        let codec = Hooks;
        let v = sample_hook_value();
        let art = codec.disk_bytes(&v).unwrap();
        assert_eq!(
            art.sidecars.len(),
            1,
            "expected exactly one sidecar for a function hook with code"
        );
        let (label, bytes) = &art.sidecars[0];
        assert_eq!(label, "code", "sidecar label must be 'code'");
        assert_eq!(
            std::str::from_utf8(bytes).unwrap(),
            "def x():\n    return 1\n"
        );
    }

    #[test]
    fn no_sidecar_when_no_code() {
        let codec = Hooks;
        let v = json!({
            "id": 2,
            "url": "u",
            "name": "Webhook",
            "type": "webhook",
            "queues": [],
            "events": [],
            "config": {}
        });
        let art = codec.disk_bytes(&v).unwrap();
        assert!(
            art.sidecars.is_empty(),
            "webhook with no code must produce no sidecars"
        );
    }

    #[test]
    fn modified_at_stripped_from_disk() {
        let codec = Hooks;
        let v = sample_hook_value();
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            !disk_str.contains("modified_at"),
            "modified_at must be stripped from disk; got:\n{disk_str}"
        );
    }
}
