use anyhow::{bail, Result};
use std::collections::BTreeMap;

/// Replace every string of the form `@<kind>/<key>` anywhere in `body` with
/// the resolved URL from `resolved[(kind, key)]`. Errors if a placeholder has
/// no resolution (a dependency that was not created/declared first).
pub fn resolve_placeholders(
    body: &mut serde_json::Value,
    resolved: &BTreeMap<(String, String), String>,
) -> Result<()> {
    match body {
        serde_json::Value::String(s) => {
            if let Some(rest) = s.strip_prefix('@') {
                let (kind, key) = rest
                    .split_once('/')
                    .ok_or_else(|| anyhow::anyhow!("bad placeholder '{s}': expected @kind/key"))?;
                match resolved.get(&(kind.to_string(), key.to_string())) {
                    Some(url) => *s = url.clone(),
                    None => bail!("unresolved placeholder '{s}'"),
                }
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            for it in items {
                resolve_placeholders(it, resolved)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (_k, v) in map.iter_mut() {
                resolve_placeholders(v, resolved)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_nested_placeholders() {
        let mut body = json!({
            "name": "Q",
            "workspace": "@workspace/ws",
            "hooks": ["@hook/v1", "literal"],
        });
        let mut r = BTreeMap::new();
        r.insert(("workspace".into(), "ws".into()), "https://h/v1/workspaces/1".into());
        r.insert(("hook".into(), "v1".into()), "https://h/v1/hooks/9".into());
        resolve_placeholders(&mut body, &r).unwrap();
        assert_eq!(body["workspace"], "https://h/v1/workspaces/1");
        assert_eq!(body["hooks"][0], "https://h/v1/hooks/9");
        assert_eq!(body["hooks"][1], "literal");
    }

    #[test]
    fn errors_on_unresolved() {
        let mut body = json!({ "workspace": "@workspace/missing" });
        assert!(resolve_placeholders(&mut body, &BTreeMap::new()).is_err());
    }
}
