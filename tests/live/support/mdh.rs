use crate::support::config::LiveConfig;
use crate::support::run_id::RunId;
use anyhow::{anyhow, Context, Result};
use rdc::api::DataStorageClient;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Stable prefix shared by every throwaway MDH collection this harness
/// creates. Mongo-safe (underscores), distinct from the core-API `rdc-it-`
/// marker because collection names use a different charset convention.
pub const MDH_COLLECTION_MARKER: &str = "rdc_it_";

/// Per-run throwaway collection name: `rdc_it_<id>_mdh`. The run id is base36
/// alnum, so the whole name is Mongo-safe.
pub fn mdh_collection_name(run_id: &RunId) -> String {
    format!("{}{}_mdh", MDH_COLLECTION_MARKER, run_id.as_str())
}

/// Raw Data-Storage client for the 3 collection-lifecycle endpoints rdc's
/// `DataStorageClient` does not expose. Everything else (index create/drop,
/// listing) reuses `DataStorageClient` via [`MdhRaw::ds_client`].
#[allow(dead_code)]
pub struct MdhRaw {
    http: reqwest::Client,
    base: String,
    token: String,
}

#[allow(dead_code)]
impl MdhRaw {
    pub fn connect(cfg: &LiveConfig) -> Result<MdhRaw> {
        // Derive the Data-Storage base the same way rdc does (single source of
        // truth), via the already-public EnvConfig helper.
        let base = rdc::config::EnvConfig {
            api_base: cfg.api_base.clone(),
            org_id: cfg.org_id,
        }
        .data_storage_base();
        let http = reqwest::Client::builder()
            .build()
            .context("building reqwest client for MDH raw helper")?;
        Ok(MdhRaw { http, base, token: cfg.token.clone() })
    }

    /// A fresh rdc DataStorageClient over the same base + token, for index
    /// create/drop + listing in tests.
    pub fn ds_client(&self) -> DataStorageClient {
        DataStorageClient::new(self.base.clone(), self.token.clone())
            .expect("construct DataStorageClient")
    }

    async fn post(&self, path: &str, body: Value) -> Result<(reqwest::StatusCode, String)> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    pub async fn create_collection(&self, name: &str) -> Result<()> {
        let (status, body) = self
            .post("/v1/collections/create", json!({ "collectionName": name }))
            .await?;
        if !status.is_success() {
            return Err(anyhow!("create_collection {name}: {status} {body}"));
        }
        Ok(())
    }

    pub async fn insert_one(&self, name: &str, doc: Value) -> Result<()> {
        let (status, body) = self
            .post(
                "/v1/data/insert_one",
                json!({ "collectionName": name, "document": doc }),
            )
            .await?;
        if !status.is_success() {
            return Err(anyhow!("insert_one {name}: {status} {body}"));
        }
        Ok(())
    }

    /// Every row in a collection, via the raw find endpoint.
    pub async fn find_all(&self, name: &str) -> Result<Vec<Value>> {
        let (status, body) = self
            .post("/v1/data/find", json!({ "collectionName": name, "query": {} }))
            .await?;
        if !status.is_success() {
            return Err(anyhow!("find_all {name}: {status} {body}"));
        }
        let env: Value = serde_json::from_str(&body)
            .with_context(|| format!("decoding find_all response for {name}"))?;
        Ok(env
            .get("result")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Drop a collection. Async (202); best-effort — a missing collection is
    /// not an error (idempotent teardown).
    pub async fn drop_collection(&self, name: &str) -> Result<()> {
        let (status, body) = self
            .post("/v1/collections/drop", json!({ "collectionName": name }))
            .await?;
        // 2xx (incl. 202) = accepted; 404 / "not found" = already gone.
        if status.is_success()
            || status.as_u16() == 404
            || body.to_lowercase().contains("not found")
        {
            return Ok(());
        }
        Err(anyhow!("drop_collection {name}: {status} {body}"))
    }

    pub async fn list_collection_names(&self) -> Result<Vec<String>> {
        let client = self.ds_client();
        let cols = client.list_collections(None).await?;
        Ok(cols.into_iter().map(|c| c.name).collect())
    }

    /// Poll until the named regular index is present (`present=true`) or absent
    /// (`present=false`). Bounded at 30s — index create/drop is async (202).
    pub async fn wait_for_regular_index(&self, coll: &str, name: &str, present: bool) -> Result<()> {
        self.wait_for_index(coll, name, present, false, Duration::from_secs(30)).await
    }

    /// Same as `wait_for_regular_index` but for Atlas Search indexes, with a
    /// longer bound (Atlas create/drop runs in the background; bound is 90s).
    pub async fn wait_for_search_index(&self, coll: &str, name: &str, present: bool) -> Result<()> {
        self.wait_for_index(coll, name, present, true, Duration::from_secs(90)).await
    }

    async fn wait_for_index(
        &self,
        coll: &str,
        name: &str,
        present: bool,
        search: bool,
        bound: Duration,
    ) -> Result<()> {
        let client = self.ds_client();
        let start = Instant::now();
        loop {
            let list = if search {
                client.list_search_indexes(coll, None).await?
            } else {
                client.list_indexes(coll, None).await?
            };
            let found = list
                .iter()
                .any(|ix| ix.get("name").and_then(|n| n.as_str()) == Some(name));
            if found == present {
                return Ok(());
            }
            if start.elapsed() >= bound {
                return Err(anyhow!(
                    "timed out waiting for {} index '{name}' on '{coll}' to be present={present}",
                    if search { "search" } else { "regular" }
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Attempt to create an Atlas Search index out-of-band. Returns `Ok(true)`
    /// on success, `Ok(false)` when the cluster does not support Search (so the
    /// caller can skip the search sub-phase instead of failing). Other errors
    /// propagate.
    pub async fn try_create_search_index(&self, coll: &str, name: &str) -> Result<bool> {
        let client = self.ds_client();
        match client
            .create_search_index(coll, name, &json!({ "mappings": { "dynamic": true } }), None)
            .await
        {
            Ok(()) => Ok(true),
            Err(e) => {
                let msg = format!("{e:#}").to_lowercase();
                // Treat "not supported / not enabled / 404 / 501" as "no Search
                // on this cluster" → graceful skip; anything else is a real error.
                if msg.contains("not support")
                    || msg.contains("not enabled")
                    || msg.contains("404")
                    || msg.contains("501")
                    || msg.contains("unavailable")
                {
                    eprintln!("MDH: Atlas Search unsupported on this cluster, skipping search sub-phase ({e:#})");
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::run_id::RunId;

    #[test]
    fn collection_name_is_mongo_safe_and_marked() {
        let id = RunId::new();
        let name = mdh_collection_name(&id);
        assert!(name.starts_with(MDH_COLLECTION_MARKER), "{name}");
        assert!(name.contains(id.as_str()), "{name}");
        // Mongo-safe: lowercase alnum + underscore only (no hyphens/spaces).
        assert!(
            name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "collection name not mongo-safe: {name}"
        );
    }
}
