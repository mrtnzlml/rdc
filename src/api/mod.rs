pub mod data_storage;
pub mod error;
pub mod retry;

pub use data_storage::DataStorageClient;
pub use error::{anyhow_has_status, ApiError};

use crate::model::Hook;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

/// Rossum API client. Holds a base URL (e.g. `https://X.rossum.app/api/v1`)
/// and a static API token. Pagination is followed transparently for `list_*`
/// methods; PATCH calls go through the shared `patch_json` helper which
/// retries on 429 / 502 / 503 / 504 via `retry::send_with_retry`.
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

    pub async fn list_hooks(&self, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Vec<Hook>> {
        let mut url = format!("{}/hooks", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<Hook> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn update_rule(&self, id: u64, rule: &crate::model::Rule, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>)
        -> Result<crate::model::Rule>
    {
        self.patch_json(&format!("/rules/{id}"), rule, progress).await
    }

    pub async fn update_label(&self, id: u64, label: &crate::model::Label, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>)
        -> Result<crate::model::Label>
    {
        self.patch_json(&format!("/labels/{id}"), label, progress).await
    }

    /// PATCH /hooks/{id}. Returns the server's authoritative response.
    pub async fn update_hook(&self, id: u64, hook: &Hook, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Hook> {
        self.patch_json(&format!("/hooks/{id}"), hook, progress).await
    }

    pub async fn get_organization(&self, id: u64, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::Organization> {
        let url = format!("{}/organizations/{id}", self.base_url);
        self.get_json(&url, progress).await
    }

    /// GET /hooks/{id}. Used by `rdc diff` for single-hook fetch.
    pub async fn get_hook(&self, id: u64, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Hook> {
        let url = format!("{}/hooks/{id}", self.base_url);
        self.get_json(&url, progress).await
    }

    pub async fn list_workspaces(&self, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Vec<crate::model::Workspace>> {
        let mut url = format!("{}/workspaces", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Workspace> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_queues(&self, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Vec<crate::model::Queue>> {
        let mut url = format!("{}/queues", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Queue> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn get_inbox(&self, id: u64, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::Inbox> {
        let url = format!("{}/inboxes/{id}", self.base_url);
        self.get_json(&url, progress).await
    }

    pub async fn get_schema(&self, id: u64, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::Schema> {
        let url = format!("{}/schemas/{id}", self.base_url);
        self.get_json(&url, progress).await
    }

    pub async fn update_schema(&self, id: u64, schema: &crate::model::Schema, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>)
        -> Result<crate::model::Schema>
    {
        self.patch_json(&format!("/schemas/{id}"), schema, progress).await
    }

    pub async fn update_queue(&self, id: u64, queue: &crate::model::Queue, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>)
        -> Result<crate::model::Queue>
    {
        self.patch_json(&format!("/queues/{id}"), queue, progress).await
    }

    pub async fn update_inbox(&self, id: u64, inbox: &crate::model::Inbox, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>)
        -> Result<crate::model::Inbox>
    {
        self.patch_json(&format!("/inboxes/{id}"), inbox, progress).await
    }

    pub async fn update_email_template(&self, id: u64, t: &crate::model::EmailTemplate, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>)
        -> Result<crate::model::EmailTemplate>
    {
        self.patch_json(&format!("/email_templates/{id}"), t, progress).await
    }

    pub async fn update_engine(&self, id: u64, engine: &crate::model::Engine, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>)
        -> Result<crate::model::Engine>
    {
        self.patch_json(&format!("/engines/{id}"), engine, progress).await
    }

    pub async fn update_engine_field(&self, id: u64, field: &crate::model::EngineField, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>)
        -> Result<crate::model::EngineField>
    {
        self.patch_json(&format!("/engine_fields/{id}"), field, progress).await
    }

    pub async fn list_rules(&self, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Vec<crate::model::Rule>> {
        let mut url = format!("{}/rules", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Rule> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_labels(&self, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Vec<crate::model::Label>> {
        let mut url = format!("{}/labels", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Label> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_engines(&self, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Vec<crate::model::Engine>> {
        let mut url = format!("{}/engines", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Engine> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_engine_fields(&self, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Vec<crate::model::EngineField>> {
        let mut url = format!("{}/engine_fields", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::EngineField> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_workflows(&self, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Vec<crate::model::Workflow>> {
        let mut url = format!("{}/workflows", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::Workflow> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_workflow_steps(&self, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Vec<crate::model::WorkflowStep>> {
        let mut url = format!("{}/workflow_steps", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::WorkflowStep> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn list_email_templates(&self, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Vec<crate::model::EmailTemplate>> {
        let mut url = format!("{}/email_templates", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<crate::model::EmailTemplate> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<T> {
        let resp = retry::send_with_retry(
            || self.http.get(url).header("Authorization", format!("token {}", self.token)),
            &format!("GET {url}"),
            progress,
        ).await?;

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

    /// Generic PATCH `<base>/<path>` with `body` as JSON. Used by every
    /// `update_X` method. Centralises 429 retry/backoff via `retry::send_with_retry`.
    async fn patch_json<TBody, TResp>(&self, path: &str, body: &TBody, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<TResp>
    where
        TBody: serde::Serialize,
        TResp: serde::de::DeserializeOwned,
    {
        let url = format!("{}{}", self.base_url, path);
        let resp = retry::send_with_retry(
            || self.http
                .patch(&url)
                .header("Authorization", format!("token {}", self.token))
                .json(body),
            &format!("PATCH {url}"),
            progress,
        ).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body }.into());
        }
        resp.json::<TResp>().await
            .with_context(|| format!("decoding PATCH response from {url}"))
    }

    /// Generic POST `<base>/<path>` with `body` as JSON. Used by every
    /// `create_X` method. Body is pre-stripped of server-managed fields
    /// by the caller (`strip_for_create` in `src/snapshot/create.rs`).
    async fn post_json<TResp>(&self, path: &str, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<TResp>
    where
        TResp: serde::de::DeserializeOwned,
    {
        let url = format!("{}{}", self.base_url, path);
        let resp = retry::send_with_retry(
            || self.http
                .post(&url)
                .header("Authorization", format!("token {}", self.token))
                .json(body),
            &format!("POST {url}"),
            progress,
        ).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body }.into());
        }
        resp.json::<TResp>().await
            .with_context(|| format!("decoding POST response from {url}"))
    }

    pub async fn create_hook(&self, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<Hook> {
        self.post_json("/hooks", body, progress).await
    }

    pub async fn create_workspace(&self, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::Workspace> {
        self.post_json("/workspaces", body, progress).await
    }

    pub async fn create_queue(&self, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::Queue> {
        self.post_json("/queues", body, progress).await
    }

    pub async fn create_schema(&self, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::Schema> {
        self.post_json("/schemas", body, progress).await
    }

    pub async fn create_inbox(&self, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::Inbox> {
        self.post_json("/inboxes", body, progress).await
    }

    pub async fn create_label(&self, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::Label> {
        self.post_json("/labels", body, progress).await
    }

    pub async fn create_rule(&self, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::Rule> {
        self.post_json("/rules", body, progress).await
    }

    pub async fn create_email_template(&self, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::EmailTemplate> {
        self.post_json("/email_templates", body, progress).await
    }

    pub async fn create_engine(&self, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::Engine> {
        self.post_json("/engines", body, progress).await
    }

    pub async fn create_engine_field(&self, body: &serde_json::Value, progress: Option<std::sync::Arc<crate::progress::OverallProgress>>) -> Result<crate::model::EngineField> {
        self.post_json("/engine_fields", body, progress).await
    }
}
