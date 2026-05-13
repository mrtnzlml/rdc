//! Store-extension support for `rdc push` and `rdc deploy`. Centralises:
//!   - Effective `token_owner` resolution (per-hook overlay → defaults → None).
//!   - Template-URL resolution against the target cluster (later tasks).
//!   - Install-body construction (later tasks).
//!   - Interactive `token_owner` picker (later tasks).

use crate::overlay::Overlay;
use serde_json::Value;

/// Resolve the effective `token_owner` URL for a store extension on a
/// given environment. Order: per-hook overlay `token_owner` → overlay
/// `[defaults] store_extension_token_owner` → `None`.
pub fn effective_token_owner<'a>(overlay: Option<&'a Overlay>, slug: &str) -> Option<&'a str> {
    let overlay = overlay?;
    if let Some(per_hook) = overlay.hook(slug)
        .and_then(|m| m.get("token_owner"))
        .and_then(Value::as_str)
    {
        return Some(per_hook);
    }
    overlay.defaults.store_extension_token_owner.as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{Defaults, Overlay};
    use std::collections::BTreeMap;

    fn ov_with(per_hook: Option<&str>, default_url: Option<&str>) -> Overlay {
        let mut hooks = BTreeMap::new();
        if let Some(url) = per_hook {
            let mut entry = BTreeMap::new();
            entry.insert("token_owner".into(), Value::String(url.into()));
            hooks.insert("master-data-hub".into(), entry);
        }
        Overlay {
            version: 1,
            hooks,
            rules: BTreeMap::new(),
            labels: BTreeMap::new(),
            schemas: BTreeMap::new(),
            queues: BTreeMap::new(),
            inboxes: BTreeMap::new(),
            email_templates: BTreeMap::new(),
            engines: BTreeMap::new(),
            engine_fields: BTreeMap::new(),
            defaults: Defaults {
                store_extension_token_owner: default_url.map(|s| s.into()),
            },
        }
    }

    #[test]
    fn per_hook_wins_over_defaults() {
        let ov = ov_with(Some("https://per-hook"), Some("https://default"));
        assert_eq!(effective_token_owner(Some(&ov), "master-data-hub"), Some("https://per-hook"));
    }

    #[test]
    fn falls_back_to_defaults_when_no_per_hook() {
        let ov = ov_with(None, Some("https://default"));
        assert_eq!(effective_token_owner(Some(&ov), "master-data-hub"), Some("https://default"));
    }

    #[test]
    fn returns_none_when_neither_set() {
        let ov = ov_with(None, None);
        assert_eq!(effective_token_owner(Some(&ov), "master-data-hub"), None);
    }

    #[test]
    fn returns_none_when_no_overlay() {
        assert_eq!(effective_token_owner(None, "master-data-hub"), None);
    }
}
