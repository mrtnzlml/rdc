pub mod data_storage;
pub mod error;

pub use data_storage::DataStorageClient;
pub use error::ApiError;

use crate::model::Hook;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

/// Rossum API client. Holds a base URL (e.g. `https://X.rossum.app/api/v1`)
/// and a static API token. Pagination is followed transparently for `list_*`
/// methods. As of M5, supports list/get for organizations, workspaces, queues,
/// inboxes, schemas, hooks, rules, labels, engines, engine fields, workflows,
/// workflow steps, and email templates.
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

    pub async fn update_rule(&self, id: u64, rule: &crate::model::Rule) -> Result<crate::model::Rule> {
        let url = format!("{}/rules/{id}", self.base_url);
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", format!("token {}", self.token))
            .json(rule)
            .send()
            .await
            .with_context(|| format!("PATCH {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body }.into());
        }
        let value = resp.json::<crate::model::Rule>().await
            .with_context(|| format!("decoding PATCH response from {url}"))?;
        Ok(value)
    }

    pub async fn update_label(&self, id: u64, label: &crate::model::Label) -> Result<crate::model::Label> {
        let url = format!("{}/labels/{id}", self.base_url);
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", format!("token {}", self.token))
            .json(label)
            .send()
            .await
            .with_context(|| format!("PATCH {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body }.into());
        }
        let value = resp.json::<crate::model::Label>().await
            .with_context(|| format!("decoding PATCH response from {url}"))?;
        Ok(value)
    }

    /// PATCH /hooks/{id} with the given hook body. Returns the server's
    /// authoritative response (including server-set fields like modified_at).
    pub async fn update_hook(&self, id: u64, hook: &Hook) -> Result<Hook> {
        let url = format!("{}/hooks/{id}", self.base_url);
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", format!("token {}", self.token))
            .json(hook)
            .send()
            .await
            .with_context(|| format!("PATCH {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body }.into());
        }
        let value = resp
            .json::<Hook>()
            .await
            .with_context(|| format!("decoding PATCH response from {url}"))?;
        Ok(value)
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

    pub async fn list_queues(&self) -> Result<Vec<crate::model::Queue>> {
        let mut url = format!("{}/queues", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Queue> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn get_inbox(&self, id: u64) -> Result<crate::model::Inbox> {
        let url = format!("{}/inboxes/{id}", self.base_url);
        self.get_json(&url).await
    }

    pub async fn get_schema(&self, id: u64) -> Result<crate::model::Schema> {
        let url = format!("{}/schemas/{id}", self.base_url);
        self.get_json(&url).await
    }

    /// PATCH /schemas/{id} with the given schema body. Returns the server's
    /// authoritative response (including server-set fields like modified_at).
    /// Used by `rdc push` to apply local schema edits (including formula
    /// changes spliced back via `read_schema`).
    pub async fn update_schema(&self, id: u64, schema: &crate::model::Schema)
        -> Result<crate::model::Schema>
    {
        let url = format!("{}/schemas/{id}", self.base_url);
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", format!("token {}", self.token))
            .json(schema)
            .send()
            .await
            .with_context(|| format!("PATCH {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body }.into());
        }
        let value = resp.json::<crate::model::Schema>().await
            .with_context(|| format!("decoding PATCH response from {url}"))?;
        Ok(value)
    }

    pub async fn list_rules(&self) -> Result<Vec<crate::model::Rule>> {
        let mut url = format!("{}/rules", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Rule> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_labels(&self) -> Result<Vec<crate::model::Label>> {
        let mut url = format!("{}/labels", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Label> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_engines(&self) -> Result<Vec<crate::model::Engine>> {
        let mut url = format!("{}/engines", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Engine> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_engine_fields(&self) -> Result<Vec<crate::model::EngineField>> {
        let mut url = format!("{}/engine_fields", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::EngineField> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_workflows(&self) -> Result<Vec<crate::model::Workflow>> {
        let mut url = format!("{}/workflows", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Workflow> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_workflow_steps(&self) -> Result<Vec<crate::model::WorkflowStep>> {
        let mut url = format!("{}/workflow_steps", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::WorkflowStep> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_email_templates(&self) -> Result<Vec<crate::model::EmailTemplate>> {
        let mut url = format!("{}/email_templates", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::EmailTemplate> = self.get_json(&url).await?;
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
