use serde::{Deserialize, Serialize};
use serde_json::Value;
use indexmap::IndexMap;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct HookTemplate {
    #[serde(default)]
    pub url: String,
    pub name: String,
    #[serde(rename = "type")]
    pub template_type: String,
    pub extension_source: String,
    pub install_action: String,
    /// Forward-compat: any field not modelled here survives via round-trip.
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn round_trip_preserves_unknown_fields() {
        let payload = json!({
            "url": "https://elis.rossum.ai/api/v1/hook_templates/39",
            "name": "Master Data Hub",
            "type": "webhook",
            "extension_source": "rossum_store",
            "install_action": "copy",
            "description": "Enhance ...",
            "guide": "<div>...</div>",
            "settings_schema": null
        });
        let t: HookTemplate = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(t.name, "Master Data Hub");
        assert_eq!(t.template_type, "webhook");
        assert_eq!(t.install_action, "copy");
        let round_trip = serde_json::to_value(&t).unwrap();
        assert_eq!(round_trip, payload);
    }
}
