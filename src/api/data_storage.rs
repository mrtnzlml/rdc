use crate::api::ApiError;
use crate::model::Collection;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

/// Rossum Data Storage API client. Distinct from `RossumClient` because it
/// targets a different base URL (typically `https://X.rossum.app/data/v1`).
/// Reuses the same API token.
pub struct DataStorageClient {
    base_url: String,
    token: String,
    http: Client,
}

#[derive(Debug, Deserialize)]
struct CollectionsResponse {
    #[serde(default)]
    collections: Vec<Collection>,
}

#[derive(Debug, Deserialize)]
struct IndexesResponse {
    #[serde(default)]
    indexes: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct SearchIndexesResponse {
    #[serde(default)]
    search_indexes: Vec<Value>,
}

impl DataStorageClient {
    pub fn new(base_url: String, token: String) -> Result<Self> {
        let http = Client::builder()
            .build()
            .map_err(|e| anyhow::anyhow!("building reqwest client: {e}"))?;
        Ok(Self { base_url, token, http })
    }

    pub async fn list_collections(&self) -> Result<Vec<Collection>> {
        let url = format!("{}/collections", self.base_url);
        let resp: CollectionsResponse = self.get_json(&url).await?;
        Ok(resp.collections)
    }

    pub async fn list_indexes(&self, collection: &str) -> Result<Vec<Value>> {
        let url = format!("{}/collections/{}/indexes", self.base_url, collection);
        let resp: IndexesResponse = self.get_json(&url).await?;
        Ok(resp.indexes)
    }

    pub async fn list_search_indexes(&self, collection: &str) -> Result<Vec<Value>> {
        let url = format!("{}/collections/{}/search-indexes", self.base_url, collection);
        let resp: SearchIndexesResponse = self.get_json(&url).await?;
        Ok(resp.search_indexes)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self
            .http
            .get(url)
            .header("Authorization", format!("token {}", self.token))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body }.into());
        }
        let value = resp
            .json::<T>()
            .await
            .with_context(|| format!("decoding response from {url}"))?;
        Ok(value)
    }
}
