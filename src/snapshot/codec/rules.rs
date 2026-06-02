//! [`KindCodec`] implementation for the `rules` kind.
//!
//! Rules carry an optional `trigger_condition` string that is extracted to a
//! sidecar (mirroring how hooks extract `config.code`). The sidecar label
//! `"trigger_condition"` matches the key name used by `rule_combined_hash`,
//! so the default `combined_hash` over `(json, sidecars)` reproduces the
//! legacy combined hash byte-for-byte.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::model::Rule;
use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::rule::serialize_rule;

use anyhow::Context as _;

pub struct Rules;

impl KindCodec for Rules {
    fn kind(&self) -> &'static str {
        "rules"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let rule: Rule = serde_json::from_value(value.clone())
            .with_context(|| "deserializing rule from API body")?;
        let (json, code) = serialize_rule(&rule)?;
        let sidecars = if let Some(code) = code {
            vec![("trigger_condition".to_string(), code.into_bytes())]
        } else {
            vec![]
        };
        Ok(DiskArtifact { json, sidecars })
    }

    fn create_body(&self, body: &mut Value) {
        strip_for_create(body, "rules");
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "rules");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        overlay.rule(slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.rules_dir().join(format!("{slug}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::codec::KindCodec;
    use serde_json::json;

    fn sample_rule_with_condition() -> Value {
        json!({
            "id": 1,
            "url": "https://example/api/v1/rules/1",
            "name": "Validate Totals",
            "queues": [],
            "trigger_condition": "annotation_content.total > 0\n",
            "modified_at": "2026-05-01T12:00:00Z"
        })
    }

    fn sample_rule_without_condition() -> Value {
        json!({
            "id": 2,
            "url": "https://example/api/v1/rules/2",
            "name": "Always Fires",
            "queues": [],
            "modified_at": "2026-05-01T12:00:00Z"
        })
    }

    #[test]
    fn trigger_condition_extracted_to_sidecar() {
        let codec = Rules;
        let v = sample_rule_with_condition();
        let art = codec.disk_bytes(&v).unwrap();
        assert_eq!(
            art.sidecars.len(),
            1,
            "expected exactly one sidecar for a rule with trigger_condition"
        );
        let (label, bytes) = &art.sidecars[0];
        assert_eq!(
            label, "trigger_condition",
            "sidecar label must be 'trigger_condition'"
        );
        assert_eq!(
            std::str::from_utf8(bytes).unwrap(),
            "annotation_content.total > 0\n"
        );
    }

    #[test]
    fn trigger_condition_not_in_json() {
        let codec = Rules;
        let v = sample_rule_with_condition();
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            !disk_str.contains("trigger_condition"),
            "trigger_condition must be extracted to sidecar, not left in JSON; got:\n{disk_str}"
        );
    }

    #[test]
    fn no_sidecar_when_no_trigger_condition() {
        let codec = Rules;
        let v = sample_rule_without_condition();
        let art = codec.disk_bytes(&v).unwrap();
        assert!(
            art.sidecars.is_empty(),
            "rule without trigger_condition must produce no sidecars"
        );
    }

    #[test]
    fn modified_at_stripped_from_disk() {
        let codec = Rules;
        let v = sample_rule_with_condition();
        let art = codec.disk_bytes(&v).unwrap();
        let disk_str = std::str::from_utf8(&art.json).unwrap();
        assert!(
            !disk_str.contains("modified_at"),
            "modified_at must be stripped from disk; got:\n{disk_str}"
        );
    }

    #[test]
    fn base_hash_matches_legacy_rule_combined_hash() {
        let v = sample_rule_with_condition();
        let rule: crate::model::Rule = serde_json::from_value(v.clone()).unwrap();
        let (json, code) = crate::snapshot::rule::serialize_rule(&rule).unwrap();
        let legacy = crate::state::rule_combined_hash(&json, &code);

        let codec = Rules;
        let codec_hash = codec.base_hash(&v).unwrap();
        assert_eq!(
            codec_hash, legacy,
            "codec base_hash must match legacy rule_combined_hash"
        );
    }

    #[test]
    fn base_hash_matches_legacy_rule_combined_hash_no_condition() {
        let v = sample_rule_without_condition();
        let rule: crate::model::Rule = serde_json::from_value(v.clone()).unwrap();
        let (json, code) = crate::snapshot::rule::serialize_rule(&rule).unwrap();
        let legacy = crate::state::rule_combined_hash(&json, &code);

        let codec = Rules;
        let codec_hash = codec.base_hash(&v).unwrap();
        assert_eq!(
            codec_hash, legacy,
            "codec base_hash must match legacy rule_combined_hash (no condition)"
        );
    }
}
