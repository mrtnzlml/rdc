pub mod data_storage;
pub mod error;
pub mod rate_limit;
pub mod retry;

pub use data_storage::DataStorageClient;
pub use error::{anyhow_has_status, anyhow_status_env, ApiError};
pub use rate_limit::RateLimiter;

use crate::model::{
    EmailTemplate, Engine, EngineField, Hook, HookTemplate, Inbox, Label, Organization, Queue,
    Rule, Schema, User, Workflow, WorkflowStep, Workspace,
};
use crate::api::retry::ProgressHandle;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;

/// Rossum API client. Holds a base URL (e.g. `https://X.rossum.app/api/v1`)
/// and a static API token. Pagination is followed transparently for `list_*`
/// methods; PATCH and POST calls go through shared `patch_json`/`post_json`
/// helpers which retry on 429 / 502 / 503 / 504 via `retry::send_with_retry`.
pub struct RossumClient {
    base_url: String,
    token: String,
    http: Client,
    /// Env name (e.g. `"dev-mtr"`) attached to errors via [`EnvTag`] so a
    /// caller juggling multiple clients (notably `rdc deploy`, which holds
    /// src+tgt) can attribute a 401 back to the right env and refresh its
    /// token. `None` means errors are not env-tagged.
    env: Option<String>,
    /// Per-token client-side rate limiter. Defaults to the
    /// `default.core_api` policy Rossum enforces server-side (10 req/s,
    /// burst 10). `Arc` so all in-flight calls share one bucket; two
    /// clients (e.g. deploy's src + tgt) get independent buckets which
    /// matches the server's per-token scope.
    limiter: Arc<RateLimiter>,
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

/// Construct the shared reqwest Client used by every rdc HTTP path.
///
/// Single source of truth so a future change (timeout, user-agent, TLS
/// tweak, proxy config) lands in one place instead of three.
///
/// Nagle is disabled: rdc's hot path is many small JSON requests
/// (pagination + per-resource fetches), and the ~40 ms segment-delay
/// Nagle adds compounds noticeably across hundreds of round-trips
/// during a full sync.
pub(crate) fn build_http_client() -> Result<Client> {
    Client::builder()
        .tcp_nodelay(true)
        .build()
        .map_err(|e| anyhow::anyhow!("building reqwest client: {e}"))
}

impl RossumClient {
    pub fn new(base_url: String, token: String) -> Result<Self> {
        let http = build_http_client()?;
        Ok(Self {
            base_url,
            token,
            http,
            env: None,
            limiter: Arc::new(RateLimiter::rossum_core_api()),
        })
    }

    /// Attach an env label so any non-2xx error this client produces
    /// carries the env name in its `ApiError::Status { env, .. }`. Used by
    /// `rdc deploy`, which holds two clients (src + tgt); the retry
    /// wrapper inspects the tag to know which env's token to refresh on a
    /// 401.
    pub fn with_env_label(mut self, env: impl Into<String>) -> Self {
        self.env = Some(env.into());
        self
    }

    // --- list endpoints (paginated) -----------------------------------

    pub async fn list_hooks(&self, progress: ProgressHandle) -> Result<Vec<Hook>> {
        self.list_paginated("/hooks", progress).await
    }

    pub async fn list_workspaces(&self, progress: ProgressHandle) -> Result<Vec<Workspace>> {
        self.list_paginated("/workspaces", progress).await
    }

    pub async fn list_queues(&self, progress: ProgressHandle) -> Result<Vec<Queue>> {
        self.list_paginated("/queues", progress).await
    }

    pub async fn list_rules(&self, progress: ProgressHandle) -> Result<Vec<Rule>> {
        self.list_paginated("/rules", progress).await
    }

    pub async fn list_labels(&self, progress: ProgressHandle) -> Result<Vec<Label>> {
        self.list_paginated("/labels", progress).await
    }

    pub async fn list_engines(&self, progress: ProgressHandle) -> Result<Vec<Engine>> {
        self.list_paginated("/engines", progress).await
    }

    pub async fn list_engine_fields(&self, progress: ProgressHandle) -> Result<Vec<EngineField>> {
        self.list_paginated("/engine_fields", progress).await
    }

    pub async fn list_workflows(&self, progress: ProgressHandle) -> Result<Vec<Workflow>> {
        self.list_paginated("/workflows", progress).await
    }

    pub async fn list_workflow_steps(&self, progress: ProgressHandle) -> Result<Vec<WorkflowStep>> {
        self.list_paginated("/workflow_steps", progress).await
    }

    pub async fn list_email_templates(&self, progress: ProgressHandle) -> Result<Vec<EmailTemplate>> {
        self.list_paginated("/email_templates", progress).await
    }

    pub async fn list_hook_templates(&self, progress: ProgressHandle) -> Result<Vec<HookTemplate>> {
        self.list_paginated("/hook_templates", progress).await
    }

    pub async fn list_users(&self, progress: ProgressHandle) -> Result<Vec<User>> {
        self.list_paginated("/users", progress).await
    }

    // --- get endpoints ------------------------------------------------

    pub async fn get_organization(&self, id: u64, progress: ProgressHandle) -> Result<Organization> {
        self.get_json(&format!("{}/organizations/{id}", self.base_url), progress).await
    }

    pub async fn get_hook(&self, id: u64, progress: ProgressHandle) -> Result<Hook> {
        self.get_json(&format!("{}/hooks/{id}", self.base_url), progress).await
    }

    pub async fn get_workspace(&self, id: u64, progress: ProgressHandle) -> Result<Workspace> {
        self.get_json(&format!("{}/workspaces/{id}", self.base_url), progress).await
    }

    pub async fn get_inbox(&self, id: u64, progress: ProgressHandle) -> Result<Inbox> {
        self.get_json(&format!("{}/inboxes/{id}", self.base_url), progress).await
    }

    pub async fn get_schema(&self, id: u64, progress: ProgressHandle) -> Result<Schema> {
        self.get_json(&format!("{}/schemas/{id}", self.base_url), progress).await
    }

    /// `GET /hooks/<id>/secrets_keys` — list the secret key names
    /// configured on a hook. The Rossum API returns the keys only, never
    /// the values (those are server-side encrypted). Used by deploy to
    /// check that the target env has values for every key the source
    /// hook depends on before any write hits the target.
    ///
    /// Path note: the Rossum endpoint is `/secrets_keys` (with `s` on
    /// `secrets`, no hyphen) — verified against the live API and the
    /// existing rossum-api MCP server source. The simpler-looking
    /// variants (`/secrets`, `/secret_keys`, `/secret-keys`) all 404.
    pub async fn get_hook_secrets_keys(&self, id: u64, progress: ProgressHandle) -> Result<Vec<String>> {
        self.get_json(&format!("{}/hooks/{id}/secrets_keys", self.base_url), progress).await
    }

    // --- create endpoints ---------------------------------------------

    pub async fn create_hook(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Hook> {
        self.post_json("/hooks", body, progress).await
    }

    /// POST `/hooks/create` — the Rossum store install endpoint. Unlike
    /// `create_hook` (which posts to `/hooks/`), this accepts a minimal body
    /// `{name, hook_template, events, queues, token_owner}` and the server
    /// fills in the rest from the referenced template (per the template's
    /// `install_action: "copy"`). Required for store extensions because
    /// `POST /hooks/` rejects them with 400 (`config.url` is required for
    /// webhook-type hooks, but store webhooks have `config.private: true`
    /// and no URL).
    pub async fn create_hook_via_install(
        &self,
        body: &serde_json::Value,
        progress: ProgressHandle,
    ) -> Result<Hook> {
        self.post_json("/hooks/create", body, progress).await
    }

    pub async fn create_workspace(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Workspace> {
        self.post_json("/workspaces", body, progress).await
    }

    pub async fn create_queue(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Queue> {
        self.post_json("/queues", body, progress).await
    }

    pub async fn create_schema(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Schema> {
        self.post_json("/schemas", body, progress).await
    }

    pub async fn create_inbox(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Inbox> {
        self.post_json("/inboxes", body, progress).await
    }

    pub async fn create_label(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Label> {
        self.post_json("/labels", body, progress).await
    }

    pub async fn create_rule(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Rule> {
        self.post_json("/rules", body, progress).await
    }

    pub async fn create_email_template(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<EmailTemplate> {
        self.post_json("/email_templates", body, progress).await
    }

    pub async fn create_engine(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<Engine> {
        self.post_json("/engines", body, progress).await
    }

    pub async fn create_engine_field(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<EngineField> {
        self.post_json("/engine_fields", body, progress).await
    }

    // --- update endpoints (PATCH) -------------------------------------

    pub async fn update_hook(&self, id: u64, hook: &Hook, progress: ProgressHandle) -> Result<Hook> {
        self.patch_json(&format!("/hooks/{id}"), hook, progress).await
    }

    /// `PATCH /hooks/<id>` with a raw JSON body. Used when the outbound
    /// payload contains fields not represented on the `Hook` model —
    /// notably the write-only top-level `secrets` map, which `GET /hooks`
    /// never returns and which therefore has no place on the typed
    /// model. The body is sent through the same retry pipeline as
    /// `update_hook` and the response is decoded back to a `Hook`.
    pub async fn update_hook_value(&self, id: u64, body: &serde_json::Value, progress: ProgressHandle) -> Result<Hook> {
        self.patch_json(&format!("/hooks/{id}"), body, progress).await
    }

    pub async fn update_workspace(&self, id: u64, workspace: &Workspace, progress: ProgressHandle) -> Result<Workspace> {
        self.patch_json(&format!("/workspaces/{id}"), workspace, progress).await
    }

    pub async fn update_queue(&self, id: u64, queue: &Queue, progress: ProgressHandle) -> Result<Queue> {
        self.patch_json(&format!("/queues/{id}"), queue, progress).await
    }

    pub async fn update_schema(&self, id: u64, schema: &Schema, progress: ProgressHandle) -> Result<Schema> {
        self.patch_json(&format!("/schemas/{id}"), schema, progress).await
    }

    pub async fn update_inbox(&self, id: u64, inbox: &Inbox, progress: ProgressHandle) -> Result<Inbox> {
        self.patch_json(&format!("/inboxes/{id}"), inbox, progress).await
    }

    pub async fn update_email_template(&self, id: u64, t: &EmailTemplate, progress: ProgressHandle) -> Result<EmailTemplate> {
        self.patch_json(&format!("/email_templates/{id}"), t, progress).await
    }

    pub async fn update_rule(&self, id: u64, rule: &Rule, progress: ProgressHandle) -> Result<Rule> {
        self.patch_json(&format!("/rules/{id}"), rule, progress).await
    }

    pub async fn update_label(&self, id: u64, label: &Label, progress: ProgressHandle) -> Result<Label> {
        self.patch_json(&format!("/labels/{id}"), label, progress).await
    }

    pub async fn update_engine(&self, id: u64, engine: &Engine, progress: ProgressHandle) -> Result<Engine> {
        self.patch_json(&format!("/engines/{id}"), engine, progress).await
    }

    pub async fn update_engine_field(&self, id: u64, field: &EngineField, progress: ProgressHandle) -> Result<EngineField> {
        self.patch_json(&format!("/engine_fields/{id}"), field, progress).await
    }

    // --- delete endpoints (DELETE) ------------------------------------
    //
    // Used by `rdc deploy --mirror`, which prunes tgt-only resources so
    // PROD becomes exactly TEST. Mirror mode is opt-in and gated behind
    // an interactive confirmation; same-env `rdc push` never deletes.

    /// Generic DELETE `<base>/<path>`. Accepts 204 (deleted) and 404
    /// (already gone) as success; surfaces every other non-2xx.
    pub async fn delete_path(&self, path: &str, progress: ProgressHandle) -> Result<()> {
        let url = format!("{}{}", self.base_url, path);
        let resp = retry::send_with_retry(
            || self.http
                .delete(&url)
                .header("Authorization", format!("token {}", self.token)),
            &format!("DELETE {url}"),
            progress,
            Some(&self.limiter),
        ).await?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(ApiError::Status { status: status.as_u16(), body, env: self.env.clone() }.into())
    }

    pub async fn delete_hook(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/hooks/{id}"), progress).await
    }
    pub async fn delete_workspace(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/workspaces/{id}"), progress).await
    }
    pub async fn delete_queue(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/queues/{id}"), progress).await
    }
    pub async fn delete_schema(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/schemas/{id}"), progress).await
    }
    pub async fn delete_inbox(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/inboxes/{id}"), progress).await
    }
    pub async fn delete_email_template(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/email_templates/{id}"), progress).await
    }
    pub async fn delete_rule(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/rules/{id}"), progress).await
    }
    pub async fn delete_label(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/labels/{id}"), progress).await
    }
    pub async fn delete_engine(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/engines/{id}"), progress).await
    }
    pub async fn delete_engine_field(&self, id: u64, progress: ProgressHandle) -> Result<()> {
        self.delete_path(&format!("/engine_fields/{id}"), progress).await
    }

    // --- private helpers ----------------------------------------------

    /// Fetch every page of `<base>/<path>` and concatenate `results`.
    /// Used by every `list_*` method.
    async fn list_paginated<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        progress: ProgressHandle,
    ) -> Result<Vec<T>> {
        let mut url = format!("{}{}", self.base_url, path);
        let mut out = Vec::new();
        loop {
            let page: Page<T> = self.get_json(&url, progress.clone()).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str, progress: ProgressHandle) -> Result<T> {
        let resp = retry::send_with_retry(
            || self.http.get(url).header("Authorization", format!("token {}", self.token)),
            &format!("GET {url}"),
            progress,
            Some(&self.limiter),
        ).await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body, env: self.env.clone() }.into());
        }
        resp.json::<T>().await
            .with_context(|| format!("decoding response from {url}"))
    }

    /// Public escape hatch for cross-env apply, which builds a stripped
    /// JSON body (no id/url/organization, no server-computed sub-collections
    /// like `queue.hooks`) and sends it via PATCH. The body has already been
    /// shaped by the caller so we don't go through a typed struct.
    pub async fn patch_value(&self, path: &str, body: &serde_json::Value, progress: ProgressHandle) -> Result<serde_json::Value> {
        self.patch_json(path, body, progress).await
    }

    /// Generic PATCH `<base>/<path>` with `body` as JSON. Used by every
    /// `update_*` method. Centralises 429 retry/backoff via `retry::send_with_retry`.
    async fn patch_json<TBody, TResp>(&self, path: &str, body: &TBody, progress: ProgressHandle) -> Result<TResp>
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
            Some(&self.limiter),
        ).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body, env: self.env.clone() }.into());
        }
        resp.json::<TResp>().await
            .with_context(|| format!("decoding PATCH response from {url}"))
    }

    /// Generic POST `<base>/<path>` with `body` as JSON. Used by every
    /// `create_*` method. Body is pre-stripped of server-managed fields
    /// by the caller (`strip_for_create` in `src/snapshot/create.rs`).
    async fn post_json<TResp>(&self, path: &str, body: &serde_json::Value, progress: ProgressHandle) -> Result<TResp>
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
            Some(&self.limiter),
        ).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body, env: self.env.clone() }.into());
        }
        resp.json::<TResp>().await
            .with_context(|| format!("decoding POST response from {url}"))
    }
}

/// Exchange username/password for an API token via
/// `POST /v1/auth/login`. Returns the issued `key`.
///
/// This is a free function rather than a method on [`RossumClient`]
/// because login doesn't take a token (it produces one). Used by
/// `secrets::resolve_token` to obtain a fresh token when the cache is
/// missing/expired and `RDC_USER_<ENV>` + `RDC_PASS_<ENV>` are set,
/// and by `cli::auth::run` to handle `rdc auth <env> --username <u>`.
///
/// Retries on transient 429/502/503/504 via [`retry::send_with_retry`].
/// 401 is **not** retried (a bad password isn't going to fix itself).
pub async fn login(api_base: &str, username: &str, password: &str) -> Result<String> {
    let http = build_http_client()?;
    let url = format!("{api_base}/auth/login");
    let body = serde_json::json!({
        "username": username,
        "password": password,
    });
    let progress: ProgressHandle = None;
    let resp = retry::send_with_retry(
        || http.post(&url).json(&body),
        &format!("POST {url}"),
        progress.clone(),
        None, // no rate limiter — login is rare
    )
    .await?;
    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        return Err(ApiError::Status {
            status: status.as_u16(),
            body: body_text,
            env: None,
        }
        .into());
    }
    #[derive(Deserialize)]
    struct LoginResponse {
        key: String,
    }
    let parsed: LoginResponse = resp
        .json()
        .await
        .with_context(|| format!("decoding login response from {url}"))?;
    Ok(parsed.key)
}
