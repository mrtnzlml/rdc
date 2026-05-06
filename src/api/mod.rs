pub mod error;

pub use error::ApiError;

use crate::model::Hook;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

/// Rossum API client. Holds a base URL (e.g. `https://X.rossum.app/api/v1`)
/// and a static API token. M1 only implements the methods needed for `pull`
/// of hooks. Pagination is followed transparently.
pub struct RossumClient {
    base_url: String,
    token: String,
    http: Client,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    pagination: Pagination,
    results: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct Pagination {
    next: Option<String>,
}

impl RossumClient {
    pub fn new(base_url: String, token: String) -> Result<Self> {
        let http = Client::builder()
            .build()
            .map_err(|e| anyhow::anyhow!("building reqwest client: {e}"))?;
        Ok(Self { base_url, token, http })
    }

    pub async fn list_hooks(&self) -> Result<Vec<Hook>> {
        let mut url = format!("{}/hooks", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<Hook> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn get_organization(&self, id: u64) -> Result<crate::model::Organization> {
        let url = format!("{}/organizations/{id}", self.base_url);
        self.get_json(&url).await
    }

    pub async fn list_workspaces(&self) -> Result<Vec<crate::model::Workspace>> {
        let mut url = format!("{}/workspaces", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Workspace> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
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
            return Err(ApiError::Status {
                status: status.as_u16(),
                body,
            }
            .into());
        }
        let value = resp
            .json::<T>()
            .await
            .with_context(|| format!("decoding response from {url}"))?;
        Ok(value)
    }
}
