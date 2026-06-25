use crate::support::client::LiveClient;
use crate::support::manifest::Manifest;
use crate::support::refs::resolve_placeholders;
use crate::support::run_id::RunId;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SeedEntry {
    pub key: String,
    pub kind: String,
    pub id: u64,
    pub url: String,
    pub name: String,
}

#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct SeedIndex {
    by_key: BTreeMap<String, SeedEntry>,
}

#[allow(dead_code)]
impl SeedIndex {
    pub fn url(&self, _kind: &str, key: &str) -> Option<&str> {
        self.by_key.get(key).map(|e| e.url.as_str())
    }
    pub fn id(&self, key: &str) -> Option<u64> {
        self.by_key.get(key).map(|e| e.id)
    }
    pub fn entries(&self) -> impl Iterator<Item = &SeedEntry> {
        self.by_key.values()
    }
}

/// Create every object in the manifest, in dependency order, with names
/// prefixed by the run id. Cross-refs (`@kind/key`) resolve to the URL of the
/// already-created dependency. Hook code sidecars (`*.py` named by the body's
/// `config.code_file`) are inlined into `config.code` before POST.
#[allow(dead_code)]
pub async fn seed(
    client: &LiveClient,
    run_id: &RunId,
    dir: &Path,
    manifest: &Manifest,
) -> Result<SeedIndex> {
    let mut index = SeedIndex::default();
    // (kind, key) -> url, for placeholder resolution. Also map the special
    // "organization"/"self" to the org url so bodies can reference it.
    let mut resolved: BTreeMap<(String, String), String> = BTreeMap::new();
    resolved.insert(("organization".into(), "self".into()), client.org_url.clone());

    for spec in manifest.topo_order()? {
        let body_path = dir.join(&spec.body);
        let raw = std::fs::read_to_string(&body_path)
            .with_context(|| format!("reading body {}", body_path.display()))?;
        let mut body: serde_json::Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", spec.body))?;

        // Prefix the display name.
        if let Some(name) = body.get("name").and_then(|n| n.as_str()) {
            body["name"] = serde_json::Value::String(run_id.prefix(name));
        }

        // Prefix the inbox `email_prefix` too: the live API requires it (or
        // `email`) on inbox create, and it forms a globally-unique inbox email
        // address, so it must be unique per run. The run-id prefix keeps the
        // address slug-safe and teardown-matchable.
        if let Some(ep) = body.get("email_prefix").and_then(|e| e.as_str()) {
            body["email_prefix"] = serde_json::Value::String(run_id.prefix(ep));
        }

        // Inline a hook code sidecar if `config.code_file` is present.
        if let Some(code_file) = body
            .get("config")
            .and_then(|c| c.get("code_file"))
            .and_then(|f| f.as_str())
            .map(|s| s.to_string())
        {
            let code = std::fs::read_to_string(dir.join(&code_file))
                .with_context(|| format!("reading code sidecar {code_file}"))?;
            body["config"]["code"] = serde_json::Value::String(code);
            body["config"]
                .as_object_mut()
                .unwrap()
                .remove("code_file");
        }

        resolve_placeholders(&mut body, &resolved)?;

        let (id, url) = client
            .create(&spec.kind, &body)
            .await
            .with_context(|| format!("creating {} ({})", spec.key, spec.kind))?;

        let name = body.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
        resolved.insert((spec.kind.clone(), spec.key.clone()), url.clone());
        index.by_key.insert(
            spec.key.clone(),
            SeedEntry { key: spec.key.clone(), kind: spec.kind.clone(), id, url, name },
        );
    }
    Ok(index)
}
